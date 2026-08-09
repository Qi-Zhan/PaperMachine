use crate::NewWorkflow;
use crate::StoreError;
use crate::StoreShared;
use crate::TurnContextCheckpoint;
use crate::artifact::read_artifact_file;
use crate::artifact::reconcile_artifact_files;
use crate::artifact::remove_artifact_file;
use crate::artifact::store_artifact_file;
use crate::filesystem::ManagedFs;
use crate::filesystem::remove_entry;
use crate::filesystem::write_atomic;
use chrono::Duration;
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

const SCHEMA_VERSION: u32 = 16;
const PROJECT_SYSTEM_PROMPT_PATH: &str = "prompts/system.md";
const MAX_SYSTEM_PROMPT_BYTES: usize = 256 * 1024;
const MAX_PROJECT_CHANGES_PER_READ: usize = 10_001;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectChange {
    pub sequence: u64,
    pub kind: String,
    pub entity_id: String,
    pub workflow_id: Option<WorkflowId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectChangeBatch {
    pub captured_cursor: u64,
    pub changes: Vec<ProjectChange>,
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
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO sessions (id, project_id, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.id.to_string(),
                session.project_id.to_string(),
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
            SessionEventPayload::SessionCreated,
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

    pub fn list_recent_sessions(
        &self,
        project_id: ProjectId,
        limit: usize,
    ) -> Result<Vec<Session>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM sessions WHERE project_id = ?1
             AND status != 'archived'
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

    pub fn project_changes_after(
        &self,
        project_id: ProjectId,
        after_cursor: Option<u64>,
    ) -> Result<ProjectChangeBatch, StoreError> {
        self.get_project(project_id)?;
        let connection = self.connection()?;
        let captured_cursor = connection.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM project_changes WHERE project_id = ?1",
            [project_id.to_string()],
            |row| row.get::<_, u64>(0),
        )?;
        let Some(after_cursor) = after_cursor else {
            return Ok(ProjectChangeBatch {
                captured_cursor,
                changes: Vec::new(),
            });
        };
        if after_cursor > captured_cursor {
            return Err(StoreError::Invariant(format!(
                "Project snapshot cursor {after_cursor} is ahead of current cursor {captured_cursor}"
            )));
        }
        let mut statement = connection.prepare(
            "SELECT sequence, entity_kind, entity_id, workflow_id
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
            let (sequence, kind, entity_id, workflow_id) = row?;
            changes.push(ProjectChange {
                sequence,
                kind,
                entity_id,
                workflow_id: workflow_id
                    .map(|id| WorkflowId::from_str(&id))
                    .transpose()
                    .map_err(|error| StoreError::Invariant(error.to_string()))?,
            });
        }
        Ok(ProjectChangeBatch {
            captured_cursor,
            changes,
        })
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
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
    pub fn create_turn_for_attempt(
        &self,
        attempt_id: ActionAttemptId,
        session_id: SessionId,
        origin: TurnOrigin,
        input: impl Into<String>,
        model_route: ModelRouteSnapshot,
        prompt: PromptSnapshot,
        tools_enabled: bool,
        expected_access: AccessPreset,
        tool_set: papermachine_protocol::ToolSetSnapshot,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        skill_snapshots: Vec<SkillSnapshot>,
    ) -> Result<Turn, StoreError> {
        self.create_turn_inner(
            attempt_id,
            session_id,
            origin,
            input,
            model_route,
            prompt,
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
        attempt_id: ActionAttemptId,
        session_id: SessionId,
        origin: TurnOrigin,
        input: impl Into<String>,
        model_route: ModelRouteSnapshot,
        prompt: PromptSnapshot,
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
        model_route.validate().map_err(StoreError::Invariant)?;
        if session.status == SessionStatus::Archived {
            return Err(StoreError::Invariant(
                "cannot add a Turn to an archived Session".to_string(),
            ));
        }
        let mut attempt = self.get_action_attempt(attempt_id)?;
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
            input,
            output: None,
            model_route,
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
        attempt.turn_id = Some(turn.id);
        attempt.updated_at = now;
        let event = pending_session_event(
            session_id,
            Some(turn.id),
            None,
            SessionEventPayload::TurnCreated,
        );
        self.commit_session_rollout_item_locked(
            session_id,
            SessionRolloutItem::TurnCreated {
                turn: turn.clone(),
                action_attempt: Some(attempt),
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

    pub fn interrupt_turn(
        &self,
        id: TurnId,
        reason: impl Into<String>,
    ) -> Result<Turn, StoreError> {
        self.interrupt_turn_with_controls(id, reason, &[])
    }

    pub fn interrupt_turn_with_controls(
        &self,
        id: TurnId,
        reason: impl Into<String>,
        control_message_ids: &[ControlMessageId],
    ) -> Result<Turn, StoreError> {
        self.transition_turn(
            id,
            &[TurnStatus::Running, TurnStatus::Paused],
            TurnStatus::Interrupted,
            Some(reason.into()),
            control_message_ids.to_vec(),
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
        acknowledged_control_ids: Vec<ControlMessageId>,
    ) -> Result<Turn, StoreError> {
        let existing = self.get_turn(id)?;
        let session_lock = self.shared.session_rollout_lock(existing.session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(existing.session_id)?;
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
        self.persist_turn_status_locked(turn, status, error, acknowledged_control_ids)
    }

    pub fn checkpoint_turn_context(
        &self,
        id: TurnId,
        checkpoint: TurnContextCheckpoint,
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
                mutation: checkpoint.mutation,
                usage: checkpoint.usage,
                completed_model_steps: checkpoint.completed_model_steps,
                hosted_search_calls_used: checkpoint.hosted_search_calls_used,
                checkpoint_message: checkpoint.checkpoint_message,
                acknowledged_control_ids: checkpoint.acknowledged_control_ids,
            },
        )?;
        self.get_turn(id)
    }

    fn persist_turn_status_locked(
        &self,
        mut turn: Turn,
        status: TurnStatus,
        error: Option<String>,
        acknowledged_control_ids: Vec<ControlMessageId>,
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
                acknowledged_control_ids,
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
        let turn = self.get_turn(turn_id)?;
        let session_lock = self.shared.session_rollout_lock(turn.session_id)?;
        let _guard = session_lock.lock().map_err(|_| StoreError::LockPoisoned)?;
        self.replay_session_rollout_locked(turn.session_id)?;
        let name = name.into();
        if let Some(call_id) = tool_call_id.as_ref() {
            let existing = self
                .connection()?
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
        self.commit_session_rollout_item_locked(
            turn.session_id,
            SessionRolloutItem::StepsCreated {
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

    pub fn list_session_steps(&self, session_id: SessionId) -> Result<Vec<AgentStep>, StoreError> {
        self.query_documents(
            "SELECT steps.document_json FROM steps
             INNER JOIN turns ON turns.id = steps.turn_id
             WHERE turns.session_id = ?1
             ORDER BY turns.created_at ASC, turns.id ASC, steps.sequence ASC",
            [session_id.to_string()],
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
        if is_transient_session_event(&payload) {
            return Err(StoreError::Invariant(
                "transient Session events must be published without persistence".to_string(),
            ));
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

    pub fn publish_transient_session_event(
        &self,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        step_id: Option<StepId>,
        payload: SessionEventPayload,
    ) -> Result<SessionEvent, StoreError> {
        self.get_session(session_id)?;
        if !is_transient_session_event(&payload) {
            return Err(StoreError::Invariant(
                "durable Session events must be appended to the rollout".to_string(),
            ));
        }
        let event = pending_session_event(session_id, turn_id, step_id, payload);
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
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO workflows
             (id, project_id, started_from_session_id, program_slug, status, attention_required, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
            params![
                workflow.id.to_string(),
                workflow.project_id.to_string(),
                workflow.started_from_session_id.map(|id| id.to_string()),
                &workflow.program.manifest.slug,
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

    pub fn cleanup_terminal_workflow_state(&self, id: WorkflowId) -> Result<(), StoreError> {
        let workflow = self.get_workflow(id)?;
        if !workflow.status.is_terminal() {
            return Err(StoreError::Invariant(format!(
                "cannot clean runtime state for active Workflow {id}"
            )));
        }
        let draft = self
            .managed_root
            .join("runtime/project-home-drafts")
            .join(id.to_string());
        if draft.exists() || std::fs::symlink_metadata(&draft).is_ok() {
            remove_entry(&draft)?;
        }
        Ok(())
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

    pub fn workflow_involves_session(
        &self,
        workflow_id: WorkflowId,
        session_id: SessionId,
    ) -> Result<bool, StoreError> {
        Ok(self.connection()?.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM workflows
               WHERE id = ?1 AND started_from_session_id = ?2
               UNION ALL
               SELECT 1 FROM workflow_participants
               WHERE workflow_id = ?1 AND session_id = ?2
             )",
            params![workflow_id.to_string(), session_id.to_string()],
            |row| row.get(0),
        )?)
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

    pub fn latest_project_workflow_for_program(
        &self,
        project_id: ProjectId,
        program_slug: &str,
    ) -> Result<Option<Workflow>, StoreError> {
        self.connection()?
            .query_row(
                "SELECT document_json FROM workflows
                 WHERE project_id = ?1 AND program_slug = ?2
                 ORDER BY updated_at DESC, id ASC LIMIT 1",
                params![project_id.to_string(), program_slug],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|document| serde_json::from_str(&document).map_err(StoreError::from))
            .transpose()
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

    pub fn start_workflow(&self, id: WorkflowId) -> Result<Workflow, StoreError> {
        self.transition_workflow(
            id,
            &[WorkflowStatus::Created, WorkflowStatus::Running],
            WorkflowStatus::Running,
            None,
        )
    }

    pub fn pause_workflow(
        &self,
        id: WorkflowId,
        reason: Option<String>,
    ) -> Result<Workflow, StoreError> {
        self.transition_workflow(
            id,
            &[
                WorkflowStatus::Running,
                WorkflowStatus::WaitingForUser,
                WorkflowStatus::WaitingForTimer,
                WorkflowStatus::WaitingForSignal,
                WorkflowStatus::Paused,
            ],
            WorkflowStatus::Paused,
            reason,
        )
    }

    pub fn resume_workflow(&self, id: WorkflowId) -> Result<Workflow, StoreError> {
        self.transition_workflow(
            id,
            &[
                WorkflowStatus::Paused,
                WorkflowStatus::WaitingForUser,
                WorkflowStatus::WaitingForTimer,
                WorkflowStatus::WaitingForSignal,
                WorkflowStatus::Running,
            ],
            WorkflowStatus::Running,
            None,
        )
    }

    pub fn wait_workflow_for_user(&self, id: WorkflowId) -> Result<Workflow, StoreError> {
        self.transition_workflow(
            id,
            &[WorkflowStatus::Running, WorkflowStatus::WaitingForUser],
            WorkflowStatus::WaitingForUser,
            None,
        )
    }

    pub fn wait_workflow_for_timer(&self, id: WorkflowId) -> Result<Workflow, StoreError> {
        self.transition_workflow(
            id,
            &[WorkflowStatus::Running, WorkflowStatus::WaitingForTimer],
            WorkflowStatus::WaitingForTimer,
            None,
        )
    }

    pub fn wait_workflow_for_signal(&self, id: WorkflowId) -> Result<Workflow, StoreError> {
        self.transition_workflow(
            id,
            &[WorkflowStatus::Running, WorkflowStatus::WaitingForSignal],
            WorkflowStatus::WaitingForSignal,
            None,
        )
    }

    pub fn fail_workflow(
        &self,
        id: WorkflowId,
        error: impl Into<String>,
    ) -> Result<Workflow, StoreError> {
        self.transition_workflow(
            id,
            &[
                WorkflowStatus::Created,
                WorkflowStatus::Running,
                WorkflowStatus::WaitingForUser,
                WorkflowStatus::WaitingForTimer,
                WorkflowStatus::WaitingForSignal,
                WorkflowStatus::Paused,
            ],
            WorkflowStatus::Failed,
            Some(error.into()),
        )
    }

    pub fn cancel_workflow(
        &self,
        id: WorkflowId,
        reason: impl Into<String>,
    ) -> Result<Workflow, StoreError> {
        self.transition_workflow(
            id,
            &[
                WorkflowStatus::Created,
                WorkflowStatus::Running,
                WorkflowStatus::WaitingForUser,
                WorkflowStatus::WaitingForTimer,
                WorkflowStatus::WaitingForSignal,
                WorkflowStatus::Paused,
            ],
            WorkflowStatus::Cancelled,
            Some(reason.into()),
        )
    }

    fn transition_workflow(
        &self,
        id: WorkflowId,
        allowed_from: &[WorkflowStatus],
        status: WorkflowStatus,
        reason: Option<String>,
    ) -> Result<Workflow, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut run: Workflow =
            load_document_tx(&transaction, "workflows", &id.to_string(), "workflow")?;
        if run.status == status {
            return Ok(run);
        }
        if !allowed_from.contains(&run.status) {
            return Err(StoreError::Invariant(format!(
                "cannot change Workflow {id} from {:?} to {status:?}",
                run.status
            )));
        }
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
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut run: Workflow =
            load_document_tx(&transaction, "workflows", &id.to_string(), "workflow")?;
        if run.status != WorkflowStatus::Running {
            return Err(StoreError::Invariant(format!(
                "cannot complete Workflow {id} from {:?}",
                run.status
            )));
        }
        run.status = WorkflowStatus::Completed;
        run.output = Some(output.clone());
        run.error = None;
        run.attention_required = false;
        run.updated_at = Utc::now();
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
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let document = transaction.query_row(
            "SELECT document_json FROM workflows WHERE id = ?1",
            [id.to_string()],
            |row| row.get::<_, String>(0),
        )?;
        let mut run: Workflow = serde_json::from_str(&document)?;
        apply_workflow_usage_delta(&mut run.usage, delta);
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
        let system_prompt = system_prompt.into();
        validate_system_prompt(&system_prompt)?;
        let now = Utc::now();
        let name = name.into();
        let role = role.into();
        let requested_model = model.into();
        let class_name = class_name.into();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &workflow_id.to_string(),
            "workflow",
        )?;
        if run.status != WorkflowStatus::Running {
            return Err(StoreError::Invariant(
                "cannot add an Agent unless its Workflow is running".to_string(),
            ));
        }
        let model = {
            if requested_model.trim().is_empty() {
                run.default_model.clone()
            } else {
                requested_model
            }
        };
        let session = Session {
            id: session_id,
            project_id: run.project_id,
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
            class_name,
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
        transaction.execute(
            "INSERT INTO sessions (id, project_id, status, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.id.to_string(),
                session.project_id.to_string(),
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
            SessionEventPayload::SessionCreated,
        )?;
        let attached = append_session_event_tx(
            &transaction,
            session.id,
            None,
            None,
            SessionEventPayload::WorkflowAgentAttached {
                workflow_id: run.id,
                agent_instance_id: participant.id,
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

    pub fn list_session_participants(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<WorkflowParticipant>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_participants WHERE session_id = ?1
             ORDER BY created_at ASC, id ASC",
            [session_id.to_string()],
        )
    }

    pub fn list_workflow_sessions(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<Vec<Session>, StoreError> {
        self.query_documents(
            "SELECT sessions.document_json FROM sessions
             INNER JOIN workflow_participants
               ON workflow_participants.session_id = sessions.id
             WHERE workflow_participants.workflow_id = ?1
             ORDER BY workflow_participants.created_at ASC, sessions.id ASC",
            [workflow_id.to_string()],
        )
    }

    pub fn retire_participant(
        &self,
        id: AgentInstanceId,
    ) -> Result<WorkflowParticipant, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut participant: WorkflowParticipant = load_document_tx(
            &transaction,
            "workflow_participants",
            &id.to_string(),
            "workflow participant",
        )?;
        if participant.status == ParticipantStatus::Retired {
            return Ok(participant);
        }
        participant.status = ParticipantStatus::Retired;
        participant.updated_at = Utc::now();
        let run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &participant.workflow_id.to_string(),
            "workflow",
        )?;
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
        let action_name = action_name.into();
        let contract = contract.into();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let participant: WorkflowParticipant = load_document_tx(
            &transaction,
            "workflow_participants",
            &agent_id.to_string(),
            "workflow participant",
        )?;
        if participant.workflow_id != workflow_id || participant.status != ParticipantStatus::Active
        {
            return Err(StoreError::Invariant(
                "action Agent is not active in this Workflow".to_string(),
            ));
        }
        let run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &workflow_id.to_string(),
            "workflow",
        )?;
        if run.status != WorkflowStatus::Running {
            return Err(StoreError::Invariant(
                "cannot schedule an Action unless its Workflow is running".to_string(),
            ));
        }
        let now = Utc::now();
        let invocation = ActionInvocation {
            id: invocation_id,
            workflow_id,
            task_scope_id: scope_id,
            agent_instance_id: agent_id,
            session_id: participant.session_id,
            action_name,
            contract,
            arguments,
            requested_tools,
            source_human_request_id,
            status: ActionStatus::Scheduled,
            output: None,
            error: None,
            created_at: now,
            updated_at: now,
        };
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
        let mut run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &invocation.workflow_id.to_string(),
            "workflow",
        )?;
        if run.status != WorkflowStatus::Running {
            return Err(StoreError::Invariant("Workflow is not running".to_string()));
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

    pub fn list_workflow_action_attempts(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<Vec<ActionAttempt>, StoreError> {
        self.query_documents(
            "SELECT action_attempts.document_json FROM action_attempts
             INNER JOIN action_invocations
               ON action_invocations.id = action_attempts.invocation_id
             WHERE action_invocations.workflow_id = ?1
             ORDER BY action_invocations.created_at ASC,
                      action_invocations.id ASC,
                      action_attempts.number ASC",
            [workflow_id.to_string()],
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
        let mut run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &invocation.workflow_id.to_string(),
            "workflow",
        )?;
        if status == ActionStatus::Completed {
            run.usage.actions_completed = run.usage.actions_completed.saturating_add(1);
            run.updated_at = now;
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
    ) -> Result<WorkflowTimer, StoreError> {
        self.create_timer_with_id(TimerId::new(), workflow_id, name, interval_ms)
    }

    pub fn create_timer_with_id(
        &self,
        timer_id: TimerId,
        workflow_id: WorkflowId,
        name: impl Into<String>,
        interval_ms: u64,
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
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut timer: WorkflowTimer = load_document_tx(
            &transaction,
            "workflow_timers",
            &id.to_string(),
            "workflow timer",
        )?;
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
        let mut run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &timer.workflow_id.to_string(),
            "workflow",
        )?;
        if run.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot fire a timer for a terminal Workflow".to_string(),
            ));
        }
        run.usage.timer_fires = run.usage.timer_fires.saturating_add(1);
        run.updated_at = now;
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

    pub fn list_workflow_signals(
        &self,
        workflow_id: WorkflowId,
    ) -> Result<Vec<WorkflowSignal>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM workflow_signals WHERE workflow_id = ?1
             ORDER BY created_at ASC, id ASC",
            [workflow_id.to_string()],
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
        let question = question.into();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &workflow_id.to_string(),
            "workflow",
        )?;
        if !matches!(run.status, WorkflowStatus::Running | WorkflowStatus::Paused) {
            return Err(StoreError::Invariant(
                "cannot request human input unless its Workflow is running or paused".to_string(),
            ));
        }
        let now = Utc::now();
        let request = HumanRequest {
            id: request_id,
            workflow_id,
            session_id,
            question,
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

    pub fn list_session_human_requests(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<HumanRequest>, StoreError> {
        self.query_documents(
            "SELECT document_json FROM human_requests WHERE session_id = ?1
             ORDER BY created_at ASC, id ASC",
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
        let mut run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &request.workflow_id.to_string(),
            "workflow",
        )?;
        let previous_status = run.status;
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
        let content = content.into();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run: Workflow = load_document_tx(
            &transaction,
            "workflows",
            &workflow_id.to_string(),
            "workflow",
        )?;
        if run.status.is_terminal() {
            return Err(StoreError::Invariant(
                "cannot control a terminal Workflow".to_string(),
            ));
        }
        let session: Session =
            load_document_tx(&transaction, "sessions", &session_id.to_string(), "session")?;
        if session.project_id != run.project_id {
            return Err(StoreError::Invariant(
                "control Session does not belong to the Workflow Project".to_string(),
            ));
        }
        if let Some(invocation_id) = invocation_id {
            let invocation: ActionInvocation = load_document_tx(
                &transaction,
                "action_invocations",
                &invocation_id.to_string(),
                "action invocation",
            )?;
            if invocation.workflow_id != workflow_id || invocation.session_id != session_id {
                return Err(StoreError::Invariant(
                    "control Action does not belong to the target Workflow Session".to_string(),
                ));
            }
        }
        let message = ControlMessage {
            id: ControlMessageId::new(),
            workflow_id,
            session_id,
            action_invocation_id: invocation_id,
            kind,
            content,
            status: ControlMessageStatus::Pending,
            created_at: Utc::now(),
            claimed_turn_id: None,
            claimed_at: None,
            applied_at: None,
        };
        transaction.execute(
            "INSERT INTO control_messages
             (id, workflow_id, session_id, status, claimed_turn_id, updated_at, document_json)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
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

    pub fn claim_control_messages(
        &self,
        workflow_id: WorkflowId,
        session_id: SessionId,
        invocation_id: Option<ActionInvocationId>,
        turn_id: TurnId,
    ) -> Result<Vec<ControlMessage>, StoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_workflow_accepts_effect_tx(&transaction, workflow_id)?;
        let turn: Turn = load_document_tx(&transaction, "turns", &turn_id.to_string(), "turn")?;
        if turn.session_id != session_id || turn.status.is_terminal() {
            return Err(StoreError::Invariant(
                "control messages can only be claimed by their active target Turn".to_string(),
            ));
        }
        let mut statement = transaction.prepare(
            "SELECT document_json FROM control_messages
             WHERE workflow_id = ?1 AND session_id = ?2
               AND (status = 'pending' OR (status = 'claimed' AND claimed_turn_id = ?3))
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = statement.query_map(
            params![
                workflow_id.to_string(),
                session_id.to_string(),
                turn_id.to_string()
            ],
            |row| row.get::<_, String>(0),
        )?;
        let documents = rows.collect::<Result<Vec<String>, _>>()?;
        drop(statement);
        let mut messages = documents
            .into_iter()
            .map(|document| serde_json::from_str(&document))
            .collect::<Result<Vec<ControlMessage>, _>>()?;
        messages.retain(|message| {
            message.action_invocation_id.is_none() || message.action_invocation_id == invocation_id
        });
        let claimed_at = Utc::now();
        for message in &mut messages {
            if message.status == ControlMessageStatus::Pending {
                message.status = ControlMessageStatus::Claimed;
                message.claimed_turn_id = Some(turn_id);
                message.claimed_at = Some(claimed_at);
                let changed = transaction.execute(
                    "UPDATE control_messages
                     SET status = 'claimed', claimed_turn_id = ?1, updated_at = ?2, document_json = ?3
                     WHERE id = ?4 AND status = 'pending'",
                    params![
                        turn_id.to_string(),
                        claimed_at.to_rfc3339(),
                        serde_json::to_string(message)?,
                        message.id.to_string(),
                    ],
                )?;
                ensure_one(changed, "control_messages")?;
            }
        }
        transaction.commit()?;
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
        let created_file = stored.created;
        let inserted = self.insert_indexed_document(
            "artifacts",
            &id.to_string(),
            &[workflow_id.to_string()],
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
            || source.workflow_id != page.workflow_id
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
            &[source.workflow_id.to_string()],
            "created",
            source.created_at,
            source,
        )?;
        insert_indexed_document_tx(
            &transaction,
            "artifacts",
            &page.id.to_string(),
            &[page.workflow_id.to_string()],
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
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_rollout_item_tx(&transaction, &item)?;
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
        let projection = (|| -> Result<ProjectionEvents, StoreError> {
            let events = apply_rollout_record_tx(&transaction, &record)?;
            set_rollout_projection_tx(&transaction, session_id, sequence)?;
            transaction.commit()?;
            Ok(events)
        })();
        drop(connection);
        let projection_events = match projection {
            Ok(events) => events,
            Err(initial_error) => {
                if let Err(replay_error) = self.replay_session_rollout_locked(session_id) {
                    return Err(StoreError::Invariant(format!(
                        "Session rollout is durable but projection failed ({initial_error}) and immediate replay failed ({replay_error})"
                    )));
                }
                ProjectionEvents::default()
            }
        };
        for event in rollout_events(&record.item) {
            self.shared.publish_session(event.clone());
        }
        for event in projection_events.workflow {
            self.shared.publish_workflow(event);
        }
        for event in projection_events.session {
            self.shared.publish_session(event);
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
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        "workflows",
        "skills",
        "sources",
        "state",
        "artifacts",
        "workflow-runtime",
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
        "workflows",
        "skills",
        "sources",
        "state",
        "artifacts",
        "workflow-runtime",
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
            "Workflow instructions exceed the {MAX_SYSTEM_PROMPT_BYTES} byte limit"
        )));
    }
    Ok(())
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
           status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
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
           tool_call_id TEXT, status TEXT NOT NULL, updated_at TEXT NOT NULL, document_json TEXT NOT NULL,
           UNIQUE(turn_id, sequence), UNIQUE(turn_id, tool_call_id)
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
         CREATE TABLE IF NOT EXISTS workflows (
           id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
           started_from_session_id TEXT REFERENCES sessions(id), program_slug TEXT NOT NULL,
           status TEXT NOT NULL,
           attention_required INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS workflows_project_updated ON workflows(project_id, updated_at DESC);
         CREATE INDEX IF NOT EXISTS workflows_project_program_updated ON workflows(project_id, program_slug, updated_at DESC);
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
         CREATE INDEX IF NOT EXISTS workflow_participants_session ON workflow_participants(session_id, created_at ASC);
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
         CREATE INDEX IF NOT EXISTS human_requests_session ON human_requests(session_id, created_at ASC);
         CREATE TABLE IF NOT EXISTS control_messages (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id),
           session_id TEXT NOT NULL REFERENCES sessions(id), status TEXT NOT NULL,
           claimed_turn_id TEXT REFERENCES turns(id),
           created_at TEXT GENERATED ALWAYS AS (json_extract(document_json, '$.created_at')) VIRTUAL,
           updated_at TEXT NOT NULL, document_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS control_messages_claim
           ON control_messages(workflow_id, session_id, status, claimed_turn_id, created_at ASC);
         CREATE TABLE IF NOT EXISTS artifacts (
           id TEXT PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflows(id), status TEXT NOT NULL,
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
           workflow_id TEXT
         );
         CREATE INDEX IF NOT EXISTS project_changes_project_sequence
           ON project_changes(project_id, sequence);

         CREATE TRIGGER IF NOT EXISTS project_change_project_insert
         AFTER INSERT ON projects BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           VALUES (NEW.id, 'project', NEW.id, NULL);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_project_update
         AFTER UPDATE ON projects BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           VALUES (NEW.id, 'project', NEW.id, NULL);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_session_insert
         AFTER INSERT ON sessions BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           VALUES (NEW.project_id, 'session', NEW.id, NULL);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_session_update
         AFTER UPDATE ON sessions BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           VALUES (NEW.project_id, 'session', NEW.id, NULL);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_turn_insert
         AFTER INSERT ON turns BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           SELECT project_id, 'session', NEW.session_id, NULL
           FROM sessions WHERE id = NEW.session_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_turn_update
         AFTER UPDATE ON turns BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           SELECT project_id, 'session', NEW.session_id, NULL
           FROM sessions WHERE id = NEW.session_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_workflow_insert
         AFTER INSERT ON workflows BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           VALUES (NEW.project_id, 'workflow', NEW.id, NEW.id);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_workflow_update
         AFTER UPDATE ON workflows BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           VALUES (NEW.project_id, 'workflow', NEW.id, NEW.id);
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_artifact_insert
         AFTER INSERT ON artifacts BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           SELECT project_id, 'artifact', NEW.id, NEW.workflow_id
           FROM workflows WHERE id = NEW.workflow_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_home_insert
         AFTER INSERT ON project_homes BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           SELECT NEW.project_id, 'project_home', NEW.project_id, workflow_id
           FROM artifacts WHERE id = NEW.artifact_id;
         END;
         CREATE TRIGGER IF NOT EXISTS project_change_home_update
         AFTER UPDATE ON project_homes BEGIN
           INSERT INTO project_changes(project_id, entity_kind, entity_id, workflow_id)
           SELECT NEW.project_id, 'project_home', NEW.project_id, workflow_id
           FROM artifacts WHERE id = NEW.artifact_id;
         END;"
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

fn message_workflow_project_id_tx(
    transaction: &Transaction<'_>,
    workflow_id: WorkflowId,
) -> Result<ProjectId, StoreError> {
    let workflow: Workflow = load_document_tx(
        transaction,
        "workflows",
        &workflow_id.to_string(),
        "workflow",
    )?;
    Ok(workflow.project_id)
}

fn apply_workflow_usage_delta(usage: &mut WorkflowUsage, delta: WorkflowUsage) {
    usage.agents_created = usage.agents_created.saturating_add(delta.agents_created);
    usage.actions_started = usage.actions_started.saturating_add(delta.actions_started);
    usage.actions_completed = usage
        .actions_completed
        .saturating_add(delta.actions_completed);
    usage.action_steps = usage.action_steps.saturating_add(delta.action_steps);
    usage.timer_fires = usage.timer_fires.saturating_add(delta.timer_fires);
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
         WHERE workflow_id = ?1 AND status IN ('pending', 'claimed')",
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

#[derive(Default)]
struct ProjectionEvents {
    workflow: Vec<WorkflowEvent>,
    session: Vec<SessionEvent>,
}

fn validate_rollout_item_tx(
    transaction: &Transaction<'_>,
    item: &SessionRolloutItem,
) -> Result<(), StoreError> {
    let (turn_id, acknowledged_control_ids) = match item {
        SessionRolloutItem::ContextCheckpoint {
            turn_id,
            acknowledged_control_ids,
            ..
        } => (*turn_id, acknowledged_control_ids),
        SessionRolloutItem::TurnUpdated {
            turn,
            acknowledged_control_ids,
            ..
        } => (turn.id, acknowledged_control_ids),
        _ => return Ok(()),
    };
    let mut seen = std::collections::HashSet::new();
    for control_id in acknowledged_control_ids {
        if !seen.insert(*control_id) {
            return Err(StoreError::Invariant(format!(
                "duplicate acknowledged control message {control_id}"
            )));
        }
        let message: ControlMessage = load_document_tx(
            transaction,
            "control_messages",
            &control_id.to_string(),
            "control message",
        )?;
        if message.status != ControlMessageStatus::Claimed
            || message.claimed_turn_id != Some(turn_id)
        {
            return Err(StoreError::Invariant(format!(
                "control message {control_id} is not claimed by Turn {turn_id}"
            )));
        }
    }
    Ok(())
}

fn apply_control_acknowledgements_tx(
    transaction: &Transaction<'_>,
    turn_id: TurnId,
    control_ids: &[ControlMessageId],
    occurred_at: chrono::DateTime<Utc>,
) -> Result<ProjectionEvents, StoreError> {
    let mut events = ProjectionEvents::default();
    for control_id in control_ids {
        let mut message: ControlMessage = load_document_tx(
            transaction,
            "control_messages",
            &control_id.to_string(),
            "control message",
        )?;
        if message.status == ControlMessageStatus::Applied {
            continue;
        }
        if message.status != ControlMessageStatus::Claimed
            || message.claimed_turn_id != Some(turn_id)
        {
            return Err(StoreError::Invariant(format!(
                "control message {control_id} is not claimed by Turn {turn_id}"
            )));
        }
        message.status = ControlMessageStatus::Applied;
        message.applied_at = Some(occurred_at);
        let changed = transaction.execute(
            "UPDATE control_messages
             SET status = 'applied', updated_at = ?1, document_json = ?2
             WHERE id = ?3 AND status = 'claimed' AND claimed_turn_id = ?4",
            params![
                occurred_at.to_rfc3339(),
                serde_json::to_string(&message)?,
                control_id.to_string(),
                turn_id.to_string(),
            ],
        )?;
        ensure_one(changed, "control_messages")?;
        events.workflow.push(append_workflow_event_tx(
            transaction,
            message_workflow_project_id_tx(transaction, message.workflow_id)?,
            message.workflow_id,
            WorkflowEventPayload::ControlMessageApplied {
                control_message_id: *control_id,
            },
        )?);
        events.session.push(append_session_event_tx(
            transaction,
            message.session_id,
            Some(turn_id),
            None,
            SessionEventPayload::ControlMessageApplied {
                workflow_id: message.workflow_id,
                control_message_id: *control_id,
                kind: message.kind,
            },
        )?);
    }
    Ok(events)
}

fn apply_rollout_record_tx(
    transaction: &Transaction<'_>,
    record: &SessionRolloutRecord,
) -> Result<ProjectionEvents, StoreError> {
    let mut projected_events = ProjectionEvents::default();
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
            acknowledged_control_ids,
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
                let workflow_id = transaction
                    .query_row(
                        "SELECT workflow_id FROM action_attempts
                         WHERE json_extract(document_json, '$.turn_id') = ?1",
                        [turn_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        StoreError::Invariant(format!(
                            "Turn {turn_id} has no owning Workflow ActionAttempt"
                        ))
                    })?;
                let workflow_id = WorkflowId::from_str(&workflow_id)
                    .map_err(|error| StoreError::Invariant(error.to_string()))?;
                let mut workflow: Workflow = load_document_tx(
                    transaction,
                    "workflows",
                    &workflow_id.to_string(),
                    "workflow",
                )?;
                apply_workflow_usage_delta(
                    &mut workflow.usage,
                    WorkflowUsage {
                        tokens: token_delta,
                        hosted_search_calls: hosted_search_delta,
                        ..WorkflowUsage::default()
                    },
                );
                workflow.updated_at = record.occurred_at;
                update_workflow_tx(transaction, &workflow)?;
                projected_events.workflow.push(append_workflow_event_tx(
                    transaction,
                    workflow.project_id,
                    workflow.id,
                    WorkflowEventPayload::UsageUpdated {
                        usage: workflow.usage,
                    },
                )?);
            }
            let control_events = apply_control_acknowledgements_tx(
                transaction,
                *turn_id,
                acknowledged_control_ids,
                record.occurred_at,
            )?;
            projected_events.workflow.extend(control_events.workflow);
            projected_events.session.extend(control_events.session);
        }
        SessionRolloutItem::TurnUpdated {
            turn,
            session,
            events,
            acknowledged_control_ids,
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
            let control_events = apply_control_acknowledgements_tx(
                transaction,
                turn.id,
                acknowledged_control_ids,
                record.occurred_at,
            )?;
            projected_events.workflow.extend(control_events.workflow);
            projected_events.session.extend(control_events.session);
            if turn.status.is_terminal() {
                transaction.execute(
                    "UPDATE control_messages
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
        SessionRolloutItem::StepsCreated { steps } => {
            for step in steps {
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
    Ok(projected_events)
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

fn is_transient_session_event(payload: &SessionEventPayload) -> bool {
    matches!(
        payload,
        SessionEventPayload::AssistantMessageDelta { .. }
            | SessionEventPayload::AssistantMessageReset
            | SessionEventPayload::ModelStepStarted
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
