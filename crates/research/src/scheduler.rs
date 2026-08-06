use async_trait::async_trait;
use papermachine_protocol::BudgetUsage;
use papermachine_protocol::WorkflowRunId;
use papermachine_protocol::WorkflowRunStatus;
use papermachine_store::Store;
use papermachine_store::StoreError;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub type RunOutcome = Result<Value, String>;

#[async_trait]
pub trait WorkflowRunExecutor: Send + Sync {
    async fn execute(
        &self,
        workflow_run_id: WorkflowRunId,
        cancellation: CancellationToken,
    ) -> RunOutcome;
}

#[derive(Clone)]
pub struct WorkflowRunScheduler {
    inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
    store: Arc<Store>,
    executor: Arc<dyn WorkflowRunExecutor>,
    permits: Arc<Semaphore>,
    handles: Mutex<HashMap<WorkflowRunId, ScheduledRun>>,
}

struct ScheduledRun {
    cancellation: CancellationToken,
    outcome: watch::Receiver<Option<RunOutcome>>,
}

impl WorkflowRunScheduler {
    pub fn new(
        store: Arc<Store>,
        executor: Arc<dyn WorkflowRunExecutor>,
        max_concurrent_runs: usize,
    ) -> Self {
        Self {
            inner: Arc::new(SchedulerInner {
                store,
                executor,
                permits: Arc::new(Semaphore::new(max_concurrent_runs.max(1))),
                handles: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn start(&self, run_id: WorkflowRunId) -> Result<bool, WorkflowSchedulerError> {
        let run = self.inner.store.get_workflow_run(run_id)?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalRun {
                run_id,
                status: run.status,
            });
        }
        let mut handles = self.inner.handles.lock().await;
        if handles.contains_key(&run_id) {
            return Ok(false);
        }
        let cancellation = CancellationToken::new();
        let (outcome_tx, outcome_rx) = watch::channel(None);
        handles.insert(
            run_id,
            ScheduledRun {
                cancellation: cancellation.clone(),
                outcome: outcome_rx,
            },
        );
        drop(handles);
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let outcome = run_scheduled(Arc::clone(&inner), run_id, cancellation).await;
            let _ = outcome_tx.send(Some(outcome));
        });
        Ok(true)
    }

    /// Marks runs that lost their in-memory Python control state as failed.
    /// This must happen before SessionRuntime recovers standalone Turns.
    pub fn reconcile_process_restart(&self) -> Result<Vec<WorkflowRunId>, WorkflowSchedulerError> {
        const REASON: &str = "WorkflowRun interrupted by server restart; durable Python effect replay is not available. Start a new run to retry safely.";
        let mut failed = Vec::new();
        for run in self.inner.store.list_interrupted_workflow_runs()? {
            self.inner
                .store
                .fail_interrupted_workflow_run(run.id, REASON)?;
            failed.push(run.id);
        }
        Ok(failed)
    }

    /// Starts runs that were durably created but had not begun executing when
    /// the previous process stopped. Running/paused Python programs cannot be
    /// restarted safely until workflow effects have deterministic replay keys.
    pub async fn recover(&self) -> Result<Vec<WorkflowRunId>, WorkflowSchedulerError> {
        let mut started = Vec::new();
        for run in self.inner.store.list_created_workflow_runs()? {
            if self.start(run.id).await? {
                started.push(run.id);
            }
        }
        Ok(started)
    }

    pub async fn pause(&self, run_id: WorkflowRunId) -> Result<(), WorkflowSchedulerError> {
        let run = self.inner.store.get_workflow_run(run_id)?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalRun {
                run_id,
                status: run.status,
            });
        }
        self.inner.store.set_workflow_run_status(
            run_id,
            WorkflowRunStatus::Paused,
            Some("paused by user".to_string()),
        )?;
        Ok(())
    }

    pub async fn resume(&self, run_id: WorkflowRunId) -> Result<(), WorkflowSchedulerError> {
        let run = self.inner.store.get_workflow_run(run_id)?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalRun {
                run_id,
                status: run.status,
            });
        }
        self.inner
            .store
            .set_workflow_run_status(run_id, WorkflowRunStatus::Running, None)?;
        self.start(run_id).await?;
        Ok(())
    }

    pub async fn cancel(&self, run_id: WorkflowRunId) -> Result<(), WorkflowSchedulerError> {
        let run = self.inner.store.get_workflow_run(run_id)?;
        if run.status.is_terminal() {
            return Err(WorkflowSchedulerError::TerminalRun {
                run_id,
                status: run.status,
            });
        }
        self.inner.store.set_workflow_run_status(
            run_id,
            WorkflowRunStatus::Cancelled,
            Some("cancelled by user".to_string()),
        )?;
        if let Some(handle) = self.inner.handles.lock().await.get(&run_id) {
            handle.cancellation.cancel();
        }
        Ok(())
    }

    pub async fn wait(&self, run_id: WorkflowRunId) -> Result<RunOutcome, WorkflowSchedulerError> {
        let mut outcome = self
            .inner
            .handles
            .lock()
            .await
            .get(&run_id)
            .map(|handle| handle.outcome.clone())
            .ok_or(WorkflowSchedulerError::NotScheduled(run_id))?;
        loop {
            if let Some(result) = outcome.borrow().clone() {
                return Ok(result);
            }
            outcome
                .changed()
                .await
                .map_err(|_| WorkflowSchedulerError::OutcomeChannelClosed(run_id))?;
        }
    }
}

