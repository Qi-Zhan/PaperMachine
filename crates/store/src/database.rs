use crate::AgentInterrupt;
use crate::NewActionInvocation;
use crate::NewSession;
use crate::StoreError;
use crate::StoreShared;
use crate::TurnContextCheckpoint;
use crate::artifact::read_artifact_file;
use crate::artifact::reconcile_artifact_files;
use crate::artifact::remove_artifact_file;
use crate::artifact::store_artifact_file;
use crate::filesystem::ManagedFs;
use crate::filesystem::write_atomic;
use chrono::Utc;
use papermachine_protocol::*;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::OptionalExtension;
use rusqlite::Transaction;
use rusqlite::TransactionBehavior;
use rusqlite::params;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::broadcast;

const SCHEMA_VERSION: u32 = 25;
const PROJECT_SYSTEM_PROMPT_PATH: &str = "prompts/system.md";
const MAX_SYSTEM_PROMPT_BYTES: usize = 256 * 1024;
const MAX_PROJECT_CHANGES_PER_READ: usize = 10_001;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectChange {
    pub sequence: u64,
    pub kind: String,
    pub entity_id: String,
    pub session_id: Option<SessionId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectChangeBatch {
    pub captured_cursor: u64,
    pub changes: Vec<ProjectChange>,
    pub last_sequence: Option<u64>,
}

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    shared: StoreShared,
    managed_fs: ManagedFs,
    managed_root: PathBuf,
}

