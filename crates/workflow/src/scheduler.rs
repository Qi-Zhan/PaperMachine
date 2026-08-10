use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;
use papermachine_protocol::HumanRequestStatus;
use papermachine_protocol::SessionId;
use papermachine_protocol::SessionStatus;
use papermachine_protocol::SessionUsage;
#[cfg(test)]
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_store::StoreHandle;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::ActionRunner;

pub type SessionOutcome = Result<Value, String>;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionSuspension {
    pub status: SessionStatus,
    pub wake_at: Option<DateTime<Utc>>,
}

impl SessionSuspension {
    pub const fn new(status: SessionStatus, wake_at: Option<DateTime<Utc>>) -> Self {
        Self { status, wake_at }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionExecution {
    Completed(Value),
    Suspended(SessionSuspension),
}

#[async_trait]
pub trait SessionExecutor: Send + Sync {
    async fn execute(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> Result<SessionExecution, String>;
}

#[derive(Clone)]
pub struct SessionScheduler {
    inner: Arc<SessionSchedulerInner>,
}

struct SessionSchedulerInner {
    store: StoreHandle,
    executor: Arc<dyn SessionExecutor>,
    action_runner: Option<ActionRunner>,
    permits: Arc<Semaphore>,
    handles: Mutex<HashMap<SessionId, ScheduledRun>>,
}

struct ScheduledRun {
    cancellation: CancellationToken,
    outcome: watch::Receiver<Option<SessionOutcome>>,
}

impl SessionScheduler {
    pub fn new(
        store: StoreHandle,
        executor: Arc<dyn SessionExecutor>,
        max_concurrent_runs: usize,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(max_concurrent_runs.max(1)));
        Self {
            inner: Arc::new(SessionSchedulerInner {
                store,
                executor,
                action_runner: None,
                permits,
                handles: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn new_with_permits(
        store: StoreHandle,
        executor: Arc<dyn SessionExecutor>,
        permits: Arc<Semaphore>,
        action_runner: ActionRunner,
    ) -> Self {
        Self {
            inner: Arc::new(SessionSchedulerInner {
                store,
                executor,
                action_runner: Some(action_runner),
                permits,
                handles: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn start(&self, session_id: SessionId) -> Result<bool, SessionSchedulerError> {
        let session = self
            .inner
            .store
            .call(move |store| store.get_session(session_id))
            .await?;
        if session.status.is_terminal() {
            return Err(SessionSchedulerError::TerminalSession {
                session_id,
                status: session.status,
            });
        }
        let mut handles = self.inner.handles.lock().await;
        if handles.contains_key(&session_id) {
            return Ok(false);
        }
        let cancellation = CancellationToken::new();
        let (outcome_tx, outcome_rx) = watch::channel(None);
        handles.insert(
            session_id,
            ScheduledRun {
                cancellation: cancellation.clone(),
                outcome: outcome_rx,
            },
        );
        drop(handles);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let outcome = run_scheduled(Arc::clone(&inner), session_id, cancellation).await;
            inner.handles.lock().await.remove(&session_id);
            let _ = outcome_tx.send(Some(outcome));
        });
        Ok(true)
    }

    /// Restarts every non-terminal Session. The Python program executes from
    /// its snapshotted source while deterministic effects replay from the
    /// durable journal; an unfinished Action resumes its checkpointed Turn.
    pub async fn recover(&self) -> Result<Vec<SessionId>, SessionSchedulerError> {
        let mut started = Vec::new();
        let sessions = self
            .inner
            .store
            .call(|store| store.list_recoverable_sessions())
            .await?;
        for session in sessions {
            if self.start(session.id).await? {
                started.push(session.id);
            }
        }
        Ok(started)
    }

    pub async fn pause(&self, session_id: SessionId) -> Result<(), SessionSchedulerError> {
        let session = self
            .inner
            .store
            .call(move |store| store.get_session(session_id))
            .await?;
        if session.status.is_terminal() {
            return Err(SessionSchedulerError::TerminalSession {
                session_id,
                status: session.status,
            });
        }
        self.inner
            .store
            .call(move |store| store.pause_session(session_id, Some("paused by user".to_string())))
            .await?;
        Ok(())
    }

    pub async fn resume(&self, session_id: SessionId) -> Result<(), SessionSchedulerError> {
        let session = self
            .inner
            .store
            .call(move |store| store.get_session(session_id))
            .await?;
        if session.status.is_terminal() {
            return Err(SessionSchedulerError::TerminalSession {
                session_id,
                status: session.status,
            });
        }
        self.inner
            .store
            .call(move |store| store.resume_session(session_id))
            .await?;
        self.start(session_id).await?;
        Ok(())
    }

    pub async fn cancel(&self, session_id: SessionId) -> Result<(), SessionSchedulerError> {
        let session = self
            .inner
            .store
            .call(move |store| store.get_session(session_id))
            .await?;
        if session.status.is_terminal() {
            return Err(SessionSchedulerError::TerminalSession {
                session_id,
                status: session.status,
            });
        }
        self.inner
            .store
            .call(move |store| {
                store.begin_session_closing(
                    session_id,
                    SessionStatus::Cancelled,
                    None,
                    Some("cancelled by user".to_string()),
                )
            })
            .await?;
        if let Some(handle) = self.inner.handles.lock().await.get(&session_id) {
            handle.cancellation.cancel();
        }
        Ok(())
    }

    pub async fn wait(
        &self,
        session_id: SessionId,
    ) -> Result<SessionOutcome, SessionSchedulerError> {
        let outcome = self
            .inner
            .handles
            .lock()
            .await
            .get(&session_id)
            .map(|handle| handle.outcome.clone());
        let Some(mut outcome) = outcome else {
            return self.persisted_outcome(session_id).await;
        };
        loop {
            if let Some(result) = outcome.borrow().clone() {
                return Ok(result);
            }
            if outcome.changed().await.is_err() {
                return self.persisted_outcome(session_id).await;
            }
        }
    }

    async fn persisted_outcome(
        &self,
        session_id: SessionId,
    ) -> Result<SessionOutcome, SessionSchedulerError> {
        let session = self
            .inner
            .store
            .call(move |store| store.get_session(session_id))
            .await?;
        if !session.status.is_terminal() {
            return Err(SessionSchedulerError::NotScheduled(session_id));
        }
        Ok(match session.status {
            SessionStatus::Completed => session.output.ok_or_else(|| {
                session
                    .error
                    .unwrap_or_else(|| "Session completed without output".to_string())
            }),
            SessionStatus::Failed | SessionStatus::Cancelled => Err(session
                .error
                .unwrap_or_else(|| format!("Session ended as {:?}", session.status))),
            _ => unreachable!("terminal status checked above"),
        })
    }
}

async fn run_scheduled(
    inner: Arc<SessionSchedulerInner>,
    session_id: SessionId,
    cancellation: CancellationToken,
) -> SessionOutcome {
    let Some(action_runner) = inner.action_runner.clone() else {
        let outcome = run_workflow(Arc::clone(&inner), session_id, cancellation).await;
        let current = inner
            .store
            .call(move |store| store.get_session(session_id))
            .await
            .map_err(|error| error.to_string())?;
        return if current.status == SessionStatus::Closing {
            finish_closing(&inner.store, session_id).await
        } else {
            outcome
        };
    };
    let current = inner
        .store
        .call(move |store| store.get_session(session_id))
        .await
        .map_err(|error| error.to_string())?;
    if current.status == SessionStatus::Closing {
        action_runner
            .run_session(session_id, cancellation.child_token())
            .await
            .map_err(|error| error.to_string())?;
        let session = inner
            .store
            .call(move |store| store.finish_session_closing(session_id))
            .await
            .map_err(|error| error.to_string())?;
        return session_outcome(session);
    }
    let action_cancellation = cancellation.child_token();
    let workflow_cancellation = cancellation.child_token();
    let mut action_task = tokio::spawn({
        let action_cancellation = action_cancellation.clone();
        async move {
            action_runner
                .run_session(session_id, action_cancellation)
                .await
                .map_err(|error| error.to_string())
        }
    });
    let mut workflow_task = tokio::spawn(run_workflow(
        Arc::clone(&inner),
        session_id,
        workflow_cancellation.clone(),
    ));
    tokio::select! {
        result = &mut workflow_task => {
            action_task
                .await
                .map_err(|error| format!("Action runner task failed: {error}"))??;
            let outcome = result.map_err(|error| format!("Session runtime task failed: {error}"))?;
            finish_closing(&inner.store, session_id).await?;
            outcome
        }
        result = &mut action_task => {
            let result = result
                .map_err(|error| format!("Action runner task failed: {error}"))?;
            match result {
                Ok(()) => {
                    let outcome = workflow_task
                        .await
                        .map_err(|error| format!("Session runtime task failed: {error}"))?;
                    finish_closing(&inner.store, session_id).await?;
                    outcome
                }
                Err(error) => {
                    workflow_cancellation.cancel();
                    let _ = workflow_task.await;
                    let current = inner
                        .store
                        .call(move |store| store.get_session(session_id))
                        .await
                        .map_err(|store_error| store_error.to_string())?;
                    if !current.status.is_terminal() {
                        inner
                            .store
                            .call({
                                let error = error.clone();
                                move |store| {
                                    store.begin_session_closing(
                                        session_id,
                                        SessionStatus::Failed,
                                        None,
                                        Some(error),
                                    )
                                }
                            })
                            .await
                            .map_err(|store_error| store_error.to_string())?;
                    }
                    Err(error)
                }
            }
        }
    }
}

async fn run_workflow(
    inner: Arc<SessionSchedulerInner>,
    session_id: SessionId,
    cancellation: CancellationToken,
) -> SessionOutcome {
    let mut wake_at_hint = None;
    loop {
        let current = inner
            .store
            .call(move |store| store.get_session(session_id))
            .await
            .map_err(|error| error.to_string())?;
        if current.status.is_terminal() {
            return current.output.ok_or_else(|| {
                current
                    .error
                    .unwrap_or_else(|| format!("Session ended as {:?}", current.status))
            });
        }
        if !matches!(
            current.status,
            SessionStatus::Created | SessionStatus::Running
        ) {
            wait_until_runnable(&inner.store, session_id, &cancellation, wake_at_hint.take())
                .await?;
            continue;
        }

        let permit = tokio::select! {
            permit = Arc::clone(&inner.permits).acquire_owned() => permit.map_err(|error| error.to_string())?,
            _ = cancellation.cancelled() => return Err("cancelled before execution started".to_string()),
        };
        let mut session = inner
            .store
            .call(move |store| store.get_session(session_id))
            .await
            .map_err(|error| error.to_string())?;
        if session.status == SessionStatus::Created {
            session = inner
                .store
                .call(move |store| store.start_session(session_id))
                .await
                .map_err(|error| error.to_string())?;
        }
        if session.status != SessionStatus::Running {
            drop(permit);
            continue;
        }

        let started = Instant::now();
        let execution = cancellation.child_token();
        let result = inner.executor.execute(session_id, execution).await;
        let elapsed = started
            .elapsed()
            .as_secs()
            .max(u64::from(!started.elapsed().is_zero()));
        inner
            .store
            .call(move |store| {
                store.add_session_usage(
                    session_id,
                    SessionUsage {
                        wall_time_seconds: elapsed,
                        ..SessionUsage::default()
                    },
                )
            })
            .await
            .map_err(|error| error.to_string())?;
        drop(permit);

        let current = inner
            .store
            .call(move |store| store.get_session(session_id))
            .await
            .map_err(|error| error.to_string())?;
        if cancellation.is_cancelled() {
            if !current.status.is_terminal() && current.status != SessionStatus::Closing {
                inner
                    .store
                    .call(move |store| {
                        store.begin_session_closing(
                            session_id,
                            SessionStatus::Cancelled,
                            None,
                            Some("cancelled by user".to_string()),
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
            }
            return Err("cancelled by user".to_string());
        }
        if current.status.is_terminal() {
            continue;
        }
        match result {
            Ok(SessionExecution::Completed(output)) => {
                inner
                    .store
                    .call({
                        let output = output.clone();
                        move |store| {
                            store.begin_session_closing(
                                session_id,
                                SessionStatus::Completed,
                                Some(output),
                                None,
                            )
                        }
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(output);
            }
            Ok(SessionExecution::Suspended(suspension)) => {
                if current.status != suspension.status {
                    match suspension.status {
                        SessionStatus::WaitingForInput => {
                            inner
                                .store
                                .call(move |store| store.wait_session_for_input(session_id))
                                .await
                        }
                        SessionStatus::WaitingForDeadline => {
                            inner
                                .store
                                .call(move |store| store.wait_session_for_deadline(session_id))
                                .await
                        }
                        status => Err(StoreError::Invariant(format!(
                            "Session runtime returned invalid suspension status {status:?}"
                        ))),
                    }
                    .map_err(|error| error.to_string())?;
                }
                wake_at_hint = suspension.wake_at;
            }
            Err(error) => {
                inner
                    .store
                    .call({
                        let error = error.clone();
                        move |store| {
                            store.begin_session_closing(
                                session_id,
                                SessionStatus::Failed,
                                None,
                                Some(error),
                            )
                        }
                    })
                    .await
                    .map_err(|store_error| store_error.to_string())?;
                return Err(error);
            }
        }
    }
}

async fn finish_closing(store: &StoreHandle, session_id: SessionId) -> SessionOutcome {
    let session = store
        .call(move |store| store.finish_session_closing(session_id))
        .await
        .map_err(|error| error.to_string())?;
    session_outcome(session)
}

fn session_outcome(session: papermachine_protocol::Session) -> SessionOutcome {
    match session.status {
        SessionStatus::Completed => session.output.ok_or_else(|| {
            session
                .error
                .unwrap_or_else(|| "Session completed without output".to_string())
        }),
        SessionStatus::Failed | SessionStatus::Cancelled => Err(session
            .error
            .unwrap_or_else(|| format!("Session ended as {:?}", session.status))),
        status => Err(format!("Session did not finish Closing: {status:?}")),
    }
}

async fn wait_until_runnable(
    store: &StoreHandle,
    session_id: SessionId,
    cancellation: &CancellationToken,
    wake_at_hint: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let mut events = store
        .call::<_, StoreError, _>(|store| Ok(store.subscribe()))
        .await
        .map_err(|error| error.to_string())?;
    loop {
        if cancellation.is_cancelled() {
            return Err("cancelled while Session was suspended".to_string());
        }
        let session = store
            .call::<_, StoreError, _>(move |store| store.get_session(session_id))
            .await
            .map_err(|error| error.to_string())?;
        match session.status {
            SessionStatus::Created | SessionStatus::Running => return Ok(()),
            SessionStatus::Closing
            | SessionStatus::Completed
            | SessionStatus::Failed
            | SessionStatus::Cancelled => {
                return Err(session
                    .error
                    .unwrap_or_else(|| format!("Session ended as {:?}", session.status)));
            }
            SessionStatus::Paused => {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err("cancelled while Session was paused".to_string()),
                    _ = events.recv() => {}
                }
            }
            SessionStatus::WaitingForInput => {
                let open_request = store
                    .call(move |store| {
                        Ok::<_, StoreError>(
                            store
                                .list_human_requests(session_id)?
                                .into_iter()
                                .any(|request| request.status == HumanRequestStatus::Open),
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                if !open_request {
                    store
                        .call(move |store| store.resume_session(session_id))
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                tokio::select! {
                    _ = cancellation.cancelled() => return Err("cancelled while Session was suspended".to_string()),
                    _ = events.recv() => {}
                }
            }
            SessionStatus::WaitingForDeadline => {
                let wake_at = wake_at_hint.ok_or_else(|| {
                    "Session deadline suspension is missing its wake time".to_string()
                })?;
                let wait = (wake_at - Utc::now()).to_std().unwrap_or_default();
                if wait.is_zero() {
                    store
                        .call(move |store| store.resume_session(session_id))
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                tokio::select! {
                    _ = cancellation.cancelled() => return Err("cancelled while Session was suspended".to_string()),
                    _ = tokio::time::sleep(wait) => {
                        store
                            .call(move |store| store.resume_session(session_id))
                            .await
                            .map_err(|error| error.to_string())?;
                        return Ok(());
                    }
                    _ = events.recv() => {}
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum SessionSchedulerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Session {session_id} is terminal with status {status:?}")]
    TerminalSession {
        session_id: SessionId,
        status: SessionStatus,
    },
    #[error("Session {0} is not scheduled in this process")]
    NotScheduled(SessionId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use papermachine_protocol::AccessPreset;
    use papermachine_protocol::Session;
    use papermachine_protocol::WorkflowProgramId;
    use papermachine_protocol::WorkflowProgramManifest;
    use papermachine_protocol::WorkflowProgramSnapshot;
    use papermachine_protocol::WorkflowProgramSource;
    use papermachine_store::NewSession;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;
    use tempfile::tempdir;

    struct StaticExecutor {
        output: Value,
    }

    struct SuspendOnceExecutor {
        suspend_once: HashSet<SessionId>,
        suspended: StdMutex<HashSet<SessionId>>,
    }

    struct BlockingExecutor;

    #[async_trait]
    impl SessionExecutor for BlockingExecutor {
        async fn execute(
            &self,
            _session_id: SessionId,
            cancellation: CancellationToken,
        ) -> Result<SessionExecution, String> {
            cancellation.cancelled().await;
            Err("cancelled by scheduler".to_string())
        }
    }

    #[async_trait]
    impl SessionExecutor for SuspendOnceExecutor {
        async fn execute(
            &self,
            session_id: SessionId,
            _cancellation: CancellationToken,
        ) -> Result<SessionExecution, String> {
            let should_suspend = self.suspend_once.contains(&session_id)
                && self
                    .suspended
                    .lock()
                    .map_err(|error| error.to_string())?
                    .insert(session_id);
            if should_suspend {
                Ok(SessionExecution::Suspended(SessionSuspension::new(
                    SessionStatus::WaitingForDeadline,
                    Some(Utc::now() + chrono::Duration::hours(1)),
                )))
            } else {
                Ok(SessionExecution::Completed(json!({
                    "session_id": session_id
                })))
            }
        }
    }

    #[async_trait]
    impl SessionExecutor for StaticExecutor {
        async fn execute(
            &self,
            _session_id: SessionId,
            _cancellation: CancellationToken,
        ) -> Result<SessionExecution, String> {
            Ok(SessionExecution::Completed(self.output.clone()))
        }
    }

    fn workflow() -> WorkflowProgramSnapshot {
        WorkflowProgramSnapshot {
            project_id: None,
            manifest: WorkflowProgramManifest {
                id: WorkflowProgramId::new(),
                slug: "scheduler-test".to_string(),
                name: "Scheduler test".to_string(),
                description: String::new(),
                entrypoint: "main".to_string(),
                request_mode: Default::default(),
                params_schema: json!({"type": "object"}),
            },
            source: WorkflowProgramSource::Builtin,
            definition_path: "builtin/scheduler-test/workflow.py".to_string(),
            sha256: "test".to_string(),
            runtime_sha256: "test-runtime".to_string(),
            source_code: String::new(),
        }
    }

    fn create_test_session(
        store: &Store,
        project_id: papermachine_protocol::ProjectId,
        objective: &str,
    ) -> Session {
        store
            .create_session(NewSession {
                project_id,
                program: workflow(),
                title: objective.to_string(),
                request: objective.to_string(),
                instructions: String::new(),
                trigger: Default::default(),
                params: json!({}),
                default_model: "test-model".to_string(),
                access: AccessPreset::Workspace,
                enabled_skills: Vec::new(),
                agent_access_overrides: Default::default(),
            })
            .expect("Session should be created")
    }

    #[tokio::test]
    async fn commits_output_only_after_final_usage_is_recorded() {
        let directory = tempdir().expect("temporary directory should be created");
        let store = Arc::new(
            Store::open_in_memory(directory.path().join("managed"))
                .expect("store should open in memory"),
        );
        let research = store
            .create_project("Scheduler", directory.path().join("project"))
            .expect("research should be created");
        let session = create_test_session(&store, research.id, "Run");
        let scheduler = SessionScheduler::new(
            StoreHandle::spawn((*store).clone()).expect("Store thread should start"),
            Arc::new(StaticExecutor {
                output: json!({"report": "done"}),
            }),
            1,
        );

        scheduler
            .start(session.id)
            .await
            .expect("Session should start");
        let output = scheduler
            .wait(session.id)
            .await
            .expect("Session should remain scheduled")
            .expect("execution should succeed");
        let completed = store
            .get_session(session.id)
            .expect("completed Session should load");

        assert_eq!(output, json!({"report": "done"}));
        assert_eq!(completed.status, SessionStatus::Completed);
        assert_eq!(completed.output, Some(output));
        assert_eq!(completed.usage.wall_time_seconds, 1);
        tokio::task::yield_now().await;
        assert!(
            !scheduler
                .inner
                .handles
                .lock()
                .await
                .contains_key(&session.id)
        );
        assert_eq!(
            scheduler
                .wait(session.id)
                .await
                .expect("late wait should read the durable terminal outcome")
                .expect("completed Session should retain its output"),
            json!({"report": "done"})
        );
    }

    #[tokio::test]
    async fn process_restart_recovers_running_and_created_sessions() {
        let directory = tempdir().expect("temporary directory should be created");
        let store = Arc::new(
            Store::open_in_memory(directory.path().join("managed"))
                .expect("store should open in memory"),
        );
        let research = store
            .create_project("Restart", directory.path().join("project"))
            .expect("research should be created");
        let running_run = create_test_session(&store, research.id, "Running");
        store
            .start_session(running_run.id)
            .expect("Session should be running");
        let created_run = create_test_session(&store, research.id, "Created");

        let scheduler = SessionScheduler::new(
            StoreHandle::spawn((*store).clone()).expect("Store thread should start"),
            Arc::new(StaticExecutor {
                output: json!({"report": "done"}),
            }),
            1,
        );
        let recovered = scheduler
            .recover()
            .await
            .expect("non-terminal runs should be recovered");
        assert_eq!(recovered.len(), 2);
        assert!(recovered.contains(&running_run.id));
        assert!(recovered.contains(&created_run.id));
        for session_id in [running_run.id, created_run.id] {
            scheduler
                .wait(session_id)
                .await
                .expect("recovered Session should remain scheduled")
                .expect("recovered Session should complete");
            assert_eq!(
                store
                    .get_session(session_id)
                    .expect("recovered Session should load")
                    .status,
                SessionStatus::Completed
            );
        }
    }

    #[tokio::test]
    async fn cancelling_a_running_session_reaches_its_executor() {
        let directory = tempdir().expect("temporary directory should be created");
        let store = Arc::new(
            Store::open_in_memory(directory.path().join("managed"))
                .expect("store should open in memory"),
        );
        let project = store
            .create_project("Cancellation", directory.path().join("workspace"))
            .expect("Project should be created");
        let session = create_test_session(&store, project.id, "Block until cancelled");
        let scheduler = SessionScheduler::new(
            StoreHandle::spawn((*store).clone()).expect("Store thread should start"),
            Arc::new(BlockingExecutor),
            1,
        );

        scheduler
            .start(session.id)
            .await
            .expect("Session should start");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if store
                    .get_session(session.id)
                    .expect("Session should load")
                    .status
                    == SessionStatus::Running
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Session should become running");
        scheduler
            .cancel(session.id)
            .await
            .expect("Session should cancel");
        assert!(
            tokio::time::timeout(Duration::from_secs(5), scheduler.wait(session.id))
                .await
                .expect("cancel should reach the executor promptly")
                .expect("cancelled Session should remain scheduled")
                .is_err()
        );
        assert_eq!(
            store
                .get_session(session.id)
                .expect("Session should load")
                .status,
            SessionStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn suspended_session_releases_the_global_execution_permit() {
        let directory = tempdir().expect("temporary directory should be created");
        let store = Arc::new(
            Store::open_in_memory(directory.path().join("managed"))
                .expect("store should open in memory"),
        );
        let research = store
            .create_project("Suspension", directory.path().join("project"))
            .expect("research should be created");
        let suspended_run = create_test_session(&store, research.id, "Suspend");
        let other_run = create_test_session(&store, research.id, "Other");
        let scheduler = SessionScheduler::new(
            StoreHandle::spawn((*store).clone()).expect("Store thread should start"),
            Arc::new(SuspendOnceExecutor {
                suspend_once: HashSet::from([suspended_run.id]),
                suspended: StdMutex::new(HashSet::new()),
            }),
            1,
        );

        scheduler
            .start(suspended_run.id)
            .await
            .expect("suspending Session should start");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if store
                    .get_session(suspended_run.id)
                    .expect("Session should load")
                    .status
                    == SessionStatus::WaitingForDeadline
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Session should suspend");

        scheduler
            .start(other_run.id)
            .await
            .expect("other Session should start");
        tokio::time::timeout(Duration::from_secs(5), scheduler.wait(other_run.id))
            .await
            .expect("other Session must not starve behind the suspended Session")
            .expect("other Session should remain scheduled")
            .expect("other Session should complete");

        store
            .resume_session(suspended_run.id)
            .expect("suspended Session should be resumed");
        tokio::time::timeout(Duration::from_secs(5), scheduler.wait(suspended_run.id))
            .await
            .expect("resumed Session should finish")
            .expect("resumed Session should remain scheduled")
            .expect("resumed Session should complete");
    }
}
