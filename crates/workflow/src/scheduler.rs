use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;
use papermachine_protocol::HumanRequestStatus;
use papermachine_protocol::WorkflowId;
use papermachine_protocol::WorkflowStatus;
use papermachine_protocol::WorkflowUsage;
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

pub type WorkflowOutcome = Result<Value, String>;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WorkflowSuspension {
    pub status: WorkflowStatus,
    pub wake_at: Option<DateTime<Utc>>,
}

impl WorkflowSuspension {
    pub const fn new(status: WorkflowStatus, wake_at: Option<DateTime<Utc>>) -> Self {
        Self { status, wake_at }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkflowExecution {
    Completed(Value),
    Suspended(WorkflowSuspension),
}

#[async_trait]
pub trait WorkflowRuntime: Send + Sync {
    async fn execute(
        &self,
        workflow_id: WorkflowId,
        cancellation: CancellationToken,
    ) -> Result<WorkflowExecution, String>;
}

#[derive(Clone)]
pub struct WorkflowScheduler {
    inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
    store: StoreHandle,
    executor: Arc<dyn WorkflowRuntime>,
    permits: Arc<Semaphore>,
    handles: Mutex<HashMap<WorkflowId, ScheduledRun>>,
}

struct ScheduledRun {
    cancellation: CancellationToken,
    outcome: watch::Receiver<Option<WorkflowOutcome>>,
}

impl WorkflowScheduler {
    pub fn new(
        store: StoreHandle,
        executor: Arc<dyn WorkflowRuntime>,
        max_concurrent_runs: usize,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(max_concurrent_runs.max(1)));
        Self::new_with_permits(store, executor, permits)
    }

    pub fn new_with_permits(
        store: StoreHandle,
        executor: Arc<dyn WorkflowRuntime>,
        permits: Arc<Semaphore>,
    ) -> Self {
        Self {
            inner: Arc::new(SchedulerInner {
                store,
                executor,
                permits,
                handles: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn start(&self, workflow_id: WorkflowId) -> Result<bool, WorkflowSchedulerError> {
        let run = self
            .inner
            .store
            .call(move |store| store.get_workflow(workflow_id))
            .await?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalWorkflow {
                workflow_id,
                status: run.status,
            });
        }
        let mut handles = self.inner.handles.lock().await;
        if handles.contains_key(&workflow_id) {
            return Ok(false);
        }
        let cancellation = CancellationToken::new();
        let (outcome_tx, outcome_rx) = watch::channel(None);
        handles.insert(
            workflow_id,
            ScheduledRun {
                cancellation: cancellation.clone(),
                outcome: outcome_rx,
            },
        );
        drop(handles);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let outcome = run_scheduled(Arc::clone(&inner), workflow_id, cancellation).await;
            inner.handles.lock().await.remove(&workflow_id);
            let _ = outcome_tx.send(Some(outcome));
        });
        Ok(true)
    }

    /// Restarts every non-terminal Workflow. The Python program executes from
    /// its snapshotted source while deterministic effects replay from the
    /// durable journal; an unfinished Action resumes its checkpointed Turn.
    pub async fn recover(&self) -> Result<Vec<WorkflowId>, WorkflowSchedulerError> {
        let mut started = Vec::new();
        let workflows = self
            .inner
            .store
            .call(|store| store.list_recoverable_workflows())
            .await?;
        for run in workflows {
            if self.start(run.id).await? {
                started.push(run.id);
            }
        }
        Ok(started)
    }

    pub async fn pause(&self, workflow_id: WorkflowId) -> Result<(), WorkflowSchedulerError> {
        let run = self
            .inner
            .store
            .call(move |store| store.get_workflow(workflow_id))
            .await?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalWorkflow {
                workflow_id,
                status: run.status,
            });
        }
        self.inner
            .store
            .call(move |store| {
                store.pause_workflow(workflow_id, Some("paused by user".to_string()))
            })
            .await?;
        Ok(())
    }

    pub async fn resume(&self, workflow_id: WorkflowId) -> Result<(), WorkflowSchedulerError> {
        let run = self
            .inner
            .store
            .call(move |store| store.get_workflow(workflow_id))
            .await?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalWorkflow {
                workflow_id,
                status: run.status,
            });
        }
        self.inner
            .store
            .call(move |store| store.resume_workflow(workflow_id))
            .await?;
        self.start(workflow_id).await?;
        Ok(())
    }

    pub async fn cancel(&self, workflow_id: WorkflowId) -> Result<(), WorkflowSchedulerError> {
        let run = self
            .inner
            .store
            .call(move |store| store.get_workflow(workflow_id))
            .await?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalWorkflow {
                workflow_id,
                status: run.status,
            });
        }
        self.inner
            .store
            .call(move |store| store.cancel_workflow(workflow_id, "cancelled by user"))
            .await?;
        if let Some(handle) = self.inner.handles.lock().await.get(&workflow_id) {
            handle.cancellation.cancel();
        }
        Ok(())
    }

    pub async fn wait(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<WorkflowOutcome, WorkflowSchedulerError> {
        let outcome = self
            .inner
            .handles
            .lock()
            .await
            .get(&workflow_id)
            .map(|handle| handle.outcome.clone());
        let Some(mut outcome) = outcome else {
            return self.persisted_outcome(workflow_id).await;
        };
        loop {
            if let Some(result) = outcome.borrow().clone() {
                return Ok(result);
            }
            if outcome.changed().await.is_err() {
                return self.persisted_outcome(workflow_id).await;
            }
        }
    }

    async fn persisted_outcome(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<WorkflowOutcome, WorkflowSchedulerError> {
        let workflow = self
            .inner
            .store
            .call(move |store| store.get_workflow(workflow_id))
            .await?;
        if !workflow.status.is_terminal() {
            return Err(WorkflowSchedulerError::NotScheduled(workflow_id));
        }
        Ok(match workflow.status {
            WorkflowStatus::Completed => workflow.output.ok_or_else(|| {
                workflow
                    .error
                    .unwrap_or_else(|| "Workflow completed without output".to_string())
            }),
            WorkflowStatus::Failed | WorkflowStatus::Cancelled => Err(workflow
                .error
                .unwrap_or_else(|| format!("Workflow ended as {:?}", workflow.status))),
            _ => unreachable!("terminal status checked above"),
        })
    }
}

async fn run_scheduled(
    inner: Arc<SchedulerInner>,
    workflow_id: WorkflowId,
    cancellation: CancellationToken,
) -> WorkflowOutcome {
    let mut wake_at_hint = None;
    loop {
        let current = inner
            .store
            .call(move |store| store.get_workflow(workflow_id))
            .await
            .map_err(|error| error.to_string())?;
        if current.status.is_terminal() {
            return current.output.ok_or_else(|| {
                current
                    .error
                    .unwrap_or_else(|| format!("Workflow ended as {:?}", current.status))
            });
        }
        if !matches!(
            current.status,
            WorkflowStatus::Created | WorkflowStatus::Running
        ) {
            wait_until_runnable(
                &inner.store,
                workflow_id,
                &cancellation,
                wake_at_hint.take(),
            )
            .await?;
            continue;
        }

        let permit = tokio::select! {
            permit = Arc::clone(&inner.permits).acquire_owned() => permit.map_err(|error| error.to_string())?,
            _ = cancellation.cancelled() => return Err("cancelled before execution started".to_string()),
        };
        let mut run = inner
            .store
            .call(move |store| store.get_workflow(workflow_id))
            .await
            .map_err(|error| error.to_string())?;
        if run.status == WorkflowStatus::Created {
            run = inner
                .store
                .call(move |store| store.start_workflow(workflow_id))
                .await
                .map_err(|error| error.to_string())?;
        }
        if run.status != WorkflowStatus::Running {
            drop(permit);
            continue;
        }

        let started = Instant::now();
        let execution = cancellation.child_token();
        let result = inner.executor.execute(workflow_id, execution).await;
        let elapsed = started
            .elapsed()
            .as_secs()
            .max(u64::from(!started.elapsed().is_zero()));
        inner
            .store
            .call(move |store| {
                store.add_workflow_usage(
                    workflow_id,
                    WorkflowUsage {
                        wall_time_seconds: elapsed,
                        ..WorkflowUsage::default()
                    },
                )
            })
            .await
            .map_err(|error| error.to_string())?;
        drop(permit);

        let current = inner
            .store
            .call(move |store| store.get_workflow(workflow_id))
            .await
            .map_err(|error| error.to_string())?;
        if cancellation.is_cancelled() {
            if !current.status.is_terminal() {
                inner
                    .store
                    .call(move |store| store.cancel_workflow(workflow_id, "cancelled by user"))
                    .await
                    .map_err(|error| error.to_string())?;
            }
            return Err("cancelled by user".to_string());
        }
        if current.status.is_terminal() {
            continue;
        }
        match result {
            Ok(WorkflowExecution::Completed(output)) => {
                inner
                    .store
                    .call({
                        let output = output.clone();
                        move |store| store.complete_workflow(workflow_id, output)
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(output);
            }
            Ok(WorkflowExecution::Suspended(suspension)) => {
                if current.status != suspension.status {
                    match suspension.status {
                        WorkflowStatus::WaitingForUser => {
                            inner
                                .store
                                .call(move |store| store.wait_workflow_for_user(workflow_id))
                                .await
                        }
                        WorkflowStatus::WaitingForDeadline => {
                            inner
                                .store
                                .call(move |store| store.wait_workflow_for_deadline(workflow_id))
                                .await
                        }
                        status => Err(StoreError::Invariant(format!(
                            "Workflow runtime returned invalid suspension status {status:?}"
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
                        move |store| store.fail_workflow(workflow_id, error)
                    })
                    .await
                    .map_err(|store_error| store_error.to_string())?;
                return Err(error);
            }
        }
    }
}

async fn wait_until_runnable(
    store: &StoreHandle,
    workflow_id: WorkflowId,
    cancellation: &CancellationToken,
    wake_at_hint: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let mut events = store
        .call::<_, StoreError, _>(|store| Ok(store.subscribe()))
        .await
        .map_err(|error| error.to_string())?;
    loop {
        if cancellation.is_cancelled() {
            return Err("cancelled while Workflow was suspended".to_string());
        }
        let run = store
            .call::<_, StoreError, _>(move |store| store.get_workflow(workflow_id))
            .await
            .map_err(|error| error.to_string())?;
        match run.status {
            WorkflowStatus::Created | WorkflowStatus::Running => return Ok(()),
            WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Cancelled => {
                return Err(run
                    .error
                    .unwrap_or_else(|| format!("Workflow ended as {:?}", run.status)));
            }
            WorkflowStatus::Paused => {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err("cancelled while Workflow was paused".to_string()),
                    _ = events.recv() => {}
                }
            }
            WorkflowStatus::WaitingForUser => {
                let open_request = store
                    .call(move |store| {
                        Ok::<_, StoreError>(
                            store
                                .list_human_requests(workflow_id)?
                                .into_iter()
                                .any(|request| request.status == HumanRequestStatus::Open),
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                if !open_request {
                    store
                        .call(move |store| store.resume_workflow(workflow_id))
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                tokio::select! {
                    _ = cancellation.cancelled() => return Err("cancelled while Workflow was suspended".to_string()),
                    _ = events.recv() => {}
                }
            }
            WorkflowStatus::WaitingForDeadline => {
                let wake_at = wake_at_hint.ok_or_else(|| {
                    "Workflow deadline suspension is missing its wake time".to_string()
                })?;
                let wait = (wake_at - Utc::now()).to_std().unwrap_or_default();
                if wait.is_zero() {
                    store
                        .call(move |store| store.resume_workflow(workflow_id))
                        .await
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                tokio::select! {
                    _ = cancellation.cancelled() => return Err("cancelled while Workflow was suspended".to_string()),
                    _ = tokio::time::sleep(wait) => {
                        store
                            .call(move |store| store.resume_workflow(workflow_id))
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
pub enum WorkflowSchedulerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Workflow {workflow_id} is terminal with status {status:?}")]
    TerminalWorkflow {
        workflow_id: WorkflowId,
        status: WorkflowStatus,
    },
    #[error("Workflow {0} is not scheduled in this process")]
    NotScheduled(WorkflowId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use papermachine_protocol::AccessPreset;
    use papermachine_protocol::Session;
    use papermachine_protocol::Workflow;
    use papermachine_protocol::WorkflowProgramId;
    use papermachine_protocol::WorkflowProgramManifest;
    use papermachine_protocol::WorkflowProgramSnapshot;
    use papermachine_protocol::WorkflowProgramSource;
    use papermachine_store::NewWorkflow;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;
    use tempfile::tempdir;

    struct StaticExecutor {
        output: Value,
    }

    struct SuspendOnceExecutor {
        suspend_once: HashSet<WorkflowId>,
        suspended: StdMutex<HashSet<WorkflowId>>,
    }

    struct BlockingExecutor;

    #[async_trait]
    impl WorkflowRuntime for BlockingExecutor {
        async fn execute(
            &self,
            _workflow_id: WorkflowId,
            cancellation: CancellationToken,
        ) -> Result<WorkflowExecution, String> {
            cancellation.cancelled().await;
            Err("cancelled by scheduler".to_string())
        }
    }

    #[async_trait]
    impl WorkflowRuntime for SuspendOnceExecutor {
        async fn execute(
            &self,
            workflow_id: WorkflowId,
            _cancellation: CancellationToken,
        ) -> Result<WorkflowExecution, String> {
            let should_suspend = self.suspend_once.contains(&workflow_id)
                && self
                    .suspended
                    .lock()
                    .map_err(|error| error.to_string())?
                    .insert(workflow_id);
            if should_suspend {
                Ok(WorkflowExecution::Suspended(WorkflowSuspension::new(
                    WorkflowStatus::WaitingForDeadline,
                    Some(Utc::now() + chrono::Duration::hours(1)),
                )))
            } else {
                Ok(WorkflowExecution::Completed(json!({
                    "workflow_id": workflow_id
                })))
            }
        }
    }

    #[async_trait]
    impl WorkflowRuntime for StaticExecutor {
        async fn execute(
            &self,
            _workflow_id: WorkflowId,
            _cancellation: CancellationToken,
        ) -> Result<WorkflowExecution, String> {
            Ok(WorkflowExecution::Completed(self.output.clone()))
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

    fn create_test_workflow(store: &Store, session: &Session, objective: &str) -> Workflow {
        store
            .create_workflow(NewWorkflow {
                project_id: session.project_id,
                started_from_session_id: Some(session.id),
                program: workflow(),
                request: objective.to_string(),
                instructions: String::new(),
                trigger: Default::default(),
                params: json!({}),
                default_model: "test-model".to_string(),
                access: AccessPreset::Research,
                enabled_skills: Vec::new(),
                agent_access_overrides: Default::default(),
            })
            .expect("workflow should be created")
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
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let run = create_test_workflow(&store, &session, "Run");
        let scheduler = WorkflowScheduler::new(
            StoreHandle::spawn((*store).clone()).expect("Store thread should start"),
            Arc::new(StaticExecutor {
                output: json!({"report": "done"}),
            }),
            1,
        );

        scheduler.start(run.id).await.expect("run should start");
        let output = scheduler
            .wait(run.id)
            .await
            .expect("run should remain scheduled")
            .expect("execution should succeed");
        let completed = store
            .get_workflow(run.id)
            .expect("completed run should load");

        assert_eq!(output, json!({"report": "done"}));
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(completed.output, Some(output));
        assert_eq!(completed.usage.wall_time_seconds, 1);
        tokio::task::yield_now().await;
        assert!(!scheduler.inner.handles.lock().await.contains_key(&run.id));
        assert_eq!(
            scheduler
                .wait(run.id)
                .await
                .expect("late wait should read the durable terminal outcome")
                .expect("completed Workflow should retain its output"),
            json!({"report": "done"})
        );
    }

    #[tokio::test]
    async fn process_restart_recovers_running_and_created_workflows() {
        let directory = tempdir().expect("temporary directory should be created");
        let store = Arc::new(
            Store::open_in_memory(directory.path().join("managed"))
                .expect("store should open in memory"),
        );
        let research = store
            .create_project("Restart", directory.path().join("project"))
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let running_run = create_test_workflow(&store, &session, "Running");
        store
            .start_workflow(running_run.id)
            .expect("run should be running");
        let created_run = create_test_workflow(&store, &session, "Created");

        let scheduler = WorkflowScheduler::new(
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
        for workflow_id in [running_run.id, created_run.id] {
            scheduler
                .wait(workflow_id)
                .await
                .expect("recovered run should remain scheduled")
                .expect("recovered run should complete");
            assert_eq!(
                store
                    .get_workflow(workflow_id)
                    .expect("recovered run should load")
                    .status,
                WorkflowStatus::Completed
            );
        }
    }

    #[tokio::test]
    async fn cancelling_a_running_workflow_reaches_its_executor() {
        let directory = tempdir().expect("temporary directory should be created");
        let store = Arc::new(
            Store::open_in_memory(directory.path().join("managed"))
                .expect("store should open in memory"),
        );
        let project = store
            .create_project("Cancellation", directory.path().join("workspace"))
            .expect("Project should be created");
        let session = store
            .create_session(project.id, "Origin", "", "test-model", Vec::new())
            .expect("Session should be created");
        let run = create_test_workflow(&store, &session, "Block until cancelled");
        let scheduler = WorkflowScheduler::new(
            StoreHandle::spawn((*store).clone()).expect("Store thread should start"),
            Arc::new(BlockingExecutor),
            1,
        );

        scheduler
            .start(run.id)
            .await
            .expect("Workflow should start");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if store
                    .get_workflow(run.id)
                    .expect("Workflow should load")
                    .status
                    == WorkflowStatus::Running
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Workflow should become running");
        scheduler
            .cancel(run.id)
            .await
            .expect("Workflow should cancel");
        assert!(
            tokio::time::timeout(Duration::from_secs(5), scheduler.wait(run.id))
                .await
                .expect("cancel should reach the executor promptly")
                .expect("cancelled Workflow should remain scheduled")
                .is_err()
        );
        assert_eq!(
            store
                .get_workflow(run.id)
                .expect("Workflow should load")
                .status,
            WorkflowStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn suspended_workflow_releases_the_global_execution_permit() {
        let directory = tempdir().expect("temporary directory should be created");
        let store = Arc::new(
            Store::open_in_memory(directory.path().join("managed"))
                .expect("store should open in memory"),
        );
        let research = store
            .create_project("Suspension", directory.path().join("project"))
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let suspended_run = create_test_workflow(&store, &session, "Suspend");
        let other_run = create_test_workflow(&store, &session, "Other");
        let scheduler = WorkflowScheduler::new(
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
            .expect("suspending Workflow should start");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if store
                    .get_workflow(suspended_run.id)
                    .expect("Workflow should load")
                    .status
                    == WorkflowStatus::WaitingForDeadline
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Workflow should suspend");

        scheduler
            .start(other_run.id)
            .await
            .expect("other Workflow should start");
        tokio::time::timeout(Duration::from_secs(5), scheduler.wait(other_run.id))
            .await
            .expect("other Workflow must not starve behind the suspended run")
            .expect("other Workflow should remain scheduled")
            .expect("other Workflow should complete");

        store
            .resume_workflow(suspended_run.id)
            .expect("suspended Workflow should be resumed");
        tokio::time::timeout(Duration::from_secs(5), scheduler.wait(suspended_run.id))
            .await
            .expect("resumed Workflow should finish")
            .expect("resumed Workflow should remain scheduled")
            .expect("resumed Workflow should complete");
    }
}