impl Store {
    /// Create one fresh current-schema Project store. Existing managed state is
    /// never opened or upgraded through this path.
    pub fn create(managed_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let requested_root = managed_root.as_ref();
        let database = requested_root.join("state/project.db");
        if database.exists() {
            return Err(StoreError::Invariant(format!(
                "Project database already exists: {}",
                database.display()
            )));
        }
        let managed_root = create_managed_root(requested_root)?;
        let connection = Connection::open(managed_root.join("state/project.db"))?;
        initialize_new(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            shared: StoreShared::new(&managed_root)?,
            managed_fs: ManagedFs::open(&managed_root)?,
            managed_root,
        })
    }

    /// Open one existing Project store only when its schema and managed layout
    /// exactly match the current contract.
    pub fn open(managed_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let managed_root = open_managed_root(managed_root.as_ref())?;
        let database = managed_root.join("state/project.db");
        let connection = Connection::open_with_flags(
            &database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        verify_current(&connection)?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
            shared: StoreShared::new(&managed_root)?,
            managed_fs: ManagedFs::open(&managed_root)?,
            managed_root,
        };
        store.replay_all_agent_rollouts()?;
        Ok(store)
    }

    pub fn open_in_memory(managed_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let managed_root = create_managed_root(managed_root.as_ref())?;
        let connection = Connection::open_in_memory()?;
        initialize_new(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            shared: StoreShared::new(&managed_root)?,
            managed_fs: ManagedFs::open(&managed_root)?,
            managed_root,
        })
    }

    pub fn managed_root(&self) -> &Path {
        &self.managed_root
    }

    pub fn write_managed_file(
        &self,
        relative_path: impl AsRef<Path>,
        content: &[u8],
    ) -> Result<PathBuf, StoreError> {
        self.managed_fs.write_atomic(relative_path, content)
    }

    pub fn read_managed_text(
        &self,
        relative_path: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<String, StoreError> {
        self.managed_fs.read_string(relative_path, max_bytes)
    }

    pub fn read_managed_file(
        &self,
        relative_path: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StoreError> {
        self.managed_fs.read(relative_path, max_bytes)
    }

    pub fn ensure_managed_directory(
        &self,
        relative_path: impl AsRef<Path>,
    ) -> Result<(), StoreError> {
        self.managed_fs.ensure_dir(relative_path)
    }

    pub fn list_managed_directories(
        &self,
        relative_path: impl AsRef<Path>,
    ) -> Result<Vec<String>, StoreError> {
        self.managed_fs.list_directories(relative_path)
    }

    pub fn managed_file_exists(&self, relative_path: impl AsRef<Path>) -> Result<bool, StoreError> {
        self.managed_fs.is_regular_file(relative_path)
    }

    pub fn remove_managed_entry(&self, relative_path: impl AsRef<Path>) -> Result<(), StoreError> {
        self.managed_fs.remove(relative_path)
    }

    pub fn managed_path(&self, relative_path: impl AsRef<Path>) -> Result<PathBuf, StoreError> {
        self.managed_fs.absolute_path(relative_path)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.shared.session_events.subscribe()
    }

    pub fn create_project(
        &self,
        name: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
    ) -> Result<Project, StoreError> {
        self.create_project_with_id(ProjectId::new(), name, workspace_root)
    }

    pub fn create_project_with_id(
        &self,
        id: ProjectId,
        name: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
    ) -> Result<Project, StoreError> {
        let requested_workspace = workspace_root.into();
        if !requested_workspace.is_absolute() {
            return Err(StoreError::Invariant(
                "Project Workspace must be an absolute path".to_string(),
            ));
        }
        std::fs::create_dir_all(&requested_workspace)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let workspace_root = requested_workspace
            .canonicalize()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        ensure_workspace_is_external(&workspace_root, &self.managed_root)?;
        let workspace_string = workspace_root.to_string_lossy().into_owned();
        let exists = self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE json_extract(document_json, '$.workspace.path') = ?1)",
            [&workspace_string],
            |row| row.get::<_, bool>(0),
        )?;
        if exists {
            return Err(StoreError::Invariant(format!(
                "Project Workspace is already registered: {}",
                workspace_root.display()
            )));
        }
        let now = Utc::now();
        let project = Project {
            id,
            name: name.into(),
            workspace: WorkspaceAttachment::single(workspace_string),
            created_at: now,
            updated_at: now,
        };
        self.insert_document(
            "projects",
            &project.id.to_string(),
            None,
            project.updated_at,
            &project,
        )?;
        Ok(project)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM projects ORDER BY updated_at DESC, id ASC",
            [],
        )
    }

    pub fn get_project(&self, id: ProjectId) -> Result<Project, StoreError> {
        self.query_document_by_id("projects", id.to_string(), "project")
    }

    pub fn relocate_project(
        &self,
        id: ProjectId,
        workspace_root: impl Into<PathBuf>,
    ) -> Result<Project, StoreError> {
        let requested_workspace = workspace_root.into();
        if !requested_workspace.is_absolute() {
            return Err(StoreError::Invariant(
                "Project Workspace must be an absolute path".to_string(),
            ));
        }
        let workspace_root = requested_workspace
            .canonicalize()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        ensure_workspace_is_external(&workspace_root, &self.managed_root)?;
        let mut project = self.get_project(id)?;
        project.workspace.path = workspace_root.to_string_lossy().into_owned();
        project.workspace.revision = project
            .workspace
            .revision
            .checked_add(1)
            .ok_or_else(|| StoreError::Invariant("Workspace revision overflow".to_string()))?;
        project.updated_at = Utc::now();
        self.update_document(
            "projects",
            &project.id.to_string(),
            project.updated_at,
            &project,
        )?;
        Ok(project)
    }

    pub fn get_project_system_prompt(
        &self,
        id: ProjectId,
    ) -> Result<ProjectSystemPrompt, StoreError> {
        self.ensure_project(id)?;
        let content = self
            .managed_fs
            .read_string(PROJECT_SYSTEM_PROMPT_PATH, MAX_SYSTEM_PROMPT_BYTES)?;
        Ok(ProjectSystemPrompt {
            relative_path: PROJECT_SYSTEM_PROMPT_PATH.to_string(),
            sha256: hash_text(&content),
            content,
        })
    }

    pub fn set_project_system_prompt(
        &self,
        id: ProjectId,
        content: impl Into<String>,
    ) -> Result<ProjectSystemPrompt, StoreError> {
        self.ensure_project(id)?;
        let content = content.into();
        validate_system_prompt(&content)?;
        self.managed_fs
            .write_atomic(PROJECT_SYSTEM_PROMPT_PATH, content.as_bytes())?;
        self.get_project_system_prompt(id)
    }

    pub fn get_session(&self, id: SessionId) -> Result<Session, StoreError> {
        self.query_document_by_id("sessions", id.to_string(), "session")
    }

    pub fn list_sessions(&self, project_id: ProjectId) -> Result<Vec<Session>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM sessions WHERE project_id = ?1
             AND json_extract(document_json, '$.archived_at') IS NULL
             ORDER BY updated_at DESC, id ASC",
            [project_id.to_string()],
        )
    }

    pub fn list_recent_sessions(
        &self,
        project_id: ProjectId,
        limit: usize,
    ) -> Result<Vec<Session>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM sessions WHERE project_id = ?1
             AND json_extract(document_json, '$.archived_at') IS NULL
             ORDER BY updated_at DESC, id ASC LIMIT ?2",
            params![project_id.to_string(), limit],
        )
    }

    /// All Sessions that belong to a Project, including archived history.
    pub fn list_project_sessions(&self, project_id: ProjectId) -> Result<Vec<Session>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM sessions WHERE project_id = ?1
             ORDER BY updated_at DESC, id ASC",
            [project_id.to_string()],
        )
    }

    pub fn archive_session(&self, id: SessionId) -> Result<Session, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session: Session =
            load_document_tx(&transaction, "sessions", &id.to_string(), "session")?;
        if session.archived_at.is_some() {
            return Ok(session);
        }
        if !session.status.is_terminal() {
            return Err(StoreError::Invariant(format!(
                "cannot archive active Session {id}"
            )));
        }
        session.archived_at = Some(Utc::now());
        session.updated_at = Utc::now();
        update_session_tx(&transaction, &session)?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            None,
            SessionEventPayload::SessionChanged {
                status: session.status,
                reason: Some("archived".to_string()),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    pub(crate) fn project_changes_after(
        &self,
        project_id: ProjectId,
        after_cursor: u64,
    ) -> Result<ProjectChangeBatch, StoreError> {
        self.get_project(project_id)?;
        let connection = self.connection()?;
        let captured_cursor = connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM project_changes WHERE project_id = ?1",
            [project_id.to_string()],
            |row| row.get::<_, u64>(0),
        )?;
        if after_cursor > captured_cursor {
            return Err(StoreError::Invariant(format!(
                "Project change cursor {after_cursor} is ahead of current cursor {captured_cursor}"
            )));
        }
        let mut statement = connection.prepare(
            "SELECT sequence, entity_kind, entity_id, session_id
             FROM project_changes
             WHERE project_id = ?1 AND sequence > ?2 AND sequence <= ?3
             ORDER BY sequence ASC
             LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                project_id.to_string(),
                after_cursor,
                captured_cursor,
                MAX_PROJECT_CHANGES_PER_READ,
            ],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )?;
        let mut changes = Vec::new();
        for row in rows {
            let (sequence, kind, entity_id, session_id) = row?;
            changes.push(ProjectChange {
                sequence,
                kind,
                entity_id,
                session_id: session_id
                    .map(|id| SessionId::from_str(&id))
                    .transpose()
                    .map_err(|error| StoreError::Invariant(error.to_string()))?,
            });
        }
        let last_sequence = changes.last().map(|change| change.sequence);
        Ok(ProjectChangeBatch {
            captured_cursor,
            changes,
            last_sequence,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_turn_for_attempt(
        &self,
        attempt_id: ActionAttemptId,
        agent_id: AgentId,
        input: impl Into<String>,
        model_route: ModelRouteSnapshot,
        prompt: PromptSnapshot,
        expected_access: AccessPreset,
        tool_set: papermachine_protocol::ToolSetSnapshot,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        skill_snapshots: Vec<SkillSnapshot>,
    ) -> Result<Turn, StoreError> {
        let input = input.into();
        let agent_lock = self.shared.agent_rollout_lock(agent_id)?;
        let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_agent_rollout_locked(agent_id)?;
        let agent = self.get_agent(agent_id)?;
        let session = self.get_session(agent.session_id)?;
        if agent.access != expected_access {
            return Err(StoreError::Invariant(
                "Agent access changed while its Turn tool set was materialized".to_string(),
            ));
        }
        tool_set.validate().map_err(StoreError::Invariant)?;
        model_route.validate().map_err(StoreError::Invariant)?;
        if session.archived_at.is_some() || session.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot add a Turn to an archived or terminal Session".to_string(),
            ));
        }
        let mut attempt = self.get_action_attempt(attempt_id)?;
        let invocation = self.get_action_invocation(attempt.invocation_id)?;
        let valid_input = match invocation.source {
            ActionSource::HumanRequest { request_id } => {
                let request = self.get_human_request(request_id)?;
                request.session_id == invocation.session_id
                    && request.agent_id == agent_id
                    && request.status == HumanRequestStatus::Answered
                    && request.answer.as_ref().and_then(Value::as_str) == Some(input.as_str())
            }
            ActionSource::Workflow | ActionSource::Agent { .. } => invocation.input == input,
        };
        if !valid_input
            || attempt.status.is_terminal()
            || attempt.turn_id.is_some()
            || invocation.session_id != session.id
            || invocation.agent_id != agent_id
        {
            return Err(StoreError::Invariant(
                "ActionAttempt cannot attach this Turn".to_string(),
            ));
        }
        let active = self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE agent_id = ?1
             AND status IN ('queued', 'running', 'paused'))",
            [agent_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if active {
            return Err(StoreError::Invariant(
                "Agent already has an active Turn".to_string(),
            ));
        }
        let now = Utc::now();
        let project = self.get_project(session.project_id)?;
        ensure_workspace_attachment_available(&project.workspace)?;
        let environment = TurnEnvironmentSnapshot::materialize(
            project.workspace,
            self.managed_root.to_string_lossy().into_owned(),
            agent.access,
        )
        .map_err(StoreError::Invariant)?;
        let turn = Turn {
            id: TurnId::new(),
            agent_id,
            status: TurnStatus::Queued,
            input,
            output: None,
            model_route,
            prompt,
            environment,
            tool_set,
            web_search_context_size,
            response_format,
            skill_snapshots,
            usage: TokenUsage::default(),
            completed_model_steps: 0,
            hosted_search_calls_used: 0,
            checkpoint_message: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        attempt.turn_id = Some(turn.id);
        attempt.updated_at = now;
        self.commit_agent_rollout_item_locked(
            agent_id,
            AgentRolloutItem::TurnCreated {
                turn: turn.clone(),
                action_attempt: attempt,
            },
        )?;
        Ok(turn)
    }

    pub fn get_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.query_document_by_id("turns", id.to_string(), "turn")
    }

    pub fn list_turns(&self, agent_id: AgentId) -> Result<Vec<Turn>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM turns WHERE agent_id = ?1 ORDER BY created_at ASC, id ASC",
            [agent_id.to_string()],
        )
    }

    pub fn list_session_turns(&self, session_id: SessionId) -> Result<Vec<Turn>, StoreError> {
        self.query_documents(
            "SELECT turns.document_json FROM turns
             INNER JOIN agents ON agents.id = turns.agent_id
             WHERE agents.session_id = ?1
             ORDER BY turns.created_at ASC, turns.id ASC",
            [session_id.to_string()],
        )
    }

    pub fn start_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.transition_turn(
            id,
            &[TurnStatus::Queued, TurnStatus::Paused, TurnStatus::Running],
            TurnStatus::Running,
            None,
            Vec::new(),
        )
    }

    pub fn pause_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.transition_turn(
            id,
            &[TurnStatus::Running, TurnStatus::Paused],
            TurnStatus::Paused,
            None,
            Vec::new(),
        )
    }

    pub fn resume_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.transition_turn(
            id,
            &[TurnStatus::Paused, TurnStatus::Running],
            TurnStatus::Running,
            None,
            Vec::new(),
        )
    }

    pub fn complete_turn(
        &self,
        id: TurnId,
        output: String,
        usage: TokenUsage,
    ) -> Result<Turn, StoreError> {
        let existing = self.get_turn(id)?;
        let agent_lock = self.shared.agent_rollout_lock(existing.agent_id)?;
        let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_agent_rollout_locked(existing.agent_id)?;
        let active =
            crate::rollout::reconstruct_file(&self.shared.rollout_root, existing.agent_id)?
                .active_turn
                .ok_or_else(|| {
                    StoreError::Invariant(format!(
                        "cannot complete Turn {id} without active rollout state"
                    ))
                })?;
        if active.turn_id != id || active.checkpoint_message.as_deref() != Some(output.as_str()) {
            return Err(StoreError::Invariant(format!(
                "cannot complete Turn {id} without its matching terminal rollout checkpoint"
            )));
        }
        let mut turn = self.get_turn(id)?;
        if turn.status != TurnStatus::Running {
            return Err(StoreError::Invariant(format!(
                "cannot complete Turn {id} from {:?}",
                turn.status
            )));
        }
        turn.output = Some(output);
        turn.usage = usage;
        self.persist_turn_status_locked(turn, TurnStatus::Completed, None, Vec::new())
    }

    pub fn fail_turn(&self, id: TurnId, error: impl Into<String>) -> Result<Turn, StoreError> {
        self.transition_turn(
            id,
            &[TurnStatus::Queued, TurnStatus::Running, TurnStatus::Paused],
            TurnStatus::Failed,
            Some(error.into()),
            Vec::new(),
        )
    }

    pub fn interrupt_turn_with_inputs(
        &self,
        id: TurnId,
        reason: impl Into<String>,
        agent_input_ids: &[AgentInputId],
    ) -> Result<Turn, StoreError> {
        self.transition_turn(
            id,
            &[TurnStatus::Running, TurnStatus::Paused],
            TurnStatus::Interrupted,
            Some(reason.into()),
            agent_input_ids.to_vec(),
        )
    }

    pub fn cancel_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.transition_turn(
            id,
            &[TurnStatus::Queued, TurnStatus::Running, TurnStatus::Paused],
            TurnStatus::Cancelled,
            Some("cancelled by user".to_string()),
            Vec::new(),
        )
    }

    fn transition_turn(
        &self,
        id: TurnId,
        allowed_from: &[TurnStatus],
        status: TurnStatus,
        error: Option<String>,
        acknowledged_agent_input_ids: Vec<AgentInputId>,
    ) -> Result<Turn, StoreError> {
        let existing = self.get_turn(id)?;
        let agent_lock = self.shared.agent_rollout_lock(existing.agent_id)?;
        let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_agent_rollout_locked(existing.agent_id)?;
        let turn = self.get_turn(id)?;
        if turn.status == status {
            return Ok(turn);
        }
        if !allowed_from.contains(&turn.status) {
            return Err(StoreError::Invariant(format!(
                "cannot change Turn {id} from {:?} to {status:?}",
                turn.status
            )));
        }
        self.persist_turn_status_locked(turn, status, error, acknowledged_agent_input_ids)
    }

    pub fn checkpoint_turn_context(
        &self,
        id: TurnId,
        checkpoint: TurnContextCheckpoint,
    ) -> Result<Turn, StoreError> {
        let existing = self.get_turn(id)?;
        let agent_lock = self.shared.agent_rollout_lock(existing.agent_id)?;
        let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_agent_rollout_locked(existing.agent_id)?;
        let turn = self.get_turn(id)?;
        if turn.status.is_terminal() {
            return Err(StoreError::Invariant(format!(
                "cannot checkpoint terminal Turn {id}"
            )));
        }
        self.commit_agent_rollout_item_locked(
            turn.agent_id,
            AgentRolloutItem::ContextCheckpoint {
                turn_id: id,
                mutation: checkpoint.mutation,
                usage: checkpoint.usage,
                completed_model_steps: checkpoint.completed_model_steps,
                hosted_search_calls_used: checkpoint.hosted_search_calls_used,
                checkpoint_message: checkpoint.checkpoint_message,
                acknowledged_agent_input_ids: checkpoint.acknowledged_agent_input_ids,
            },
        )?;
        self.get_turn(id)
    }

    fn persist_turn_status_locked(
        &self,
        mut turn: Turn,
        status: TurnStatus,
        error: Option<String>,
        acknowledged_agent_input_ids: Vec<AgentInputId>,
    ) -> Result<Turn, StoreError> {
        if turn.status.is_terminal() {
            return Err(StoreError::Invariant(format!(
                "cannot change terminal Turn {} from {:?} to {:?}",
                turn.id, turn.status, status
            )));
        }
        turn.status = status;
        turn.error = error;
        turn.updated_at = Utc::now();
        self.commit_agent_rollout_item_locked(
            turn.agent_id,
            AgentRolloutItem::TurnUpdated {
                turn: turn.clone(),
                acknowledged_agent_input_ids,
            },
        )?;
        Ok(turn)
    }

    pub fn create_step(
        &self,
        turn_id: TurnId,
        kind: StepKind,
        name: impl Into<String>,
        input: Value,
    ) -> Result<AgentStep, StoreError> {
        self.create_step_inner(
            turn_id,
            kind,
            name,
            None,
            StepStatus::Running,
            input,
            None,
            TokenUsage::default(),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_terminal_step(
        &self,
        turn_id: TurnId,
        kind: StepKind,
        name: impl Into<String>,
        input: Value,
        status: StepStatus,
        output: Option<Value>,
        usage: TokenUsage,
        duration_ms: Option<u64>,
    ) -> Result<AgentStep, StoreError> {
        if status == StepStatus::Running {
            return Err(StoreError::Invariant(
                "terminal Step creation requires a terminal status".to_string(),
            ));
        }
        self.create_step_inner(
            turn_id,
            kind,
            name,
            None,
            status,
            input,
            output,
            usage,
            duration_ms,
        )
    }

    pub fn create_tool_step(
        &self,
        turn_id: TurnId,
        call_id: impl Into<String>,
        name: impl Into<String>,
        input: Value,
    ) -> Result<AgentStep, StoreError> {
        self.create_step_inner(
            turn_id,
            StepKind::Tool,
            name,
            Some(call_id.into()),
            StepStatus::Running,
            input,
            None,
            TokenUsage::default(),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_step_inner(
        &self,
        turn_id: TurnId,
        kind: StepKind,
        name: impl Into<String>,
        tool_call_id: Option<String>,
        status: StepStatus,
        input: Value,
        output: Option<Value>,
        usage: TokenUsage,
        duration_ms: Option<u64>,
    ) -> Result<AgentStep, StoreError> {
        self.get_turn(turn_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let name = name.into();
        if let Some(call_id) = tool_call_id.as_ref() {
            let existing = transaction
                .query_row(
                    "SELECT document_json FROM steps WHERE turn_id = ?1 AND tool_call_id = ?2",
                    params![turn_id.to_string(), call_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(document) = existing {
                let existing: AgentStep = serde_json::from_str(&document)?;
                if existing.kind == kind && existing.name == name && existing.input == input {
                    return Ok(existing);
                }
                return Err(StoreError::Invariant(format!(
                    "tool call {call_id} already belongs to a different Step"
                )));
            }
        }
        let now = Utc::now();
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
            name,
            tool_call_id,
            status,
            input,
            output,
            usage,
            duration_ms,
            created_at: now,
            updated_at: now,
        };
        transaction.execute(
            "INSERT INTO steps
             (id, turn_id, sequence, tool_call_id, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                step.id.to_string(),
                step.turn_id.to_string(),
                step.sequence,
                step.tool_call_id,
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
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut step: AgentStep = load_document_tx(&transaction, "steps", &id.to_string(), "step")?;
        if step.status != StepStatus::Running {
            return Err(StoreError::Invariant(format!(
                "cannot finish terminal Step {id} from {:?}",
                step.status
            )));
        }
        if status == StepStatus::Running {
            return Err(StoreError::Invariant(format!(
                "cannot finish Step {id} with running status"
            )));
        }
        step.status = status;
        step.output = output;
        step.usage = usage;
        step.duration_ms = duration_ms;
        step.updated_at = Utc::now();
        update_status_document_tx(
            &transaction,
            "steps",
            &step.id.to_string(),
            step.status,
            &step,
        )?;
        transaction.commit()?;
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

    pub fn list_agent_steps(&self, agent_id: AgentId) -> Result<Vec<AgentStep>, StoreError> {
        self.query_documents(
            "SELECT steps.document_json FROM steps
             INNER JOIN turns ON turns.id = steps.turn_id
             WHERE turns.agent_id = ?1
             ORDER BY turns.created_at ASC, turns.id ASC, steps.sequence ASC",
            [agent_id.to_string()],
        )
    }

    pub fn list_session_steps(&self, session_id: SessionId) -> Result<Vec<AgentStep>, StoreError> {
        self.query_documents(
            "SELECT steps.document_json FROM steps
             INNER JOIN turns ON turns.id = steps.turn_id
             INNER JOIN agents ON agents.id = turns.agent_id
             WHERE agents.session_id = ?1
             ORDER BY turns.created_at ASC, turns.id ASC, steps.sequence ASC",
            [session_id.to_string()],
        )
    }

    pub fn append_session_event(
        &self,
        session_id: SessionId,
        agent_id: Option<AgentId>,
        turn_id: Option<TurnId>,
        step_id: Option<StepId>,
        payload: SessionEventPayload,
    ) -> Result<SessionEvent, StoreError> {
        self.get_session(session_id)?;
        if is_transient_session_event(&payload) {
            return Err(StoreError::Invariant(
                "transient Session events must be published without persistence".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let event = append_session_event_tx(
            &transaction,
            session_id,
            agent_id,
            turn_id,
            step_id,
            payload,
        )?;
        transaction.commit()?;
        self.shared.publish_session(event.clone());
        Ok(event)
    }

    pub fn publish_transient_session_event(
        &self,
        session_id: SessionId,
        agent_id: Option<AgentId>,
        turn_id: Option<TurnId>,
        step_id: Option<StepId>,
        payload: SessionEventPayload,
    ) -> Result<SessionEvent, StoreError> {
        self.get_session(session_id)?;
        if !is_transient_session_event(&payload) {
            return Err(StoreError::Invariant(
                "durable Session events must be appended to the event log".to_string(),
            ));
        }
        let event = pending_session_event(
            self.get_session(session_id)?.project_id,
            session_id,
            agent_id,
            turn_id,
            step_id,
            payload,
        );
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

    pub fn create_session(&self, request: NewSession) -> Result<Session, StoreError> {
        self.ensure_project(request.project_id)?;
        match (request.trigger.kind, request.trigger.source_session_id) {
            (SessionTriggerKind::User, None) => {
                return Err(StoreError::Invariant(
                    "user-triggered Session requires a source Session".to_string(),
                ));
            }
            (SessionTriggerKind::Manual, Some(_)) => {
                return Err(StoreError::Invariant(
                    "manual Session cannot name a source Session".to_string(),
                ));
            }
            (SessionTriggerKind::User, Some(_)) | (SessionTriggerKind::Manual, None) => {}
        }
        if request
            .program
            .project_id
            .is_some_and(|owner| owner != request.project_id)
        {
            return Err(StoreError::Invariant(
                "WorkflowProgram belongs to a different Project".to_string(),
            ));
        }
        let source_session = request
            .trigger
            .source_session_id
            .map(|session_id| self.get_session(session_id))
            .transpose()?;
        if source_session
            .as_ref()
            .is_some_and(|session| session.project_id != request.project_id)
        {
            return Err(StoreError::Invariant(
                "starting Session belongs to a different Project".to_string(),
            ));
        }
        if source_session
            .as_ref()
            .is_some_and(|session| session.archived_at.is_some())
        {
            return Err(StoreError::Invariant(
                "cannot start a Session from an archived Session".to_string(),
            ));
        }
        if let Some(session) = source_session.as_ref()
            && request.access > session.access
        {
            return Err(StoreError::Invariant(format!(
                "Session access {} exceeds starting Session access {}",
                request.access, session.access
            )));
        }
        if let Some((class_name, access)) = request
            .agent_access_overrides
            .iter()
            .find(|(_, access)| **access > request.access)
        {
            return Err(StoreError::Invariant(format!(
                "Agent override {class_name}={access} exceeds Session access {}",
                request.access
            )));
        }
        let default_model = request.default_model;
        if default_model.trim().is_empty() {
            return Err(StoreError::Invariant(
                "Session requires a default model".to_string(),
            ));
        }
        let enabled_skills = if request.enabled_skills.is_empty() {
            source_session
                .as_ref()
                .map(|session| session.enabled_skills.clone())
                .unwrap_or_default()
        } else {
            request.enabled_skills
        };
        validate_workflow_instructions(&request.instructions)?;
        let now = Utc::now();
        let session = Session {
            id: SessionId::new(),
            project_id: request.project_id,
            program: request.program,
            title: request.title,
            request: request.request,
            instructions: request.instructions,
            trigger: request.trigger,
            default_model,
            access: request.access,
            enabled_skills,
            agent_access_overrides: request.agent_access_overrides,
            status: SessionStatus::Created,
            closing_status: None,
            params: request.params,
            output: None,
            error: None,
            attention_required: false,
            usage: SessionUsage::default(),
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO sessions
             (id, project_id, program_slug, status, attention_required, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![
                session.id.to_string(),
                session.project_id.to_string(),
                &session.program.manifest.slug,
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
            None,
            SessionEventPayload::SessionCreated {
                request: session.request.clone(),
                program_slug: session.program.manifest.slug.clone(),
                source_sha256: session.program.sha256.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    pub fn latest_project_session_for_program(
        &self,
        project_id: ProjectId,
        program_slug: &str,
    ) -> Result<Option<Session>, StoreError> {
        self.connection()?
            .query_row(
                "SELECT document_json FROM sessions
                 WHERE project_id = ?1 AND program_slug = ?2
                 ORDER BY updated_at DESC, id ASC LIMIT 1",
                params![project_id.to_string(), program_slug],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|document| serde_json::from_str(&document).map_err(StoreError::from))
            .transpose()
    }

    pub fn begin_session_effect(
        &self,
        session_id: SessionId,
        key: impl Into<String>,
        kind: impl Into<String>,
        payload: Value,
    ) -> Result<SessionEffect, StoreError> {
        let key = key.into();
        let kind = kind.into();
        let request_sha256 = session_effect_hash(&kind, &payload)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT document_json FROM session_effects
                 WHERE session_id = ?1 AND effect_key = ?2",
                params![session_id.to_string(), &key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(document) = existing {
            let effect: SessionEffect = serde_json::from_str(&document)?;
            if effect.kind != kind
                || effect.request_sha256 != request_sha256
                || effect.payload != payload
            {
                return Err(StoreError::Invariant(format!(
                    "Session effect {key} was replayed with a different request"
                )));
            }
            return Ok(effect);
        }
        ensure_session_accepts_effect_tx(&transaction, session_id)?;
        let effect = SessionEffect {
            session_id,
            key,
            kind,
            request_sha256,
            payload,
            status: SessionEffectStatus::Started,
            result: None,
            error: None,
            started_at: Utc::now(),
            completed_at: None,
        };
        transaction.execute(
            "INSERT INTO session_effects
             (session_id, effect_key, status, started_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id.to_string(),
                &effect.key,
                enum_string(effect.status)?,
                effect.started_at.to_rfc3339(),
                serde_json::to_string(&effect)?,
            ],
        )?;
        transaction.commit()?;
        Ok(effect)
    }

    pub fn finish_session_effect(
        &self,
        session_id: SessionId,
        key: &str,
        outcome: Result<Value, String>,
    ) -> Result<SessionEffect, StoreError> {
        let mut effect = self.get_session_effect(session_id, key)?;
        let (status, result, error) = match outcome {
            Ok(result) => (SessionEffectStatus::Completed, Some(result), None),
            Err(error) => (SessionEffectStatus::Failed, None, Some(error)),
        };
        if effect.status != SessionEffectStatus::Started {
            if effect.status == status && effect.result == result && effect.error == error {
                return Ok(effect);
            }
            return Err(StoreError::Invariant(format!(
                "Session effect {key} already has a different terminal outcome"
            )));
        }
        effect.status = status;
        effect.result = result;
        effect.error = error;
        effect.completed_at = Some(Utc::now());
        let changed = self.connection()?.execute(
            "UPDATE session_effects SET status = ?1, document_json = ?2
             WHERE session_id = ?3 AND effect_key = ?4 AND status = 'started'",
            params![
                enum_string(effect.status)?,
                serde_json::to_string(&effect)?,
                session_id.to_string(),
                key,
            ],
        )?;
        ensure_one(changed, "session_effects")?;
        Ok(effect)
    }

    pub fn get_session_effect(
        &self,
        session_id: SessionId,
        key: &str,
    ) -> Result<SessionEffect, StoreError> {
        let document = self
            .connection()?
            .query_row(
                "SELECT document_json FROM session_effects
                 WHERE session_id = ?1 AND effect_key = ?2",
                params![session_id.to_string(), key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "session effect",
                id: format!("{session_id}:{key}"),
            })?;
        Ok(serde_json::from_str(&document)?)
    }

    pub fn list_session_effects(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEffect>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM session_effects WHERE session_id = ?1
             ORDER BY started_at ASC, effect_key ASC",
            [session_id.to_string()],
        )
    }

    pub fn list_recoverable_sessions(&self) -> Result<Vec<Session>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM sessions
             WHERE status NOT IN ('completed', 'failed', 'cancelled')
             ORDER BY updated_at ASC, id ASC",
            [],
        )
    }

    pub fn start_session(&self, id: SessionId) -> Result<Session, StoreError> {
        self.transition_session(
            id,
            &[SessionStatus::Created, SessionStatus::Running],
            SessionStatus::Running,
            None,
        )
    }

    pub fn pause_session(
        &self,
        id: SessionId,
        reason: Option<String>,
    ) -> Result<Session, StoreError> {
        self.transition_session(
            id,
            &[
                SessionStatus::Running,
                SessionStatus::WaitingForInput,
                SessionStatus::WaitingForDeadline,
                SessionStatus::Paused,
            ],
            SessionStatus::Paused,
            reason,
        )
    }

    pub fn resume_session(&self, id: SessionId) -> Result<Session, StoreError> {
        self.transition_session(
            id,
            &[
                SessionStatus::Paused,
                SessionStatus::WaitingForInput,
                SessionStatus::WaitingForDeadline,
                SessionStatus::Running,
            ],
            SessionStatus::Running,
            None,
        )
    }

    pub fn wait_session_for_input(&self, id: SessionId) -> Result<Session, StoreError> {
        self.transition_session(
            id,
            &[SessionStatus::Running, SessionStatus::WaitingForInput],
            SessionStatus::WaitingForInput,
            None,
        )
    }

    pub fn wait_session_for_deadline(&self, id: SessionId) -> Result<Session, StoreError> {
        self.transition_session(
            id,
            &[SessionStatus::Running, SessionStatus::WaitingForDeadline],
            SessionStatus::WaitingForDeadline,
            None,
        )
    }

    pub fn fail_session(
        &self,
        id: SessionId,
        error: impl Into<String>,
    ) -> Result<Session, StoreError> {
        self.transition_session(
            id,
            &[
                SessionStatus::Created,
                SessionStatus::Running,
                SessionStatus::WaitingForInput,
                SessionStatus::WaitingForDeadline,
                SessionStatus::Paused,
            ],
            SessionStatus::Failed,
            Some(error.into()),
        )
    }

    pub fn cancel_session(
        &self,
        id: SessionId,
        reason: impl Into<String>,
    ) -> Result<Session, StoreError> {
        self.transition_session(
            id,
            &[
                SessionStatus::Created,
                SessionStatus::Running,
                SessionStatus::WaitingForInput,
                SessionStatus::WaitingForDeadline,
                SessionStatus::Paused,
            ],
            SessionStatus::Cancelled,
            Some(reason.into()),
        )
    }

    pub fn begin_session_closing(
        &self,
        id: SessionId,
        final_status: SessionStatus,
        output: Option<Value>,
        error: Option<String>,
    ) -> Result<Session, StoreError> {
        if !final_status.is_terminal() {
            return Err(StoreError::Invariant(
                "Session Closing requires a terminal final status".to_string(),
            ));
        }
        if (final_status == SessionStatus::Completed) != output.is_some() {
            return Err(StoreError::Invariant(
                "only a completed Session Closing transition carries output".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session: Session =
            load_document_tx(&transaction, "sessions", &id.to_string(), "session")?;
        if session.status == SessionStatus::Closing {
            if session.closing_status == Some(final_status)
                && session.output == output
                && session.error == error
            {
                return Ok(session);
            }
            return Err(StoreError::Invariant(format!(
                "Session {id} is already Closing with a different outcome"
            )));
        }
        if session.status.is_terminal() {
            if session.status == final_status && session.output == output && session.error == error
            {
                return Ok(session);
            }
            return Err(StoreError::Invariant(format!(
                "terminal Session {id} cannot enter Closing"
            )));
        }
        session.status = SessionStatus::Closing;
        session.closing_status = Some(final_status);
        session.output = output;
        session.error = error.clone();
        session.attention_required = false;
        session.updated_at = Utc::now();
        update_session_tx(&transaction, &session)?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            None,
            SessionEventPayload::SessionChanged {
                status: SessionStatus::Closing,
                reason: error,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    pub fn finish_session_closing(&self, id: SessionId) -> Result<Session, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session: Session =
            load_document_tx(&transaction, "sessions", &id.to_string(), "session")?;
        if session.status.is_terminal() {
            return Ok(session);
        }
        if session.status != SessionStatus::Closing {
            return Err(StoreError::Invariant(format!(
                "cannot finish Session {id} from {:?}",
                session.status
            )));
        }
        let final_status = session.closing_status.ok_or_else(|| {
            StoreError::Invariant(format!("Closing Session {id} has no final status"))
        })?;
        if !final_status.is_terminal() {
            return Err(StoreError::Invariant(format!(
                "Closing Session {id} has invalid final status {final_status:?}"
            )));
        }
        session.status = final_status;
        session.closing_status = None;
        session.updated_at = Utc::now();
        terminalize_session_resources_tx(&transaction, session.id, session.updated_at)?;
        update_session_tx(&transaction, &session)?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            None,
            SessionEventPayload::SessionChanged {
                status: final_status,
                reason: session.error.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    fn transition_session(
        &self,
        id: SessionId,
        allowed_from: &[SessionStatus],
        status: SessionStatus,
        reason: Option<String>,
    ) -> Result<Session, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session: Session =
            load_document_tx(&transaction, "sessions", &id.to_string(), "session")?;
        if session.status == status {
            return Ok(session);
        }
        if !allowed_from.contains(&session.status) {
            return Err(StoreError::Invariant(format!(
                "cannot change Session {id} from {:?} to {status:?}",
                session.status
            )));
        }
        session.status = status;
        session.closing_status = None;
        session.error = if status == SessionStatus::Failed {
            reason.clone()
        } else {
            None
        };
        if status.is_terminal() {
            session.attention_required = false;
        }
        session.updated_at = Utc::now();
        if status.is_terminal() {
            terminalize_session_resources_tx(&transaction, session.id, session.updated_at)?;
        }
        update_session_tx(&transaction, &session)?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            None,
            SessionEventPayload::SessionChanged { status, reason },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    pub fn complete_session(&self, id: SessionId, output: Value) -> Result<Session, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session: Session =
            load_document_tx(&transaction, "sessions", &id.to_string(), "session")?;
        if session.status != SessionStatus::Running {
            return Err(StoreError::Invariant(format!(
                "cannot complete Session {id} from {:?}",
                session.status
            )));
        }
        session.status = SessionStatus::Completed;
        session.closing_status = None;
        session.output = Some(output);
        session.error = None;
        session.attention_required = false;
        session.updated_at = Utc::now();
        terminalize_session_resources_tx(&transaction, session.id, session.updated_at)?;
        update_session_tx(&transaction, &session)?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            None,
            SessionEventPayload::SessionChanged {
                status: SessionStatus::Completed,
                reason: None,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    pub fn add_session_usage(
        &self,
        id: SessionId,
        delta: SessionUsage,
    ) -> Result<Session, StoreError> {
        // Read and update under one database lock. Session actions can finish in
        // parallel, so a separate get followed by update loses concurrent deltas.
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let document = transaction.query_row(
            "SELECT document_json FROM sessions WHERE id = ?1",
            [id.to_string()],
            |row| row.get::<_, String>(0),
        )?;
        let mut session: Session = serde_json::from_str(&document)?;
        apply_session_usage_delta(&mut session.usage, delta);
        session.updated_at = Utc::now();
        update_session_tx(&transaction, &session)?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            None,
            SessionEventPayload::UsageUpdated {
                usage: session.usage.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(session)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_agent(
        &self,
        session_id: SessionId,
        class_name: impl Into<String>,
        name: impl Into<String>,
        role: impl Into<String>,
        system_prompt: impl Into<String>,
        model: impl Into<String>,
        skills: Vec<String>,
        access: AccessPreset,
    ) -> Result<Agent, StoreError> {
        self.create_agent_with_id(
            session_id,
            AgentId::new(),
            class_name,
            name,
            role,
            system_prompt,
            model,
            skills,
            access,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_agent_with_id(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        class_name: impl Into<String>,
        name: impl Into<String>,
        role: impl Into<String>,
        system_prompt: impl Into<String>,
        model: impl Into<String>,
        skills: Vec<String>,
        access: AccessPreset,
    ) -> Result<Agent, StoreError> {
        let system_prompt = system_prompt.into();
        validate_system_prompt(&system_prompt)?;
        let now = Utc::now();
        let name = name.into();
        let role = role.into();
        let requested_model = model.into();
        let class_name = class_name.into();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session: Session =
            load_document_tx(&transaction, "sessions", &session_id.to_string(), "session")?;
        if session.status != SessionStatus::Running {
            return Err(StoreError::Invariant(
                "cannot add an Agent unless its Session is running".to_string(),
            ));
        }
        let model = {
            if requested_model.trim().is_empty() {
                session.default_model.clone()
            } else {
                requested_model
            }
        };
        let agent = Agent {
            id: agent_id,
            session_id: session.id,
            parent_agent_id: None,
            class_name,
            name,
            role,
            system_prompt,
            model,
            access: std::cmp::min(access, session.access),
            skills: if skills.is_empty() {
                session.enabled_skills.clone()
            } else {
                skills
            },
            created_at: now,
        };
        session.usage.agents_created = session.usage.agents_created.saturating_add(1);
        session.updated_at = now;
        transaction.execute(
            "INSERT INTO agents
             (id, session_id, parent_agent_id, document_json)
             VALUES (?1, ?2, NULL, ?3)",
            params![
                agent.id.to_string(),
                session.id.to_string(),
                serde_json::to_string(&agent)?,
            ],
        )?;
        update_session_tx(&transaction, &session)?;
        let event = append_session_event_tx(
            &transaction,
            session.id,
            Some(agent.id),
            None,
            None,
            SessionEventPayload::AgentCreated {
                name: agent.name.clone(),
                role: agent.role.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        Ok(agent)
    }

    pub fn get_agent(&self, id: AgentId) -> Result<Agent, StoreError> {
        self.query_document_by_id("agents", id.to_string(), "agent")
    }

    pub fn list_agents(&self, session_id: SessionId) -> Result<Vec<Agent>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM agents WHERE session_id = ?1
             ORDER BY created_at ASC, id ASC",
            [session_id.to_string()],
        )
    }

    pub fn interrupt_descendant_agent(
        &self,
        caller_agent_id: AgentId,
        target_agent_id: AgentId,
        input_id: AgentInputId,
        reason: String,
    ) -> Result<AgentInterrupt, StoreError> {
        if caller_agent_id == target_agent_id {
            return Err(StoreError::Invariant(
                "an Agent cannot interrupt itself with the collaboration tool".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let caller: Agent = load_document_tx(
            &transaction,
            "agents",
            &caller_agent_id.to_string(),
            "caller Agent",
        )?;
        let target: Agent = load_document_tx(
            &transaction,
            "agents",
            &target_agent_id.to_string(),
            "target Agent",
        )?;
        if caller.session_id != target.session_id {
            return Err(StoreError::Invariant(
                "only descendants in the caller Session may be interrupted".to_string(),
            ));
        }
        let mut ancestor = target.parent_agent_id;
        let mut is_descendant = false;
        while let Some(agent_id) = ancestor {
            if agent_id == caller.id {
                is_descendant = true;
                break;
            }
            let agent: Agent = load_document_tx(
                &transaction,
                "agents",
                &agent_id.to_string(),
                "ancestor Agent",
            )?;
            if agent.session_id != caller.session_id {
                return Err(StoreError::Invariant(
                    "Agent ancestry crossed a Session boundary".to_string(),
                ));
            }
            ancestor = agent.parent_agent_id;
        }
        if !is_descendant {
            return Err(StoreError::Invariant(
                "interrupt_agent may only target a caller descendant".to_string(),
            ));
        }
        let session: Session = load_document_tx(
            &transaction,
            "sessions",
            &target.session_id.to_string(),
            "Session",
        )?;
        if session.status.is_terminal() || session.archived_at.is_some() {
            return Err(StoreError::Invariant(
                "cannot interrupt an Agent in a terminal or archived Session".to_string(),
            ));
        }
        let documents = {
            let mut statement = transaction.prepare(
                "SELECT document_json FROM action_invocations
                 WHERE agent_id = ?1 AND status IN ('scheduled', 'running', 'interrupted')
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows =
                statement.query_map([target.id.to_string()], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut actions = documents
            .into_iter()
            .map(|document| serde_json::from_str::<ActionInvocation>(&document))
            .collect::<Result<Vec<_>, _>>()?;
        if actions
            .iter()
            .filter(|action| action.status == ActionStatus::Running)
            .count()
            > 1
        {
            return Err(StoreError::Invariant(
                "an Agent has more than one running Action".to_string(),
            ));
        }
        let now = Utc::now();
        let mut cancelled_action_ids = Vec::new();
        let mut running_action_ids = Vec::new();
        let mut events = Vec::new();
        for action in &mut actions {
            match action.status {
                ActionStatus::Scheduled | ActionStatus::Interrupted => {
                    action.status = ActionStatus::Cancelled;
                    action.output = None;
                    action.error = Some(reason.clone());
                    action.updated_at = now;
                    update_status_document_tx(
                        &transaction,
                        "action_invocations",
                        &action.id.to_string(),
                        action.status,
                        action,
                    )?;
                    events.push(append_session_event_tx(
                        &transaction,
                        action.session_id,
                        Some(action.agent_id),
                        None,
                        None,
                        action_event_payload(action, None),
                    )?);
                    events.extend(apply_terminal_action_inputs_tx(
                        &transaction,
                        action.id,
                        now,
                    )?);
                    cancelled_action_ids.push(action.id);
                }
                ActionStatus::Running => {
                    let (_, event) = insert_agent_input_tx(
                        &transaction,
                        input_id,
                        action.session_id,
                        action.agent_id,
                        Some(action.id),
                        AgentInputSource::Agent {
                            sender_agent_id: caller.id,
                        },
                        AgentInputKind::Interrupt,
                        reason.clone(),
                    )?;
                    if let Some(event) = event {
                        events.push(event);
                    }
                    running_action_ids.push(action.id);
                }
                ActionStatus::Completed | ActionStatus::Failed | ActionStatus::Cancelled => {}
            }
        }
        transaction.commit()?;
        drop(connection);
        for event in events {
            self.shared.publish_session(event);
        }
        Ok(AgentInterrupt {
            cancelled_action_ids,
            running_action_ids,
        })
    }

    pub fn set_agent_access(
        &self,
        agent_id: AgentId,
        access: AccessPreset,
    ) -> Result<Agent, StoreError> {
        let agent_lock = self.shared.agent_rollout_lock(agent_id)?;
        let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_agent_rollout_locked(agent_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut agent: Agent =
            load_document_tx(&transaction, "agents", &agent_id.to_string(), "agent")?;
        let session: Session = load_document_tx(
            &transaction,
            "sessions",
            &agent.session_id.to_string(),
            "session",
        )?;
        if session.status.is_terminal() || session.archived_at.is_some() {
            return Err(StoreError::Invariant(
                "cannot change an Agent in a terminal or archived Session".to_string(),
            ));
        }
        if access > session.access {
            return Err(StoreError::Invariant(format!(
                "Agent access {access} exceeds Session access {}",
                session.access
            )));
        }
        let active = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE agent_id = ?1
             AND status IN ('queued', 'running', 'paused'))",
            [agent_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if active {
            return Err(StoreError::Invariant(
                "cannot change access while the Agent has an active Turn".to_string(),
            ));
        }
        agent.access = access;
        let changed = transaction.execute(
            "UPDATE agents SET document_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(&agent)?, agent.id.to_string()],
        )?;
        ensure_one(changed, "agents")?;
        transaction.commit()?;
        Ok(agent)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_child_agent_task(
        &self,
        parent_agent_id: AgentId,
        child_agent_id: AgentId,
        invocation_id: ActionInvocationId,
        name: Option<String>,
        task: String,
        access: Option<AccessPreset>,
        max_children: usize,
    ) -> Result<(Agent, ActionInvocation), StoreError> {
        if task.trim().is_empty() {
            return Err(StoreError::Invariant(
                "child Agent task must not be empty".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let parent: Agent = load_document_tx(
            &transaction,
            "agents",
            &parent_agent_id.to_string(),
            "parent Agent",
        )?;
        let requested_name = name.filter(|name| !name.trim().is_empty());
        let requested_access = access.unwrap_or(parent.access);

        match load_document_tx::<Agent>(
            &transaction,
            "agents",
            &child_agent_id.to_string(),
            "child Agent",
        ) {
            Ok(existing) => {
                let action: ActionInvocation = load_document_tx(
                    &transaction,
                    "action_invocations",
                    &invocation_id.to_string(),
                    "child Action",
                )?;
                if existing.session_id == parent.session_id
                    && existing.parent_agent_id == Some(parent.id)
                    && requested_name
                        .as_ref()
                        .is_none_or(|name| existing.name == *name)
                    && existing.access == requested_access
                    && action.session_id == existing.session_id
                    && action.agent_id == existing.id
                    && action.action_name == "agent_task"
                    && action.input == task
                    && action.source
                        == (ActionSource::Agent {
                            sender_agent_id: parent.id,
                        })
                {
                    transaction.commit()?;
                    return Ok((existing, action));
                }
                return Err(StoreError::Invariant(format!(
                    "child Agent id {child_agent_id} was reused with different provenance"
                )));
            }
            Err(StoreError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }

        let mut session: Session = load_document_tx(
            &transaction,
            "sessions",
            &parent.session_id.to_string(),
            "Session",
        )?;
        if !session.status.accepts_actions() || session.archived_at.is_some() {
            return Err(StoreError::Invariant(
                "cannot spawn an Agent in a terminal, archived, or non-running Session".to_string(),
            ));
        }
        if parent.parent_agent_id.is_some() {
            return Err(StoreError::Invariant(
                "child Agents cannot spawn descendants".to_string(),
            ));
        }
        let child_count = transaction.query_row(
            "SELECT COUNT(*) FROM agents WHERE parent_agent_id = ?1",
            [parent.id.to_string()],
            |row| row.get::<_, usize>(0),
        )?;
        if child_count >= max_children.max(1) {
            return Err(StoreError::Invariant(format!(
                "Agent child limit {} reached",
                max_children.max(1)
            )));
        }
        let child_access = requested_access;
        if child_access > parent.access {
            return Err(StoreError::Invariant(format!(
                "child access {child_access} exceeds parent access {}",
                parent.access
            )));
        }
        let child = Agent {
            id: child_agent_id,
            session_id: parent.session_id,
            parent_agent_id: Some(parent.id),
            class_name: parent.class_name.clone(),
            name: requested_name
                .unwrap_or_else(|| format!("{} child {}", parent.name, child_count + 1)),
            role: parent.role.clone(),
            system_prompt: parent.system_prompt.clone(),
            model: parent.model.clone(),
            access: child_access,
            skills: parent.skills.clone(),
            created_at: Utc::now(),
        };
        session.usage.agents_created = session.usage.agents_created.saturating_add(1);
        session.updated_at = child.created_at;
        transaction.execute(
            "INSERT INTO agents (id, session_id, parent_agent_id, document_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                child.id.to_string(),
                child.session_id.to_string(),
                parent.id.to_string(),
                serde_json::to_string(&child)?,
            ],
        )?;
        update_session_tx(&transaction, &session)?;
        let agent_event = append_session_event_tx(
            &transaction,
            session.id,
            Some(child.id),
            None,
            None,
            SessionEventPayload::AgentCreated {
                name: child.name.clone(),
                role: child.role.clone(),
            },
        )?;
        let (action, action_event) = insert_action_invocation_tx(
            &transaction,
            invocation_id,
            NewActionInvocation {
                session_id: session.id,
                agent_id: child.id,
                action_name: "agent_task".to_string(),
                contract: "Complete the delegated task and return a concise, self-contained result to the sending Agent.".to_string(),
                arguments: json!({
                    "task": task,
                    "sender_agent_id": parent.id,
                }),
                input: task,
                source: ActionSource::Agent {
                    sender_agent_id: parent.id,
                },
                tool_policy: None,
                web_search_context_size: None,
                reasoning_effort: None,
                response_format: None,
            },
        )?;
        let usage_event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            None,
            SessionEventPayload::UsageUpdated {
                usage: session.usage.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(agent_event);
        if let Some(event) = action_event {
            self.shared.publish_session(event);
        }
        self.shared.publish_session(usage_event);
        Ok((child, action))
    }

    pub fn create_action_invocation(
        &self,
        action: NewActionInvocation,
    ) -> Result<ActionInvocation, StoreError> {
        self.create_action_invocation_with_id(ActionInvocationId::new(), action)
    }

    pub fn create_action_invocation_with_id(
        &self,
        invocation_id: ActionInvocationId,
        action: NewActionInvocation,
    ) -> Result<ActionInvocation, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (invocation, event) = insert_action_invocation_tx(&transaction, invocation_id, action)?;
        transaction.commit()?;
        drop(connection);
        if let Some(event) = event {
            self.shared.publish_session(event);
        }
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
        session_id: SessionId,
    ) -> Result<Vec<ActionInvocation>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM action_invocations WHERE session_id = ?1
             ORDER BY created_at ASC, id ASC",
            [session_id.to_string()],
        )
    }

    pub fn cancel_pending_action(
        &self,
        invocation_id: ActionInvocationId,
        reason: impl Into<String>,
    ) -> Result<ActionInvocation, StoreError> {
        let reason = reason.into();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut invocation: ActionInvocation = load_document_tx(
            &transaction,
            "action_invocations",
            &invocation_id.to_string(),
            "action invocation",
        )?;
        if invocation.status == ActionStatus::Cancelled {
            return Ok(invocation);
        }
        if !matches!(
            invocation.status,
            ActionStatus::Scheduled | ActionStatus::Interrupted
        ) {
            return Err(StoreError::Invariant(format!(
                "cannot cancel pending Action {} from {:?}",
                invocation.id, invocation.status
            )));
        }
        invocation.status = ActionStatus::Cancelled;
        invocation.output = None;
        invocation.error = Some(reason);
        invocation.updated_at = Utc::now();
        update_status_document_tx(
            &transaction,
            "action_invocations",
            &invocation.id.to_string(),
            invocation.status,
            &invocation,
        )?;
        let event = append_session_event_tx(
            &transaction,
            invocation.session_id,
            Some(invocation.agent_id),
            None,
            None,
            action_event_payload(&invocation, None),
        )?;
        let input_events =
            apply_terminal_action_inputs_tx(&transaction, invocation.id, invocation.updated_at)?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(event);
        for event in input_events {
            self.shared.publish_session(event);
        }
        Ok(invocation)
    }

    pub fn start_action_attempt(
        &self,
        invocation_id: ActionInvocationId,
    ) -> Result<ActionAttempt, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut invocation: ActionInvocation = load_document_tx(
            &transaction,
            "action_invocations",
            &invocation_id.to_string(),
            "action invocation",
        )?;
        if !matches!(
            invocation.status,
            ActionStatus::Scheduled | ActionStatus::Interrupted
        ) {
            return Err(StoreError::Invariant(format!(
                "cannot start Action {} from {:?}",
                invocation.id, invocation.status
            )));
        }
        let mut session: Session = load_document_tx(
            &transaction,
            "sessions",
            &invocation.session_id.to_string(),
            "session",
        )?;
        if !session.status.accepts_actions() {
            return Err(StoreError::Invariant(
                "Session is not accepting Actions".to_string(),
            ));
        }
        let has_active = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM action_attempts
             WHERE invocation_id = ?1 AND status = 'running')",
            [invocation_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if has_active {
            return Err(StoreError::Invariant(
                "Action invocation already has a running attempt".to_string(),
            ));
        }
        let number = transaction.query_row(
            "SELECT COALESCE(MAX(number), 0) + 1 FROM action_attempts WHERE invocation_id = ?1",
            [invocation_id.to_string()],
            |row| row.get::<_, u32>(0),
        )?;
        let now = Utc::now();
        let attempt = ActionAttempt {
            id: ActionAttemptId::new(),
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
        session.usage.actions_started = session.usage.actions_started.saturating_add(1);
        session.updated_at = now;
        update_status_document_tx(
            &transaction,
            "action_invocations",
            &invocation.id.to_string(),
            invocation.status,
            &invocation,
        )?;
        transaction.execute(
            "INSERT INTO action_attempts
             (id, invocation_id, number, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                attempt.id.to_string(),
                attempt.invocation_id.to_string(),
                attempt.number,
                enum_string(attempt.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&attempt)?,
            ],
        )?;
        update_session_tx(&transaction, &session)?;
        let action_event = append_session_event_tx(
            &transaction,
            session.id,
            Some(invocation.agent_id),
            None,
            None,
            action_event_payload(&invocation, Some(attempt.id)),
        )?;
        let usage_event = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            None,
            SessionEventPayload::UsageUpdated {
                usage: session.usage.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(action_event);
        self.shared.publish_session(usage_event);
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

    pub fn list_session_action_attempts(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<ActionAttempt>, StoreError> {
        self.query_documents(
            "SELECT action_attempts.document_json FROM action_attempts
             INNER JOIN action_invocations
               ON action_invocations.id = action_attempts.invocation_id
             WHERE action_invocations.session_id = ?1
             ORDER BY action_invocations.created_at ASC,
                      action_invocations.id ASC,
                      action_attempts.number ASC",
            [session_id.to_string()],
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
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut invocation: ActionInvocation = load_document_tx(
            &transaction,
            "action_invocations",
            &invocation_id.to_string(),
            "action invocation",
        )?;
        let mut attempt: ActionAttempt = load_document_tx(
            &transaction,
            "action_attempts",
            &attempt_id.to_string(),
            "action attempt",
        )?;
        if attempt.invocation_id != invocation.id {
            return Err(StoreError::Invariant(
                "ActionAttempt does not belong to invocation".to_string(),
            ));
        }
        if invocation.status == status && attempt.status == status {
            if invocation.output == output && invocation.error == error && attempt.error == error {
                return Ok(invocation);
            }
            return Err(StoreError::Invariant(format!(
                "terminal Action {} cannot accept a different result",
                invocation.id
            )));
        }
        if invocation.status != ActionStatus::Running || attempt.status != ActionStatus::Running {
            return Err(StoreError::Invariant(format!(
                "cannot finish Action {} from invocation {:?} and attempt {:?}",
                invocation.id, invocation.status, attempt.status
            )));
        }
        let now = Utc::now();
        invocation.status = status;
        invocation.output = output;
        invocation.error = error.clone();
        invocation.updated_at = now;
        attempt.status = status;
        attempt.error = error;
        attempt.updated_at = now;
        let mut session: Session = load_document_tx(
            &transaction,
            "sessions",
            &invocation.session_id.to_string(),
            "session",
        )?;
        if status == ActionStatus::Completed {
            session.usage.actions_completed = session.usage.actions_completed.saturating_add(1);
            session.updated_at = now;
        }
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
        if status == ActionStatus::Completed {
            update_session_tx(&transaction, &session)?;
        }
        let action_event = append_session_event_tx(
            &transaction,
            session.id,
            Some(invocation.agent_id),
            None,
            None,
            action_event_payload(&invocation, Some(attempt.id)),
        )?;
        let usage_event = if status == ActionStatus::Completed {
            Some(append_session_event_tx(
                &transaction,
                session.id,
                None,
                None,
                None,
                SessionEventPayload::UsageUpdated {
                    usage: session.usage.clone(),
                },
            )?)
        } else {
            None
        };
        let input_events =
            apply_terminal_action_inputs_tx(&transaction, invocation.id, invocation.updated_at)?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(action_event);
        if let Some(event) = usage_event {
            self.shared.publish_session(event);
        }
        for event in input_events {
            self.shared.publish_session(event);
        }
        Ok(invocation)
    }

    pub fn create_human_request_with_id(
        &self,
        request_id: HumanRequestId,
        session_id: SessionId,
        agent_id: AgentId,
        question: impl Into<String>,
        response_schema: Value,
    ) -> Result<HumanRequest, StoreError> {
        let question = question.into();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut session: Session =
            load_document_tx(&transaction, "sessions", &session_id.to_string(), "session")?;
        let agent: Agent =
            load_document_tx(&transaction, "agents", &agent_id.to_string(), "agent")?;
        if agent.session_id != session_id {
            return Err(StoreError::Invariant(
                "HumanRequest Agent belongs to another Session".to_string(),
            ));
        }
        if !matches!(
            session.status,
            SessionStatus::Running | SessionStatus::Paused
        ) {
            return Err(StoreError::Invariant(
                "cannot request human input unless its Session is running or paused".to_string(),
            ));
        }
        let now = Utc::now();
        let request = HumanRequest {
            id: request_id,
            session_id,
            agent_id,
            question,
            response_schema,
            status: HumanRequestStatus::Open,
            answer: None,
            created_at: now,
            resolved_at: None,
        };
        session.attention_required = true;
        let previous_status = session.status;
        if session.status != SessionStatus::Paused {
            session.status = SessionStatus::WaitingForInput;
        }
        session.updated_at = now;
        transaction.execute(
            "INSERT INTO human_requests
             (id, session_id, agent_id, status, created_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                request.id.to_string(),
                session_id.to_string(),
                agent_id.to_string(),
                enum_string(request.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&request)?
            ],
        )?;
        update_session_tx(&transaction, &session)?;
        let request_event = append_session_event_tx(
            &transaction,
            session_id,
            Some(agent_id),
            None,
            None,
            SessionEventPayload::HumanRequestOpened {
                human_request_id: request.id,
                question: request.question.clone(),
            },
        )?;
        let status_event = (session.status != previous_status)
            .then(|| {
                append_session_event_tx(
                    &transaction,
                    session_id,
                    None,
                    None,
                    None,
                    SessionEventPayload::SessionChanged {
                        status: session.status,
                        reason: None,
                    },
                )
            })
            .transpose()?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(request_event);
        if let Some(event) = status_event {
            self.shared.publish_session(event);
        }
        Ok(request)
    }

    pub fn get_human_request(&self, id: HumanRequestId) -> Result<HumanRequest, StoreError> {
        self.query_document_by_id("human_requests", id.to_string(), "human request")
    }

    pub fn list_human_requests(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<HumanRequest>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM human_requests WHERE session_id = ?1 ORDER BY created_at ASC",
            [session_id.to_string()],
        )
    }

    pub fn answer_human_request(
        &self,
        id: HumanRequestId,
        answer: Value,
    ) -> Result<HumanRequest, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut request: HumanRequest = load_document_tx(
            &transaction,
            "human_requests",
            &id.to_string(),
            "human request",
        )?;
        if request.status != HumanRequestStatus::Open {
            return Err(StoreError::Invariant(
                "human request is already resolved".to_string(),
            ));
        }
        request.status = HumanRequestStatus::Answered;
        request.answer = Some(answer);
        request.resolved_at = Some(Utc::now());
        let mut session: Session = load_document_tx(
            &transaction,
            "sessions",
            &request.session_id.to_string(),
            "session",
        )?;
        let previous_status = session.status;
        let changed = transaction.execute(
            "UPDATE human_requests SET status = ?1, document_json = ?2
             WHERE id = ?3 AND status = 'open'",
            params![
                enum_string(request.status)?,
                serde_json::to_string(&request)?,
                request.id.to_string(),
            ],
        )?;
        ensure_one(changed, "human_requests")?;
        let remaining = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM human_requests WHERE session_id = ?1
             AND status = 'open' AND id != ?2)",
            params![session.id.to_string(), id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        session.attention_required = remaining;
        if !remaining && session.status == SessionStatus::WaitingForInput {
            session.status = SessionStatus::Running;
        }
        session.updated_at = Utc::now();
        update_session_tx(&transaction, &session)?;
        let resolved_event = append_session_event_tx(
            &transaction,
            session.id,
            Some(request.agent_id),
            None,
            None,
            SessionEventPayload::HumanRequestResolved {
                human_request_id: id,
            },
        )?;
        let status_event = (session.status != previous_status)
            .then(|| {
                append_session_event_tx(
                    &transaction,
                    session.id,
                    None,
                    None,
                    None,
                    SessionEventPayload::SessionChanged {
                        status: session.status,
                        reason: None,
                    },
                )
            })
            .transpose()?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_session(resolved_event);
        if let Some(event) = status_event {
            self.shared.publish_session(event);
        }
        Ok(request)
    }

    pub fn create_agent_input(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        invocation_id: Option<ActionInvocationId>,
        source: AgentInputSource,
        kind: AgentInputKind,
        content: impl Into<String>,
    ) -> Result<AgentInput, StoreError> {
        self.create_agent_input_with_id(
            AgentInputId::new(),
            session_id,
            agent_id,
            invocation_id,
            source,
            kind,
            content,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_agent_input_with_id(
        &self,
        id: AgentInputId,
        session_id: SessionId,
        agent_id: AgentId,
        invocation_id: Option<ActionInvocationId>,
        source: AgentInputSource,
        kind: AgentInputKind,
        content: impl Into<String>,
    ) -> Result<AgentInput, StoreError> {
        let content = content.into();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (message, event) = insert_agent_input_tx(
            &transaction,
            id,
            session_id,
            agent_id,
            invocation_id,
            source,
            kind,
            content,
        )?;
        transaction.commit()?;
        drop(connection);
        if let Some(event) = event {
            self.shared.publish_session(event);
        }
        Ok(message)
    }

    pub fn claim_agent_inputs(
        &self,
        session_id: SessionId,
        agent_id: AgentId,
        invocation_id: Option<ActionInvocationId>,
        turn_id: TurnId,
    ) -> Result<Vec<AgentInput>, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_session_accepts_effect_tx(&transaction, session_id)?;
        let turn: Turn = load_document_tx(&transaction, "turns", &turn_id.to_string(), "turn")?;
        if turn.agent_id != agent_id || turn.status.is_terminal() {
            return Err(StoreError::Invariant(
                "Agent inputs can only be claimed by their active target Turn".to_string(),
            ));
        }
        let mut statement = transaction.prepare(
            "SELECT document_json FROM agent_inputs
             WHERE session_id = ?1 AND agent_id = ?2
               AND (status = 'pending' OR (status = 'claimed' AND claimed_turn_id = ?3))
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = statement.query_map(
            params![
                session_id.to_string(),
                agent_id.to_string(),
                turn_id.to_string()
            ],
            |row| row.get::<_, String>(0),
        )?;
        let documents = rows.collect::<Result<Vec<String>, _>>()?;
        drop(statement);
        let mut messages = documents
            .into_iter()
            .map(|document| serde_json::from_str(&document))
            .collect::<Result<Vec<AgentInput>, _>>()?;
        messages.retain(|message| {
            message.action_invocation_id.is_none() || message.action_invocation_id == invocation_id
        });
        let claimed_at = Utc::now();
        for message in &mut messages {
            if message.status == AgentInputStatus::Pending {
                message.status = AgentInputStatus::Claimed;
                message.claimed_turn_id = Some(turn_id);
                message.claimed_at = Some(claimed_at);
                let changed = transaction.execute(
                    "UPDATE agent_inputs
                     SET status = 'claimed', claimed_turn_id = ?1, updated_at = ?2, document_json = ?3
                     WHERE id = ?4 AND status = 'pending'",
                    params![
                        turn_id.to_string(),
                        claimed_at.to_rfc3339(),
                        serde_json::to_string(message)?,
                        message.id.to_string(),
                    ],
                )?;
                ensure_one(changed, "agent_inputs")?;
            }
        }
        transaction.commit()?;
        Ok(messages)
    }

    pub fn list_agent_inputs(&self, session_id: SessionId) -> Result<Vec<AgentInput>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM agent_inputs WHERE session_id = ?1 ORDER BY created_at ASC",
            [session_id.to_string()],
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_artifact(
        &self,
        project_id: ProjectId,
        session_id: SessionId,
        agent_id: Option<AgentId>,
        action_invocation_id: Option<ActionInvocationId>,
        kind: ArtifactKind,
        name: impl Into<String>,
        media_type: impl Into<String>,
        metadata: Value,
        bytes: &[u8],
    ) -> Result<Artifact, StoreError> {
        self.create_artifact_with_id(
            ArtifactId::new(),
            project_id,
            session_id,
            agent_id,
            action_invocation_id,
            kind,
            name,
            media_type,
            metadata,
            bytes,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_artifact_with_id(
        &self,
        id: ArtifactId,
        project_id: ProjectId,
        session_id: SessionId,
        agent_id: Option<AgentId>,
        action_invocation_id: Option<ActionInvocationId>,
        kind: ArtifactKind,
        name: impl Into<String>,
        media_type: impl Into<String>,
        metadata: Value,
        bytes: &[u8],
    ) -> Result<Artifact, StoreError> {
        let session = self.get_session(session_id)?;
        if session.project_id != project_id {
            return Err(StoreError::Invariant(
                "artifact Project does not match Session".to_string(),
            ));
        }
        if let Some(agent_id) = agent_id {
            let agent = self.get_agent(agent_id)?;
            if agent.session_id != session_id {
                return Err(StoreError::Invariant(
                    "Artifact Agent belongs to another Session".to_string(),
                ));
            }
        }
        if let Some(invocation_id) = action_invocation_id {
            let invocation = self.get_action_invocation(invocation_id)?;
            if invocation.session_id != session_id || Some(invocation.agent_id) != agent_id {
                return Err(StoreError::Invariant(
                    "Artifact Action ownership does not match its Session and Agent".to_string(),
                ));
            }
        }
        let name = name.into();
        let stored = store_artifact_file(
            &self.shared.artifact_root,
            session_id,
            agent_id,
            id,
            &name,
            bytes,
        )?;
        let artifact = Artifact {
            id,
            project_id,
            session_id,
            agent_id,
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
        let created_file = stored.created;
        let inserted = self.insert_indexed_document(
            "artifacts",
            &id.to_string(),
            &[session_id.to_string()],
            "created",
            artifact.created_at,
            &artifact,
        );
        if let Err(error) = inserted {
            if created_file {
                let _ = remove_artifact_file(&self.shared.artifact_root, &artifact.relative_path);
            }
            return Err(error);
        }
        Ok(artifact)
    }

    pub fn list_artifacts(&self, session_id: SessionId) -> Result<Vec<Artifact>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM artifacts WHERE session_id = ?1 ORDER BY created_at ASC",
            [session_id.to_string()],
        )
    }

    pub fn list_project_artifacts(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Artifact>, StoreError> {
        self.query_documents(
            "SELECT a.document_json FROM artifacts a JOIN sessions wr ON wr.id = a.session_id
             WHERE wr.project_id = ?1 ORDER BY a.created_at DESC",
            [project_id.to_string()],
        )
    }

    pub fn get_project_home(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<ProjectHome>, StoreError> {
        self.get_project(project_id)?;
        let document = self
            .connection()?
            .query_row(
                "SELECT document_json FROM project_homes WHERE project_id = ?1",
                [project_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        document
            .map(|document| serde_json::from_str(&document).map_err(StoreError::from))
            .transpose()
    }

    pub(crate) fn commit_project_home(
        &self,
        expected_base: Option<ArtifactId>,
        home: &ProjectHome,
        source: &Artifact,
        page: &Artifact,
    ) -> Result<ProjectHome, StoreError> {
        if source.project_id != home.project_id
            || page.project_id != home.project_id
            || source.session_id != page.session_id
            || source.id != home.source_artifact_id
            || page.id != home.artifact_id
        {
            return Err(StoreError::Invariant(
                "Project-home publication has inconsistent ownership".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = transaction
            .query_row(
                "SELECT document_json FROM project_homes WHERE project_id = ?1",
                [home.project_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|document| serde_json::from_str::<ProjectHome>(&document))
            .transpose()?;
        if let Some(current) = current.as_ref()
            && current.artifact_id == home.artifact_id
        {
            return Ok(current.clone());
        }
        if current.as_ref().map(|current| current.artifact_id) != expected_base {
            return Err(StoreError::Invariant(
                "Project-home base revision changed before publication".to_string(),
            ));
        }
        insert_indexed_document_tx(
            &transaction,
            "artifacts",
            &source.id.to_string(),
            &[source.session_id.to_string()],
            "created",
            source.created_at,
            source,
        )?;
        insert_indexed_document_tx(
            &transaction,
            "artifacts",
            &page.id.to_string(),
            &[page.session_id.to_string()],
            "created",
            page.created_at,
            page,
        )?;
        transaction.execute(
            "INSERT INTO project_homes
             (project_id, artifact_id, source_artifact_id, revision, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(project_id) DO UPDATE SET
               artifact_id = excluded.artifact_id,
               source_artifact_id = excluded.source_artifact_id,
               revision = excluded.revision,
               updated_at = excluded.updated_at,
               document_json = excluded.document_json",
            params![
                home.project_id.to_string(),
                home.artifact_id.to_string(),
                home.source_artifact_id.to_string(),
                home.revision,
                home.updated_at.to_rfc3339(),
                serde_json::to_string(home)?,
            ],
        )?;
        transaction.commit()?;
        Ok(home.clone())
    }

    pub fn get_artifact(&self, id: ArtifactId) -> Result<Artifact, StoreError> {
        self.query_document_by_id("artifacts", id.to_string(), "artifact")
    }

    pub fn read_artifact(&self, artifact: &Artifact) -> Result<Vec<u8>, StoreError> {
        read_artifact_file(&self.shared.artifact_root, artifact)
    }

    /// Reconcile the filesystem side of Artifact commits before this Project
    /// runtime starts accepting work. Callers must ensure no Artifact write is
    /// concurrently between its file and database commit.
    pub fn reconcile_artifacts(&self) -> Result<(), StoreError> {
        let artifacts: Vec<Artifact> =
            self.query_documents("SELECT document_json FROM artifacts ORDER BY id ASC", [])?;
        reconcile_artifact_files(&self.shared.artifact_root, &artifacts)
    }

    pub fn agent_rollout_path(&self, agent_id: AgentId) -> PathBuf {
        crate::rollout::path(&self.shared.rollout_root, agent_id)
    }

    pub fn list_agent_rollout_records(
        &self,
        agent_id: AgentId,
    ) -> Result<Vec<AgentRolloutRecord>, StoreError> {
        let agent_lock = self.shared.agent_rollout_lock(agent_id)?;
        let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        crate::rollout::read(&self.shared.rollout_root, agent_id)
    }

    pub fn reconstruct_agent_rollout(
        &self,
        agent_id: AgentId,
    ) -> Result<AgentRolloutState, StoreError> {
        let agent_lock = self.shared.agent_rollout_lock(agent_id)?;
        let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        crate::rollout::reconstruct_file(&self.shared.rollout_root, agent_id)
    }

    pub fn agent_rollout_status(
        &self,
        agent_id: AgentId,
    ) -> Result<AgentRolloutStatus, StoreError> {
        self.get_agent(agent_id)?;
        let agent_lock = self.shared.agent_rollout_lock(agent_id)?;
        let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_agent_rollout_locked(agent_id)?;
        let last_sequence = crate::rollout::last_sequence(&self.shared.rollout_root, agent_id)?;
        let projected_sequence = self
            .connection()?
            .query_row(
                "SELECT last_sequence FROM agent_rollout_projection WHERE agent_id = ?1",
                [agent_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(AgentRolloutStatus {
            version: AGENT_ROLLOUT_VERSION,
            last_sequence,
            projected_sequence,
        })
    }

    fn commit_agent_rollout_item_locked(
        &self,
        agent_id: AgentId,
        item: AgentRolloutItem,
    ) -> Result<AgentRolloutRecord, StoreError> {
        self.replay_agent_rollout_locked(agent_id)?;

        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_rollout_item_tx(&transaction, &item)?;
        let last_sequence = self
            .shared
            .rollout_sequences
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .get(&agent_id)
            .copied()
            .unwrap_or(0);
        let sequence = last_sequence
            .checked_add(1)
            .ok_or_else(|| StoreError::Invariant("Agent rollout sequence overflow".to_string()))?;
        let record = AgentRolloutRecord {
            version: AGENT_ROLLOUT_VERSION,
            agent_id,
            sequence,
            occurred_at: Utc::now(),
            item,
        };

        // Match Codex's live-writer ordering: the JSONL durability barrier
        // must complete before SQLite is allowed to observe this sequence.
        // A failed projection is immediately rebuilt from the canonical file.
        crate::rollout::append(&self.shared.rollout_root, &record)?;
        self.shared
            .rollout_sequences
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .insert(agent_id, sequence);
        crate::process_fault::reach_process_fault_boundary(
            crate::process_fault::ROLLOUT_APPENDED_BEFORE_PROJECTION,
        );
        let projection = (|| -> Result<ProjectionEvents, StoreError> {
            let events = apply_rollout_record_tx(&transaction, &record)?;
            set_rollout_projection_tx(&transaction, agent_id, sequence)?;
            transaction.commit()?;
            Ok(events)
        })();
        drop(connection);
        let projection_events = match projection {
            Ok(events) => events,
            Err(initial_error) => {
                if let Err(replay_error) = self.replay_agent_rollout_locked(agent_id) {
                    return Err(StoreError::Invariant(format!(
                        "Agent rollout is durable but projection failed ({initial_error}) and immediate replay failed ({replay_error})"
                    )));
                }
                ProjectionEvents::default()
            }
        };
        for event in projection_events.events {
            self.shared.publish_session(event);
        }
        Ok(record)
    }

    fn replay_all_agent_rollouts(&self) -> Result<(), StoreError> {
        let mut agent_ids = Vec::new();
        for entry in std::fs::read_dir(&*self.shared.rollout_root)
            .map_err(|error| StoreError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| StoreError::Io(error.to_string()))?;
            let metadata = entry
                .metadata()
                .map_err(|error| StoreError::Io(error.to_string()))?;
            if !metadata.is_file() {
                return Err(StoreError::Invariant(format!(
                    "unexpected entry in Agent rollout directory: {}",
                    entry.path().display()
                )));
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                return Err(StoreError::Invariant(format!(
                    "unexpected entry in Agent rollout directory: {}",
                    path.display()
                )));
            }
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    StoreError::Invariant(format!("invalid Agent rollout path: {}", path.display()))
                })?;
            agent_ids.push(
                AgentId::from_str(stem)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?,
            );
        }
        agent_ids.sort_unstable();
        for agent_id in agent_ids {
            let agent_lock = self.shared.agent_rollout_lock(agent_id)?;
            let _guard = agent_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
            self.replay_agent_rollout_locked(agent_id)?;
        }
        Ok(())
    }

    fn replay_agent_rollout_locked(&self, agent_id: AgentId) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let projected = connection
            .query_row(
                "SELECT last_sequence FROM agent_rollout_projection WHERE agent_id = ?1",
                [agent_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let cached = self
            .shared
            .rollout_sequences
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .get(&agent_id)
            .copied();
        if cached == Some(projected) {
            return Ok(());
        }
        let (last_sequence, records) =
            crate::rollout::read_after(&self.shared.rollout_root, agent_id, projected)?;
        if projected > last_sequence {
            return Err(StoreError::Invariant(format!(
                "Agent {agent_id} projection sequence {projected} is ahead of rollout {last_sequence}"
            )));
        }
        if projected < last_sequence {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for record in &records {
                apply_rollout_record_tx(&transaction, record)?;
            }
            set_rollout_projection_tx(&transaction, agent_id, last_sequence)?;
            transaction.commit()?;
        }
        self.shared
            .rollout_sequences
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .insert(agent_id, last_sequence);
        Ok(())
    }

    fn ensure_project(&self, id: ProjectId) -> Result<(), StoreError> {
        let exists = self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
            [id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if exists {
            Ok(())
        } else {
            Err(StoreError::NotFound {
                entity: "project",
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
            1 => "id, session_id, status, updated_at, document_json",
            2 => "id, session_id, session_id, status, updated_at, document_json",
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

fn ensure_workspace_is_external(workspace: &Path, managed_root: &Path) -> Result<(), StoreError> {
    if workspace.starts_with(managed_root) || managed_root.starts_with(workspace) {
        return Err(StoreError::Invariant(
            "Project Workspace must be separate from PaperMachine managed state".to_string(),
        ));
    }
    Ok(())
}

fn ensure_workspace_attachment_available(
    workspace: &WorkspaceAttachment,
) -> Result<(), StoreError> {
    workspace.validate().map_err(StoreError::Invariant)?;
    let attached = Path::new(&workspace.path);
    let metadata = std::fs::symlink_metadata(attached).map_err(|error| {
        StoreError::Invariant(format!(
            "Workspace is unavailable: {}: {error}",
            workspace.path
        ))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(StoreError::Invariant(format!(
            "Workspace is not a real directory: {}",
            workspace.path
        )));
    }
    let canonical = attached.canonicalize().map_err(|error| {
        StoreError::Invariant(format!(
            "Workspace cannot be resolved: {}: {error}",
            workspace.path
        ))
    })?;
    if canonical != attached {
        return Err(StoreError::Invariant(format!(
            "Workspace no longer matches its attached canonical path: {}",
            workspace.path
        )));
    }
    Ok(())
}

fn create_managed_root(root: &Path) -> Result<PathBuf, StoreError> {
    std::fs::create_dir_all(root).map_err(|error| StoreError::Io(error.to_string()))?;
    for directory in [
        "prompts",
        "rollouts",
        "sessions",
        "skills",
        "sources",
        "state",
        "artifacts",
        "runtime/sandboxes",
    ] {
        std::fs::create_dir_all(root.join(directory))
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    let system_prompt = root.join(PROJECT_SYSTEM_PROMPT_PATH);
    if !system_prompt.exists() {
        write_atomic(&system_prompt, b"")?;
    }
    root.canonicalize()
        .map_err(|error| StoreError::Io(error.to_string()))
}

fn open_managed_root(root: &Path) -> Result<PathBuf, StoreError> {
    let root = root
        .canonicalize()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    for relative in [
        "prompts",
        "rollouts",
        "sessions",
        "skills",
        "sources",
        "state",
        "artifacts",
        "runtime/sandboxes",
    ] {
        let directory = root.join(relative);
        let metadata = std::fs::symlink_metadata(&directory)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(StoreError::Invariant(format!(
                "Project managed directory is missing or not a real directory: {}",
                directory.display()
            )));
        }
    }
    let system_prompt = root.join(PROJECT_SYSTEM_PROMPT_PATH);
    let prompt_metadata = std::fs::symlink_metadata(&system_prompt)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    if !prompt_metadata.is_file() || prompt_metadata.file_type().is_symlink() {
        return Err(StoreError::Invariant(format!(
            "Project system prompt is missing or not a real file: {}",
            system_prompt.display()
        )));
    }
    let database = root.join("state/project.db");
    let database_metadata =
        std::fs::symlink_metadata(&database).map_err(|error| StoreError::Io(error.to_string()))?;
    if !database_metadata.is_file() || database_metadata.file_type().is_symlink() {
        return Err(StoreError::Invariant(format!(
            "Project database is missing or not a real file: {}",
            database.display()
        )));
    }
    Ok(root)
}

fn validate_system_prompt(content: &str) -> Result<(), StoreError> {
    if content.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(StoreError::Invariant(format!(
            "system prompt exceeds the {MAX_SYSTEM_PROMPT_BYTES} byte limit"
        )));
    }
    Ok(())
}

fn validate_workflow_instructions(content: &str) -> Result<(), StoreError> {
    if content.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(StoreError::Invariant(format!(
            "Session instructions exceed the {MAX_SYSTEM_PROMPT_BYTES} byte limit"
        )));
    }
    Ok(())
}

fn hash_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

fn session_effect_hash(kind: &str, payload: &Value) -> Result<String, StoreError> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(
        &json!({"kind": kind, "payload": payload}),
    )?);
    Ok(hex::encode(hasher.finalize()))
}

fn initialize_new(connection: &Connection) -> Result<(), StoreError> {
    let current: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current != 0 {
        return Err(StoreError::Invariant(format!(
            "fresh Project database unexpectedly has schema {current}"
        )));
    }
    connection.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA user_version = {SCHEMA_VERSION};
         CREATE TABLE IF NOT EXISTS projects (
           id TEXT PRIMARY KEY, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sessions (
           id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
           program_slug TEXT NOT NULL, status TEXT NOT NULL,
           attention_required INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS sessions_project_updated ON sessions(project_id, updated_at DESC);
         CREATE INDEX IF NOT EXISTS sessions_project_program_updated ON sessions(project_id, program_slug, updated_at DESC);
         CREATE TABLE IF NOT EXISTS agents (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
           parent_agent_id TEXT REFERENCES agents(id),
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS agents_session_created ON agents(session_id, created_at ASC);
         CREATE INDEX IF NOT EXISTS agents_parent ON agents(parent_agent_id);
         CREATE TABLE IF NOT EXISTS session_effects (
           session_id TEXT NOT NULL REFERENCES sessions(id), effect_key TEXT NOT NULL,
           status TEXT NOT NULL, started_at TEXT NOT NULL, document_json TEXT NOT NULL,
           PRIMARY KEY(session_id, effect_key)
         );
         CREATE INDEX IF NOT EXISTS session_effects_started ON session_effects(session_id, started_at ASC);
         CREATE TABLE IF NOT EXISTS action_invocations (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
           agent_id TEXT NOT NULL REFERENCES agents(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS action_attempts (
           id TEXT PRIMARY KEY, invocation_id TEXT NOT NULL REFERENCES action_invocations(id), number INTEGER NOT NULL,
           status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL,
           UNIQUE(invocation_id, number)
         );
         CREATE TABLE IF NOT EXISTS turns (
           id TEXT PRIMARY KEY, agent_id TEXT NOT NULL REFERENCES agents(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS turns_agent_created ON turns(agent_id, created_at ASC);
         CREATE TABLE IF NOT EXISTS steps (
           id TEXT PRIMARY KEY, turn_id TEXT NOT NULL REFERENCES turns(id), sequence INTEGER NOT NULL,
           tool_call_id TEXT, status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL,
           UNIQUE(turn_id, sequence), UNIQUE(turn_id, tool_call_id)
         );
         CREATE TABLE IF NOT EXISTS session_events (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
           agent_id TEXT REFERENCES agents(id), turn_id TEXT REFERENCES turns(id), step_id TEXT REFERENCES steps(id),
           sequence INTEGER NOT NULL, occurred_at TEXT NOT NULL, event_json TEXT NOT NULL,
           UNIQUE(session_id, sequence)
         );
         CREATE INDEX IF NOT EXISTS session_events_sequence ON session_events(session_id, sequence ASC);
         CREATE TABLE IF NOT EXISTS agent_rollout_projection (
           agent_id TEXT PRIMARY KEY REFERENCES agents(id), last_sequence INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS human_requests (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
           agent_id TEXT NOT NULL REFERENCES agents(id), status TEXT NOT NULL,
           created_at TEXT NOT NULL, updated_at TEXT GENERATED ALWAYS AS (COALESCE(json_extract(document_json, '$.resolved_at'), created_at)) VIRTUAL,
           document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS human_requests_session ON human_requests(session_id, created_at ASC);
         CREATE TABLE IF NOT EXISTS agent_inputs (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
           agent_id TEXT NOT NULL REFERENCES agents(id), status TEXT NOT NULL,
           claimed_turn_id TEXT REFERENCES turns(id),
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS agent_inputs_claim
           ON agent_inputs(session_id, agent_id, status, claimed_turn_id, created_at ASC);
         CREATE TABLE IF NOT EXISTS artifacts (
           id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS project_homes (
           project_id TEXT PRIMARY KEY REFERENCES projects(id),
           artifact_id TEXT NOT NULL REFERENCES artifacts(id),
           source_artifact_id TEXT NOT NULL REFERENCES artifacts(id),
           revision TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS project_changes (
           sequence INTEGER PRIMARY KEY AUTOINCREMENT,
           project_id TEXT NOT NULL REFERENCES projects(id),
           entity_kind TEXT NOT NULL,
           entity_id TEXT NOT NULL,
           session_id TEXT
         );
         CREATE INDEX IF NOT EXISTS project_changes_project_sequence
           ON project_changes(project_id, sequence);
         CREATE TRIGGER IF NOT EXISTS project_change_project_insert
         AFTER INSERT ON projects BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           VALUES (NEW.id, 'project', NEW.id, NULL);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_project_update
         AFTER UPDATE ON projects BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           VALUES (NEW.id, 'project', NEW.id, NULL);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_session_insert
         AFTER INSERT ON sessions BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           VALUES (NEW.project_id, 'session', NEW.id, NEW.id);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_session_update
         AFTER UPDATE ON sessions BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           VALUES (NEW.project_id, 'session', NEW.id, NEW.id);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_agent_insert
         AFTER INSERT ON agents BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           SELECT project_id, 'agent', NEW.id, NEW.session_id FROM sessions WHERE id = NEW.session_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_agent_update
         AFTER UPDATE ON agents BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           SELECT project_id, 'agent', NEW.id, NEW.session_id FROM sessions WHERE id = NEW.session_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_turn_insert
         AFTER INSERT ON turns BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           SELECT sessions.project_id, 'turn', NEW.id, agents.session_id
           FROM agents JOIN sessions ON sessions.id = agents.session_id WHERE agents.id = NEW.agent_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_turn_update
         AFTER UPDATE ON turns BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           SELECT sessions.project_id, 'turn', NEW.id, agents.session_id
           FROM agents JOIN sessions ON sessions.id = agents.session_id WHERE agents.id = NEW.agent_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_artifact_insert
         AFTER INSERT ON artifacts BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           SELECT project_id, 'artifact', NEW.id, NEW.session_id
           FROM sessions WHERE id = NEW.session_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_home_insert
         AFTER INSERT ON project_homes BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           SELECT NEW.project_id, 'project_home', NEW.project_id, session_id
           FROM artifacts WHERE id = NEW.artifact_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_home_update
         AFTER UPDATE ON project_homes BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, session_id)
           SELECT NEW.project_id, 'project_home', NEW.project_id, session_id
           FROM artifacts WHERE id = NEW.artifact_id;
         END;"
    ))?;
    reconcile_terminal_session_resources(connection)?;
    Ok(())
}

fn verify_current(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;",
    )?;
    let current: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current != SCHEMA_VERSION {
        return Err(StoreError::Invariant(format!(
            "Project database schema {current} is not current schema {SCHEMA_VERSION}; create fresh PaperMachine state"
        )));
    }
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(StoreError::Invariant(format!(
            "Project database integrity check failed: {integrity}"
        )));
    }
    reconcile_terminal_session_resources(connection)?;
    Ok(())
}

fn ensure_session_accepts_effect_tx(
    transaction: &Transaction<'_>,
    session_id: SessionId,
) -> Result<(), StoreError> {
    let status = transaction.query_row(
        "SELECT status FROM sessions WHERE id = ?1",
        [session_id.to_string()],
        |row| row.get::<_, String>(0),
    )?;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        return Err(StoreError::Invariant(format!(
            "Session is terminal with status {status}"
        )));
    }
    Ok(())
}

fn load_document_tx<T: DeserializeOwned>(
    transaction: &Transaction<'_>,
    table: &str,
    id: &str,
    entity: &'static str,
) -> Result<T, StoreError> {
    let document = transaction
        .query_row(
            &format!("SELECT document_json FROM {table} WHERE id = ?1"),
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            entity,
            id: id.to_string(),
        })?;
    Ok(serde_json::from_str(&document)?)
}

fn apply_session_usage_delta(usage: &mut SessionUsage, delta: SessionUsage) {
    usage.agents_created = usage.agents_created.saturating_add(delta.agents_created);
    usage.actions_started = usage.actions_started.saturating_add(delta.actions_started);
    usage.actions_completed = usage
        .actions_completed
        .saturating_add(delta.actions_completed);
    usage.action_steps = usage.action_steps.saturating_add(delta.action_steps);
    usage.hosted_search_calls = usage
        .hosted_search_calls
        .saturating_add(delta.hosted_search_calls);
    usage.tokens.saturating_add_assign(delta.tokens);
    usage.wall_time_seconds = usage
        .wall_time_seconds
        .saturating_add(delta.wall_time_seconds);
    usage.estimated_cost_usd = match (usage.estimated_cost_usd, delta.estimated_cost_usd) {
        (Some(left), Some(right)) => Some(left + right),
        (value, None) | (None, value) => value,
    };
}

fn token_usage_delta(current: TokenUsage, previous: TokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: current.input_tokens.saturating_sub(previous.input_tokens),
        output_tokens: current.output_tokens.saturating_sub(previous.output_tokens),
        cached_input_tokens: current
            .cached_input_tokens
            .saturating_sub(previous.cached_input_tokens),
        cache_write_input_tokens: current
            .cache_write_input_tokens
            .saturating_sub(previous.cache_write_input_tokens),
    }
}

fn terminalize_session_resources_tx(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    now: chrono::DateTime<Utc>,
) -> Result<(), StoreError> {
    let now = now.to_rfc3339();
    transaction.execute(
        "UPDATE agent_inputs
         SET status = 'applied', updated_at = ?2,
             document_json = json_set(
                 json_set(document_json, '$.status', 'applied'),
                 '$.applied_at', ?2
             )
         WHERE session_id = ?1 AND status IN ('pending', 'claimed')",
        params![session_id.to_string(), now],
    )?;
    transaction.execute(
        "UPDATE human_requests
         SET status = 'cancelled',
             document_json = json_set(
                 json_set(document_json, '$.status', 'cancelled'),
                 '$.resolved_at', ?2
             )
         WHERE session_id = ?1 AND status = 'open'",
        params![session_id.to_string(), now],
    )?;
    Ok(())
}

fn reconcile_terminal_session_resources(connection: &Connection) -> Result<(), StoreError> {
    let session_ids = {
        let mut statement = connection.prepare(
            "SELECT id FROM sessions WHERE status IN ('completed', 'failed', 'cancelled')",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for id in session_ids {
        let session_id =
            SessionId::from_str(&id).map_err(|error| StoreError::Invariant(error.to_string()))?;
        let transaction = connection.unchecked_transaction()?;
        terminalize_session_resources_tx(&transaction, session_id, Utc::now())?;
        transaction.commit()?;
    }
    Ok(())
}

fn append_session_event_tx(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    agent_id: Option<AgentId>,
    turn_id: Option<TurnId>,
    step_id: Option<StepId>,
    payload: SessionEventPayload,
) -> Result<SessionEvent, StoreError> {
    let session: Session =
        load_document_tx(transaction, "sessions", &session_id.to_string(), "session")?;
    if let Some(agent_id) = agent_id {
        let agent: Agent = load_document_tx(transaction, "agents", &agent_id.to_string(), "agent")?;
        if agent.session_id != session_id {
            return Err(StoreError::Invariant(
                "Session event Agent belongs to another Session".to_string(),
            ));
        }
    }
    if let Some(turn_id) = turn_id {
        let turn: Turn = load_document_tx(transaction, "turns", &turn_id.to_string(), "turn")?;
        let turn_agent: Agent =
            load_document_tx(transaction, "agents", &turn.agent_id.to_string(), "agent")?;
        if turn_agent.session_id != session_id || agent_id.is_some_and(|id| id != turn.agent_id) {
            return Err(StoreError::Invariant(
                "Session event Turn ownership does not match its Session and Agent".to_string(),
            ));
        }
    }
    if let Some(step_id) = step_id {
        let step: AgentStep = load_document_tx(transaction, "steps", &step_id.to_string(), "step")?;
        if turn_id != Some(step.turn_id) {
            return Err(StoreError::Invariant(
                "Session event Step does not belong to its Turn".to_string(),
            ));
        }
    }
    let sequence = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM session_events WHERE session_id = ?1",
        [session_id.to_string()],
        |row| row.get::<_, u64>(0),
    )?;
    let event = SessionEvent {
        id: EventId::new(),
        sequence,
        project_id: session.project_id,
        session_id,
        agent_id,
        turn_id,
        step_id,
        occurred_at: Utc::now(),
        payload,
    };
    transaction.execute(
        "INSERT INTO session_events
         (id, session_id, agent_id, turn_id, step_id, sequence, occurred_at, event_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.id.to_string(),
            session_id.to_string(),
            agent_id.map(|id| id.to_string()),
            turn_id.map(|id| id.to_string()),
            step_id.map(|id| id.to_string()),
            sequence,
            event.occurred_at.to_rfc3339(),
            serde_json::to_string(&event)?
        ],
    )?;
    Ok(event)
}

#[derive(Default)]
struct ProjectionEvents {
    events: Vec<SessionEvent>,
}

fn validate_rollout_item_tx(
    transaction: &Transaction<'_>,
    item: &AgentRolloutItem,
) -> Result<(), StoreError> {
    let (turn_id, acknowledged_agent_input_ids) = match item {
        AgentRolloutItem::ContextCheckpoint {
            turn_id,
            acknowledged_agent_input_ids,
            ..
        } => (*turn_id, acknowledged_agent_input_ids),
        AgentRolloutItem::TurnUpdated {
            turn,
            acknowledged_agent_input_ids,
            ..
        } => (turn.id, acknowledged_agent_input_ids),
        _ => return Ok(()),
    };
    let mut seen = std::collections::HashSet::new();
    for input_id in acknowledged_agent_input_ids {
        if !seen.insert(*input_id) {
            return Err(StoreError::Invariant(format!(
                "duplicate acknowledged Agent input {input_id}"
            )));
        }
        let message: AgentInput = load_document_tx(
            transaction,
            "agent_inputs",
            &input_id.to_string(),
            "Agent input",
        )?;
        if message.status != AgentInputStatus::Claimed || message.claimed_turn_id != Some(turn_id) {
            return Err(StoreError::Invariant(format!(
                "Agent input {input_id} is not claimed by Turn {turn_id}"
            )));
        }
    }
    Ok(())
}

fn apply_agent_input_acknowledgements_tx(
    transaction: &Transaction<'_>,
    turn_id: TurnId,
    input_ids: &[AgentInputId],
    occurred_at: chrono::DateTime<Utc>,
) -> Result<ProjectionEvents, StoreError> {
    let mut events = ProjectionEvents::default();
    for input_id in input_ids {
        let mut message: AgentInput = load_document_tx(
            transaction,
            "agent_inputs",
            &input_id.to_string(),
            "Agent input",
        )?;
        if message.status == AgentInputStatus::Applied {
            continue;
        }
        if message.status != AgentInputStatus::Claimed || message.claimed_turn_id != Some(turn_id) {
            return Err(StoreError::Invariant(format!(
                "Agent input {input_id} is not claimed by Turn {turn_id}"
            )));
        }
        message.status = AgentInputStatus::Applied;
        message.applied_at = Some(occurred_at);
        let changed = transaction.execute(
            "UPDATE agent_inputs
             SET status = 'applied', updated_at = ?1, document_json = ?2
             WHERE id = ?3 AND status = 'claimed' AND claimed_turn_id = ?4",
            params![
                occurred_at.to_rfc3339(),
                serde_json::to_string(&message)?,
                input_id.to_string(),
                turn_id.to_string(),
            ],
        )?;
        ensure_one(changed, "agent_inputs")?;
        events.events.push(append_session_event_tx(
            transaction,
            message.session_id,
            Some(message.agent_id),
            Some(turn_id),
            None,
            SessionEventPayload::AgentInputApplied {
                agent_input_id: *input_id,
                kind: message.kind,
            },
        )?);
    }
    Ok(events)
}

fn apply_rollout_record_tx(
    transaction: &Transaction<'_>,
    record: &AgentRolloutRecord,
) -> Result<ProjectionEvents, StoreError> {
    let mut projected_events = ProjectionEvents::default();
    match &record.item {
        AgentRolloutItem::TurnCreated {
            turn,
            action_attempt,
        } => {
            transaction.execute(
                "INSERT INTO turns (id, agent_id, status, updated_at, document_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    turn.id.to_string(),
                    turn.agent_id.to_string(),
                    enum_string(turn.status)?,
                    turn.updated_at.to_rfc3339(),
                    serde_json::to_string(turn)?,
                ],
            )?;
            update_status_document_tx(
                transaction,
                "action_attempts",
                &action_attempt.id.to_string(),
                action_attempt.status,
                action_attempt,
            )?;
            let agent: Agent =
                load_document_tx(transaction, "agents", &turn.agent_id.to_string(), "agent")?;
            projected_events.events.push(append_session_event_tx(
                transaction,
                agent.session_id,
                Some(turn.agent_id),
                Some(turn.id),
                None,
                SessionEventPayload::TurnCreated,
            )?);
        }
        AgentRolloutItem::ContextCheckpoint {
            turn_id,
            usage,
            completed_model_steps,
            hosted_search_calls_used,
            checkpoint_message,
            acknowledged_agent_input_ids,
            ..
        } => {
            let document = transaction
                .query_row(
                    "SELECT document_json FROM turns WHERE id = ?1",
                    [turn_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound {
                    entity: "turn",
                    id: turn_id.to_string(),
                })?;
            let mut turn: Turn = serde_json::from_str(&document)?;
            let token_delta = token_usage_delta(*usage, turn.usage);
            let hosted_search_delta =
                hosted_search_calls_used.saturating_sub(turn.hosted_search_calls_used);
            turn.usage = *usage;
            turn.completed_model_steps = *completed_model_steps;
            turn.hosted_search_calls_used = *hosted_search_calls_used;
            turn.checkpoint_message.clone_from(checkpoint_message);
            turn.updated_at = record.occurred_at;
            update_status_document_tx(
                transaction,
                "turns",
                &turn.id.to_string(),
                turn.status,
                &turn,
            )?;
            if token_delta != TokenUsage::default() || hosted_search_delta > 0 {
                let agent: Agent =
                    load_document_tx(transaction, "agents", &turn.agent_id.to_string(), "agent")?;
                let mut session: Session = load_document_tx(
                    transaction,
                    "sessions",
                    &agent.session_id.to_string(),
                    "session",
                )?;
                apply_session_usage_delta(
                    &mut session.usage,
                    SessionUsage {
                        tokens: token_delta,
                        hosted_search_calls: hosted_search_delta,
                        ..SessionUsage::default()
                    },
                );
                session.updated_at = record.occurred_at;
                update_session_tx(transaction, &session)?;
                projected_events.events.push(append_session_event_tx(
                    transaction,
                    session.id,
                    Some(agent.id),
                    Some(*turn_id),
                    None,
                    SessionEventPayload::UsageUpdated {
                        usage: session.usage,
                    },
                )?);
            }
            let input_events = apply_agent_input_acknowledgements_tx(
                transaction,
                *turn_id,
                acknowledged_agent_input_ids,
                record.occurred_at,
            )?;
            projected_events.events.extend(input_events.events);
        }
        AgentRolloutItem::TurnUpdated {
            turn,
            acknowledged_agent_input_ids,
        } => {
            update_status_document_tx(
                transaction,
                "turns",
                &turn.id.to_string(),
                turn.status,
                turn,
            )?;
            let agent: Agent =
                load_document_tx(transaction, "agents", &turn.agent_id.to_string(), "agent")?;
            projected_events.events.push(append_session_event_tx(
                transaction,
                agent.session_id,
                Some(agent.id),
                Some(turn.id),
                None,
                SessionEventPayload::TurnStatusChanged {
                    status: turn.status,
                    error: turn.error.clone(),
                },
            )?);
            let input_events = apply_agent_input_acknowledgements_tx(
                transaction,
                turn.id,
                acknowledged_agent_input_ids,
                record.occurred_at,
            )?;
            projected_events.events.extend(input_events.events);
            if turn.status.is_terminal() {
                transaction.execute(
                    "UPDATE agent_inputs
                     SET status = 'pending', claimed_turn_id = NULL, updated_at = ?1,
                         document_json = json_set(
                             json_set(
                                 json_set(document_json, '$.status', 'pending'),
                                 '$.claimed_turn_id', NULL
                             ),
                             '$.claimed_at', NULL
                         )
                     WHERE status = 'claimed' AND claimed_turn_id = ?2",
                    params![record.occurred_at.to_rfc3339(), turn.id.to_string()],
                )?;
            }
        }
    }
    Ok(projected_events)
}

fn set_rollout_projection_tx(
    transaction: &Transaction<'_>,
    agent_id: AgentId,
    sequence: u64,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO agent_rollout_projection (agent_id, last_sequence)
         VALUES (?1, ?2)
         ON CONFLICT(agent_id) DO UPDATE SET last_sequence = excluded.last_sequence",
        params![agent_id.to_string(), sequence],
    )?;
    Ok(())
}

fn pending_session_event(
    project_id: ProjectId,
    session_id: SessionId,
    agent_id: Option<AgentId>,
    turn_id: Option<TurnId>,
    step_id: Option<StepId>,
    payload: SessionEventPayload,
) -> SessionEvent {
    SessionEvent {
        id: EventId::new(),
        sequence: 0,
        project_id,
        session_id,
        agent_id,
        turn_id,
        step_id,
        occurred_at: Utc::now(),
        payload,
    }
}

fn is_transient_session_event(payload: &SessionEventPayload) -> bool {
    matches!(
        payload,
        SessionEventPayload::AssistantMessageDelta { .. }
            | SessionEventPayload::AssistantMessageReset
            | SessionEventPayload::ModelStepStarted
    )
}

fn insert_indexed_document_tx<T: Serialize, S: Serialize>(
    transaction: &Transaction<'_>,
    table: &str,
    id: &str,
    indexes: &[String],
    status: S,
    updated_at: chrono::DateTime<Utc>,
    document: &T,
) -> Result<(), StoreError> {
    let columns = match indexes.len() {
        1 => "id, session_id, status, updated_at, document_json",
        2 => "id, session_id, session_id, status, updated_at, document_json",
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
    if indexes.len() == 1 {
        transaction.execute(
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
        transaction.execute(
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

fn update_session_tx(transaction: &Transaction<'_>, session: &Session) -> Result<(), StoreError> {
    let changed = transaction.execute(
        "UPDATE sessions SET status = ?1, attention_required = ?2, updated_at = ?3,
         document_json = ?4 WHERE id = ?5",
        params![
            enum_string(session.status)?,
            session.attention_required,
            session.updated_at.to_rfc3339(),
            serde_json::to_string(session)?,
            session.id.to_string()
        ],
    )?;
    ensure_one(changed, "sessions")
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

#[allow(clippy::too_many_arguments)]
fn insert_agent_input_tx(
    transaction: &Transaction<'_>,
    id: AgentInputId,
    session_id: SessionId,
    agent_id: AgentId,
    invocation_id: Option<ActionInvocationId>,
    source: AgentInputSource,
    kind: AgentInputKind,
    content: String,
) -> Result<(AgentInput, Option<SessionEvent>), StoreError> {
    match load_document_tx::<AgentInput>(
        transaction,
        "agent_inputs",
        &id.to_string(),
        "Agent input",
    ) {
        Ok(existing) => {
            if existing.session_id == session_id
                && existing.agent_id == agent_id
                && existing.action_invocation_id == invocation_id
                && existing.source == source
                && existing.kind == kind
                && existing.content == content
            {
                return Ok((existing, None));
            }
            return Err(StoreError::Invariant(format!(
                "Agent input id {id} was reused with different content"
            )));
        }
        Err(StoreError::NotFound { .. }) => {}
        Err(error) => return Err(error),
    }

    let session: Session =
        load_document_tx(transaction, "sessions", &session_id.to_string(), "session")?;
    if session.status.is_terminal() || session.archived_at.is_some() {
        return Err(StoreError::Invariant(
            "cannot deliver input to a terminal or archived Session".to_string(),
        ));
    }
    let agent: Agent = load_document_tx(transaction, "agents", &agent_id.to_string(), "agent")?;
    if agent.session_id != session.id {
        return Err(StoreError::Invariant(
            "input Agent does not belong to the Session".to_string(),
        ));
    }
    if let AgentInputSource::Agent { sender_agent_id } = &source {
        let sender: Agent = load_document_tx(
            transaction,
            "agents",
            &sender_agent_id.to_string(),
            "sender Agent",
        )?;
        let sender_session: Session = load_document_tx(
            transaction,
            "sessions",
            &sender.session_id.to_string(),
            "sender Session",
        )?;
        if sender_session.project_id != session.project_id
            || sender_session.status.is_terminal()
            || sender_session.archived_at.is_some()
        {
            return Err(StoreError::Invariant(
                "Agent input must stay inside one live Project".to_string(),
            ));
        }
    }
    if let Some(invocation_id) = invocation_id {
        let invocation: ActionInvocation = load_document_tx(
            transaction,
            "action_invocations",
            &invocation_id.to_string(),
            "action invocation",
        )?;
        if invocation.session_id != session_id || invocation.agent_id != agent_id {
            return Err(StoreError::Invariant(
                "input Action does not belong to the target Agent".to_string(),
            ));
        }
    }
    let message = AgentInput {
        id,
        session_id,
        agent_id,
        action_invocation_id: invocation_id,
        source,
        kind,
        content,
        status: AgentInputStatus::Pending,
        created_at: Utc::now(),
        claimed_turn_id: None,
        claimed_at: None,
        applied_at: None,
    };
    transaction.execute(
        "INSERT INTO agent_inputs
         (id, session_id, agent_id, status, claimed_turn_id, updated_at, document_json)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
        params![
            message.id.to_string(),
            session_id.to_string(),
            agent_id.to_string(),
            enum_string(message.status)?,
            message.created_at.to_rfc3339(),
            serde_json::to_string(&message)?,
        ],
    )?;
    let event = append_session_event_tx(
        transaction,
        session.id,
        Some(agent_id),
        None,
        None,
        SessionEventPayload::AgentInputQueued {
            agent_input_id: message.id,
            kind,
        },
    )?;
    Ok((message, Some(event)))
}

fn apply_terminal_action_inputs_tx(
    transaction: &Transaction<'_>,
    invocation_id: ActionInvocationId,
    occurred_at: chrono::DateTime<Utc>,
) -> Result<Vec<SessionEvent>, StoreError> {
    let documents = {
        let mut statement = transaction.prepare(
            "SELECT document_json FROM agent_inputs
             WHERE json_extract(document_json, '$.action_invocation_id') = ?1
               AND status IN ('pending', 'claimed')
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows =
            statement.query_map([invocation_id.to_string()], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut events = Vec::new();
    for document in documents {
        let mut input: AgentInput = serde_json::from_str(&document)?;
        input.status = AgentInputStatus::Applied;
        input.applied_at = Some(occurred_at);
        let changed = transaction.execute(
            "UPDATE agent_inputs SET status = 'applied', updated_at = ?1, document_json = ?2
             WHERE id = ?3 AND status IN ('pending', 'claimed')",
            params![
                occurred_at.to_rfc3339(),
                serde_json::to_string(&input)?,
                input.id.to_string(),
            ],
        )?;
        ensure_one(changed, "agent_inputs")?;
        events.push(append_session_event_tx(
            transaction,
            input.session_id,
            Some(input.agent_id),
            input.claimed_turn_id,
            None,
            SessionEventPayload::AgentInputApplied {
                agent_input_id: input.id,
                kind: input.kind,
            },
        )?);
    }
    Ok(events)
}

fn insert_action_invocation_tx(
    transaction: &Transaction<'_>,
    invocation_id: ActionInvocationId,
    action: NewActionInvocation,
) -> Result<(ActionInvocation, Option<SessionEvent>), StoreError> {
    match load_document_tx::<ActionInvocation>(
        transaction,
        "action_invocations",
        &invocation_id.to_string(),
        "action invocation",
    ) {
        Ok(existing) => {
            let same_request = existing.session_id == action.session_id
                && existing.agent_id == action.agent_id
                && existing.action_name == action.action_name
                && existing.contract == action.contract
                && existing.arguments == action.arguments
                && existing.input == action.input
                && existing.source == action.source
                && existing.tool_policy == action.tool_policy
                && existing.web_search_context_size == action.web_search_context_size
                && existing.reasoning_effort == action.reasoning_effort
                && existing.response_format == action.response_format;
            if same_request {
                return Ok((existing, None));
            }
            return Err(StoreError::Invariant(format!(
                "Action id {invocation_id} was reused with a different request"
            )));
        }
        Err(StoreError::NotFound { .. }) => {}
        Err(error) => return Err(error),
    }

    let agent: Agent =
        load_document_tx(transaction, "agents", &action.agent_id.to_string(), "agent")?;
    if agent.session_id != action.session_id {
        return Err(StoreError::Invariant(
            "Action Agent belongs to another Session".to_string(),
        ));
    }
    let session: Session = load_document_tx(
        transaction,
        "sessions",
        &action.session_id.to_string(),
        "session",
    )?;
    if !session.status.accepts_actions() || session.archived_at.is_some() {
        return Err(StoreError::Invariant(
            "cannot schedule an Action unless its Session is live and running".to_string(),
        ));
    }
    match &action.source {
        ActionSource::Workflow => {}
        ActionSource::HumanRequest { request_id } => {
            let request: HumanRequest = load_document_tx(
                transaction,
                "human_requests",
                &request_id.to_string(),
                "HumanRequest",
            )?;
            if request.session_id != action.session_id || request.agent_id != action.agent_id {
                return Err(StoreError::Invariant(
                    "HumanRequest Action source does not match its target".to_string(),
                ));
            }
        }
        ActionSource::Agent { sender_agent_id } => {
            let sender: Agent = load_document_tx(
                transaction,
                "agents",
                &sender_agent_id.to_string(),
                "sender Agent",
            )?;
            let sender_session: Session = load_document_tx(
                transaction,
                "sessions",
                &sender.session_id.to_string(),
                "sender Session",
            )?;
            if sender_session.project_id != session.project_id
                || sender_session.status.is_terminal()
                || sender_session.archived_at.is_some()
            {
                return Err(StoreError::Invariant(
                    "Agent Action source must belong to the same live Project".to_string(),
                ));
            }
        }
    }

    let now = Utc::now();
    let invocation = ActionInvocation {
        id: invocation_id,
        session_id: action.session_id,
        agent_id: action.agent_id,
        action_name: action.action_name,
        contract: action.contract,
        arguments: action.arguments,
        input: action.input,
        source: action.source,
        tool_policy: action.tool_policy,
        web_search_context_size: action.web_search_context_size,
        reasoning_effort: action.reasoning_effort,
        response_format: action.response_format,
        status: ActionStatus::Scheduled,
        output: None,
        error: None,
        created_at: now,
        updated_at: now,
    };
    transaction.execute(
        "INSERT INTO action_invocations
         (id, session_id, agent_id, status, updated_at, document_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            invocation.id.to_string(),
            invocation.session_id.to_string(),
            invocation.agent_id.to_string(),
            enum_string(invocation.status)?,
            now.to_rfc3339(),
            serde_json::to_string(&invocation)?,
        ],
    )?;
    let event = append_session_event_tx(
        transaction,
        invocation.session_id,
        Some(invocation.agent_id),
        None,
        None,
        action_event_payload(&invocation, None),
    )?;
    Ok((invocation, Some(event)))
}

fn action_event_payload(
    invocation: &ActionInvocation,
    attempt_id: Option<ActionAttemptId>,
) -> SessionEventPayload {
    SessionEventPayload::ActionChanged {
        action_invocation_id: invocation.id,
        action_attempt_id: attempt_id,
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
