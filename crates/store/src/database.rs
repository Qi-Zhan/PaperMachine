use crate::NewWorkflow;
use crate::StoreError;
use crate::StoreShared;
use crate::artifact::store_artifact_file;
use chrono::Duration;
use chrono::Utc;
use papermachine_protocol::*;
use rusqlite::Connection;
use rusqlite::OpenFlags;
use rusqlite::OptionalExtension;
use rusqlite::Transaction;
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

const SCHEMA_VERSION: u32 = 5;
const PROJECT_SYSTEM_PROMPT_PATH: &str = "prompts/system.md";
const MAX_SYSTEM_PROMPT_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    shared: StoreShared,
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
            managed_root,
        })
    }

    /// Open one existing current-schema Project store. This path never creates
    /// directories, databases, tables, or compatibility state.
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
            managed_root,
        };
        store.replay_all_session_rollouts()?;
        Ok(store)
    }

    pub fn open_in_memory(managed_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let managed_root = create_managed_root(managed_root.as_ref())?;
        let connection = Connection::open_in_memory()?;
        initialize_new(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            shared: StoreShared::new(&managed_root)?,
            managed_root,
        })
    }

    pub fn managed_root(&self) -> &Path {
        &self.managed_root
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.shared.workflow_events.subscribe()
    }

    pub fn subscribe_sessions(&self) -> broadcast::Receiver<SessionEvent> {
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
            "SELECT EXISTS(SELECT 1 FROM projects WHERE json_extract(document_json, '$.workspace.roots[0]') = ?1)",
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
        project.workspace.roots = vec![workspace_root.to_string_lossy().into_owned()];
        project.workspace.primary_root = 0;
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
        let path = self.managed_root.join(PROJECT_SYSTEM_PROMPT_PATH);
        reject_prompt_symlink(&path)?;
        let content =
            std::fs::read_to_string(&path).map_err(|error| StoreError::Io(error.to_string()))?;
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
        let path = self.managed_root.join(PROJECT_SYSTEM_PROMPT_PATH);
        reject_prompt_symlink(&path)?;
        write_atomic(&path, content.as_bytes())?;
        self.get_project_system_prompt(id)
    }

    pub fn create_session(
        &self,
        project_id: ProjectId,
        title: impl Into<String>,
        system_prompt: impl Into<String>,
        model: impl Into<String>,
        enabled_skills: Vec<String>,
    ) -> Result<Session, StoreError> {
        self.create_session_with_access(
            project_id,
            title,
            system_prompt,
            model,
            enabled_skills,
            AccessPreset::Research,
        )
    }

    pub fn create_session_with_access(
        &self,
        project_id: ProjectId,
        title: impl Into<String>,
        system_prompt: impl Into<String>,
        model: impl Into<String>,
        enabled_skills: Vec<String>,
        access: AccessPreset,
    ) -> Result<Session, StoreError> {
        self.ensure_project(project_id)?;
        let system_prompt = system_prompt.into();
        validate_system_prompt(&system_prompt)?;
        self.insert_session(Session {
            id: SessionId::new(),
            project_id,
            origin: SessionOrigin::User,
            title: title.into(),
            system_prompt,
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
            "INSERT INTO sessions (id, project_id, origin, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session.id.to_string(),
                session.project_id.to_string(),
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

    pub fn list_sessions(&self, project_id: ProjectId) -> Result<Vec<Session>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM sessions WHERE project_id = ?1
             AND status != 'archived'
             ORDER BY updated_at DESC, id ASC",
            [project_id.to_string()],
        )
    }

    pub fn set_session_status(
        &self,
        session_id: SessionId,
        status: SessionStatus,
        reason: Option<String>,
    ) -> Result<Session, StoreError> {
        let session_lock = self.shared.session_rollout_lock(session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(session_id)?;
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
        let session_lock = self.shared.session_rollout_lock(session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(session_id)?;
        let mut session = self.get_session(session_id)?;
        if session.status == SessionStatus::Archived {
            return Err(StoreError::Invariant(
                "cannot change skills on an archived Session".to_string(),
            ));
        }
        if self.session_has_active_turn(session_id)? {
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

    pub fn set_session_system_prompt(
        &self,
        session_id: SessionId,
        system_prompt: impl Into<String>,
    ) -> Result<Session, StoreError> {
        let session_lock = self.shared.session_rollout_lock(session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(session_id)?;
        let mut session = self.get_session(session_id)?;
        if session.status == SessionStatus::Archived {
            return Err(StoreError::Invariant(
                "cannot change the system prompt on an archived Session".to_string(),
            ));
        }
        if self.session_has_active_turn(session_id)? {
            return Err(StoreError::Invariant(
                "cannot change the system prompt while a Session has an active Turn".to_string(),
            ));
        }
        let system_prompt = system_prompt.into();
        validate_system_prompt(&system_prompt)?;
        session.system_prompt = system_prompt;
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
        access: AccessPreset,
    ) -> Result<Session, StoreError> {
        let session_lock = self.shared.session_rollout_lock(session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(session_id)?;
        let mut session = self.get_session(session_id)?;
        if session.status == SessionStatus::Archived {
            return Err(StoreError::Invariant(
                "cannot change access on an archived Session".to_string(),
            ));
        }
        if session.access == access {
            return Ok(session);
        }
        if self.session_has_active_turn(session_id)? {
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

    fn session_has_active_turn(&self, session_id: SessionId) -> Result<bool, StoreError> {
        Ok(self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE session_id = ?1
             AND status IN ('queued', 'running', 'paused'))",
            [session_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_turn(
        &self,
        session_id: SessionId,
        origin: TurnOrigin,
        input: impl Into<String>,
        model: impl Into<String>,
        prompt: PromptSnapshot,
        reasoning_effort: Option<ReasoningEffort>,
        tools_enabled: bool,
        expected_access: AccessPreset,
        tool_set: papermachine_protocol::ToolSetSnapshot,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        skill_snapshots: Vec<SkillSnapshot>,
    ) -> Result<Turn, StoreError> {
        self.create_turn_inner(
            None,
            None,
            session_id,
            origin,
            input,
            model,
            prompt,
            reasoning_effort,
            tools_enabled,
            expected_access,
            tool_set,
            web_search_context_size,
            response_format,
            skill_snapshots,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_resumed_turn(
        &self,
        resumed_from_turn_id: TurnId,
        session_id: SessionId,
        input: impl Into<String>,
        model: impl Into<String>,
        prompt: PromptSnapshot,
        reasoning_effort: Option<ReasoningEffort>,
        tools_enabled: bool,
        expected_access: AccessPreset,
        tool_set: papermachine_protocol::ToolSetSnapshot,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        skill_snapshots: Vec<SkillSnapshot>,
    ) -> Result<Turn, StoreError> {
        self.create_turn_inner(
            None,
            Some(resumed_from_turn_id),
            session_id,
            TurnOrigin::User,
            input,
            model,
            prompt,
            reasoning_effort,
            tools_enabled,
            expected_access,
            tool_set,
            web_search_context_size,
            response_format,
            skill_snapshots,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_turn_for_attempt(
        &self,
        attempt_id: ActionAttemptId,
        session_id: SessionId,
        origin: TurnOrigin,
        input: impl Into<String>,
        model: impl Into<String>,
        prompt: PromptSnapshot,
        reasoning_effort: Option<ReasoningEffort>,
        tools_enabled: bool,
        expected_access: AccessPreset,
        tool_set: papermachine_protocol::ToolSetSnapshot,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        skill_snapshots: Vec<SkillSnapshot>,
    ) -> Result<Turn, StoreError> {
        self.create_turn_inner(
            Some(attempt_id),
            None,
            session_id,
            origin,
            input,
            model,
            prompt,
            reasoning_effort,
            tools_enabled,
            expected_access,
            tool_set,
            web_search_context_size,
            response_format,
            skill_snapshots,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn create_turn_inner(
        &self,
        attempt_id: Option<ActionAttemptId>,
        resumed_from_turn_id: Option<TurnId>,
        session_id: SessionId,
        origin: TurnOrigin,
        input: impl Into<String>,
        model: impl Into<String>,
        prompt: PromptSnapshot,
        reasoning_effort: Option<ReasoningEffort>,
        tools_enabled: bool,
        expected_access: AccessPreset,
        tool_set: papermachine_protocol::ToolSetSnapshot,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        skill_snapshots: Vec<SkillSnapshot>,
    ) -> Result<Turn, StoreError> {
        let input = input.into();
        let session_lock = self.shared.session_rollout_lock(session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(session_id)?;
        let session = self.get_session(session_id)?;
        if session.access != expected_access {
            return Err(StoreError::Invariant(
                "Session access changed while its Turn tool set was materialized".to_string(),
            ));
        }
        tool_set.validate().map_err(StoreError::Invariant)?;
        if session.status == SessionStatus::Archived {
            return Err(StoreError::Invariant(
                "cannot add a Turn to an archived Session".to_string(),
            ));
        }
        let mut attempt = attempt_id
            .map(|id| self.get_action_attempt(id))
            .transpose()?;
        if attempt_id.is_some() && resumed_from_turn_id.is_some() {
            return Err(StoreError::Invariant(
                "A Workflow Action Turn cannot resume a standalone Turn".to_string(),
            ));
        }
        if let Some(interrupted_id) = resumed_from_turn_id {
            let interrupted = self.get_turn(interrupted_id)?;
            if interrupted.session_id != session_id
                || interrupted.status != TurnStatus::Interrupted
                || self.is_workflow_turn(interrupted_id)?
            {
                return Err(StoreError::Invariant(
                    "Resume source must be an interrupted standalone Turn in the same Session"
                        .to_string(),
                ));
            }
            if self.is_turn_resumed(interrupted_id)? {
                return Err(StoreError::Invariant(format!(
                    "Interrupted Turn {interrupted_id} was already resumed"
                )));
            }
        }
        if let Some(attempt) = attempt.as_ref() {
            let invocation = self.get_action_invocation(attempt.invocation_id)?;
            let valid_origin = match (origin, invocation.source_human_request_id) {
                (TurnOrigin::Workflow, None) => true,
                (TurnOrigin::User, Some(request_id)) => {
                    let request = self.get_human_request(request_id)?;
                    request.workflow_id == invocation.workflow_id
                        && request.session_id == session_id
                        && request.status == HumanRequestStatus::Answered
                        && request.answer.as_ref().and_then(Value::as_str) == Some(input.as_str())
                }
                _ => false,
            };
            if !valid_origin
                || attempt.status.is_terminal()
                || attempt.turn_id.is_some()
                || invocation.session_id != session_id
            {
                return Err(StoreError::Invariant(
                    "ActionAttempt cannot attach this Turn".to_string(),
                ));
            }
        }
        let active = self.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE session_id = ?1
             AND status IN ('queued', 'running', 'paused'))",
            [session_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if active {
            return Err(StoreError::Invariant(
                "Session already has an active Turn".to_string(),
            ));
        }
        let now = Utc::now();
        let project = self.get_project(session.project_id)?;
        ensure_workspace_attachment_available(&project.workspace)?;
        let environment = TurnEnvironmentSnapshot::materialize(
            project.workspace,
            self.managed_root.to_string_lossy().into_owned(),
            session.access,
        )
        .map_err(StoreError::Invariant)?;
        let turn = Turn {
            id: TurnId::new(),
            session_id,
            status: TurnStatus::Queued,
            origin,
            resumed_from_turn_id,
            input,
            output: None,
            model: model.into(),
            reasoning_effort,
            prompt,
            environment,
            tool_set,
            tools_enabled,
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
        if let Some(attempt) = attempt.as_mut() {
            attempt.turn_id = Some(turn.id);
            attempt.updated_at = now;
        }
        let event = pending_session_event(
            session_id,
            Some(turn.id),
            None,
            SessionEventPayload::TurnCreated {
                origin: turn.origin,
                input: turn.input.clone(),
                model: turn.model.clone(),
            },
        );
        self.commit_session_rollout_item_locked(
            session_id,
            SessionRolloutItem::TurnCreated {
                turn: turn.clone(),
                action_attempt: attempt,
                events: vec![event],
            },
        )?;
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

    pub fn list_resumable_standalone_turns(&self) -> Result<Vec<Turn>, StoreError> {
        self.query_documents(
            "SELECT t.document_json FROM turns t
             WHERE t.status IN ('queued', 'running')
             AND NOT EXISTS (
               SELECT 1 FROM action_attempts a
               WHERE json_extract(a.document_json, '$.turn_id') = t.id
             )
             ORDER BY t.updated_at ASC, t.id ASC",
            [],
        )
    }

    pub fn is_workflow_turn(&self, turn_id: TurnId) -> Result<bool, StoreError> {
        self.get_turn(turn_id)?;
        self.connection()?
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM action_attempts
                   WHERE json_extract(document_json, '$.turn_id') = ?1
                 )",
                [turn_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn is_turn_resumed(&self, turn_id: TurnId) -> Result<bool, StoreError> {
        self.get_turn(turn_id)?;
        self.connection()?
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM turns
                   WHERE json_extract(document_json, '$.resumed_from_turn_id') = ?1
                 )",
                [turn_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn start_turn(&self, id: TurnId) -> Result<Turn, StoreError> {
        self.set_turn_status(id, TurnStatus::Running, None)
    }

    pub fn complete_turn(
        &self,
        id: TurnId,
        output: String,
        usage: TokenUsage,
    ) -> Result<Turn, StoreError> {
        let existing = self.get_turn(id)?;
        let session_lock = self.shared.session_rollout_lock(existing.session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(existing.session_id)?;
        let records = crate::rollout::read(&self.shared.rollout_root, existing.session_id)?;
        let active = crate::rollout::reconstruct(&records)?
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
        turn.output = Some(output);
        turn.usage = usage;
        self.persist_turn_status_locked(turn, TurnStatus::Completed, None)
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
        let existing = self.get_turn(id)?;
        let session_lock = self.shared.session_rollout_lock(existing.session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(existing.session_id)?;
        let turn = self.get_turn(id)?;
        self.persist_turn_status_locked(turn, status, error)
    }

    pub fn checkpoint_turn_context(
        &self,
        id: TurnId,
        mutation: ModelContextMutation,
        usage: TokenUsage,
        completed_model_steps: u32,
        hosted_search_calls_used: u32,
        checkpoint_message: Option<String>,
    ) -> Result<Turn, StoreError> {
        let existing = self.get_turn(id)?;
        let session_lock = self.shared.session_rollout_lock(existing.session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(existing.session_id)?;
        let turn = self.get_turn(id)?;
        if turn.status.is_terminal() {
            return Err(StoreError::Invariant(format!(
                "cannot checkpoint terminal Turn {id}"
            )));
        }
        self.commit_session_rollout_item_locked(
            turn.session_id,
            SessionRolloutItem::ContextCheckpoint {
                turn_id: id,
                mutation,
                usage,
                completed_model_steps,
                hosted_search_calls_used,
                checkpoint_message,
            },
        )?;
        self.get_turn(id)
    }

    fn persist_turn_status_locked(
        &self,
        mut turn: Turn,
        status: TurnStatus,
        error: Option<String>,
    ) -> Result<Turn, StoreError> {
        if turn.status.is_terminal() {
            return Err(StoreError::Invariant(format!(
                "cannot change terminal Turn {} from {:?} to {:?}",
                turn.id, turn.status, status
            )));
        }
        turn.status = status;
        turn.error = error.clone();
        turn.updated_at = Utc::now();
        let mut session = self.get_session(turn.session_id)?;
        if session.status != SessionStatus::Archived {
            session.status = match status {
                TurnStatus::Queued | TurnStatus::Running => SessionStatus::Running,
                TurnStatus::Paused => SessionStatus::Paused,
                TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Cancelled => {
                    SessionStatus::Ready
                }
                TurnStatus::Failed => SessionStatus::Failed,
            };
        }
        session.updated_at = turn.updated_at;
        let turn_event = pending_session_event(
            session.id,
            Some(turn.id),
            None,
            SessionEventPayload::TurnStatusChanged { status, error },
        );
        let session_event = pending_session_event(
            session.id,
            Some(turn.id),
            None,
            SessionEventPayload::SessionStatusChanged {
                status: session.status,
                reason: None,
            },
        );
        self.commit_session_rollout_item_locked(
            session.id,
            SessionRolloutItem::TurnUpdated {
                turn: turn.clone(),
                session,
                events: vec![turn_event, session_event],
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
        self.create_step_inner(turn_id, kind, name, None, None, input)
    }

    pub fn create_tool_step(
        &self,
        turn_id: TurnId,
        call_id: impl Into<String>,
        name: impl Into<String>,
        input: Value,
        effect_disposition: ToolEffectDisposition,
    ) -> Result<AgentStep, StoreError> {
        self.create_step_inner(
            turn_id,
            StepKind::Tool,
            name,
            Some(call_id.into()),
            Some(effect_disposition),
            input,
        )
    }

    fn create_step_inner(
        &self,
        turn_id: TurnId,
        kind: StepKind,
        name: impl Into<String>,
        tool_call_id: Option<String>,
        effect_disposition: Option<ToolEffectDisposition>,
        input: Value,
    ) -> Result<AgentStep, StoreError> {
        let turn = self.get_turn(turn_id)?;
        let session_lock = self.shared.session_rollout_lock(turn.session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(turn.session_id)?;
        let now = Utc::now();
        let sequence = self.connection()?.query_row(
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
            tool_call_id,
            effect_disposition,
            execution_state: effect_disposition.map(|_| ToolExecutionState::Prepared),
            status: StepStatus::Running,
            input,
            output: None,
            usage: TokenUsage::default(),
            duration_ms: None,
            created_at: now,
            updated_at: now,
        };
        self.commit_session_rollout_item_locked(
            turn.session_id,
            SessionRolloutItem::StepsCreated {
                steps: vec![step.clone()],
            },
        )?;
        Ok(step)
    }

    pub fn start_tool_execution(&self, id: StepId) -> Result<AgentStep, StoreError> {
        let existing = self.get_step(id)?;
        let turn = self.get_turn(existing.turn_id)?;
        let session_lock = self.shared.session_rollout_lock(turn.session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(turn.session_id)?;
        let mut step = self.get_step(id)?;
        if step.kind != StepKind::Tool
            || step.status != StepStatus::Running
            || step.execution_state != Some(ToolExecutionState::Prepared)
        {
            return Err(StoreError::Invariant(format!(
                "Step {id} is not a prepared Tool execution"
            )));
        }
        step.execution_state = Some(ToolExecutionState::Executing);
        step.updated_at = Utc::now();
        self.commit_session_rollout_item_locked(
            turn.session_id,
            SessionRolloutItem::StepsUpdated {
                steps: vec![step.clone()],
            },
        )?;
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
        let existing = self.get_step(id)?;
        let turn = self.get_turn(existing.turn_id)?;
        let session_lock = self.shared.session_rollout_lock(turn.session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(turn.session_id)?;
        let mut step = self.get_step(id)?;
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
        if step.effect_disposition.is_some() {
            step.execution_state = Some(match (status, step.execution_state) {
                (StepStatus::ExecutionUnknown, _) => ToolExecutionState::ExecutionUnknown,
                (_, Some(ToolExecutionState::Prepared)) => ToolExecutionState::Prepared,
                (_, Some(ToolExecutionState::Executing)) => ToolExecutionState::Completed,
                (_, state) => {
                    return Err(StoreError::Invariant(format!(
                        "cannot finish Tool Step {id} from execution state {state:?}"
                    )));
                }
            });
        }
        step.updated_at = Utc::now();
        self.commit_session_rollout_item_locked(
            turn.session_id,
            SessionRolloutItem::StepsUpdated {
                steps: vec![step.clone()],
            },
        )?;
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
        if !is_stable_rollout_event(&payload) {
            let session_lock = self.shared.session_rollout_lock(session_id)?;
            let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
            self.replay_session_rollout_locked(session_id)?;
            let mut connection = self.connection()?;
            let transaction = connection.transaction()?;
            let event =
                append_session_event_tx(&transaction, session_id, turn_id, step_id, payload)?;
            transaction.commit()?;
            drop(connection);
            self.shared.publish_session(event.clone());
            return Ok(event);
        }
        let pending = pending_session_event(session_id, turn_id, step_id, payload);
        let record = self.commit_session_rollout_item(
            session_id,
            SessionRolloutItem::SessionEventAppended { event: pending },
        )?;
        match record.item {
            SessionRolloutItem::SessionEventAppended { event } => Ok(event),
            _ => Err(StoreError::Invariant(
                "Session rollout returned the wrong item".to_string(),
            )),
        }
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

    pub fn register_workflow_program(
        &self,
        registration: &WorkflowProgram,
    ) -> Result<(), StoreError> {
        let owner_key = registration
            .project_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "builtin".to_string());
        self.connection()?.execute(
            "INSERT INTO workflow_programs (owner_key, id, slug, source, definition_path, sha256, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(owner_key, slug) DO UPDATE SET id=excluded.id, source=excluded.source,
             definition_path=excluded.definition_path, sha256=excluded.sha256,
             updated_at=excluded.updated_at, document_json=excluded.document_json",
            params![
                owner_key,
                registration.manifest.id.to_string(),
                registration.manifest.slug,
                enum_string(registration.source)?,
                registration.definition_path,
                registration.sha256,
                registration.updated_at.to_rfc3339(),
                serde_json::to_string(registration)?,
            ],
        )?;
        Ok(())
    }

    pub fn list_workflow_programs(&self) -> Result<Vec<WorkflowProgram>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_programs ORDER BY source ASC, owner_key ASC, slug ASC",
            [],
        )
    }

    pub fn create_workflow(&self, request: NewWorkflow) -> Result<Workflow, StoreError> {
        self.ensure_project(request.project_id)?;
        if request
            .program
            .project_id
            .is_some_and(|owner| owner != request.project_id)
        {
            return Err(StoreError::Invariant(
                "WorkflowProgram belongs to a different Project".to_string(),
            ));
        }
        let started_from = request
            .started_from_session_id
            .map(|session_id| self.get_session(session_id))
            .transpose()?;
        if started_from
            .as_ref()
            .is_some_and(|session| session.project_id != request.project_id)
        {
            return Err(StoreError::Invariant(
                "starting Session belongs to a different Project".to_string(),
            ));
        }
        if started_from
            .as_ref()
            .is_some_and(|session| session.status == SessionStatus::Archived)
        {
            return Err(StoreError::Invariant(
                "cannot start a Workflow from an archived Session".to_string(),
            ));
        }
        if let Some(session) = started_from.as_ref()
            && request.access > session.access
        {
            return Err(StoreError::Invariant(format!(
                "Workflow access {} exceeds starting Session access {}",
                request.access, session.access
            )));
        }
        if let Some((class_name, access)) = request
            .agent_access_overrides
            .iter()
            .find(|(_, access)| **access > request.access)
        {
            return Err(StoreError::Invariant(format!(
                "Agent override {class_name}={access} exceeds Workflow access {}",
                request.access
            )));
        }
        let requested_model = request.default_model;
        let default_model = if requested_model.trim().is_empty() {
            started_from
                .as_ref()
                .map(|session| session.model.clone())
                .ok_or_else(|| {
                    StoreError::Invariant(
                        "Project-level Workflow requires a default model".to_string(),
                    )
                })?
        } else {
            requested_model
        };
        let enabled_skills = if request.enabled_skills.is_empty() {
            started_from
                .as_ref()
                .map(|session| session.enabled_skills.clone())
                .unwrap_or_default()
        } else {
            request.enabled_skills
        };
        validate_workflow_instructions(&request.instructions)?;
        let now = Utc::now();
        let workflow = Workflow {
            id: WorkflowId::new(),
            project_id: request.project_id,
            started_from_session_id: request.started_from_session_id,
            program: request.program,
            request: request.request,
            instructions: request.instructions,
            trigger: request.trigger,
            default_model,
            access: request.access,
            enabled_skills,
            launch_context: request.launch_context,
            agent_access_overrides: request.agent_access_overrides,
            status: WorkflowStatus::Created,
            params: request.params,
            output: None,
            error: None,
            attention_required: false,
            usage: WorkflowUsage::default(),
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO workflows
             (id, project_id, started_from_session_id, status, attention_required, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![
                workflow.id.to_string(),
                workflow.project_id.to_string(),
                workflow.started_from_session_id.map(|id| id.to_string()),
                enum_string(workflow.status)?,
                workflow.updated_at.to_rfc3339(),
                serde_json::to_string(&workflow)?,
            ],
        )?;
        let workflow_event = append_workflow_event_tx(
            &transaction,
            workflow.project_id,
            workflow.id,
            WorkflowEventPayload::WorkflowCreated {
                request: workflow.request.clone(),
                program_slug: workflow.program.manifest.slug.clone(),
                source_sha256: workflow.program.sha256.clone(),
            },
        )?;
        let session_event = started_from
            .as_ref()
            .map(|session| {
                append_session_event_tx(
                    &transaction,
                    session.id,
                    None,
                    None,
                    SessionEventPayload::Warning {
                        message: format!("Started workflow {}", workflow.program.manifest.slug),
                    },
                )
            })
            .transpose()?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(workflow_event);
        if let Some(session_event) = session_event {
            self.shared.publish_session(session_event);
        }
        Ok(workflow)
    }

    pub fn get_workflow(&self, id: WorkflowId) -> Result<Workflow, StoreError> {
        self.query_document_by_id("workflows", id.to_string(), "Workflow")
    }

    pub fn list_workflows(
        &self,
        started_from_session_id: SessionId,
    ) -> Result<Vec<Workflow>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflows WHERE started_from_session_id = ?1
             ORDER BY updated_at DESC, id ASC",
            [started_from_session_id.to_string()],
        )
    }

    pub fn list_session_workflows(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Workflow>, StoreError> {
        self.query_documents(
            "SELECT DISTINCT wr.document_json FROM workflows wr
             LEFT JOIN workflow_participants p ON p.workflow_id = wr.id
             WHERE wr.started_from_session_id = ?1 OR p.session_id = ?1
             ORDER BY wr.updated_at DESC, wr.id ASC",
            [session_id.to_string()],
        )
    }

    pub fn list_project_workflows(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Workflow>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflows WHERE project_id = ?1
             ORDER BY updated_at DESC, id ASC",
            [project_id.to_string()],
        )
    }

    pub fn begin_workflow_effect(
        &self,
        workflow_id: WorkflowId,
        key: impl Into<String>,
        kind: impl Into<String>,
        payload: Value,
    ) -> Result<WorkflowEffect, StoreError> {
        let key = key.into();
        let kind = kind.into();
        let request_sha256 = workflow_effect_hash(&kind, &payload)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT document_json FROM workflow_effects
                 WHERE workflow_id = ?1 AND effect_key = ?2",
                params![workflow_id.to_string(), &key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(document) = existing {
            let effect: WorkflowEffect = serde_json::from_str(&document)?;
            if effect.kind != kind
                || effect.request_sha256 != request_sha256
                || effect.payload != payload
            {
                return Err(StoreError::Invariant(format!(
                    "Workflow effect {key} was replayed with a different request"
                )));
            }
            return Ok(effect);
        }
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        let effect = WorkflowEffect {
            workflow_id,
            key,
            kind,
            request_sha256,
            payload,
            status: WorkflowEffectStatus::Started,
            result: None,
            error: None,
            started_at: Utc::now(),
            completed_at: None,
        };
        transaction.execute(
            "INSERT INTO workflow_effects
             (workflow_id, effect_key, status, started_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                workflow_id.to_string(),
                &effect.key,
                enum_string(effect.status)?,
                effect.started_at.to_rfc3339(),
                serde_json::to_string(&effect)?,
            ],
        )?;
        transaction.commit()?;
        Ok(effect)
    }

    pub fn finish_workflow_effect(
        &self,
        workflow_id: WorkflowId,
        key: &str,
        outcome: Result<Value, String>,
    ) -> Result<WorkflowEffect, StoreError> {
        let mut effect = self.get_workflow_effect(workflow_id, key)?;
        let (status, result, error) = match outcome {
            Ok(result) => (WorkflowEffectStatus::Completed, Some(result), None),
            Err(error) => (WorkflowEffectStatus::Failed, None, Some(error)),
        };
        if effect.status != WorkflowEffectStatus::Started {
            if effect.status == status && effect.result == result && effect.error == error {
                return Ok(effect);
            }
            return Err(StoreError::Invariant(format!(
                "Workflow effect {key} already has a different terminal outcome"
            )));
        }
        effect.status = status;
        effect.result = result;
        effect.error = error;
        effect.completed_at = Some(Utc::now());
        let changed = self.connection()?.execute(
            "UPDATE workflow_effects SET status = ?1, document_json = ?2
             WHERE workflow_id = ?3 AND effect_key = ?4 AND status = 'started'",
            params![
                enum_string(effect.status)?,
                serde_json::to_string(&effect)?,
                workflow_id.to_string(),
                key,
            ],
        )?;
        ensure_one(changed, "workflow_effects")?;
        Ok(effect)
    }

    pub fn get_workflow_effect(
        &self,
        workflow_id: WorkflowId,
        key: &str,
    ) -> Result<WorkflowEffect, StoreError> {
        let document = self
            .connection()?
            .query_row(
                "SELECT document_json FROM workflow_effects
                 WHERE workflow_id = ?1 AND effect_key = ?2",
                params![workflow_id.to_string(), key],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "workflow effect",
                id: format!("{workflow_id}:{key}"),
            })?;
        Ok(serde_json::from_str(&document)?)
    }

    pub fn list_workflow_effects(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<Vec<WorkflowEffect>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_effects WHERE workflow_id = ?1
             ORDER BY started_at ASC, effect_key ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn list_recoverable_workflows(&self) -> Result<Vec<Workflow>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflows
             WHERE status NOT IN ('completed', 'failed', 'cancelled')
             ORDER BY updated_at ASC, id ASC",
            [],
        )
    }

    pub fn set_workflow_status(
        &self,
        id: WorkflowId,
        status: WorkflowStatus,
        reason: Option<String>,
    ) -> Result<Workflow, StoreError> {
        let mut run = self.get_workflow(id)?;
        run.status = status;
        run.error = if status == WorkflowStatus::Failed {
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
            terminalize_workflow_resources_tx(&transaction, run.id, status, run.updated_at)?;
        }
        update_workflow_tx(&transaction, &run)?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::WorkflowStatusChanged { status, reason },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(run)
    }

    pub fn complete_workflow(&self, id: WorkflowId, output: Value) -> Result<Workflow, StoreError> {
        let mut run = self.get_workflow(id)?;
        run.status = WorkflowStatus::Completed;
        run.output = Some(output.clone());
        run.error = None;
        run.attention_required = false;
        run.updated_at = Utc::now();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        terminalize_workflow_resources_tx(
            &transaction,
            run.id,
            WorkflowStatus::Completed,
            run.updated_at,
        )?;
        update_workflow_tx(&transaction, &run)?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::WorkflowCompleted { output },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(run)
    }

    pub fn add_workflow_usage(
        &self,
        id: WorkflowId,
        delta: WorkflowUsage,
    ) -> Result<Workflow, StoreError> {
        // Read and update under one database lock. Workflow actions can finish in
        // parallel, so a separate get followed by update loses concurrent deltas.
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let document = transaction.query_row(
            "SELECT document_json FROM workflows WHERE id = ?1",
            [id.to_string()],
            |row| row.get::<_, String>(0),
        )?;
        let mut run: Workflow = serde_json::from_str(&document)?;
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
        update_workflow_tx(&transaction, &run)?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::UsageUpdated {
                usage: run.usage.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(run)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_participant(
        &self,
        workflow_id: WorkflowId,
        class_name: impl Into<String>,
        name: impl Into<String>,
        role: impl Into<String>,
        system_prompt: impl Into<String>,
        model: impl Into<String>,
        skills: Vec<String>,
        access: AccessPreset,
    ) -> Result<WorkflowParticipant, StoreError> {
        self.create_participant_with_ids(
            workflow_id,
            AgentInstanceId::new(),
            SessionId::new(),
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
    pub fn create_participant_with_ids(
        &self,
        workflow_id: WorkflowId,
        participant_id: AgentInstanceId,
        session_id: SessionId,
        class_name: impl Into<String>,
        name: impl Into<String>,
        role: impl Into<String>,
        system_prompt: impl Into<String>,
        model: impl Into<String>,
        skills: Vec<String>,
        access: AccessPreset,
    ) -> Result<WorkflowParticipant, StoreError> {
        let mut run = self.get_workflow(workflow_id)?;
        if run.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot add an Agent to a terminal Workflow".to_string(),
            ));
        }
        let system_prompt = system_prompt.into();
        validate_system_prompt(&system_prompt)?;
        let now = Utc::now();
        let name = name.into();
        let role = role.into();
        let model = {
            let value = model.into();
            if value.trim().is_empty() {
                run.default_model.clone()
            } else {
                value
            }
        };
        let session = Session {
            id: session_id,
            project_id: run.project_id,
            origin: SessionOrigin::WorkflowAgent,
            title: name.clone(),
            system_prompt,
            model,
            access: std::cmp::min(access, run.access),
            status: SessionStatus::Ready,
            enabled_skills: if skills.is_empty() {
                run.enabled_skills.clone()
            } else {
                skills.clone()
            },
            created_at: now,
            updated_at: now,
        };
        let participant = WorkflowParticipant {
            id: participant_id,
            workflow_id: run.id,
            session_id: session.id,
            class_name: class_name.into(),
            name,
            role,
            system_prompt: session.system_prompt.clone(),
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
            "INSERT INTO sessions (id, project_id, origin, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session.id.to_string(),
                session.project_id.to_string(),
                enum_string(session.origin)?,
                enum_string(session.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&session)?,
            ],
        )?;
        transaction.execute(
            "INSERT INTO workflow_participants
             (id, workflow_id, session_id, status, updated_at, document_json)
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
        update_workflow_tx(&transaction, &run)?;
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
                workflow_id: run.id,
                agent_instance_id: participant.id,
                role: participant.role.clone(),
            },
        )?;
        let run_event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::ParticipantCreated {
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
        self.shared.publish_workflow(run_event);
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
        workflow_id: WorkflowId,
    ) -> Result<Vec<WorkflowParticipant>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_participants WHERE workflow_id = ?1
             ORDER BY created_at ASC, id ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn retire_participant(
        &self,
        id: AgentInstanceId,
    ) -> Result<WorkflowParticipant, StoreError> {
        let mut participant = self.get_participant(id)?;
        if participant.status == ParticipantStatus::Retired {
            return Ok(participant);
        }
        participant.status = ParticipantStatus::Retired;
        participant.updated_at = Utc::now();
        let run = self.get_workflow(participant.workflow_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        update_status_document_tx(
            &transaction,
            "workflow_participants",
            &id.to_string(),
            participant.status,
            &participant,
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::ParticipantRetired {
                agent_instance_id: id,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(participant)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_action_invocation(
        &self,
        workflow_id: WorkflowId,
        scope_id: Option<TaskScopeId>,
        agent_id: AgentInstanceId,
        action_name: impl Into<String>,
        contract: impl Into<String>,
        arguments: Value,
        requested_tools: Vec<String>,
    ) -> Result<ActionInvocation, StoreError> {
        self.create_action_invocation_with_id(
            ActionInvocationId::new(),
            workflow_id,
            scope_id,
            agent_id,
            action_name,
            contract,
            arguments,
            requested_tools,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_action_invocation_with_id(
        &self,
        invocation_id: ActionInvocationId,
        workflow_id: WorkflowId,
        scope_id: Option<TaskScopeId>,
        agent_id: AgentInstanceId,
        action_name: impl Into<String>,
        contract: impl Into<String>,
        arguments: Value,
        requested_tools: Vec<String>,
        source_human_request_id: Option<HumanRequestId>,
    ) -> Result<ActionInvocation, StoreError> {
        let participant = self.get_participant(agent_id)?;
        if participant.workflow_id != workflow_id || participant.status != ParticipantStatus::Active
        {
            return Err(StoreError::Invariant(
                "action Agent is not active in this Workflow".to_string(),
            ));
        }
        let run = self.get_workflow(workflow_id)?;
        let now = Utc::now();
        let invocation = ActionInvocation {
            id: invocation_id,
            workflow_id,
            task_scope_id: scope_id,
            agent_instance_id: agent_id,
            session_id: participant.session_id,
            action_name: action_name.into(),
            contract: contract.into(),
            arguments,
            requested_tools,
            source_human_request_id,
            status: ActionStatus::Scheduled,
            output: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        insert_indexed_document_tx(
            &transaction,
            "action_invocations",
            &invocation.id.to_string(),
            &[workflow_id.to_string(), participant.session_id.to_string()],
            invocation.status,
            now,
            &invocation,
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            workflow_id,
            action_event_payload(&invocation, None),
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
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
        workflow_id: WorkflowId,
    ) -> Result<Vec<ActionInvocation>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM action_invocations WHERE workflow_id = ?1
             ORDER BY created_at ASC, id ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn start_action_attempt(
        &self,
        invocation_id: ActionInvocationId,
    ) -> Result<ActionAttempt, StoreError> {
        let mut invocation = self.get_action_invocation(invocation_id)?;
        let mut run = self.get_workflow(invocation.workflow_id)?;
        if run.status != WorkflowStatus::Running {
            return Err(StoreError::Invariant("Workflow is not running".to_string()));
        }
        let number = self.connection()?.query_row(
            "SELECT COALESCE(MAX(number), 0) + 1 FROM action_attempts WHERE invocation_id = ?1",
            [invocation_id.to_string()],
            |row| row.get::<_, u32>(0),
        )?;
        let now = Utc::now();
        let attempt = ActionAttempt {
            id: ActionAttemptId::new(),
            workflow_id: invocation.workflow_id,
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
        run.usage.actions_started = run.usage.actions_started.saturating_add(1);
        run.updated_at = now;
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
             (id, workflow_id, invocation_id, number, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                attempt.id.to_string(),
                attempt.workflow_id.to_string(),
                attempt.invocation_id.to_string(),
                attempt.number,
                enum_string(attempt.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&attempt)?,
            ],
        )?;
        update_workflow_tx(&transaction, &run)?;
        let action_event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            action_event_payload(&invocation, Some(attempt.id)),
        )?;
        let usage_event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::UsageUpdated {
                usage: run.usage.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(action_event);
        self.shared.publish_workflow(usage_event);
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
        let mut run = self.get_workflow(invocation.workflow_id)?;
        if status == ActionStatus::Completed {
            run.usage.actions_completed = run.usage.actions_completed.saturating_add(1);
            run.updated_at = now;
        }
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
        if status == ActionStatus::Completed {
            update_workflow_tx(&transaction, &run)?;
        }
        let action_event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            action_event_payload(&invocation, Some(attempt.id)),
        )?;
        let usage_event = if status == ActionStatus::Completed {
            Some(append_workflow_event_tx(
                &transaction,
                run.project_id,
                run.id,
                WorkflowEventPayload::UsageUpdated {
                    usage: run.usage.clone(),
                },
            )?)
        } else {
            None
        };
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(action_event);
        if let Some(event) = usage_event {
            self.shared.publish_workflow(event);
        }
        Ok(invocation)
    }

    pub fn create_team(
        &self,
        workflow_id: WorkflowId,
        name: impl Into<String>,
        member_ids: Vec<AgentInstanceId>,
    ) -> Result<WorkflowTeam, StoreError> {
        self.create_team_with_id(TeamId::new(), workflow_id, name, member_ids)
    }

    pub fn create_team_with_id(
        &self,
        team_id: TeamId,
        workflow_id: WorkflowId,
        name: impl Into<String>,
        member_ids: Vec<AgentInstanceId>,
    ) -> Result<WorkflowTeam, StoreError> {
        self.validate_members(workflow_id, &member_ids)?;
        let run = self.get_workflow(workflow_id)?;
        let now = Utc::now();
        let team = WorkflowTeam {
            id: team_id,
            workflow_id,
            name: name.into(),
            member_ids,
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        insert_indexed_document_tx(
            &transaction,
            "workflow_teams",
            &team.id.to_string(),
            &[workflow_id.to_string()],
            "active",
            now,
            &team,
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            workflow_id,
            WorkflowEventPayload::TeamChanged {
                team_id: team.id,
                member_ids: team.member_ids.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(team)
    }

    pub fn get_team(&self, id: TeamId) -> Result<WorkflowTeam, StoreError> {
        self.query_document_by_id("workflow_teams", id.to_string(), "workflow team")
    }

    pub fn list_teams(&self, workflow_id: WorkflowId) -> Result<Vec<WorkflowTeam>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_teams WHERE workflow_id = ?1 ORDER BY created_at ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn set_team_members(
        &self,
        id: TeamId,
        member_ids: Vec<AgentInstanceId>,
    ) -> Result<WorkflowTeam, StoreError> {
        let mut team = self.get_team(id)?;
        self.validate_members(team.workflow_id, &member_ids)?;
        if team.member_ids == member_ids {
            return Ok(team);
        }
        team.member_ids = member_ids;
        team.updated_at = Utc::now();
        let run = self.get_workflow(team.workflow_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE workflow_teams SET updated_at = ?1, document_json = ?2 WHERE id = ?3",
            params![
                team.updated_at.to_rfc3339(),
                serde_json::to_string(&team)?,
                id.to_string(),
            ],
        )?;
        ensure_one(changed, "workflow_teams")?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            team.workflow_id,
            WorkflowEventPayload::TeamChanged {
                team_id: id,
                member_ids: team.member_ids.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(team)
    }

    pub fn list_relations(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<Vec<AgentRelation>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM agent_relations WHERE workflow_id = ?1 ORDER BY created_at ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn set_relation(
        &self,
        workflow_id: WorkflowId,
        source: AgentInstanceId,
        target: AgentInstanceId,
        kind: impl Into<String>,
        instructions: impl Into<String>,
    ) -> Result<AgentRelation, StoreError> {
        self.set_relation_with_id(
            RelationId::new(),
            workflow_id,
            source,
            target,
            kind,
            instructions,
        )
    }

    pub fn set_relation_with_id(
        &self,
        relation_id: RelationId,
        workflow_id: WorkflowId,
        source: AgentInstanceId,
        target: AgentInstanceId,
        kind: impl Into<String>,
        instructions: impl Into<String>,
    ) -> Result<AgentRelation, StoreError> {
        self.validate_members(workflow_id, &[source, target])?;
        if source == target {
            return Err(StoreError::Invariant(
                "Agent relation endpoints must be distinct".to_string(),
            ));
        }
        let run = self.get_workflow(workflow_id)?;
        let relation = AgentRelation {
            id: relation_id,
            workflow_id,
            source_agent_id: source,
            target_agent_id: target,
            kind: kind.into(),
            instructions: instructions.into(),
            created_at: Utc::now(),
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        insert_indexed_document_tx(
            &transaction,
            "agent_relations",
            &relation.id.to_string(),
            &[workflow_id.to_string()],
            "active",
            relation.created_at,
            &relation,
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            workflow_id,
            WorkflowEventPayload::RelationChanged {
                source_agent_id: source,
                target_agent_id: target,
                kind: relation.kind.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(relation)
    }

    pub fn get_relation(&self, id: RelationId) -> Result<AgentRelation, StoreError> {
        self.query_document_by_id("agent_relations", id.to_string(), "Agent relation")
    }

    pub fn create_task_scope(
        &self,
        workflow_id: WorkflowId,
        parent_id: Option<TaskScopeId>,
        name: impl Into<String>,
        objective: impl Into<String>,
    ) -> Result<TaskScope, StoreError> {
        self.create_task_scope_with_id(TaskScopeId::new(), workflow_id, parent_id, name, objective)
    }

    pub fn create_task_scope_with_id(
        &self,
        scope_id: TaskScopeId,
        workflow_id: WorkflowId,
        parent_id: Option<TaskScopeId>,
        name: impl Into<String>,
        objective: impl Into<String>,
    ) -> Result<TaskScope, StoreError> {
        if let Some(parent_id) = parent_id {
            let parent = self.get_task_scope(parent_id)?;
            if parent.workflow_id != workflow_id {
                return Err(StoreError::Invariant(
                    "parent scope belongs to another Workflow".to_string(),
                ));
            }
        }
        let run = self.get_workflow(workflow_id)?;
        let now = Utc::now();
        let scope = TaskScope {
            id: scope_id,
            workflow_id,
            parent_id,
            name: name.into(),
            objective: objective.into(),
            status: TaskScopeStatus::Open,
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        insert_indexed_document_tx(
            &transaction,
            "task_scopes",
            &scope.id.to_string(),
            &[workflow_id.to_string()],
            scope.status,
            now,
            &scope,
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            workflow_id,
            WorkflowEventPayload::TaskScopeChanged {
                task_scope_id: scope.id,
                status: "open".to_string(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(scope)
    }

    pub fn get_task_scope(&self, id: TaskScopeId) -> Result<TaskScope, StoreError> {
        self.query_document_by_id("task_scopes", id.to_string(), "task scope")
    }

    pub fn list_task_scopes(&self, workflow_id: WorkflowId) -> Result<Vec<TaskScope>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM task_scopes WHERE workflow_id = ?1 ORDER BY created_at ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn set_task_scope_status(
        &self,
        id: TaskScopeId,
        status: TaskScopeStatus,
    ) -> Result<TaskScope, StoreError> {
        let mut scope = self.get_task_scope(id)?;
        if scope.status == status {
            return Ok(scope);
        }
        scope.status = status;
        scope.updated_at = Utc::now();
        let run = self.get_workflow(scope.workflow_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        update_status_document_tx(&transaction, "task_scopes", &id.to_string(), status, &scope)?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            scope.workflow_id,
            WorkflowEventPayload::TaskScopeChanged {
                task_scope_id: id,
                status: enum_string(status)?,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(scope)
    }

    pub fn create_timer(
        &self,
        workflow_id: WorkflowId,
        name: impl Into<String>,
        interval_ms: u64,
        policy: TimerPolicy,
    ) -> Result<WorkflowTimer, StoreError> {
        self.create_timer_with_id(TimerId::new(), workflow_id, name, interval_ms, policy)
    }

    pub fn create_timer_with_id(
        &self,
        timer_id: TimerId,
        workflow_id: WorkflowId,
        name: impl Into<String>,
        interval_ms: u64,
        policy: TimerPolicy,
    ) -> Result<WorkflowTimer, StoreError> {
        if interval_ms == 0 {
            return Err(StoreError::Invariant(
                "timer interval must be positive".to_string(),
            ));
        }
        let run = self.get_workflow(workflow_id)?;
        let now = Utc::now();
        let interval = i64::try_from(interval_ms).unwrap_or(i64::MAX);
        let timer = WorkflowTimer {
            id: timer_id,
            workflow_id,
            name: name.into(),
            interval_ms,
            policy,
            status: TimerStatus::Active,
            fire_count: 0,
            next_fire_at: now + Duration::milliseconds(interval),
            last_fired_at: None,
            last_fire_effect_key: None,
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        insert_indexed_document_tx(
            &transaction,
            "workflow_timers",
            &timer.id.to_string(),
            &[workflow_id.to_string()],
            timer.status,
            now,
            &timer,
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            workflow_id,
            WorkflowEventPayload::TimerChanged {
                timer_id: timer.id,
                status: enum_string(timer.status)?,
                fire_count: timer.fire_count,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(timer)
    }

    pub fn get_timer(&self, id: TimerId) -> Result<WorkflowTimer, StoreError> {
        self.query_document_by_id("workflow_timers", id.to_string(), "workflow timer")
    }

    pub fn list_timers(&self, workflow_id: WorkflowId) -> Result<Vec<WorkflowTimer>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_timers WHERE workflow_id = ?1 ORDER BY created_at ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn fire_timer(&self, id: TimerId) -> Result<WorkflowTimer, StoreError> {
        self.fire_timer_inner(id, None)
    }

    pub fn fire_timer_for_effect(
        &self,
        id: TimerId,
        effect_key: &str,
    ) -> Result<WorkflowTimer, StoreError> {
        self.fire_timer_inner(id, Some(effect_key))
    }

    fn fire_timer_inner(
        &self,
        id: TimerId,
        effect_key: Option<&str>,
    ) -> Result<WorkflowTimer, StoreError> {
        let mut timer = self.get_timer(id)?;
        if effect_key.is_some_and(|key| timer.last_fire_effect_key.as_deref() == Some(key)) {
            return Ok(timer);
        }
        if timer.status != TimerStatus::Active {
            return Err(StoreError::Invariant("timer is not active".to_string()));
        }
        let now = Utc::now();
        timer.fire_count = timer.fire_count.saturating_add(1);
        timer.last_fired_at = Some(now);
        timer.next_fire_at =
            now + Duration::milliseconds(i64::try_from(timer.interval_ms).unwrap_or(i64::MAX));
        timer.last_fire_effect_key = effect_key.map(str::to_string);
        timer.updated_at = now;
        let mut run = self.get_workflow(timer.workflow_id)?;
        run.usage.timer_fires = run.usage.timer_fires.saturating_add(1);
        run.updated_at = now;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        update_status_document_tx(
            &transaction,
            "workflow_timers",
            &id.to_string(),
            timer.status,
            &timer,
        )?;
        update_workflow_tx(&transaction, &run)?;
        let timer_event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::TimerChanged {
                timer_id: timer.id,
                status: enum_string(timer.status)?,
                fire_count: timer.fire_count,
            },
        )?;
        let usage_event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::UsageUpdated {
                usage: run.usage.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(timer_event);
        self.shared.publish_workflow(usage_event);
        Ok(timer)
    }

    pub fn set_timer_status(
        &self,
        id: TimerId,
        status: TimerStatus,
    ) -> Result<WorkflowTimer, StoreError> {
        let mut timer = self.get_timer(id)?;
        if timer.status == status {
            return Ok(timer);
        }
        timer.status = status;
        timer.updated_at = Utc::now();
        let run = self.get_workflow(timer.workflow_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        update_status_document_tx(
            &transaction,
            "workflow_timers",
            &id.to_string(),
            status,
            &timer,
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::TimerChanged {
                timer_id: timer.id,
                status: enum_string(timer.status)?,
                fire_count: timer.fire_count,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(timer)
    }

    pub fn create_channel(
        &self,
        workflow_id: WorkflowId,
        name: impl Into<String>,
        schema: Value,
    ) -> Result<WorkflowChannel, StoreError> {
        self.create_channel_with_id(ChannelId::new(), workflow_id, name, schema)
    }

    pub fn create_channel_with_id(
        &self,
        channel_id: ChannelId,
        workflow_id: WorkflowId,
        name: impl Into<String>,
        schema: Value,
    ) -> Result<WorkflowChannel, StoreError> {
        let run = self.get_workflow(workflow_id)?;
        let channel = WorkflowChannel {
            id: channel_id,
            workflow_id,
            name: name.into(),
            schema,
            created_at: Utc::now(),
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        insert_indexed_document_tx(
            &transaction,
            "workflow_channels",
            &channel.id.to_string(),
            &[workflow_id.to_string()],
            "active",
            channel.created_at,
            &channel,
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            workflow_id,
            WorkflowEventPayload::ChannelCreated {
                channel_id: channel.id,
                name: channel.name.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(channel)
    }

    pub fn get_channel(&self, id: ChannelId) -> Result<WorkflowChannel, StoreError> {
        self.query_document_by_id("workflow_channels", id.to_string(), "workflow channel")
    }

    pub fn list_channels(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<Vec<WorkflowChannel>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_channels WHERE workflow_id = ?1 ORDER BY created_at ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn publish_signal(
        &self,
        channel_id: ChannelId,
        sender_agent_id: Option<AgentInstanceId>,
        value: Value,
    ) -> Result<WorkflowSignal, StoreError> {
        self.publish_signal_with_id(SignalId::new(), channel_id, sender_agent_id, value)
    }

    pub fn publish_signal_with_id(
        &self,
        signal_id: SignalId,
        channel_id: ChannelId,
        sender_agent_id: Option<AgentInstanceId>,
        value: Value,
    ) -> Result<WorkflowSignal, StoreError> {
        let channel = self.get_channel(channel_id)?;
        if let Some(sender) = sender_agent_id {
            self.validate_members(channel.workflow_id, &[sender])?;
        }
        let project_id = self.get_workflow(channel.workflow_id)?.project_id;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let sequence = transaction.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_signals WHERE channel_id = ?1",
            [channel_id.to_string()],
            |row| row.get::<_, u64>(0),
        )?;
        let signal = WorkflowSignal {
            id: signal_id,
            workflow_id: channel.workflow_id,
            channel_id,
            sender_agent_id,
            sequence,
            value,
            created_at: Utc::now(),
        };
        transaction.execute(
            "INSERT INTO workflow_signals
             (id, workflow_id, channel_id, sequence, created_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                signal.id.to_string(),
                signal.workflow_id.to_string(),
                channel_id.to_string(),
                sequence,
                signal.created_at.to_rfc3339(),
                serde_json::to_string(&signal)?
            ],
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            project_id,
            channel.workflow_id,
            WorkflowEventPayload::SignalPublished {
                channel_id,
                signal_id: signal.id,
                signal_sequence: sequence,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(signal)
    }

    pub fn get_signal(&self, id: SignalId) -> Result<WorkflowSignal, StoreError> {
        self.query_document_by_id("workflow_signals", id.to_string(), "Workflow signal")
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

    pub fn create_human_request_with_id(
        &self,
        request_id: HumanRequestId,
        workflow_id: WorkflowId,
        session_id: SessionId,
        question: impl Into<String>,
        response_schema: Value,
    ) -> Result<HumanRequest, StoreError> {
        let mut run = self.get_workflow(workflow_id)?;
        if run.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot request human input for a terminal Workflow".to_string(),
            ));
        }
        let now = Utc::now();
        let request = HumanRequest {
            id: request_id,
            workflow_id,
            session_id,
            question: question.into(),
            response_schema,
            status: HumanRequestStatus::Open,
            answer: None,
            created_at: now,
            resolved_at: None,
        };
        run.attention_required = true;
        let previous_status = run.status;
        if run.status != WorkflowStatus::Paused {
            run.status = WorkflowStatus::WaitingForUser;
        }
        run.updated_at = now;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        transaction.execute(
            "INSERT INTO human_requests
             (id, workflow_id, session_id, status, created_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                request.id.to_string(),
                workflow_id.to_string(),
                session_id.to_string(),
                enum_string(request.status)?,
                now.to_rfc3339(),
                serde_json::to_string(&request)?
            ],
        )?;
        update_workflow_tx(&transaction, &run)?;
        let run_event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            workflow_id,
            WorkflowEventPayload::HumanRequestOpened {
                human_request_id: request.id,
                session_id,
                question: request.question.clone(),
            },
        )?;
        let status_event = (run.status != previous_status)
            .then(|| {
                append_workflow_event_tx(
                    &transaction,
                    run.project_id,
                    workflow_id,
                    WorkflowEventPayload::WorkflowStatusChanged {
                        status: run.status,
                        reason: None,
                    },
                )
            })
            .transpose()?;
        let session_event = append_session_event_tx(
            &transaction,
            session_id,
            None,
            None,
            SessionEventPayload::HumanRequestOpened {
                workflow_id,
                human_request_id: request.id,
                question: request.question.clone(),
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(run_event);
        if let Some(event) = status_event {
            self.shared.publish_workflow(event);
        }
        self.shared.publish_session(session_event);
        Ok(request)
    }

    pub fn get_human_request(&self, id: HumanRequestId) -> Result<HumanRequest, StoreError> {
        self.query_document_by_id("human_requests", id.to_string(), "human request")
    }

    pub fn list_human_requests(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<Vec<HumanRequest>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM human_requests WHERE workflow_id = ?1 ORDER BY created_at ASC",
            [workflow_id.to_string()],
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
        let mut run = self.get_workflow(request.workflow_id)?;
        let previous_status = run.status;
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
            "SELECT EXISTS(SELECT 1 FROM human_requests WHERE workflow_id = ?1
             AND status = 'open' AND id != ?2)",
            params![run.id.to_string(), id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        run.attention_required = remaining;
        if !remaining && run.status == WorkflowStatus::WaitingForUser {
            run.status = WorkflowStatus::Running;
        }
        run.updated_at = Utc::now();
        update_workflow_tx(&transaction, &run)?;
        let run_event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::HumanRequestResolved {
                human_request_id: id,
            },
        )?;
        let status_event = (run.status != previous_status)
            .then(|| {
                append_workflow_event_tx(
                    &transaction,
                    run.project_id,
                    run.id,
                    WorkflowEventPayload::WorkflowStatusChanged {
                        status: run.status,
                        reason: None,
                    },
                )
            })
            .transpose()?;
        let session_event = append_session_event_tx(
            &transaction,
            request.session_id,
            None,
            None,
            SessionEventPayload::HumanRequestResolved {
                workflow_id: run.id,
                human_request_id: id,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(run_event);
        if let Some(event) = status_event {
            self.shared.publish_workflow(event);
        }
        self.shared.publish_session(session_event);
        Ok(request)
    }

    pub fn create_control_message(
        &self,
        workflow_id: WorkflowId,
        session_id: SessionId,
        invocation_id: Option<ActionInvocationId>,
        kind: ControlMessageKind,
        content: impl Into<String>,
    ) -> Result<ControlMessage, StoreError> {
        let run = self.get_workflow(workflow_id)?;
        if run.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot control a terminal Workflow".to_string(),
            ));
        }
        let message = ControlMessage {
            id: ControlMessageId::new(),
            workflow_id,
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
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        transaction.execute(
            "INSERT INTO control_messages
             (id, workflow_id, session_id, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                message.id.to_string(),
                workflow_id.to_string(),
                session_id.to_string(),
                enum_string(message.status)?,
                message.created_at.to_rfc3339(),
                serde_json::to_string(&message)?,
            ],
        )?;
        let event = append_workflow_event_tx(
            &transaction,
            run.project_id,
            run.id,
            WorkflowEventPayload::ControlMessageQueued {
                control_message_id: message.id,
                session_id,
                kind,
            },
        )?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event);
        Ok(message)
    }

    pub fn take_control_messages(
        &self,
        workflow_id: WorkflowId,
        session_id: SessionId,
        invocation_id: Option<ActionInvocationId>,
    ) -> Result<Vec<ControlMessage>, StoreError> {
        let mut messages: Vec<ControlMessage> = self.query_documents(
            "SELECT document_json FROM control_messages
             WHERE workflow_id = ?1 AND session_id = ?2 AND status = 'pending'
             ORDER BY created_at ASC",
            params![workflow_id.to_string(), session_id.to_string()],
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
            self.append_workflow_event(
                workflow_id,
                WorkflowEventPayload::ControlMessageApplied {
                    control_message_id: message.id,
                },
            )?;
            self.append_session_event(
                session_id,
                None,
                None,
                SessionEventPayload::ControlMessageApplied {
                    workflow_id,
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
        workflow_id: WorkflowId,
    ) -> Result<Vec<ControlMessage>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM control_messages WHERE workflow_id = ?1 ORDER BY created_at ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn append_workflow_event(
        &self,
        workflow_id: WorkflowId,
        payload: WorkflowEventPayload,
    ) -> Result<WorkflowEvent, StoreError> {
        let run = self.get_workflow(workflow_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let event = append_workflow_event_tx(&transaction, run.project_id, workflow_id, payload)?;
        transaction.commit()?;
        drop(connection);
        self.shared.publish_workflow(event.clone());
        Ok(event)
    }

    pub fn list_workflow_events(
        &self,
        workflow_id: WorkflowId,
        after_sequence: u64,
    ) -> Result<Vec<WorkflowEvent>, StoreError> {
        self.query_documents(
            "SELECT event_json FROM workflow_events WHERE workflow_id = ?1 AND sequence > ?2
             ORDER BY sequence ASC",
            params![workflow_id.to_string(), after_sequence],
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_artifact(
        &self,
        project_id: ProjectId,
        workflow_id: WorkflowId,
        session_id: Option<SessionId>,
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
            workflow_id,
            session_id,
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
        workflow_id: WorkflowId,
        session_id: Option<SessionId>,
        action_invocation_id: Option<ActionInvocationId>,
        kind: ArtifactKind,
        name: impl Into<String>,
        media_type: impl Into<String>,
        metadata: Value,
        bytes: &[u8],
    ) -> Result<Artifact, StoreError> {
        let run = self.get_workflow(workflow_id)?;
        if run.project_id != project_id {
            return Err(StoreError::Invariant(
                "artifact Project does not match Workflow".to_string(),
            ));
        }
        let name = name.into();
        let stored = store_artifact_file(
            &self.shared.artifact_root,
            workflow_id,
            session_id,
            id,
            &name,
            bytes,
        )?;
        let artifact = Artifact {
            id,
            project_id,
            workflow_id,
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
            &[workflow_id.to_string()],
            "created",
            artifact.created_at,
            &artifact,
        )?;
        Ok(artifact)
    }

    pub fn list_artifacts(&self, workflow_id: WorkflowId) -> Result<Vec<Artifact>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM artifacts WHERE workflow_id = ?1 ORDER BY created_at ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn list_project_artifacts(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Artifact>, StoreError> {
        self.query_documents(
            "SELECT a.document_json FROM artifacts a JOIN workflows wr ON wr.id = a.workflow_id
             WHERE wr.project_id = ?1 ORDER BY a.created_at DESC",
            [project_id.to_string()],
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

    fn validate_members(
        &self,
        workflow_id: WorkflowId,
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
            if participant.workflow_id != workflow_id
                || participant.status != ParticipantStatus::Active
            {
                return Err(StoreError::Invariant(format!(
                    "Agent {id} is not active in this Workflow"
                )));
            }
        }
        Ok(())
    }

    pub fn session_rollout_path(&self, session_id: SessionId) -> PathBuf {
        crate::rollout::path(&self.shared.rollout_root, session_id)
    }

    pub fn list_session_rollout_records(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionRolloutRecord>, StoreError> {
        let session_lock = self.shared.session_rollout_lock(session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        crate::rollout::read(&self.shared.rollout_root, session_id)
    }

    pub fn reconstruct_session_rollout(
        &self,
        session_id: SessionId,
    ) -> Result<SessionRolloutState, StoreError> {
        let records = self.list_session_rollout_records(session_id)?;
        crate::rollout::reconstruct(&records)
    }

    pub fn session_rollout_status(
        &self,
        session_id: SessionId,
    ) -> Result<SessionRolloutStatus, StoreError> {
        self.get_session(session_id)?;
        let session_lock = self.shared.session_rollout_lock(session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(session_id)?;
        let records = crate::rollout::read(&self.shared.rollout_root, session_id)?;
        let last_sequence = records.last().map_or(0, |record| record.sequence);
        let projected_sequence = self
            .connection()?
            .query_row(
                "SELECT last_sequence FROM session_rollout_projection WHERE session_id = ?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(SessionRolloutStatus {
            version: SESSION_ROLLOUT_VERSION,
            last_sequence,
            projected_sequence,
        })
    }

    fn commit_session_rollout_item(
        &self,
        session_id: SessionId,
        item: SessionRolloutItem,
    ) -> Result<SessionRolloutRecord, StoreError> {
        let session_lock = self.shared.session_rollout_lock(session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.commit_session_rollout_item_locked(session_id, item)
    }

    fn commit_session_rollout_item_locked(
        &self,
        session_id: SessionId,
        mut item: SessionRolloutItem,
    ) -> Result<SessionRolloutRecord, StoreError> {
        self.replay_session_rollout_locked(session_id)?;

        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        assign_rollout_event_sequences_tx(&transaction, session_id, &mut item)?;
        let last_sequence = self
            .shared
            .rollout_sequences
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .get(&session_id)
            .copied()
            .unwrap_or(0);
        let sequence = last_sequence.checked_add(1).ok_or_else(|| {
            StoreError::Invariant("Session rollout sequence overflow".to_string())
        })?;
        let record = SessionRolloutRecord {
            version: SESSION_ROLLOUT_VERSION,
            session_id,
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
            .insert(session_id, sequence);
        crate::process_fault::reach_process_fault_boundary(
            crate::process_fault::ROLLOUT_APPENDED_BEFORE_PROJECTION,
        );
        let projection = (|| -> Result<(), StoreError> {
            apply_rollout_record_tx(&transaction, &record)?;
            set_rollout_projection_tx(&transaction, session_id, sequence)?;
            transaction.commit()?;
            Ok(())
        })();
        drop(connection);
        if let Err(initial_error) = projection
            && let Err(replay_error) = self.replay_session_rollout_locked(session_id)
        {
            return Err(StoreError::Invariant(format!(
                "Session rollout is durable but projection failed ({initial_error}) and immediate replay failed ({replay_error})"
            )));
        }
        for event in rollout_events(&record.item) {
            self.shared.publish_session(event.clone());
        }
        Ok(record)
    }

    fn replay_all_session_rollouts(&self) -> Result<(), StoreError> {
        let mut session_ids = Vec::new();
        for entry in std::fs::read_dir(&*self.shared.rollout_root)
            .map_err(|error| StoreError::Io(error.to_string()))?
        {
            let entry = entry.map_err(|error| StoreError::Io(error.to_string()))?;
            let metadata = entry
                .metadata()
                .map_err(|error| StoreError::Io(error.to_string()))?;
            if !metadata.is_file() {
                return Err(StoreError::Invariant(format!(
                    "unexpected entry in Session rollout directory: {}",
                    entry.path().display()
                )));
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                return Err(StoreError::Invariant(format!(
                    "unexpected entry in Session rollout directory: {}",
                    path.display()
                )));
            }
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    StoreError::Invariant(format!(
                        "invalid Session rollout path: {}",
                        path.display()
                    ))
                })?;
            session_ids.push(
                SessionId::from_str(stem)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?,
            );
        }
        session_ids.sort_unstable();
        for session_id in session_ids {
            let session_lock = self.shared.session_rollout_lock(session_id)?;
            let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
            self.replay_session_rollout_locked(session_id)?;
        }
        Ok(())
    }

    fn replay_session_rollout_locked(&self, session_id: SessionId) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let projected = connection
            .query_row(
                "SELECT last_sequence FROM session_rollout_projection WHERE session_id = ?1",
                [session_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let cached = self
            .shared
            .rollout_sequences
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .get(&session_id)
            .copied();
        if cached == Some(projected) {
            return Ok(());
        }
        let records = crate::rollout::read(&self.shared.rollout_root, session_id)?;
        let last_sequence = records.last().map_or(0, |record| record.sequence);
        if projected > last_sequence {
            return Err(StoreError::Invariant(format!(
                "Session {session_id} projection sequence {projected} is ahead of rollout {last_sequence}"
            )));
        }
        if projected < last_sequence {
            let transaction = connection.transaction()?;
            for record in records.iter().filter(|record| record.sequence > projected) {
                apply_rollout_record_tx(&transaction, record)?;
            }
            set_rollout_projection_tx(&transaction, session_id, last_sequence)?;
            transaction.commit()?;
        }
        self.shared
            .rollout_sequences
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?
            .insert(session_id, last_sequence);
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
            1 => "id, workflow_id, status, updated_at, document_json",
            2 => "id, workflow_id, session_id, status, updated_at, document_json",
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
    for root in &workspace.roots {
        let attached = Path::new(root);
        let metadata = std::fs::symlink_metadata(attached).map_err(|error| {
            StoreError::Invariant(format!("Workspace root is unavailable: {root}: {error}"))
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(StoreError::Invariant(format!(
                "Workspace root is not a real directory: {root}"
            )));
        }
        let canonical = attached.canonicalize().map_err(|error| {
            StoreError::Invariant(format!(
                "Workspace root cannot be resolved: {root}: {error}"
            ))
        })?;
        if canonical != attached {
            return Err(StoreError::Invariant(format!(
                "Workspace root no longer matches its attached canonical path: {root}"
            )));
        }
    }
    Ok(())
}

fn create_managed_root(root: &Path) -> Result<PathBuf, StoreError> {
    std::fs::create_dir_all(root).map_err(|error| StoreError::Io(error.to_string()))?;
    for directory in [
        "prompts",
        "rollouts",
        "workflows",
        "skills",
        "sources",
        "state",
        "artifacts",
        "workflow-runtime",
        "runtime/sandboxes",
        "runtime/temp",
        "runtime/skill-snapshots",
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
        "workflows",
        "skills",
        "sources",
        "state",
        "artifacts",
        "workflow-runtime",
        "runtime/sandboxes",
        "runtime/temp",
        "runtime/skill-snapshots",
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
            "Workflow instructions exceed the {MAX_SYSTEM_PROMPT_BYTES} byte limit"
        )));
    }
    Ok(())
}

fn reject_prompt_symlink(path: &Path) -> Result<(), StoreError> {
    if std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(StoreError::Invariant(format!(
            "Project system prompt may not be a symlink: {}",
            path.display()
        )));
    }
    Ok(())
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<(), StoreError> {
    let parent = path.parent().ok_or_else(|| {
        StoreError::Invariant(format!("path has no parent directory: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("prompt"),
        uuid::Uuid::now_v7()
    ));
    std::fs::write(&temporary, content).map_err(|error| StoreError::Io(error.to_string()))?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(StoreError::Io(error.to_string()))
        }
    }
}

fn hash_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

fn workflow_effect_hash(kind: &str, payload: &Value) -> Result<String, StoreError> {
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
           origin TEXT NOT NULL, status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS sessions_project_updated ON sessions(project_id, updated_at DESC);
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
         CREATE TABLE IF NOT EXISTS session_rollout_projection (
           session_id TEXT PRIMARY KEY REFERENCES sessions(id), last_sequence INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflow_programs (
           owner_key TEXT NOT NULL, id TEXT NOT NULL, slug TEXT NOT NULL, source TEXT NOT NULL,
           definition_path TEXT NOT NULL, sha256 TEXT NOT NULL, updated_at TEXT NOT NULL,
           document_json TEXT NOT NULL, PRIMARY KEY(owner_key, slug)
         );
         CREATE TABLE IF NOT EXISTS workflows (
           id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
           started_from_session_id TEXT REFERENCES sessions(id), status TEXT NOT NULL,
           attention_required INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS workflows_project_updated ON workflows(project_id, updated_at DESC);
         CREATE INDEX IF NOT EXISTS workflows_starting_session_updated ON workflows(started_from_session_id, updated_at DESC);
         CREATE TABLE IF NOT EXISTS workflow_effects (
           workflow_id TEXT NOT NULL REFERENCES workflows(id), effect_key TEXT NOT NULL,
           status TEXT NOT NULL, started_at TEXT NOT NULL, document_json TEXT NOT NULL,
           PRIMARY KEY(workflow_id, effect_key)
         );
         CREATE INDEX IF NOT EXISTS workflow_effects_started ON workflow_effects(workflow_id, started_at ASC);
         CREATE TABLE IF NOT EXISTS workflow_events (
           id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workflow_id TEXT NOT NULL REFERENCES workflows(id),
           sequence INTEGER NOT NULL, occurred_at TEXT NOT NULL, event_json TEXT NOT NULL,
           UNIQUE(workflow_id, sequence)
         );
         CREATE INDEX IF NOT EXISTS workflow_events_sequence ON workflow_events(workflow_id, sequence ASC);
         CREATE TABLE IF NOT EXISTS workflow_participants (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL, UNIQUE(workflow_id, session_id)
         );
         CREATE TABLE IF NOT EXISTS action_invocations (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS action_attempts (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id),
           invocation_id TEXT NOT NULL REFERENCES action_invocations(id), number INTEGER NOT NULL,
           status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL,
           UNIQUE(invocation_id, number)
         );
         CREATE TABLE IF NOT EXISTS workflow_teams (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS agent_relations (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS task_scopes (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflow_timers (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflow_channels (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS workflow_signals (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id),
           channel_id TEXT NOT NULL REFERENCES workflow_channels(id), sequence INTEGER NOT NULL,
           created_at TEXT NOT NULL, document_json TEXT NOT NULL, UNIQUE(channel_id, sequence)
         );
         CREATE TABLE IF NOT EXISTS human_requests (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT NOT NULL, updated_at TEXT GENERATED ALWAYS AS (COALESCE(json_extract(document_json, '$.resolved_at'), created_at)) VIRTUAL,
           document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS control_messages (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS artifacts (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id), status TEXT NOT NULL,
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );"
    ))?;
    reconcile_terminal_workflow_resources(connection)?;
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
    reconcile_terminal_workflow_resources(connection)?;
    Ok(())
}

fn ensure_workflow_accepts_effect_tx(
    transaction: &Transaction<'_>,
    workflow_id: WorkflowId,
) -> Result<(), StoreError> {
    let status = transaction.query_row(
        "SELECT status FROM workflows WHERE id = ?1",
        [workflow_id.to_string()],
        |row| row.get::<_, String>(0),
    )?;
    if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
        return Err(StoreError::Invariant(format!(
            "Workflow is terminal with status {status}"
        )));
    }
    Ok(())
}

fn terminalize_workflow_resources_tx(
    transaction: &Transaction<'_>,
    workflow_id: WorkflowId,
    workflow_status: WorkflowStatus,
    now: chrono::DateTime<Utc>,
) -> Result<(), StoreError> {
    let now = now.to_rfc3339();
    transaction.execute(
        "UPDATE control_messages
         SET status = 'cancelled', updated_at = ?2,
             document_json = json_set(document_json, '$.status', 'cancelled')
         WHERE workflow_id = ?1 AND status = 'pending'",
        params![workflow_id.to_string(), now],
    )?;
    transaction.execute(
        "UPDATE human_requests
         SET status = 'cancelled',
             document_json = json_set(
                 json_set(document_json, '$.status', 'cancelled'),
                 '$.resolved_at', ?2
             )
         WHERE workflow_id = ?1 AND status = 'open'",
        params![workflow_id.to_string(), now],
    )?;
    let timer_status = if workflow_status == WorkflowStatus::Completed {
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
         WHERE workflow_id = ?1 AND status IN ('active', 'paused')",
        params![workflow_id.to_string(), timer_status, now],
    )?;
    Ok(())
}

fn reconcile_terminal_workflow_resources(connection: &Connection) -> Result<(), StoreError> {
    let workflow_ids = {
        let mut statement = connection.prepare(
            "SELECT id, status FROM workflows
             WHERE status IN ('completed', 'failed', 'cancelled')",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (id, status) in workflow_ids {
        let workflow_id =
            WorkflowId::from_str(&id).map_err(|error| StoreError::Invariant(error.to_string()))?;
        let workflow_status = match status.as_str() {
            "completed" => WorkflowStatus::Completed,
            "failed" => WorkflowStatus::Failed,
            "cancelled" => WorkflowStatus::Cancelled,
            _ => continue,
        };
        let transaction = connection.unchecked_transaction()?;
        terminalize_workflow_resources_tx(&transaction, workflow_id, workflow_status, Utc::now())?;
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

fn assign_rollout_event_sequences_tx(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    item: &mut SessionRolloutItem,
) -> Result<(), StoreError> {
    let mut sequence = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM session_events WHERE session_id = ?1",
        [session_id.to_string()],
        |row| row.get::<_, u64>(0),
    )?;
    for event in rollout_events_mut(item) {
        if event.session_id != session_id {
            return Err(StoreError::Invariant(format!(
                "Session rollout event belongs to {}, expected {session_id}",
                event.session_id
            )));
        }
        event.sequence = sequence;
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| StoreError::Invariant("Session event sequence overflow".to_string()))?;
    }
    Ok(())
}

fn rollout_events(item: &SessionRolloutItem) -> Vec<&SessionEvent> {
    match item {
        SessionRolloutItem::TurnCreated { events, .. }
        | SessionRolloutItem::TurnUpdated { events, .. } => events.iter().collect(),
        SessionRolloutItem::SessionEventAppended { event } => vec![event],
        SessionRolloutItem::ContextCheckpoint { .. }
        | SessionRolloutItem::StepsCreated { .. }
        | SessionRolloutItem::StepsUpdated { .. } => Vec::new(),
    }
}

fn rollout_events_mut(item: &mut SessionRolloutItem) -> Vec<&mut SessionEvent> {
    match item {
        SessionRolloutItem::TurnCreated { events, .. }
        | SessionRolloutItem::TurnUpdated { events, .. } => events.iter_mut().collect(),
        SessionRolloutItem::SessionEventAppended { event } => vec![event],
        SessionRolloutItem::ContextCheckpoint { .. }
        | SessionRolloutItem::StepsCreated { .. }
        | SessionRolloutItem::StepsUpdated { .. } => Vec::new(),
    }
}

fn apply_rollout_record_tx(
    transaction: &Transaction<'_>,
    record: &SessionRolloutRecord,
) -> Result<(), StoreError> {
    match &record.item {
        SessionRolloutItem::TurnCreated {
            turn,
            action_attempt,
            events,
        } => {
            transaction.execute(
                "INSERT INTO turns (id, session_id, status, updated_at, document_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    turn.id.to_string(),
                    turn.session_id.to_string(),
                    enum_string(turn.status)?,
                    turn.updated_at.to_rfc3339(),
                    serde_json::to_string(turn)?,
                ],
            )?;
            if let Some(attempt) = action_attempt {
                update_status_document_tx(
                    transaction,
                    "action_attempts",
                    &attempt.id.to_string(),
                    attempt.status,
                    attempt,
                )?;
            }
            for event in events {
                insert_session_event_exact_tx(transaction, event)?;
            }
        }
        SessionRolloutItem::ContextCheckpoint {
            turn_id,
            usage,
            completed_model_steps,
            hosted_search_calls_used,
            checkpoint_message,
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
        }
        SessionRolloutItem::TurnUpdated {
            turn,
            session,
            events,
        } => {
            update_status_document_tx(
                transaction,
                "turns",
                &turn.id.to_string(),
                turn.status,
                turn,
            )?;
            update_status_document_tx(
                transaction,
                "sessions",
                &session.id.to_string(),
                session.status,
                session,
            )?;
            for event in events {
                insert_session_event_exact_tx(transaction, event)?;
            }
        }
        SessionRolloutItem::StepsCreated { steps } => {
            for step in steps {
                transaction.execute(
                    "INSERT INTO steps
                     (id, turn_id, sequence, status, updated_at, document_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        step.id.to_string(),
                        step.turn_id.to_string(),
                        step.sequence,
                        enum_string(step.status)?,
                        step.updated_at.to_rfc3339(),
                        serde_json::to_string(step)?,
                    ],
                )?;
            }
        }
        SessionRolloutItem::StepsUpdated { steps } => {
            for step in steps {
                update_status_document_tx(
                    transaction,
                    "steps",
                    &step.id.to_string(),
                    step.status,
                    step,
                )?;
            }
        }
        SessionRolloutItem::SessionEventAppended { event } => {
            insert_session_event_exact_tx(transaction, event)?;
        }
    }
    Ok(())
}

fn insert_session_event_exact_tx(
    transaction: &Transaction<'_>,
    event: &SessionEvent,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO session_events
         (id, session_id, turn_id, step_id, sequence, occurred_at, event_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            event.id.to_string(),
            event.session_id.to_string(),
            event.turn_id.map(|id| id.to_string()),
            event.step_id.map(|id| id.to_string()),
            event.sequence,
            event.occurred_at.to_rfc3339(),
            serde_json::to_string(event)?,
        ],
    )?;
    Ok(())
}

fn set_rollout_projection_tx(
    transaction: &Transaction<'_>,
    session_id: SessionId,
    sequence: u64,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO session_rollout_projection (session_id, last_sequence)
         VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET last_sequence = excluded.last_sequence",
        params![session_id.to_string(), sequence],
    )?;
    Ok(())
}

fn pending_session_event(
    session_id: SessionId,
    turn_id: Option<TurnId>,
    step_id: Option<StepId>,
    payload: SessionEventPayload,
) -> SessionEvent {
    SessionEvent {
        id: EventId::new(),
        sequence: 0,
        session_id,
        turn_id,
        step_id,
        occurred_at: Utc::now(),
        payload,
    }
}

fn is_stable_rollout_event(payload: &SessionEventPayload) -> bool {
    !matches!(
        payload,
        SessionEventPayload::AssistantMessageDelta { .. }
            | SessionEventPayload::AssistantMessageReset
    )
}

fn append_workflow_event_tx(
    transaction: &Transaction<'_>,
    project_id: ProjectId,
    workflow_id: WorkflowId,
    payload: WorkflowEventPayload,
) -> Result<WorkflowEvent, StoreError> {
    let sequence = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = ?1",
        [workflow_id.to_string()],
        |row| row.get::<_, u64>(0),
    )?;
    let event = WorkflowEvent {
        id: EventId::new(),
        sequence,
        project_id,
        workflow_id,
        occurred_at: Utc::now(),
        payload,
    };
    transaction.execute(
        "INSERT INTO workflow_events
         (id, project_id, workflow_id, sequence, occurred_at, event_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            event.id.to_string(),
            project_id.to_string(),
            workflow_id.to_string(),
            sequence,
            event.occurred_at.to_rfc3339(),
            serde_json::to_string(&event)?
        ],
    )?;
    Ok(event)
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
        1 => "id, workflow_id, status, updated_at, document_json",
        2 => "id, workflow_id, session_id, status, updated_at, document_json",
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

fn update_workflow_tx(transaction: &Transaction<'_>, run: &Workflow) -> Result<(), StoreError> {
    let changed = transaction.execute(
        "UPDATE workflows SET status = ?1, attention_required = ?2, updated_at = ?3,
         document_json = ?4 WHERE id = ?5",
        params![
            enum_string(run.status)?,
            run.attention_required,
            run.updated_at.to_rfc3339(),
            serde_json::to_string(run)?,
            run.id.to_string()
        ],
    )?;
    ensure_one(changed, "workflows")
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
) -> WorkflowEventPayload {
    WorkflowEventPayload::ActionChanged {
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