async fn run_scheduled(
    inner: Arc<SchedulerInner>,
    run_id: WorkflowRunId,
    cancellation: CancellationToken,
) -> RunOutcome {
    let _permit = tokio::select! {
        permit = Arc::clone(&inner.permits).acquire_owned() => permit.map_err(|error| error.to_string())?,
        _ = cancellation.cancelled() => return Err("cancelled before execution started".to_string()),
    };
    let run = inner
        .store
        .get_workflow_run(run_id)
        .map_err(|error| error.to_string())?;
    if run.status == WorkflowRunStatus::Created {
        inner
            .store
            .set_workflow_run_status(run_id, WorkflowRunStatus::Running, None)
            .map_err(|error| error.to_string())?;
    }
    let started = Instant::now();
    let execution = cancellation.child_token();
    let result = match run.budget.max_wall_time_seconds {
        Some(limit) if run.usage.wall_time_seconds >= limit => Err(format!(
            "wall-time budget exhausted: used {} of {limit} seconds",
            run.usage.wall_time_seconds
        )),
        Some(limit) => {
            let remaining = limit.saturating_sub(run.usage.wall_time_seconds);
            tokio::select! {
                result = inner.executor.execute(run_id, execution.clone()) => result,
                _ = tokio::time::sleep(Duration::from_secs(remaining)) => {
                    execution.cancel();
                    Err(format!("wall-time budget exceeded after {limit} seconds"))
                }
            }
        }
        None => inner.executor.execute(run_id, execution).await,
    };
    let elapsed = started
        .elapsed()
        .as_secs()
        .max(u64::from(!started.elapsed().is_zero()));
    inner
        .store
        .add_budget_usage(
            run_id,
            BudgetUsage {
                wall_time_seconds: elapsed,
                ..BudgetUsage::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let current = inner
        .store
        .get_workflow_run(run_id)
        .map_err(|error| error.to_string())?;
    if cancellation.is_cancelled() {
        if !current.status.is_terminal() {
            inner
                .store
                .set_workflow_run_status(
                    run_id,
                    WorkflowRunStatus::Cancelled,
                    Some("cancelled by user".to_string()),
                )
                .map_err(|error| error.to_string())?;
        }
    } else if !current.status.is_terminal() {
        match &result {
            Ok(output) => {
                inner
                    .store
                    .complete_workflow_run(run_id, output.clone())
                    .map_err(|error| error.to_string())?;
            }
            Err(error) => {
                inner
                    .store
                    .set_workflow_run_status(run_id, WorkflowRunStatus::Failed, Some(error.clone()))
                    .map_err(|store_error| store_error.to_string())?;
            }
        }
    }
    result
}

#[derive(Debug, Error)]
pub enum WorkflowSchedulerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("WorkflowRun {run_id} is terminal with status {status:?}")]
    TerminalRun {
        run_id: WorkflowRunId,
        status: WorkflowRunStatus,
    },
    #[error("WorkflowRun {0} is not scheduled in this process")]
    NotScheduled(WorkflowRunId),
    #[error("outcome channel for WorkflowRun {0} closed unexpectedly")]
    OutcomeChannelClosed(WorkflowRunId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use papermachine_protocol::ActionStatus;
    use papermachine_protocol::AgentAccessProfile;
    use papermachine_protocol::Budget;
    use papermachine_protocol::SessionStatus;
    use papermachine_protocol::StepKind;
    use papermachine_protocol::StepStatus;
    use papermachine_protocol::TurnStatus;
    use papermachine_protocol::WorkflowId;
    use papermachine_protocol::WorkflowManifest;
    use papermachine_protocol::WorkflowSnapshot;
    use papermachine_protocol::WorkflowSource;
    use serde_json::json;
    use tempfile::tempdir;

    struct StaticExecutor {
        output: Value,
    }

    #[async_trait]
    impl WorkflowRunExecutor for StaticExecutor {
        async fn execute(
            &self,
            _workflow_run_id: WorkflowRunId,
            _cancellation: CancellationToken,
        ) -> RunOutcome {
            Ok(self.output.clone())
        }
    }

    fn workflow() -> WorkflowSnapshot {
        WorkflowSnapshot {
            manifest: WorkflowManifest {
                id: WorkflowId::new(),
                slug: "scheduler-test".to_string(),
                name: "Scheduler test".to_string(),
                version: "0.1.0".to_string(),
                description: String::new(),
                entrypoint: "main".to_string(),
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                default_budget: Budget::default(),
            },
            source: WorkflowSource::Builtin,
            definition_path: "builtin/scheduler-test/workflow.py".to_string(),
            sha256: "test".to_string(),
            source_code: String::new(),
        }
    }

    #[tokio::test]
    async fn commits_output_only_after_final_usage_is_recorded() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Arc::new(Store::open_in_memory(directory.path()).expect("store should open in memory"));
        let research = store
            .create_research("Scheduler", "")
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let run = store
            .create_workflow_run(session.id, workflow(), "Run", json!({}), None)
            .expect("run should be created");
        let scheduler = WorkflowRunScheduler::new(
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
            .get_workflow_run(run.id)
            .expect("completed run should load");

        assert_eq!(output, json!({"report": "done"}));
        assert_eq!(completed.status, WorkflowRunStatus::Completed);
        assert_eq!(completed.output, Some(output));
        assert_eq!(completed.usage.wall_time_seconds, 1);
    }

    #[tokio::test]
    async fn process_restart_fails_inflight_workflow_but_starts_created_run() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Arc::new(Store::open_in_memory(directory.path()).expect("store should open in memory"));
        let research = store
            .create_research("Restart", "")
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Origin", "", "test-model", Vec::new())
            .expect("session should be created");
        let interrupted_run = store
            .create_workflow_run(session.id, workflow(), "Interrupted", json!({}), None)
            .expect("run should be created");
        store
            .set_workflow_run_status(interrupted_run.id, WorkflowRunStatus::Running, None)
            .expect("run should be running");
        let participant = store
            .create_participant(
                interrupted_run.id,
                "Researcher",
                "Researcher",
                "evidence",
                "",
                "test-model",
                Vec::new(),
                AgentAccessProfile::Research,
            )
            .expect("participant should be created");
        let active_invocation = store
            .create_action_invocation(
                interrupted_run.id,
                None,
                participant.id,
                "investigate",
                "Investigate",
                json!({}),
            )
            .expect("active invocation should be created");
        let active_attempt = store
            .start_action_attempt(active_invocation.id)
            .expect("attempt should start");
        let turn = store
            .create_turn(
                participant.session_id,
                "Investigate",
                "test-model",
                "",
                None,
                8,
                None,
                None,
                None,
                None,
                Vec::new(),
            )
            .expect("workflow Turn should be created");
        store
            .attach_turn_to_attempt(active_attempt.id, turn.id)
            .expect("Turn should be attached");
        store.start_turn(turn.id).expect("Turn should be running");
        let step = store
            .create_step(turn.id, StepKind::Model, "sample", json!({}))
            .expect("step should be running");
        let scheduled_invocation = store
            .create_action_invocation(
                interrupted_run.id,
                None,
                participant.id,
                "follow_up",
                "Follow up",
                json!({}),
            )
            .expect("scheduled invocation should be created");
        let created_run = store
            .create_workflow_run(session.id, workflow(), "Created", json!({}), None)
            .expect("created run should be created");

        let scheduler = WorkflowRunScheduler::new(
            Arc::clone(&store),
            Arc::new(StaticExecutor {
                output: json!({"report": "done"}),
            }),
            1,
        );
        let failed = scheduler
            .reconcile_process_restart()
            .expect("restart reconciliation should succeed");
        assert_eq!(failed, vec![interrupted_run.id]);

        let failed_run = store
            .get_workflow_run(interrupted_run.id)
            .expect("failed run should load");
        assert_eq!(failed_run.status, WorkflowRunStatus::Failed);
        assert!(
            failed_run
                .error
                .as_deref()
                .is_some_and(|error| error.contains("durable Python effect replay"))
        );
        assert_eq!(
            store
                .get_action_invocation(active_invocation.id)
                .expect("active invocation should load")
                .status,
            ActionStatus::Interrupted
        );
        assert_eq!(
            store
                .get_action_attempt(active_attempt.id)
                .expect("active attempt should load")
                .status,
            ActionStatus::Interrupted
        );
        assert_eq!(
            store
                .get_action_invocation(scheduled_invocation.id)
                .expect("scheduled invocation should load")
                .status,
            ActionStatus::Interrupted
        );
        assert_eq!(
            store.get_turn(turn.id).expect("Turn should load").status,
            TurnStatus::Interrupted
        );
        assert_eq!(
            store.get_step(step.id).expect("step should load").status,
            StepStatus::Cancelled
        );
        assert_eq!(
            store
                .get_session(participant.session_id)
                .expect("participant Session should load")
                .status,
            SessionStatus::Ready
        );

        let recovered = scheduler
            .recover()
            .await
            .expect("created run should be recovered");
        assert_eq!(recovered, vec![created_run.id]);
        scheduler
            .wait(created_run.id)
            .await
            .expect("created run should remain scheduled")
            .expect("created run should complete");
        assert_eq!(
            store
                .get_workflow_run(created_run.id)
                .expect("created run should load")
                .status,
            WorkflowRunStatus::Completed
        );
    }
}
