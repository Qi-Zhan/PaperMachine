use crate::StoreError;
use crate::StoreShared;
use crate::artifact::store_artifact_file;
use chrono::Duration;
use chrono::Utc;
use papermachine_protocol::*;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::Transaction;
use rusqlite::params;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::broadcast;

const SCHEMA_VERSION: u32 = 3;

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    shared: StoreShared,
}

impl Store {
    pub fn open(
        database_path: impl AsRef<Path>,
        artifact_root: impl AsRef<Path>,
    ) -> Result<Self, StoreError> {
        if let Some(parent) = database_path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        let connection = Connection::open(database_path)?;
        initialize(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            shared: StoreShared::new(artifact_root)?,
        })
    }

    pub fn open_in_memory(artifact_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        initialize(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            shared: StoreShared::new(artifact_root)?,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WorkflowRunEvent> {
        self.shared.run_events.subscribe()
    }

    pub fn subscribe_sessions(&self) -> broadcast::Receiver<SessionEvent> {
        self.shared.session_events.subscribe()
    }

    pub fn create_research(
        &self,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<Research, StoreError> {
        let now = Utc::now();
        let research = Research {
            id: ResearchId::new(),
            name: name.into(),
            description: description.into(),
            created_at: now,
            updated_at: now,
        };
        self.insert_document(
            "researches",
            &research.id.to_string(),
            None,
            research.updated_at,
            &research,
        )?;
        Ok(research)
    }

    pub fn list_researches(&self) -> Result<Vec<Research>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM researches ORDER BY updated_at DESC, id ASC",
            [],
        )
    }

    pub fn get_research(&self, id: ResearchId) -> Result<Research, StoreError> {
        self.query_document_by_id("researches", id.to_string(), "research")
    }

    pub fn create_session(
        &self,
        research_id: ResearchId,
        title: impl Into<String>,
        instructions: impl Into<String>,
        model: impl Into<String>,
        enabled_skills: Vec<String>,
    ) -> Result<Session, StoreError> {
        self.create_session_with_access(
            research_id,
            title,
            instructions,
            model,
            enabled_skills,
            AgentAccessProfile::Research,
        )
    }

    pub fn create_session_with_access(
        &self,
        research_id: ResearchId,
        title: impl Into<String>,
        instructions: impl Into<String>,
        model: impl Into<String>,
        enabled_skills: Vec<String>,
        access: AgentAccessProfile,
    ) -> Result<Session, StoreError> {
        self.ensure_research(research_id)?;
        self.insert_session(Session {
            id: SessionId::new(),
            research_id,
            origin: SessionOrigin::User,
            title: title.into(),
            instructions: instructions.into(),
            model: model.into(),
            access,
            status: SessionStatus::Ready,
            enabled_skills,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
    }

    fn insert_session(&self, session: Session) -> Result<Session, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO sessions (id, research_id, origin, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session.id.to_string(),
                session.research_id.to_string(),
                enum_string(session.origin)?,
                enum_string(session.status)?,
                session.updated_at.to_rfc3339(),
                serde_json::to_string(&session)?,
            ],
        )?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            SessionEventPayload::SessionCreated {
                title: session.title.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    pub fn get_session(&self, id: SessionId) -> Result<Session, StoreError> {
        self.query_document_by_id("sessions", id.to_string(), "session")
    }

    pub fn list_sessions(&self, research_id: ResearchId) -> Result<Vec<Session>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM sessions WHERE research_id = ?1
             ORDER BY updated_at DESC, id ASC",
            [research_id.to_string()],
        )
    }

    pub fn set_session_status(
        &self,
        session_id: SessionId,
        status: SessionStatus,
        reason: Option<String>,
    ) -> Result<Session, StoreError> {
        let mut session = self.get_session(session_id)?;
        session.status = status;
        session.updated_at = Utc::now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        update_status_document_tx(
            &transaction,
            "sessions",
            &session.id.to_string(),
            status,
            &session,
        )?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            SessionEventPayload::SessionStatusChanged { status, reason },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    pub fn set_session_enabled_skills(
        &self,
        session_id: SessionId,
        enabled_skills: Vec<String>,
    ) -> Result<Session, StoreError> {
        let mut session = self.get_session(session_id)?;
        if matches!(
            session.status,
            SessionStatus::Running | SessionStatus::WaitingForHuman | SessionStatus::Paused
        ) {
            return Err(StoreError::Invariant(
                "cannot change skills while a Session has an active Turn".to_string(),
            ));
        }
        session.enabled_skills = enabled_skills;
        session.updated_at = Utc::now();
        self.update_document(
            "sessions",
            &session.id.to_string(),
            session.updated_at,
            &session,
        )?;
        Ok(session)
    }

    pub fn set_session_access(
        &self,
        session_id: SessionId,
        access: AgentAccessProfile,
    ) -> Result<Session, StoreError> {
        let mut session = self.get_session(session_id)?;
        let active = self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE session_id = ?1
             AND status IN ('queued', 'running', 'waiting_for_human', 'paused'))",
            [session_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if active {
            return Err(StoreError::Invariant(
                "cannot change access while a Session has an active Turn".to_string(),
            ));
        }
        session.access = access;
        session.updated_at = Utc::now();
        self.update_document(
            "sessions",
            &session.id.to_string(),
            session.updated_at,
            &session,
        )?;
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_turn(
        &self,
        session_id: SessionId,
        input: impl Into<String>,
        model: impl Into<String>,
        instructions: impl Into<String>,
        reasoning_effort: Option<ReasoningEffort>,
        max_steps: u32,
        max_search_calls: Option<u32>,
        web_search_context_size: Option<WebSearchContextSize>,
        max_output_tokens: Option<u32>,
        response_format: Option<ModelResponseFormat>,
        skill_snapshots: Vec<SkillSnapshot>,
    ) -> Result<Turn, StoreError> {
        let session = self.get_session(session_id)?;
        if session.status == SessionStatus::Archived {
            return Err(StoreError::Invariant(
                "cannot add a Turn to an archived Session".to_string(),
            ));
        }
        let active = self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE session_id = ?1
             AND status IN ('queued', 'running', 'waiting_for_human', 'paused'))",
            [session_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if active {
            return Err(StoreError::Invariant(
                "Session already has an active Turn".to_string(),
            ));
        }
        let now = Utc::now();
        let turn = Turn {
            id: TurnId::new(),
            session_id,
            status: TurnStatus::Queued,
            input: input.into(),
            output: None,
            model: model.into(),
            reasoning_effort,
            instructions: instructions.into(),
            access: session.access,
            max_steps: max_steps.max(1),
            max_search_calls,
            web_search_context_size,
            max_output_tokens,
            response_format,
            skill_snapshots,
            history: Vec::new(),
            usage: TokenUsage::default(),
            error: None,
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO turns (id, session_id, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                turn.id.to_string(),
                turn.session_id.to_string(),
                enum_string(turn.status)?,
                turn.updated_at.to_rfc3339(),
                serde_json::to_string(&turn)?,
            ],
        )?;
        let event = append_session_event_tx(
            &transaction,
            session_id,
            Some(turn.id),
            None,
            SessionEventPayload::TurnCreated {
                input: turn.input.clone(),
                model: turn.model.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(turn)
    }

    pub fn get_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.query_document_by_id("turns", id.to_string(), "turn")
    }

    pub fn list_turns(&self, session_id: SessionId) -> Result<Vec<Turn>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM turns WHERE session_id = ?1 ORDER BY created_at ASC, id ASC",
            [session_id.to_string()],
        )
    }

    pub fn list_resumable_turns(&self) -> Result<Vec<Turn>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM turns WHERE status IN ('queued', 'running')
             ORDER BY updated_at ASC, id ASC",
            [],
        )
    }

    pub fn start_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.set_turn_status(id, TurnStatus::Running, None)
    }

    pub fn complete_turn(
        &self,
        id: TurnId,
        output: String,
        history: Vec<ModelInputItem>,
        usage: TokenUsage,
    ) -> Result<Turn, StoreError> {
        let mut turn = self.get_turn(id)?;
        turn.output = Some(output);
        turn.history = history;
        turn.usage = usage;
        self.persist_turn_status(turn, TurnStatus::Completed, None)
    }

    pub fn fail_turn(&self, id: TurnId, error: impl Into<String>) -> Result<Turn, StoreError> {
        self.set_turn_status(id, TurnStatus::Failed, Some(error.into()))
    }

    pub fn interrupt_turn(
        &self,
        id: TurnId,
        reason: impl Into<String>,
    ) -> Result<Turn, StoreError> {
        self.set_turn_status(id, TurnStatus::Interrupted, Some(reason.into()))
    }

    pub fn cancel_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.set_turn_status(
            id,
            TurnStatus::Cancelled,
            Some("cancelled by user".to_string()),
        )
    }

    pub fn set_turn_status(
        &self,
        id: TurnId,
        status: TurnStatus,
        error: Option<String>,
    ) -> Result<Turn, StoreError> {
        let turn = self.get_turn(id)?;
        self.persist_turn_status(turn, status, error)
    }

    pub fn checkpoint_turn_history(
        &self,
        id: TurnId,
        history: Vec<ModelInputItem>,
        usage: TokenUsage,
    ) -> Result<Turn, StoreError> {
        let mut turn = self.get_turn(id)?;
        turn.history = history;
        turn.usage = usage;
        turn.updated_at = Utc::now();
        self.update_document("turns", &turn.id.to_string(), turn.updated_at, &turn)?;
        Ok(turn)
    }

    fn persist_turn_status(
        &self,
        mut turn: Turn,
        status: TurnStatus,
        error: Option<String>,
    ) -> Result<Turn, StoreError> {
        turn.status = status;
        turn.error = error.clone();
        turn.updated_at = Utc::now();
        let mut session = self.get_session(turn.session_id)?;
        session.status = match status {
            TurnStatus::Queued | TurnStatus::Running => SessionStatus::Running,
            TurnStatus::WaitingForHuman => SessionStatus::WaitingForHuman,
            TurnStatus::Paused => SessionStatus::Paused,
            TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Cancelled => {
                SessionStatus::Ready
            }
            TurnStatus::Failed => SessionStatus::Failed,
        };
        session.updated_at = turn.updated_at;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        update_status_document_tx(&transaction, "turns", &turn.id.to_string(), status, &turn)?;
        update_status_document_tx(
            &transaction,
            "sessions",
            &session.id.to_string(),
            session.status,
            &session,
        )?;
        let turn_event = append_session_event_tx(
            &transaction,
            session.id,
            Some(turn.id),
            None,
            SessionEventPayload::TurnStatusChanged { status, error },
        )?;
        let session_event = append_session_event_tx(
            &transaction,
            session.id,
            Some(turn.id),
            None,
            SessionEventPayload::SessionStatusChanged {
                status: session.status,
                reason: None,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(turn_event);
        self.shared.publish_session(session_event);
        Ok(turn)
    }

    pub fn create_step(
        &self,
        turn_id: TurnId,
        kind: StepKind,
        name: impl Into<String>,
        input: Value,
    ) -> Result<AgentStep, StoreError> {
        self.get_turn(turn_id)?;
        let now = Utc::now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let sequence = transaction.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM steps WHERE turn_id = ?1",
            [turn_id.to_string()],
            |row| row.get::<_, u32>(0),
        )?;
        let step = AgentStep {
            id: StepId::new(),
            turn_id,
            sequence,
            kind,
            name: name.into(),
            status: StepStatus::Running,
            input,
            output: None,
            usage: TokenUsage::default(),
            duration_ms: None,
            created_at: now,
            updated_at: now,
        };
        transaction.execute(
            "INSERT INTO steps (id, turn_id, sequence, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                step.id.to_string(),
                step.turn_id.to_string(),
                step.sequence,
                enum_string(step.status)?,
                step.updated_at.to_rfc3339(),
                serde_json::to_string(&step)?,
            ],
        )?;
        transaction.commit()?;
        Ok(step)
    }

    pub fn finish_step(
        &self,
        id: StepId,
        status: StepStatus,
        output: Option<Value>,
        usage: TokenUsage,
        duration_ms: Option<u64>,
    ) -> Result<AgentStep, StoreError> {
        let mut step = self.get_step(id)?;
        step.status = status;
        step.output = output;
        step.usage = usage;
        step.duration_ms = duration_ms;
        step.updated_at = Utc::now();
        self.update_status_document("steps", &step.id.to_string(), status, &step)?;
        Ok(step)
    }

    pub fn get_step(&self, id: StepId) -> Result<AgentStep, StoreError> {
        self.query_document_by_id("steps", id.to_string(), "step")
    }

    pub fn list_steps(&self, turn_id: TurnId) -> Result<Vec<AgentStep>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM steps WHERE turn_id = ?1 ORDER BY sequence ASC",
            [turn_id.to_string()],
        )
    }

    pub fn append_session_event(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        step_id: Option<StepId>,
        payload: SessionEventPayload,
    ) -> Result<SessionEvent, StoreError> {
        self.get_session(session_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let event = append_session_event_tx(&transaction, session_id, turn_id, step_id, payload)?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event.clone());
        Ok(event)
    }

    pub fn list_session_events(
        &self,
        session_id: SessionId,
        after_sequence: u64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        self.query_documents(
            "SELECT event_json FROM session_events WHERE session_id = ?1 AND sequence > ?2
             ORDER BY sequence ASC",
            params![session_id.to_string(), after_sequence],
        )
    }

    pub fn register_workflow(&self, registration: &WorkflowRegistration) -> Result<(), StoreError> {
        self.connection()?.execute(
            "INSERT INTO workflows (id, slug, version, source, definition_path, sha256, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(slug, version) DO UPDATE SET id=excluded.id, source=excluded.source,
             definition_path=excluded.definition_path, sha256=excluded.sha256,
             updated_at=excluded.updated_at, document_json=excluded.document_json",
            params![
                registration.manifest.id.to_string(),
                registration.manifest.slug,
                registration.manifest.version,
                enum_string(registration.source)?,
                registration.definition_path,
                registration.sha256,
                registration.updated_at.to_rfc3339(),
                serde_json::to_string(registration)?,
            ],
        )?;
        Ok(())
    }

    pub fn list_workflows(&self) -> Result<Vec<WorkflowRegistration>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflows ORDER BY source ASC, slug ASC, version DESC",
            [],
        )
    }

    pub fn create_workflow_run(
        &self,
        origin_session_id: SessionId,
        workflow: WorkflowSnapshot,
        objective: impl Into<String>,
        input: Value,
        budget: Option<Budget>,
    ) -> Result<WorkflowRun, StoreError> {
        let origin = self.get_session(origin_session_id)?;
        let now = Utc::now();
        let run = WorkflowRun {
            id: WorkflowRunId::new(),
            research_id: origin.research_id,
            origin_session_id,
            budget: budget.unwrap_or_else(|| workflow.manifest.default_budget.clone()),
            workflow,
            objective: objective.into(),
            status: WorkflowRunStatus::Created,
            input,
            output: None,
            error: None,
            attention_required: false,
            usage: BudgetUsage::default(),
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO workflow_runs
             (id, research_id, origin_session_id, status, attention_required, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![
                run.id.to_string(),
                run.research_id.to_string(),
                run.origin_session_id.to_string(),
                enum_string(run.status)?,
                run.updated_at.to_rfc3339(),
                serde_json::to_string(&run)?,
            ],
        )?;
        let run_event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            WorkflowRunEventPayload::WorkflowRunCreated {
                objective: run.objective.clone(),
                workflow_slug: run.workflow.manifest.slug.clone(),
                workflow_version: run.workflow.manifest.version.clone(),
                source_sha256: run.workflow.sha256.clone(),
            },
        )?;
        let session_event = append_session_event_tx(
            &transaction,
            origin.id,
            None,
            None,
            SessionEventPayload::Warning {
                message: format!(
                    "Started workflow {}@{}",
                    run.workflow.manifest.slug, run.workflow.manifest.version
                ),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(run_event);
        self.shared.publish_session(session_event);
        Ok(run)
    }

    pub fn get_workflow_run(&self, id: WorkflowRunId) -> Result<WorkflowRun, StoreError> {
        self.query_document_by_id("workflow_runs", id.to_string(), "workflow run")
    }

    pub fn list_workflow_runs(
        &self,
        origin_session_id: SessionId,
    ) -> Result<Vec<WorkflowRun>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_runs WHERE origin_session_id = ?1
             ORDER BY updated_at DESC, id ASC",
            [origin_session_id.to_string()],
        )
    }

    pub fn list_session_workflow_runs(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<WorkflowRun>, StoreError> {
        self.query_documents(
            "SELECT DISTINCT wr.document_json FROM workflow_runs wr
             LEFT JOIN workflow_participants p ON p.workflow_run_id = wr.id
             WHERE wr.origin_session_id = ?1 OR p.session_id = ?1
             ORDER BY wr.updated_at DESC, wr.id ASC",
            [session_id.to_string()],
        )
    }

    pub fn list_research_workflow_runs(
        &self,
        research_id: ResearchId,
    ) -> Result<Vec<WorkflowRun>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_runs WHERE research_id = ?1
             ORDER BY updated_at DESC, id ASC",
            [research_id.to_string()],
        )
    }

    pub fn list_created_workflow_runs(&self) -> Result<Vec<WorkflowRun>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_runs WHERE status = 'created'
             ORDER BY updated_at ASC, id ASC",
            [],
        )
    }

    pub fn list_interrupted_workflow_runs(&self) -> Result<Vec<WorkflowRun>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_runs WHERE status IN ('running', 'paused')
             ORDER BY updated_at ASC, id ASC",
            [],
        )
    }

    /// Fails a WorkflowRun whose in-memory Python control state was lost in a
    /// process restart. Without a durable effect journal it is unsafe to rerun
    /// arbitrary workflow source from its first line: committed Agent/action
    /// effects could be duplicated. This reconciliation is run before normal
    /// Session Turn recovery, so action Turns cannot be resumed without their
    /// WorkflowTurnContext.
    pub fn fail_interrupted_workflow_run(
        &self,
        id: WorkflowRunId,
        reason: impl Into<String>,
    ) -> Result<WorkflowRun, StoreError> {
        let reason = reason.into();
        let mut run = self.get_workflow_run(id)?;
        if !matches!(
            run.status,
            WorkflowRunStatus::Running | WorkflowRunStatus::Paused
        ) {
            return Err(StoreError::Invariant(format!(
                "WorkflowRun {} is not interrupted: status is {:?}",
                run.id, run.status
            )));
        }

        let now = Utc::now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let invocation_documents = {
            let mut statement = transaction.prepare(
                "SELECT document_json FROM action_invocations
                 WHERE workflow_run_id = ?1
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = statement.query_map([id.to_string()], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut run_events = Vec::new();
        let mut session_events = Vec::new();

        for document in invocation_documents {
            let mut invocation: ActionInvocation = serde_json::from_str(&document)?;
            if invocation.status.is_terminal() {
                continue;
            }
            let attempt_documents = {
                let mut statement = transaction.prepare(
                    "SELECT document_json FROM action_attempts
                     WHERE invocation_id = ?1
                     ORDER BY number ASC",
                )?;
                let rows = statement
                    .query_map([invocation.id.to_string()], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            let mut interrupted_attempt_id = None;

            for attempt_document in attempt_documents {
                let mut attempt: ActionAttempt = serde_json::from_str(&attempt_document)?;
                if attempt.status.is_terminal() {
                    continue;
                }
                interrupted_attempt_id = Some(attempt.id);

                if let Some(turn_id) = attempt.turn_id {
                    let turn_document = transaction.query_row(
                        "SELECT document_json FROM turns WHERE id = ?1",
                        [turn_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )?;
                    let turn: Turn = serde_json::from_str(&turn_document)?;
                    if !turn.status.is_terminal() {
                        let step_documents = {
                            let mut statement = transaction.prepare(
                                "SELECT document_json FROM steps
                                 WHERE turn_id = ?1 AND status = 'running'
                                 ORDER BY sequence ASC",
                            )?;
                            let rows = statement
                                .query_map([turn.id.to_string()], |row| row.get::<_, String>(0))?;
                            rows.collect::<Result<Vec<_>, _>>()?
                        };
                        for step_document in step_documents {
                            let mut step: AgentStep = serde_json::from_str(&step_document)?;
                            step.status = StepStatus::Cancelled;
                            step.output = Some(json!({"error": reason}));
                            step.updated_at = now;
                            update_status_document_tx(
                                &transaction,
                                "steps",
                                &step.id.to_string(),
                                step.status,
                                &step,
                            )?;
                        }

                        set_turn_status_tx(
                            &transaction,
                            turn.id,
                            TurnStatus::Interrupted,
                            Some(reason.clone()),
                        )?;
                        session_events.push(append_session_event_tx(
                            &transaction,
                            turn.session_id,
                            Some(turn.id),
                            None,
                            SessionEventPayload::TurnStatusChanged {
                                status: TurnStatus::Interrupted,
                                error: Some(reason.clone()),
                            },
                        )?);
                        session_events.push(append_session_event_tx(
                            &transaction,
                            turn.session_id,
                            Some(turn.id),
                            None,
                            SessionEventPayload::SessionStatusChanged {
                                status: SessionStatus::Ready,
                                reason: Some(reason.clone()),
                            },
                        )?);
                    }
                }

                attempt.status = ActionStatus::Interrupted;
                attempt.error = Some(reason.clone());
                attempt.updated_at = now;
                update_status_document_tx(
                    &transaction,
                    "action_attempts",
                    &attempt.id.to_string(),
                    attempt.status,
                    &attempt,
                )?;
            }

            invocation.status = ActionStatus::Interrupted;
            invocation.output = None;
            invocation.error = Some(reason.clone());
            invocation.updated_at = now;
            update_status_document_tx(
                &transaction,
                "action_invocations",
                &invocation.id.to_string(),
                invocation.status,
                &invocation,
            )?;
            run_events.push(append_run_event_tx(
                &transaction,
                run.research_id,
                run.id,
                action_event_payload(&invocation, interrupted_attempt_id),
            )?);
        }

        run.status = WorkflowRunStatus::Failed;
        run.error = Some(reason.clone());
        run.attention_required = false;
        run.updated_at = now;
        terminalize_run_resources_tx(&transaction, run.id, run.status, now)?;
        update_run_tx(&transaction, &run)?;
        run_events.push(append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            WorkflowRunEventPayload::WorkflowRunStatusChanged {
                status: run.status,
                reason: Some(reason),
            },
        )?);
        transaction.commit()?;
        drop(connection);

        for event in session_events {
            self.shared.publish_session(event);
        }
        for event in run_events {
            self.shared.publish_run(event);
        }
        Ok(run)
    }

    pub fn set_workflow_run_status(
        &self,
        id: WorkflowRunId,
        status: WorkflowRunStatus,
        reason: Option<String>,
    ) -> Result<WorkflowRun, StoreError> {
        let mut run = self.get_workflow_run(id)?;
        run.status = status;
        run.error = if status == WorkflowRunStatus::Failed {
            reason.clone()
        } else {
            None
        };
        if status.is_terminal() {
            run.attention_required = false;
        }
        run.updated_at = Utc::now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        if status.is_terminal() {
            terminalize_run_resources_tx(&transaction, run.id, status, run.updated_at)?;
        }
        update_run_tx(&transaction, &run)?;
        let event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            WorkflowRunEventPayload::WorkflowRunStatusChanged { status, reason },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(event);
        Ok(run)
    }

    pub fn complete_workflow_run(
        &self,
        id: WorkflowRunId,
        output: Value,
    ) -> Result<WorkflowRun, StoreError> {
        let mut run = self.get_workflow_run(id)?;
        run.status = WorkflowRunStatus::Completed;
        run.output = Some(output.clone());
        run.error = None;
        run.attention_required = false;
        run.updated_at = Utc::now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        terminalize_run_resources_tx(
            &transaction,
            run.id,
            WorkflowRunStatus::Completed,
            run.updated_at,
        )?;
        update_run_tx(&transaction, &run)?;
        let event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            WorkflowRunEventPayload::WorkflowCompleted { output },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(event);
        Ok(run)
    }

    pub fn add_budget_usage(
        &self,
        id: WorkflowRunId,
        delta: BudgetUsage,
    ) -> Result<WorkflowRun, StoreError> {
        // Read and update under one database lock. Workflow actions can finish in
        // parallel, so a separate get followed by update loses concurrent deltas.
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let document = transaction.query_row(
            "SELECT document_json FROM workflow_runs WHERE id = ?1",
            [id.to_string()],
            |row| row.get::<_, String>(0),
        )?;
        let mut run: WorkflowRun = serde_json::from_str(&document)?;
        run.usage.agents_created = run
            .usage
            .agents_created
            .saturating_add(delta.agents_created);
        run.usage.actions_started = run
            .usage
            .actions_started
            .saturating_add(delta.actions_started);
        run.usage.actions_completed = run
            .usage
            .actions_completed
            .saturating_add(delta.actions_completed);
        run.usage.action_steps = run.usage.action_steps.saturating_add(delta.action_steps);
        run.usage.timer_fires = run.usage.timer_fires.saturating_add(delta.timer_fires);
        run.usage.hosted_search_calls = run
            .usage
            .hosted_search_calls
            .saturating_add(delta.hosted_search_calls);
        run.usage.tokens.saturating_add_assign(delta.tokens);
        run.usage.wall_time_seconds = run
            .usage
            .wall_time_seconds
            .saturating_add(delta.wall_time_seconds);
        run.usage.estimated_cost_usd =
            match (run.usage.estimated_cost_usd, delta.estimated_cost_usd) {
                (Some(left), Some(right)) => Some(left + right),
                (value, None) | (None, value) => value,
            };
        run.updated_at = Utc::now();
        update_run_tx(&transaction, &run)?;
        let event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            WorkflowRunEventPayload::BudgetUpdated {
                usage: run.usage.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(event);
        Ok(run)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_participant(
        &self,
        run_id: WorkflowRunId,
        class_name: impl Into<String>,
        name: impl Into<String>,
        role: impl Into<String>,
        instructions: impl Into<String>,
        model: impl Into<String>,
        skills: Vec<String>,
        access: AgentAccessProfile,
    ) -> Result<WorkflowParticipant, StoreError> {
        let mut run = self.get_workflow_run(run_id)?;
        if run.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot add an Agent to a terminal WorkflowRun".to_string(),
            ));
        }
        if run.usage.agents_created >= run.budget.max_agents {
            return Err(StoreError::Invariant(format!(
                "Agent budget exhausted: {} of {}",
                run.usage.agents_created, run.budget.max_agents
            )));
        }
        let origin = self.get_session(run.origin_session_id)?;
        let now = Utc::now();
        let name = name.into();
        let role = role.into();
        let model = {
            let value = model.into();
            if value.trim().is_empty() {
                origin.model.clone()
            } else {
                value
            }
        };
        let session = Session {
            id: SessionId::new(),
            research_id: run.research_id,
            origin: SessionOrigin::WorkflowAgent,
            title: name.clone(),
            instructions: instructions.into(),
            model,
            access,
            status: SessionStatus::Ready,
            enabled_skills: if skills.is_empty() {
                origin.enabled_skills
            } else {
                skills.clone()
            },
            created_at: now,
            updated_at: now,
        };
        let participant = WorkflowParticipant {
            id: AgentInstanceId::new(),
            workflow_run_id: run.id,
            session_id: session.id,
            class_name: class_name.into(),
            name,
            role,
            instructions: session.instructions.clone(),
            model: session.model.clone(),
            skills: session.enabled_skills.clone(),
            status: ParticipantStatus::Active,
            created_at: now,
            updated_at: now,
        };
        run.usage.agents_created = run.usage.agents_created.saturating_add(1);
        run.updated_at = now;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO sessions (id, research_id, origin, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session.id.to_string(),
                session.research_id.to_string(),
                enum_string(session.origin)?,
                enum_string(session.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&session)?,
            ],
        )?;
        transaction.execute(
            "INSERT INTO workflow_participants
             (id, workflow_run_id, session_id, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                participant.id.to_string(),
                run.id.to_string(),
                session.id.to_string(),
                enum_string(participant.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&participant)?,
            ],
        )?;
        update_run_tx(&transaction, &run)?;
        let created = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            SessionEventPayload::SessionCreated {
                title: session.title.clone(),
            },
        )?;
        let attached = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            SessionEventPayload::WorkflowAgentAttached {
                workflow_run_id: run.id,
                agent_instance_id: participant.id,
                role: participant.role.clone(),
            },
        )?;
        let run_event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            WorkflowRunEventPayload::ParticipantCreated {
                agent_instance_id: participant.id,
                session_id: session.id,
                name: participant.name.clone(),
                role: participant.role.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(created);
        self.shared.publish_session(attached);
        self.shared.publish_run(run_event);
        Ok(participant)
    }

    pub fn get_participant(&self, id: AgentInstanceId) -> Result<WorkflowParticipant, StoreError> {
        self.query_document_by_id(
            "workflow_participants",
            id.to_string(),
            "workflow participant",
        )
    }

    pub fn list_participants(
        &self,
        run_id: WorkflowRunId,
    ) -> Result<Vec<WorkflowParticipant>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_participants WHERE workflow_run_id = ?1
             ORDER BY created_at ASC, id ASC",
            [run_id.to_string()],
        )
    }

    pub fn retire_participant(
        &self,
        id: AgentInstanceId,
    ) -> Result<WorkflowParticipant, StoreError> {
        let mut participant = self.get_participant(id)?;
        participant.status = ParticipantStatus::Retired;
        participant.updated_at = Utc::now();
        self.update_status_document(
            "workflow_participants",
            &id.to_string(),
            participant.status,
            &participant,
        )?;
        let run = self.get_workflow_run(participant.workflow_run_id)?;
        self.append_workflow_run_event(
            run.id,
            WorkflowRunEventPayload::ParticipantRetired {
                agent_instance_id: id,
            },
        )?;
        Ok(participant)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_action_invocation(
        &self,
        run_id: WorkflowRunId,
        scope_id: Option<TaskScopeId>,
        agent_id: AgentInstanceId,
        action_name: impl Into<String>,
        objective: impl Into<String>,
        arguments: Value,
    ) -> Result<ActionInvocation, StoreError> {
        let participant = self.get_participant(agent_id)?;
        if participant.workflow_run_id != run_id || participant.status != ParticipantStatus::Active
        {
            return Err(StoreError::Invariant(
                "action Agent is not active in this WorkflowRun".to_string(),
            ));
        }
        let now = Utc::now();
        let invocation = ActionInvocation {
            id: ActionInvocationId::new(),
            workflow_run_id: run_id,
            task_scope_id: scope_id,
            agent_instance_id: agent_id,
            session_id: participant.session_id,
            action_name: action_name.into(),
            objective: objective.into(),
            arguments,
            status: ActionStatus::Scheduled,
            output: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        self.insert_indexed_document(
            "action_invocations",
            &invocation.id.to_string(),
            &[run_id.to_string(), participant.session_id.to_string()],
            invocation.status,
            now,
            &invocation,
        )?;
        self.append_action_event(&invocation, None)?;
        Ok(invocation)
    }

    pub fn get_action_invocation(
        &self,
        id: ActionInvocationId,
    ) -> Result<ActionInvocation, StoreError> {
        self.query_document_by_id("action_invocations", id.to_string(), "action invocation")
    }

    pub fn list_action_invocations(
        &self,
        run_id: WorkflowRunId,
    ) -> Result<Vec<ActionInvocation>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM action_invocations WHERE workflow_run_id = ?1
             ORDER BY created_at ASC, id ASC",
            [run_id.to_string()],
        )
    }

    pub fn start_action_attempt(
        &self,
        invocation_id: ActionInvocationId,
    ) -> Result<ActionAttempt, StoreError> {
        let mut invocation = self.get_action_invocation(invocation_id)?;
        let run = self.get_workflow_run(invocation.workflow_run_id)?;
        if run.status != WorkflowRunStatus::Running {
            return Err(StoreError::Invariant(
                "WorkflowRun is not running".to_string(),
            ));
        }
        let number = self.connection()?.query_row(
            "SELECT COALESCE(MAX(number), 0) + 1 FROM action_attempts WHERE invocation_id = ?1",
            [invocation_id.to_string()],
            |row| row.get::<_, u32>(0),
        )?;
        let now = Utc::now();
        let attempt = ActionAttempt {
            id: ActionAttemptId::new(),
            workflow_run_id: invocation.workflow_run_id,
            invocation_id,
            number,
            turn_id: None,
            status: ActionStatus::Running,
            guidance: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        invocation.status = ActionStatus::Running;
        invocation.error = None;
        invocation.updated_at = now;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        update_status_document_tx(
            &transaction,
            "action_invocations",
            &invocation.id.to_string(),
            invocation.status,
            &invocation,
        )?;
        transaction.execute(
            "INSERT INTO action_attempts
             (id, workflow_run_id, invocation_id, number, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                attempt.id.to_string(),
                attempt.workflow_run_id.to_string(),
                attempt.invocation_id.to_string(),
                attempt.number,
                enum_string(attempt.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&attempt)?,
            ],
        )?;
        let event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            action_event_payload(&invocation, Some(attempt.id)),
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(event);
        self.add_budget_usage(
            run.id,
            BudgetUsage {
                actions_started: 1,
                ..BudgetUsage::default()
            },
        )?;
        Ok(attempt)
    }

    pub fn attach_turn_to_attempt(
        &self,
        attempt_id: ActionAttemptId,
        turn_id: TurnId,
    ) -> Result<ActionAttempt, StoreError> {
        let mut attempt = self.get_action_attempt(attempt_id)?;
        attempt.turn_id = Some(turn_id);
        attempt.updated_at = Utc::now();
        self.update_status_document(
            "action_attempts",
            &attempt.id.to_string(),
            attempt.status,
            &attempt,
        )?;
        Ok(attempt)
    }

    pub fn get_action_attempt(&self, id: ActionAttemptId) -> Result<ActionAttempt, StoreError> {
        self.query_document_by_id("action_attempts", id.to_string(), "action attempt")
    }

    pub fn list_action_attempts(
        &self,
        invocation_id: ActionInvocationId,
    ) -> Result<Vec<ActionAttempt>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM action_attempts WHERE invocation_id = ?1 ORDER BY number ASC",
            [invocation_id.to_string()],
        )
    }

    pub fn finish_action(
        &self,
        invocation_id: ActionInvocationId,
        attempt_id: ActionAttemptId,
        status: ActionStatus,
        output: Option<Value>,
        error: Option<String>,
    ) -> Result<ActionInvocation, StoreError> {
        if !status.is_terminal() {
            return Err(StoreError::Invariant(
                "finish_action requires a terminal status".to_string(),
            ));
        }
        let mut invocation = self.get_action_invocation(invocation_id)?;
        let mut attempt = self.get_action_attempt(attempt_id)?;
        if attempt.invocation_id != invocation.id {
            return Err(StoreError::Invariant(
                "ActionAttempt does not belong to invocation".to_string(),
            ));
        }
        let now = Utc::now();
        invocation.status = status;
        invocation.output = output;
        invocation.error = error.clone();
        invocation.updated_at = now;
        attempt.status = status;
        attempt.error = error;
        attempt.updated_at = now;
        let run = self.get_workflow_run(invocation.workflow_run_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        update_status_document_tx(
            &transaction,
            "action_invocations",
            &invocation.id.to_string(),
            status,
            &invocation,
        )?;
        update_status_document_tx(
            &transaction,
            "action_attempts",
            &attempt.id.to_string(),
            status,
            &attempt,
        )?;
        let event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            action_event_payload(&invocation, Some(attempt.id)),
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(event);
        if status == ActionStatus::Completed {
            self.add_budget_usage(
                run.id,
                BudgetUsage {
                    actions_completed: 1,
                    ..BudgetUsage::default()
                },
            )?;
        }
        Ok(invocation)
    }

    pub fn create_team(
        &self,
        run_id: WorkflowRunId,
        name: impl Into<String>,
        member_ids: Vec<AgentInstanceId>,
    ) -> Result<WorkflowTeam, StoreError> {
        self.validate_members(run_id, &member_ids)?;
        let now = Utc::now();
        let team = WorkflowTeam {
            id: TeamId::new(),
            workflow_run_id: run_id,
            name: name.into(),
            member_ids,
            created_at: now,
            updated_at: now,
        };
        self.insert_indexed_document(
            "workflow_teams",
            &team.id.to_string(),
            &[run_id.to_string()],
            "active",
            now,
            &team,
        )?;
        self.append_workflow_run_event(
            run_id,
            WorkflowRunEventPayload::TeamChanged {
                team_id: team.id,
                member_ids: team.member_ids.clone(),
            },
        )?;
        Ok(team)
    }

    pub fn get_team(&self, id: TeamId) -> Result<WorkflowTeam, StoreError> {
        self.query_document_by_id("workflow_teams", id.to_string(), "workflow team")
    }

    pub fn list_teams(&self, run_id: WorkflowRunId) -> Result<Vec<WorkflowTeam>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_teams WHERE workflow_run_id = ?1 ORDER BY created_at ASC",
            [run_id.to_string()],
        )
    }

    pub fn set_team_members(
        &self,
        id: TeamId,
        member_ids: Vec<AgentInstanceId>,
    ) -> Result<WorkflowTeam, StoreError> {
        let mut team = self.get_team(id)?;
        self.validate_members(team.workflow_run_id, &member_ids)?;
        team.member_ids = member_ids;
        team.updated_at = Utc::now();
        self.update_document("workflow_teams", &id.to_string(), team.updated_at, &team)?;
        self.append_workflow_run_event(
            team.workflow_run_id,
            WorkflowRunEventPayload::TeamChanged {
                team_id: id,
                member_ids: team.member_ids.clone(),
            },
        )?;
        Ok(team)
    }

    pub fn list_relations(&self, run_id: WorkflowRunId) -> Result<Vec<AgentRelation>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM agent_relations WHERE workflow_run_id = ?1 ORDER BY created_at ASC",
            [run_id.to_string()],
        )
    }

    pub fn set_relation(
        &self,
        run_id: WorkflowRunId,
        source: AgentInstanceId,
        target: AgentInstanceId,
        kind: impl Into<String>,
        instructions: impl Into<String>,
    ) -> Result<AgentRelation, StoreError> {
        self.validate_members(run_id, &[source, target])?;
        if source == target {
            return Err(StoreError::Invariant(
                "Agent relation endpoints must be distinct".to_string(),
            ));
        }
        let relation = AgentRelation {
            id: RelationId::new(),
            workflow_run_id: run_id,
            source_agent_id: source,
            target_agent_id: target,
            kind: kind.into(),
            instructions: instructions.into(),
            created_at: Utc::now(),
        };
        self.insert_indexed_document(
            "agent_relations",
            &relation.id.to_string(),
            &[run_id.to_string()],
            "active",
            relation.created_at,
            &relation,
        )?;
        self.append_workflow_run_event(
            run_id,
            WorkflowRunEventPayload::RelationChanged {
                source_agent_id: source,
                target_agent_id: target,
                kind: relation.kind.clone(),
            },
        )?;
        Ok(relation)
    }

    pub fn create_task_scope(
        &self,
        run_id: WorkflowRunId,
        parent_id: Option<TaskScopeId>,
        name: impl Into<String>,
        objective: impl Into<String>,
    ) -> Result<TaskScope, StoreError> {
        if let Some(parent_id) = parent_id {
            let parent = self.get_task_scope(parent_id)?;
            if parent.workflow_run_id != run_id {
                return Err(StoreError::Invariant(
                    "parent scope belongs to another WorkflowRun".to_string(),
                ));
            }
        }
        let now = Utc::now();
        let scope = TaskScope {
            id: TaskScopeId::new(),
            workflow_run_id: run_id,
            parent_id,
            name: name.into(),
            objective: objective.into(),
            status: TaskScopeStatus::Open,
            created_at: now,
            updated_at: now,
        };
        self.insert_indexed_document(
            "task_scopes",
            &scope.id.to_string(),
            &[run_id.to_string()],
            scope.status,
            now,
            &scope,
        )?;
        self.append_workflow_run_event(
            run_id,
            WorkflowRunEventPayload::TaskScopeChanged {
                task_scope_id: scope.id,
                status: "open".to_string(),
            },
        )?;
        Ok(scope)
    }

    pub fn get_task_scope(&self, id: TaskScopeId) -> Result<TaskScope, StoreError> {
        self.query_document_by_id("task_scopes", id.to_string(), "task scope")
    }

    pub fn list_task_scopes(&self, run_id: WorkflowRunId) -> Result<Vec<TaskScope>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM task_scopes WHERE workflow_run_id = ?1 ORDER BY created_at ASC",
            [run_id.to_string()],
        )
    }

    pub fn set_task_scope_status(
        &self,
        id: TaskScopeId,
        status: TaskScopeStatus,
    ) -> Result<TaskScope, StoreError> {
        let mut scope = self.get_task_scope(id)?;
        scope.status = status;
        scope.updated_at = Utc::now();
        self.update_status_document("task_scopes", &id.to_string(), status, &scope)?;
        self.append_workflow_run_event(
            scope.workflow_run_id,
            WorkflowRunEventPayload::TaskScopeChanged {
                task_scope_id: id,
                status: enum_string(status)?,
            },
        )?;
        Ok(scope)
    }

    pub fn create_timer(
        &self,
        run_id: WorkflowRunId,
        name: impl Into<String>,
        interval_ms: u64,
        policy: TimerPolicy,
    ) -> Result<WorkflowTimer, StoreError> {
        if interval_ms == 0 {
            return Err(StoreError::Invariant(
                "timer interval must be positive".to_string(),
            ));
        }
        let now = Utc::now();
        let interval = i64::try_from(interval_ms).unwrap_or(i64::MAX);
        let timer = WorkflowTimer {
            id: TimerId::new(),
            workflow_run_id: run_id,
            name: name.into(),
            interval_ms,
            policy,
            status: TimerStatus::Active,
            fire_count: 0,
            next_fire_at: now + Duration::milliseconds(interval),
            last_fired_at: None,
            created_at: now,
            updated_at: now,
        };
        self.insert_indexed_document(
            "workflow_timers",
            &timer.id.to_string(),
            &[run_id.to_string()],
            timer.status,
            now,
            &timer,
        )?;
        self.append_timer_event(&timer)?;
        Ok(timer)
    }

    pub fn get_timer(&self, id: TimerId) -> Result<WorkflowTimer, StoreError> {
        self.query_document_by_id("workflow_timers", id.to_string(), "workflow timer")
    }

    pub fn list_timers(&self, run_id: WorkflowRunId) -> Result<Vec<WorkflowTimer>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_timers WHERE workflow_run_id = ?1 ORDER BY created_at ASC",
            [run_id.to_string()],
        )
    }

    pub fn fire_timer(&self, id: TimerId) -> Result<WorkflowTimer, StoreError> {
        let mut timer = self.get_timer(id)?;
        if timer.status != TimerStatus::Active {
            return Err(StoreError::Invariant("timer is not active".to_string()));
        }
        let now = Utc::now();
        timer.fire_count = timer.fire_count.saturating_add(1);
        timer.last_fired_at = Some(now);
        timer.next_fire_at =
            now + Duration::milliseconds(i64::try_from(timer.interval_ms).unwrap_or(i64::MAX));
        timer.updated_at = now;
        self.update_status_document("workflow_timers", &id.to_string(), timer.status, &timer)?;
        self.append_timer_event(&timer)?;
        self.add_budget_usage(
            timer.workflow_run_id,
            BudgetUsage {
                timer_fires: 1,
                ..BudgetUsage::default()
            },
        )?;
        Ok(timer)
    }

    pub fn set_timer_status(
        &self,
        id: TimerId,
        status: TimerStatus,
    ) -> Result<WorkflowTimer, StoreError> {
        let mut timer = self.get_timer(id)?;
        timer.status = status;
        timer.updated_at = Utc::now();
        self.update_status_document("workflow_timers", &id.to_string(), status, &timer)?;
        self.append_timer_event(&timer)?;
        Ok(timer)
    }

    pub fn create_channel(
        &self,
        run_id: WorkflowRunId,
        name: impl Into<String>,
        schema: Value,
    ) -> Result<WorkflowChannel, StoreError> {
        let channel = WorkflowChannel {
            id: ChannelId::new(),
            workflow_run_id: run_id,
            name: name.into(),
            schema,
            created_at: Utc::now(),
        };
        self.insert_indexed_document(
            "workflow_channels",
            &channel.id.to_string(),
            &[run_id.to_string()],
            "active",
            channel.created_at,
            &channel,
        )?;
        self.append_workflow_run_event(
            run_id,
            WorkflowRunEventPayload::ChannelCreated {
                channel_id: channel.id,
                name: channel.name.clone(),
            },
        )?;
        Ok(channel)
    }

    pub fn get_channel(&self, id: ChannelId) -> Result<WorkflowChannel, StoreError> {
        self.query_document_by_id("workflow_channels", id.to_string(), "workflow channel")
    }

    pub fn list_channels(&self, run_id: WorkflowRunId) -> Result<Vec<WorkflowChannel>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_channels WHERE workflow_run_id = ?1 ORDER BY created_at ASC",
            [run_id.to_string()],
        )
    }

    pub fn publish_signal(
        &self,
        channel_id: ChannelId,
        sender_agent_id: Option<AgentInstanceId>,
        value: Value,
    ) -> Result<WorkflowSignal, StoreError> {
        let channel = self.get_channel(channel_id)?;
        if let Some(sender) = sender_agent_id {
            self.validate_members(channel.workflow_run_id, &[sender])?;
        }
        let research_id = self.get_workflow_run(channel.workflow_run_id)?.research_id;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let sequence = transaction.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_signals WHERE channel_id = ?1",
            [channel_id.to_string()],
            |row| row.get::<_, u64>(0),
        )?;
        let signal = WorkflowSignal {
            id: SignalId::new(),
            workflow_run_id: channel.workflow_run_id,
            channel_id,
            sender_agent_id,
            sequence,
            value,
            created_at: Utc::now(),
        };
        transaction.execute(
            "INSERT INTO workflow_signals
             (id, workflow_run_id, channel_id, sequence, created_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                signal.id.to_string(),
                signal.workflow_run_id.to_string(),
                channel_id.to_string(),
                sequence,
                signal.created_at.to_rfc3339(),
                serde_json::to_string(&signal)?
            ],
        )?;
        let event = append_run_event_tx(
            &transaction,
            research_id,
            channel.workflow_run_id,
            WorkflowRunEventPayload::SignalPublished {
                channel_id,
                signal_id: signal.id,
                signal_sequence: sequence,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(event);
        Ok(signal)
    }

    pub fn list_signals(
        &self,
        channel_id: ChannelId,
        after_sequence: u64,
    ) -> Result<Vec<WorkflowSignal>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_signals WHERE channel_id = ?1 AND sequence > ?2
             ORDER BY sequence ASC",
            params![channel_id.to_string(), after_sequence],
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_human_request(
        &self,
        run_id: WorkflowRunId,
        invocation_id: Option<ActionInvocationId>,
        attempt_id: Option<ActionAttemptId>,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        question: impl Into<String>,
        response_schema: Value,
    ) -> Result<HumanRequest, StoreError> {
        let mut run = self.get_workflow_run(run_id)?;
        if run.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot request human input for a terminal WorkflowRun".to_string(),
            ));
        }
        let now = Utc::now();
        let request = HumanRequest {
            id: HumanRequestId::new(),
            workflow_run_id: run_id,
            action_invocation_id: invocation_id,
            action_attempt_id: attempt_id,
            session_id,
            turn_id,
            question: question.into(),
            response_schema,
            status: HumanRequestStatus::Open,
            answer: None,
            created_at: now,
            resolved_at: None,
        };
        run.attention_required = true;
        run.updated_at = now;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_run_accepts_effect_tx(&transaction, run_id)?;
        transaction.execute(
            "INSERT INTO human_requests
             (id, workflow_run_id, session_id, status, created_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                request.id.to_string(),
                run_id.to_string(),
                session_id.to_string(),
                enum_string(request.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&request)?
            ],
        )?;
        update_run_tx(&transaction, &run)?;
        if let Some(turn_id) = turn_id {
            set_turn_status_tx(&transaction, turn_id, TurnStatus::WaitingForHuman, None)?;
        }
        let run_event = append_run_event_tx(
            &transaction,
            run.research_id,
            run_id,
            WorkflowRunEventPayload::HumanRequestOpened {
                human_request_id: request.id,
                session_id,
                question: request.question.clone(),
            },
        )?;
        let session_event = append_session_event_tx(
            &transaction,
            session_id,
            turn_id,
            None,
            SessionEventPayload::HumanRequestOpened {
                workflow_run_id: run_id,
                human_request_id: request.id,
                question: request.question.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(run_event);
        self.shared.publish_session(session_event);
        Ok(request)
    }

    pub fn get_human_request(&self, id: HumanRequestId) -> Result<HumanRequest, StoreError> {
        self.query_document_by_id("human_requests", id.to_string(), "human request")
    }

    pub fn list_human_requests(
        &self,
        run_id: WorkflowRunId,
    ) -> Result<Vec<HumanRequest>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM human_requests WHERE workflow_run_id = ?1 ORDER BY created_at ASC",
            [run_id.to_string()],
        )
    }

    pub fn answer_human_request(
        &self,
        id: HumanRequestId,
        answer: Value,
    ) -> Result<HumanRequest, StoreError> {
        let mut request = self.get_human_request(id)?;
        if request.status != HumanRequestStatus::Open {
            return Err(StoreError::Invariant(
                "human request is already resolved".to_string(),
            ));
        }
        request.status = HumanRequestStatus::Answered;
        request.answer = Some(answer);
        request.resolved_at = Some(Utc::now());
        let mut run = self.get_workflow_run(request.workflow_run_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE human_requests SET status = ?1, document_json = ?2 WHERE id = ?3",
            params![
                enum_string(request.status)?,
                serde_json::to_string(&request)?,
                request.id.to_string(),
            ],
        )?;
        ensure_one(changed, "human_requests")?;
        let remaining = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM human_requests WHERE workflow_run_id = ?1
             AND status = 'open' AND id != ?2)",
            params![run.id.to_string(), id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        run.attention_required = remaining;
        run.updated_at = Utc::now();
        update_run_tx(&transaction, &run)?;
        if let Some(turn_id) = request.turn_id {
            set_turn_status_tx(&transaction, turn_id, TurnStatus::Running, None)?;
        }
        let run_event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            WorkflowRunEventPayload::HumanRequestResolved {
                human_request_id: id,
            },
        )?;
        let session_event = append_session_event_tx(
            &transaction,
            request.session_id,
            request.turn_id,
            None,
            SessionEventPayload::HumanRequestResolved {
                workflow_run_id: run.id,
                human_request_id: id,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(run_event);
        self.shared.publish_session(session_event);
        Ok(request)
    }

    pub fn create_control_message(
        &self,
        run_id: WorkflowRunId,
        session_id: SessionId,
        invocation_id: Option<ActionInvocationId>,
        kind: ControlMessageKind,
        content: impl Into<String>,
    ) -> Result<ControlMessage, StoreError> {
        let run = self.get_workflow_run(run_id)?;
        if run.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot control a terminal WorkflowRun".to_string(),
            ));
        }
        let message = ControlMessage {
            id: ControlMessageId::new(),
            workflow_run_id: run_id,
            session_id,
            action_invocation_id: invocation_id,
            kind,
            content: content.into(),
            status: ControlMessageStatus::Pending,
            created_at: Utc::now(),
            applied_at: None,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_run_accepts_effect_tx(&transaction, run_id)?;
        transaction.execute(
            "INSERT INTO control_messages
             (id, workflow_run_id, session_id, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                message.id.to_string(),
                run_id.to_string(),
                session_id.to_string(),
                enum_string(message.status)?,
                message.created_at.to_rfc3339(),
                serde_json::to_string(&message)?,
            ],
        )?;
        let event = append_run_event_tx(
            &transaction,
            run.research_id,
            run.id,
            WorkflowRunEventPayload::ControlMessageQueued {
                control_message_id: message.id,
                session_id,
                kind,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(event);
        Ok(message)
    }

    pub fn take_control_messages(
        &self,
        run_id: WorkflowRunId,
        session_id: SessionId,
        invocation_id: Option<ActionInvocationId>,
    ) -> Result<Vec<ControlMessage>, StoreError> {
        let mut messages: Vec<ControlMessage> = self.query_documents(
            "SELECT document_json FROM control_messages
             WHERE workflow_run_id = ?1 AND session_id = ?2 AND status = 'pending'
             ORDER BY created_at ASC",
            params![run_id.to_string(), session_id.to_string()],
        )?;
        messages.retain(|message| {
            message.action_invocation_id.is_none() || message.action_invocation_id == invocation_id
        });
        for message in &mut messages {
            message.status = ControlMessageStatus::Applied;
            message.applied_at = Some(Utc::now());
            self.update_status_document(
                "control_messages",
                &message.id.to_string(),
                message.status,
                message,
            )?;
            self.append_workflow_run_event(
                run_id,
                WorkflowRunEventPayload::ControlMessageApplied {
                    control_message_id: message.id,
                },
            )?;
            self.append_session_event(
                session_id,
                None,
                None,
                SessionEventPayload::ControlMessageApplied {
                    workflow_run_id: run_id,
                    control_message_id: message.id,
                    kind: message.kind,
                    content: message.content.clone(),
                },
            )?;
        }
        Ok(messages)
    }

    pub fn list_control_messages(
        &self,
        run_id: WorkflowRunId,
    ) -> Result<Vec<ControlMessage>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM control_messages WHERE workflow_run_id = ?1 ORDER BY created_at ASC",
            [run_id.to_string()],
        )
    }

    pub fn append_workflow_run_event(
        &self,
        run_id: WorkflowRunId,
        payload: WorkflowRunEventPayload,
    ) -> Result<WorkflowRunEvent, StoreError> {
        let run = self.get_workflow_run(run_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let event = append_run_event_tx(&transaction, run.research_id, run_id, payload)?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_run(event.clone());
        Ok(event)
    }

    pub fn list_workflow_run_events(
        &self,
        run_id: WorkflowRunId,
        after_sequence: u64,
    ) -> Result<Vec<WorkflowRunEvent>, StoreError> {
        self.query_documents(
            "SELECT event_json FROM workflow_run_events WHERE workflow_run_id = ?1 AND sequence > ?2
             ORDER BY sequence ASC",
            params![run_id.to_string(), after_sequence],
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_artifact(
        &self,
        research_id: ResearchId,
        workflow_run_id: WorkflowRunId,
        session_id: Option<SessionId>,
        action_invocation_id: Option<ActionInvocationId>,
        kind: ArtifactKind,
        name: impl Into<String>,
        media_type: impl Into<String>,
        metadata: Value,
        bytes: &[u8],
    ) -> Result<Artifact, StoreError> {
        let run = self.get_workflow_run(workflow_run_id)?;
        if run.research_id != research_id {
            return Err(StoreError::Invariant(
                "artifact Research does not match WorkflowRun".to_string(),
            ));
        }
        let id = ArtifactId::new();
        let name = name.into();
        let stored = store_artifact_file(
            &self.shared.artifact_root,
            research_id,
            workflow_run_id,
            session_id,
            id,
            &name,
            bytes,
        )?;
        let artifact = Artifact {
            id,
            research_id,
            workflow_run_id,
            session_id,
            action_invocation_id,
            kind,
            name,
            media_type: media_type.into(),
            relative_path: stored.relative_path,
            sha256: stored.sha256,
            size_bytes: stored.size_bytes,
            metadata,
            created_at: Utc::now(),
        };
        self.insert_indexed_document(
            "artifacts",
            &id.to_string(),
            &[workflow_run_id.to_string()],
            "created",
            artifact.created_at,
            &artifact,
        )?;
        Ok(artifact)
    }

    pub fn list_artifacts(&self, run_id: WorkflowRunId) -> Result<Vec<Artifact>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM artifacts WHERE workflow_run_id = ?1 ORDER BY created_at ASC",
            [run_id.to_string()],
        )
    }

    pub fn list_research_artifacts(
        &self,
        research_id: ResearchId,
    ) -> Result<Vec<Artifact>, StoreError> {
        self.query_documents(
            "SELECT a.document_json FROM artifacts a JOIN workflow_runs wr ON wr.id = a.workflow_run_id
             WHERE wr.research_id = ?1 ORDER BY a.created_at DESC",
            [research_id.to_string()],
        )
    }

    pub fn get_artifact(&self, id: ArtifactId) -> Result<Artifact, StoreError> {
        self.query_document_by_id("artifacts", id.to_string(), "artifact")
    }

    pub fn read_artifact(&self, artifact: &Artifact) -> Result<Vec<u8>, StoreError> {
        let path = self.shared.artifact_root.join(&artifact.relative_path);
        let canonical_root = self
            .shared
            .artifact_root
            .canonicalize()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let canonical_path = path
            .canonicalize()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        if !canonical_path.starts_with(canonical_root) {
            return Err(StoreError::Invariant(
                "artifact path escaped the artifact root".to_string(),
            ));
        }
        std::fs::read(canonical_path).map_err(|error| StoreError::Io(error.to_string()))
    }

    fn append_action_event(
        &self,
        invocation: &ActionInvocation,
        attempt_id: Option<ActionAttemptId>,
    ) -> Result<(), StoreError> {
        self.append_workflow_run_event(
            invocation.workflow_run_id,
            action_event_payload(invocation, attempt_id),
        )?;
        Ok(())
    }

    fn append_timer_event(&self, timer: &WorkflowTimer) -> Result<(), StoreError> {
        self.append_workflow_run_event(
            timer.workflow_run_id,
            WorkflowRunEventPayload::TimerChanged {
                timer_id: timer.id,
                status: enum_string(timer.status)?,
                fire_count: timer.fire_count,
            },
        )?;
        Ok(())
    }

    fn validate_members(
        &self,
        run_id: WorkflowRunId,
        member_ids: &[AgentInstanceId],
    ) -> Result<(), StoreError> {
        let mut unique = std::collections::BTreeSet::new();
        for id in member_ids {
            if !unique.insert(*id) {
                return Err(StoreError::Invariant(format!(
                    "duplicate Agent in Team or relation: {id}"
                )));
            }
            let participant = self.get_participant(*id)?;
            if participant.workflow_run_id != run_id
                || participant.status != ParticipantStatus::Active
            {
                return Err(StoreError::Invariant(format!(
                    "Agent {id} is not active in this WorkflowRun"
                )));
            }
        }
        Ok(())
    }

    fn ensure_research(&self, id: ResearchId) -> Result<(), StoreError> {
        let exists = self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM researches WHERE id = ?1)",
            [id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if exists {
            Ok(())
        } else {
            Err(StoreError::NotFound {
                entity: "research",
                id: id.to_string(),
            })
        }
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.connection.lock().map_err(|_| StoreError::LockPoisoned)
    }

    fn insert_document<T: Serialize>(
        &self,
        table: &str,
        id: &str,
        status: Option<&str>,
        updated_at: chrono::DateTime<Utc>,
        document: &T,
    ) -> Result<(), StoreError> {
        let sql = if status.is_some() {
            format!(
                "INSERT INTO {table} (id, status, updated_at, document_json) VALUES (?1, ?2, ?3, ?4)"
            )
        } else {
            format!("INSERT INTO {table} (id, updated_at, document_json) VALUES (?1, ?3, ?4)")
        };
        self.connection()?.execute(
            &sql,
            params![
                id,
                status,
                updated_at.to_rfc3339(),
                serde_json::to_string(document)?
            ],
        )?;
        Ok(())
    }

    fn insert_indexed_document<T: Serialize, S: Serialize>(
        &self,
        table: &str,
        id: &str,
        indexes: &[String],
        status: S,
        updated_at: chrono::DateTime<Utc>,
        document: &T,
    ) -> Result<(), StoreError> {
        let columns = match indexes.len() {
            1 => "id, workflow_run_id, status, updated_at, document_json",
            2 => "id, workflow_run_id, session_id, status, updated_at, document_json",
            count => {
                return Err(StoreError::Invariant(format!(
                    "unsupported index count {count}"
                )));
            }
        };
        let placeholders = if indexes.len() == 1 {
            "?1, ?2, ?3, ?4, ?5"
        } else {
            "?1, ?2, ?3, ?4, ?5, ?6"
        };
        let sql = format!("INSERT INTO {table} ({columns}) VALUES ({placeholders})");
        let connection = self.connection()?;
        if indexes.len() == 1 {
            connection.execute(
                &sql,
                params![
                    id,
                    indexes[0],
                    enum_string(status)?,
                    updated_at.to_rfc3339(),
                    serde_json::to_string(document)?
                ],
            )?;
        } else {
            connection.execute(
                &sql,
                params![
                    id,
                    indexes[0],
                    indexes[1],
                    enum_string(status)?,
                    updated_at.to_rfc3339(),
                    serde_json::to_string(document)?
                ],
            )?;
        }
        Ok(())
    }

    fn update_document<T: Serialize>(
        &self,
        table: &str,
        id: &str,
        updated_at: chrono::DateTime<Utc>,
        document: &T,
    ) -> Result<(), StoreError> {
        let changed = self.connection()?.execute(
            &format!("UPDATE {table} SET updated_at = ?1, document_json = ?2 WHERE id = ?3"),
            params![
                updated_at.to_rfc3339(),
                serde_json::to_string(document)?,
                id
            ],
        )?;
        ensure_one(changed, table)
    }

    fn update_status_document<T: Serialize, S: Serialize>(
        &self,
        table: &str,
        id: &str,
        status: S,
        document: &T,
    ) -> Result<(), StoreError> {
        let now = Utc::now();
        let changed = self.connection()?.execute(
            &format!(
                "UPDATE {table} SET status = ?1, updated_at = ?2, document_json = ?3 WHERE id = ?4"
            ),
            params![
                enum_string(status)?,
                now.to_rfc3339(),
                serde_json::to_string(document)?,
                id
            ],
        )?;
        ensure_one(changed, table)
    }

    fn query_document_by_id<T: DeserializeOwned>(
        &self,
        table: &str,
        id: String,
        entity: &'static str,
    ) -> Result<T, StoreError> {
        let document = self
            .connection()?
            .query_row(
                &format!("SELECT document_json FROM {table} WHERE id = ?1"),
                [id.clone()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound { entity, id })?;
        Ok(serde_json::from_str(&document)?)
    }

    fn query_documents<T: DeserializeOwned, P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<T>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map(params, |row| row.get::<_, String>(0))?;
        let mut values = Vec::new();
        for row in rows {
            values.push(serde_json::from_str(&row?)?);
        }
        Ok(values)
    }
}

