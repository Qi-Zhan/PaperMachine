use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;
use papermachine_protocol::ChannelId;
use papermachine_protocol::HumanRequestStatus;
use papermachine_protocol::TimerStatus;
use papermachine_protocol::WorkflowEffectStatus;
use papermachine_protocol::WorkflowId;
use papermachine_protocol::WorkflowStatus;
use papermachine_protocol::WorkflowUsage;
use papermachine_store::Store;
use papermachine_store::StoreError;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
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
    store: Arc<Store>,
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
        store: Arc<Store>,
        executor: Arc<dyn WorkflowRuntime>,
        max_concurrent_runs: usize,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(max_concurrent_runs.max(1)));
        Self::new_with_permits(store, executor, permits)
    }

    pub fn new_with_permits(
        store: Arc<Store>,
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
        let run = self.inner.store.get_workflow(workflow_id)?;
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
            let _ = outcome_tx.send(Some(outcome));
        });
        Ok(true)
    }

    /// Restarts every non-terminal Workflow. The Python program executes from
    /// its snapshotted source while deterministic effects replay from the
    /// durable journal; an unfinished Action resumes its checkpointed Turn.
    pub async fn recover(&self) -> Result<Vec<WorkflowId>, WorkflowSchedulerError> {
        let mut started = Vec::new();
        for run in self.inner.store.list_recoverable_workflows()? {
            if self.start(run.id).await? {
                started.push(run.id);
            }
        }
        Ok(started)
    }

    pub async fn pause(&self, workflow_id: WorkflowId) -> Result<(), WorkflowSchedulerError> {
        let run = self.inner.store.get_workflow(workflow_id)?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalWorkflow {
                workflow_id,
                status: run.status,
            });
        }
        self.inner.store.set_workflow_status(
            workflow_id,
            WorkflowStatus::Paused,
            Some("paused by user".to_string()),
        )?;
        Ok(())
    }

    pub async fn resume(&self, workflow_id: WorkflowId) -> Result<(), WorkflowSchedulerError> {
        let run = self.inner.store.get_workflow(workflow_id)?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalWorkflow {
                workflow_id,
                status: run.status,
            });
        }
        self.inner
            .store
            .set_workflow_status(workflow_id, WorkflowStatus::Running, None)?;
        self.start(workflow_id).await?;
        Ok(())
    }

    pub async fn cancel(&self, workflow_id: WorkflowId) -> Result<(), WorkflowSchedulerError> {
        let run = self.inner.store.get_workflow(workflow_id)?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalWorkflow {
                workflow_id,
                status: run.status,
            });
        }
        self.inner.store.set_workflow_status(
            workflow_id,
            WorkflowStatus::Cancelled,
            Some("cancelled by user".to_string()),
        )?;
        if let Some(handle) = self.inner.handles.lock().await.get(&workflow_id) {
            handle.cancellation.cancel();
        }
        Ok(())
    }

    pub async fn wait(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<WorkflowOutcome, WorkflowSchedulerError> {
        let mut outcome = self
            .inner
            .handles
            .lock()
            .await
            .get(&workflow_id)
            .map(|handle| handle.outcome.clone())
            .ok_or(WorkflowSchedulerError::NotScheduled(workflow_id))?;
        loop {
            if let Some(result) = outcome.borrow().clone() {
                return Ok(result);
            }
            outcome
                .changed()
                .await
                .map_err(|_| WorkflowSchedulerError::OutcomeChannelClosed(workflow_id))?;
        }
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
            .get_workflow(workflow_id)
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
            .get_workflow(workflow_id)
            .map_err(|error| error.to_string())?;
        if run.status == WorkflowStatus::Created {
            run = inner
                .store
                .set_workflow_status(workflow_id, WorkflowStatus::Running, None)
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
            .add_workflow_usage(
                workflow_id,
                WorkflowUsage {
                    wall_time_seconds: elapsed,
                    ..WorkflowUsage::default()
                },
            )
            .map_err(|error| error.to_string())?;
        drop(permit);

        let current = inner
            .store
            .get_workflow(workflow_id)
            .map_err(|error| error.to_string())?;
        if cancellation.is_cancelled() {
            if !current.status.is_terminal() {
                inner
                    .store
                    .set_workflow_status(
                        workflow_id,
                        WorkflowStatus::Cancelled,
                        Some("cancelled by user".to_string()),
                    )
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
                    .complete_workflow(workflow_id, output.clone())
                    .map_err(|error| error.to_string())?;
                return Ok(output);
            }
            Ok(WorkflowExecution::Suspended(suspension)) => {
                if current.status != suspension.status {
                    inner
                        .store
                        .set_workflow_status(workflow_id, suspension.status, None)
                        .map_err(|error| error.to_string())?;
                }
                wake_at_hint = suspension.wake_at;
            }
            Err(error) => {
                inner
                    .store
                    .set_workflow_status(workflow_id, WorkflowStatus::Failed, Some(error.clone()))
                    .map_err(|store_error| store_error.to_string())?;
                return Err(error);
            }
        }
    }
}

async fn wait_until_runnable(
    store: &Store,
    workflow_id: WorkflowId,
    cancellation: &CancellationToken,
    wake_at_hint: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let mut events = store.subscribe();
    loop {
        if cancellation.is_cancelled() {
            return Err("cancelled while Workflow was suspended".to_string());
        }
        let run = store
            .get_workflow(workflow_id)
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
            WorkflowStatus::WaitingForUser
            | WorkflowStatus::WaitingForTimer
            | WorkflowStatus::WaitingForSignal => {
                let open_direct_human_request = store
                    .list_human_requests(workflow_id)
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .any(|request| request.status == HumanRequestStatus::Open);
                let next_timer = store
                    .list_timers(workflow_id)
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .filter(|timer| timer.status == TimerStatus::Active)
                    .map(|timer| timer.next_fire_at)
                    .chain(wake_at_hint.iter().cloned())
                    .min();
                let ready_signal = workflow_has_ready_signal(store, workflow_id)?;

                if ready_signal
                    || (run.status == WorkflowStatus::WaitingForUser && !open_direct_human_request)
                {
                    store
                        .set_workflow_status(workflow_id, WorkflowStatus::Running, None)
                        .map_err(|error| error.to_string())?;
                    return Ok(());
                }
                if let Some(next_fire_at) = next_timer {
                    let wait = (next_fire_at - Utc::now()).to_std().unwrap_or_default();
                    if wait.is_zero() {
                        store
                            .set_workflow_status(workflow_id, WorkflowStatus::Running, None)
                            .map_err(|error| error.to_string())?;
                        return Ok(());
                    }
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err("cancelled while Workflow was suspended".to_string()),
                        _ = tokio::time::sleep(wait) => {
                            store
                                .set_workflow_status(workflow_id, WorkflowStatus::Running, None)
                                .map_err(|error| error.to_string())?;
                            return Ok(());
                        }
                        _ = events.recv() => {}
                    }
                } else {
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err("cancelled while Workflow was suspended".to_string()),
                        _ = events.recv() => {}
                    }
                }
            }
        }
    }
}

