//! Durable metadata, append-only Workflow events, and content-addressed artifacts.

mod artifact;
mod catalog;
mod database;
#[doc(hidden)]
pub mod process_fault;
mod project_home;
mod rollout;

use papermachine_protocol::AccessPreset;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionEvent;
use papermachine_protocol::SessionId;
use papermachine_protocol::WorkflowEvent;
use papermachine_protocol::WorkflowLaunchContext;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowTrigger;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokio::sync::broadcast;

pub use catalog::CatalogProject;
pub use catalog::ProjectCatalog;
pub use database::Store;
pub use project_home::PROJECT_HOME_MEDIA_TYPE;
pub use project_home::PROJECT_HOME_ROLE;
pub use project_home::PROJECT_HOME_SOURCE_MEDIA_TYPE;
pub use project_home::PROJECT_HOME_SOURCE_ROLE;
pub use project_home::ProjectHomeBlock;
pub use project_home::ProjectHomeDraft;
pub use project_home::ProjectHomePatchOperation;
pub use project_home::ProjectHomeSource;
pub use project_home::PublishedProjectHome;

#[derive(Clone, Debug)]
pub struct NewWorkflow {
    pub project_id: ProjectId,
    pub started_from_session_id: Option<SessionId>,
    pub program: WorkflowProgramSnapshot,
    pub request: String,
    pub instructions: String,
    pub trigger: WorkflowTrigger,
    pub params: Value,
    pub default_model: String,
    pub access: AccessPreset,
    pub enabled_skills: Vec<String>,
    pub launch_context: WorkflowLaunchContext,
    pub agent_access_overrides: BTreeMap<String, AccessPreset>,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("store I/O failed: {0}")]
    Io(String),
    #[error("store serialization failed: {0}")]
    Serialization(String),
    #[error("{entity} not found: {id}")]
    NotFound { entity: &'static str, id: String },
    #[error("store invariant failed: {0}")]
    Invariant(String),
    #[error("store lock poisoned")]
    LockPoisoned,
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value.to_string())
    }
}

#[derive(Clone)]
struct StoreShared {
    artifact_root: Arc<PathBuf>,
    rollout_root: Arc<PathBuf>,
    session_rollout_locks: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
    rollout_sequences: Arc<Mutex<HashMap<SessionId, u64>>>,
    workflow_events: broadcast::Sender<WorkflowEvent>,
    session_events: broadcast::Sender<SessionEvent>,
}

impl StoreShared {
    fn new(managed_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let artifact_root = managed_root.as_ref().join("artifacts");
        let rollout_root = managed_root.as_ref().join("rollouts");
        std::fs::create_dir_all(&artifact_root)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        std::fs::create_dir_all(&rollout_root)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let (workflow_events, _) = broadcast::channel(4096);
        let (session_events, _) = broadcast::channel(4096);
        Ok(Self {
            artifact_root: Arc::new(artifact_root),
            rollout_root: Arc::new(rollout_root),
            session_rollout_locks: Arc::new(Mutex::new(HashMap::new())),
            rollout_sequences: Arc::new(Mutex::new(HashMap::new())),
            workflow_events,
            session_events,
        })
    }

    fn session_rollout_lock(&self, session_id: SessionId) -> Result<Arc<Mutex<()>>, StoreError> {
        let mut locks = self
            .session_rollout_locks
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?;
        Ok(Arc::clone(
            locks
                .entry(session_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        ))
    }

    fn publish_workflow(&self, event: WorkflowEvent) {
        let _ = self.workflow_events.send(event);
    }

    fn publish_session(&self, event: SessionEvent) {
        let _ = self.session_events.send(event);
    }
}