fn initialize(connection: &Connection) -> Result<(), StoreError> {
    let current: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current != 0 && current != SCHEMA_VERSION {
        return Err(StoreError::Invariant(format!(
            "database schema v{current} is not supported; PaperMachine requires a fresh v{SCHEMA_VERSION} database"
        )));
    }
    connection.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA user_version = {SCHEMA_VERSION};
         CREATE TABLE IF NOT EXISTS researches (
           id TEXT PRIMARY KEY, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sessions (
           id TEXT PRIMARY KEY, research_id TEXT NOT NULL REFERENCES researches(id),
           origin TEXT NOT NULL, status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS sessions_research_updated ON sessions(research_id, updated_at DESC);
         CREATE TABLE IF NOT EXISTS turns (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS turns_session_created ON turns(session_id, created_at ASC);
         CREATE TABLE IF NOT EXISTS steps (
           id TEXT PRIMARY KEY, turn_id TEXT NOT NULL REFERENCES turns(id), sequence INTEGER NOT NULL,
           status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL,
           UNIQUE(turn_id, sequence)
         );
         CREATE TABLE IF NOT EXISTS session_events (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), turn_id TEXT, step_id TEXT,
           sequence INTEGER NOT NULL, occurred_at TEXT NOT NULL, event_json TEXT NOT NULL,
           UNIQUE(session_id, sequence)
         );
         CREATE INDEX IF NOT EXISTS session_events_sequence ON session_events(session_id, sequence ASC);
         CREATE TABLE IF NOT EXISTS workflows (
           id TEXT NOT NULL, slug TEXT NOT NULL, version TEXT NOT NULL, source TEXT NOT NULL,
           definition_path TEXT NOT NULL, sha256 TEXT NOT NULL, updated_at TEXT NOT NULL,
           document_json TEXT NOT NULL, PRIMARY KEY(slug, version)
         );
         CREATE TABLE IF NOT EXISTS workflow_runs (
           id TEXT PRIMARY KEY, research_id TEXT NOT NULL REFERENCES researches(id),
           origin_session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           attention_required INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS runs_research_updated ON workflow_runs(research_id, updated_at DESC);
         CREATE INDEX IF NOT EXISTS runs_origin_updated ON workflow_runs(origin_session_id, updated_at DESC);
         CREATE TABLE IF NOT EXISTS workflow_run_events (
           id TEXT PRIMARY KEY, research_id TEXT NOT NULL, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id),
           sequence INTEGER NOT NULL, occurred_at TEXT NOT NULL, event_json TEXT NOT NULL,
           UNIQUE(workflow_run_id, sequence)
         );
         CREATE INDEX IF NOT EXISTS run_events_sequence ON workflow_run_events(workflow_run_id, sequence ASC);
         CREATE TABLE IF NOT EXISTS workflow_participants (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL, UNIQUE(workflow_run_id, session_id)
         );
         CREATE TABLE IF NOT EXISTS action_invocations (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS action_attempts (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id),
           invocation_id TEXT NOT NULL REFERENCES action_invocations(id), number INTEGER NOT NULL,
           status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL,
           UNIQUE(invocation_id, number)
         );
         CREATE TABLE IF NOT EXISTS workflow_teams (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS agent_relations (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS task_scopes (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflow_timers (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflow_channels (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflow_signals (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id),
           channel_id TEXT NOT NULL REFERENCES workflow_channels(id), sequence INTEGER NOT NULL,
           created_at TEXT NOT NULL, document_json TEXT NOT NULL, UNIQUE(channel_id, sequence)
         );
         CREATE TABLE IF NOT EXISTS human_requests (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT NOT NULL, updated_at TEXT GENERATED ALWAYS AS (COALESCE(json_extract(document_json, '$.resolved_at'), created_at)) VIRTUAL,
           document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS control_messages (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS artifacts (
           id TEXT PRIMARY KEY, workflow_run_id TEXT NOT NULL REFERENCES workflow_runs(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );"
    ))?;
    reconcile_terminal_run_resources(connection)?;
    Ok(())
}

fn ensure_run_accepts_effect_tx(
    transaction: &Transaction<'_>,
    run_id: WorkflowRunId,
) -> Result<(), StoreError> {
    let status = transaction.query_row(
        "SELECT status FROM workflow_runs WHERE id = ?1",
        [run_id.to_string()],
        |row| row.get::<_, String>(0),
    )?;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        return Err(StoreError::Invariant(format!(
            "WorkflowRun is terminal with status {status}"
        )));
    }
    Ok(())
}

fn terminalize_run_resources_tx(
    transaction: &Transaction<'_>,
    run_id: WorkflowRunId,
    run_status: WorkflowRunStatus,
    now: chrono::DateTime<Utc>,
) -> Result<(), StoreError> {
    let now = now.to_rfc3339();
    transaction.execute(
        "UPDATE control_messages
         SET status = 'cancelled', updated_at = ?2,
             document_json = json_set(document_json, '$.status', 'cancelled')
         WHERE workflow_run_id = ?1 AND status = 'pending'",
        params![run_id.to_string(), now],
    )?;
    transaction.execute(
        "UPDATE human_requests
         SET status = 'cancelled',
             document_json = json_set(
                 json_set(document_json, '$.status', 'cancelled'),
                 '$.resolved_at', ?2
             )
         WHERE workflow_run_id = ?1 AND status = 'open'",
        params![run_id.to_string(), now],
    )?;
    let timer_status = if run_status == WorkflowRunStatus::Completed {
        "completed"
    } else {
        "cancelled"
    };
    transaction.execute(
        "UPDATE workflow_timers
         SET status = ?2, updated_at = ?3,
             document_json = json_set(
                 json_set(document_json, '$.status', ?2),
                 '$.updated_at', ?3
             )
         WHERE workflow_run_id = ?1 AND status IN ('active', 'paused')",
        params![run_id.to_string(), timer_status, now],
    )?;
    Ok(())
}

fn reconcile_terminal_run_resources(connection: &Connection) -> Result<(), StoreError> {
    let run_ids = {
        let mut statement = connection.prepare(
            "SELECT id, status FROM workflow_runs
             WHERE status IN ('completed', 'failed', 'cancelled')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (id, status) in run_ids {
        let run_id = WorkflowRunId::from_str(&id)
            .map_err(|error| StoreError::Invariant(error.to_string()))?;
        let run_status = match status.as_str() {
            "completed" => WorkflowRunStatus::Completed,
            "failed" => WorkflowRunStatus::Failed,
            "cancelled" => WorkflowRunStatus::Cancelled,
            _ => continue,
        };
        let transaction = connection.unchecked_transaction()?;
        terminalize_run_resources_tx(&transaction, run_id, run_status, Utc::now())?;
        transaction.commit()?;
    }
    Ok(())
}

fn append_session_event_tx(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    turn_id: Option<TurnId>,
    step_id: Option<StepId>,
    payload: SessionEventPayload,
) -> Result<SessionEvent, StoreError> {
    let sequence = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM session_events WHERE session_id = ?1",
        [session_id.to_string()],
        |row| row.get::<_, u64>(0),
    )?;
    let event = SessionEvent {
        id: EventId::new(),
        sequence,
        session_id,
        turn_id,
        step_id,
        occurred_at: Utc::now(),
        payload,
    };
    transaction.execute(
        "INSERT INTO session_events
         (id, session_id, turn_id, step_id, sequence, occurred_at, event_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            event.id.to_string(),
            session_id.to_string(),
            turn_id.map(|id| id.to_string()),
            step_id.map(|id| id.to_string()),
            sequence,
            event.occurred_at.to_rfc3339(),
            serde_json::to_string(&event)?
        ],
    )?;
    Ok(event)
}

fn append_run_event_tx(
    transaction: &Transaction<'_>,
    research_id: ResearchId,
    run_id: WorkflowRunId,
    payload: WorkflowRunEventPayload,
) -> Result<WorkflowRunEvent, StoreError> {
    let sequence = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_run_events WHERE workflow_run_id = ?1",
        [run_id.to_string()],
        |row| row.get::<_, u64>(0),
    )?;
    let event = WorkflowRunEvent {
        id: EventId::new(),
        sequence,
        research_id,
        workflow_run_id: run_id,
        occurred_at: Utc::now(),
        payload,
    };
    transaction.execute(
        "INSERT INTO workflow_run_events
         (id, research_id, workflow_run_id, sequence, occurred_at, event_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            event.id.to_string(),
            research_id.to_string(),
            run_id.to_string(),
            sequence,
            event.occurred_at.to_rfc3339(),
            serde_json::to_string(&event)?
        ],
    )?;
    Ok(event)
}

fn update_run_tx(transaction: &Transaction<'_>, run: &WorkflowRun) -> Result<(), StoreError> {
    let changed = transaction.execute(
        "UPDATE workflow_runs SET status = ?1, attention_required = ?2, updated_at = ?3,
         document_json = ?4 WHERE id = ?5",
        params![
            enum_string(run.status)?,
            run.attention_required,
            run.updated_at.to_rfc3339(),
            serde_json::to_string(run)?,
            run.id.to_string()
        ],
    )?;
    ensure_one(changed, "workflow_runs")
}

fn set_turn_status_tx(
    transaction: &Transaction<'_>,
    turn_id: TurnId,
    status: TurnStatus,
    error: Option<String>,
) -> Result<(), StoreError> {
    let document = transaction.query_row(
        "SELECT document_json FROM turns WHERE id = ?1",
        [turn_id.to_string()],
        |row| row.get::<_, String>(0),
    )?;
    let mut turn: Turn = serde_json::from_str(&document)?;
    turn.status = status;
    turn.error = error;
    turn.updated_at = Utc::now();
    update_status_document_tx(transaction, "turns", &turn_id.to_string(), status, &turn)?;
    let session_document = transaction.query_row(
        "SELECT document_json FROM sessions WHERE id = ?1",
        [turn.session_id.to_string()],
        |row| row.get::<_, String>(0),
    )?;
    let mut session: Session = serde_json::from_str(&session_document)?;
    session.status = match status {
        TurnStatus::WaitingForHuman => SessionStatus::WaitingForHuman,
        TurnStatus::Paused => SessionStatus::Paused,
        TurnStatus::Failed => SessionStatus::Failed,
        TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Cancelled => {
            SessionStatus::Ready
        }
        TurnStatus::Queued | TurnStatus::Running => SessionStatus::Running,
    };
    session.updated_at = turn.updated_at;
    update_status_document_tx(
        transaction,
        "sessions",
        &session.id.to_string(),
        session.status,
        &session,
    )
}

fn update_status_document_tx<T: Serialize, S: Serialize>(
    transaction: &Transaction<'_>,
    table: &str,
    id: &str,
    status: S,
    document: &T,
) -> Result<(), StoreError> {
    let changed = transaction.execute(
        &format!(
            "UPDATE {table} SET status = ?1, updated_at = ?2, document_json = ?3 WHERE id = ?4"
        ),
        params![
            enum_string(status)?,
            Utc::now().to_rfc3339(),
            serde_json::to_string(document)?,
            id
        ],
    )?;
    ensure_one(changed, table)
}

fn action_event_payload(
    invocation: &ActionInvocation,
    attempt_id: Option<ActionAttemptId>,
) -> WorkflowRunEventPayload {
    WorkflowRunEventPayload::ActionChanged {
        action_invocation_id: invocation.id,
        action_attempt_id: attempt_id,
        agent_instance_id: invocation.agent_instance_id,
        action_name: invocation.action_name.clone(),
        status: invocation.status,
        error: invocation.error.clone(),
    }
}

fn enum_string<T: Serialize>(value: T) -> Result<String, StoreError> {
    let serialized = serde_json::to_string(&value)?;
    Ok(serialized.trim_matches('"').to_string())
}

fn ensure_one(changed: usize, table: &str) -> Result<(), StoreError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::Invariant(format!(
            "expected one {table} row to change, changed {changed}"
        )))
    }
}