fn workflow_has_ready_signal(store: &Store, workflow_id: WorkflowId) -> Result<bool, String> {
    for effect in store
        .list_workflow_effects(workflow_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|effect| {
            effect.kind == "wait_signal" && effect.status == WorkflowEffectStatus::Started
        })
    {
        let Some(channel_id) = effect.payload.get("channel_id").and_then(Value::as_str) else {
            continue;
        };
        let channel_id = ChannelId::from_str(channel_id).map_err(|error| error.to_string())?;
        let after_sequence = effect
            .payload
            .get("after_sequence")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if !store
            .list_signals(channel_id, after_sequence)
            .map_err(|error| error.to_string())?
            .is_empty()
        {
            return Ok(true);
        }
    }
    Ok(false)
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
    #[error("outcome channel for Workflow {0} closed unexpectedly")]
    OutcomeChannelClosed(WorkflowId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use papermachine_protocol::AgentAccessProfile;
    use papermachine_protocol::Session;
    use papermachine_protocol::Workflow;
    use papermachine_protocol::WorkflowProgramId;
    use papermachine_protocol::WorkflowProgramManifest;
    use papermachine_protocol::WorkflowProgramSnapshot;
    use papermachine_protocol::WorkflowProgramSource;
    use papermachine_store::NewWorkflow;
    use serde_json::json;
    use std::collections::HashMap;
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

    struct SignalWaitExecutor {
        waiting: WorkflowId,
        executions: StdMutex<HashMap<WorkflowId, usize>>,
    }

    #[async_trait]
    impl WorkflowRuntime for SignalWaitExecutor {
        async fn execute(
            &self,
            workflow_id: WorkflowId,
            _cancellation: CancellationToken,
        ) -> Result<WorkflowExecution, String> {
            *self
                .executions
                .lock()
                .map_err(|error| error.to_string())?
                .entry(workflow_id)
                .or_default() += 1;
            if workflow_id == self.waiting {
                Ok(WorkflowExecution::Suspended(WorkflowSuspension::new(
                    WorkflowStatus::WaitingForSignal,
                    None,
                )))
            } else {
                Ok(WorkflowExecution::Completed(json!({
                    "workflow_id": workflow_id
                })))
            }
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
                    WorkflowStatus::WaitingForTimer,
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
                params_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
            },
            source: WorkflowProgramSource::Builtin,
            definition_path: "builtin/scheduler-test/workflow.py".to_string(),
            sha256: "test".to_string(),
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
                access: AgentAccessProfile::Research,
                enabled_skills: Vec::new(),
                launch_context: Default::default(),
                agent_access_overrides: Default::default(),
            })
            .expect("workflow should be created")
    }

    #[tokio::test]
    async fn commits_output_only_after_final_usage_is_recorded() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Arc::new(Store::open_in_memory(directory.path()).expect("store should open in memory"));
        let research = store
            .create_project("Scheduler", "", directory.path().join("project"))
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let run = create_test_workflow(&store, &session, "Run");
        let scheduler = WorkflowScheduler::new(
            Arc::clone(&store),
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
    }

    #[tokio::test]
    async fn process_restart_recovers_running_and_created_workflows() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Arc::new(Store::open_in_memory(directory.path()).expect("store should open in memory"));
        let research = store
            .create_project("Restart", "", directory.path().join("project"))
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let running_run = create_test_workflow(&store, &session, "Running");
        store
            .set_workflow_status(running_run.id, WorkflowStatus::Running, None)
            .expect("run should be running");
        let created_run = create_test_workflow(&store, &session, "Created");

        let scheduler = WorkflowScheduler::new(
            Arc::clone(&store),
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
    async fn suspended_workflow_releases_the_global_execution_permit() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Arc::new(Store::open_in_memory(directory.path()).expect("store should open in memory"));
        let research = store
            .create_project("Suspension", "", directory.path().join("project"))
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let suspended_run = create_test_workflow(&store, &session, "Suspend");
        let other_run = create_test_workflow(&store, &session, "Other");
        let scheduler = WorkflowScheduler::new(
            Arc::clone(&store),
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
                    == WorkflowStatus::WaitingForTimer
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
            .set_workflow_status(suspended_run.id, WorkflowStatus::Running, None)
            .expect("suspended Workflow should be resumed");
        tokio::time::timeout(Duration::from_secs(5), scheduler.wait(suspended_run.id))
            .await
            .expect("resumed Workflow should finish")
            .expect("resumed Workflow should remain scheduled")
            .expect("resumed Workflow should complete");
    }

    #[tokio::test]
    async fn unrelated_workflow_events_do_not_replay_a_signal_waiter() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Arc::new(Store::open_in_memory(directory.path()).expect("store should open in memory"));
        let research = store
            .create_project("Signals", "", directory.path().join("project"))
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let waiting_run = create_test_workflow(&store, &session, "Wait for signal");
        let other_run = create_test_workflow(&store, &session, "Unrelated work");
        let executor = Arc::new(SignalWaitExecutor {
            waiting: waiting_run.id,
            executions: StdMutex::new(HashMap::new()),
        });
        let scheduler = WorkflowScheduler::new(
            Arc::clone(&store),
            Arc::clone(&executor) as Arc<dyn WorkflowRuntime>,
            1,
        );

        scheduler
            .start(waiting_run.id)
            .await
            .expect("signal waiter should start");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if store
                    .get_workflow(waiting_run.id)
                    .expect("Workflow should load")
                    .status
                    == WorkflowStatus::WaitingForSignal
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Workflow should wait for a signal");
        tokio::time::sleep(Duration::from_millis(50)).await;

        scheduler
            .start(other_run.id)
            .await
            .expect("unrelated Workflow should start");
        scheduler
            .wait(other_run.id)
            .await
            .expect("unrelated Workflow should remain scheduled")
            .expect("unrelated Workflow should complete");
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(
            executor
                .executions
                .lock()
                .expect("execution counts should remain available")
                .get(&waiting_run.id),
            Some(&1),
            "an unrelated Workflow event must not replay the signal waiter",
        );

        scheduler
            .cancel(waiting_run.id)
            .await
            .expect("signal waiter should be cancellable");
        assert!(
            scheduler
                .wait(waiting_run.id)
                .await
                .expect("cancelled Workflow should remain scheduled")
                .is_err()
        );
    }
}
