//! Durable Project, Session, Agent, event, rollout, and Artifact storage.

mod artifact;
mod catalog;
mod database;
mod filesystem;
mod handle;
#[doc(hidden)]
pub mod process_fault;
mod project_changes;
mod project_home;
mod rollout;

use papermachine_protocol::AccessPreset;
use papermachine_protocol::ActionSource;
use papermachine_protocol::AgentId;
use papermachine_protocol::ControlMessageId;
use papermachine_protocol::ModelContextMutation;
use papermachine_protocol::ModelResponseFormat;
use papermachine_protocol::ProjectId;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::SessionEvent;
use papermachine_protocol::SessionId;
use papermachine_protocol::SessionTrigger;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::WebSearchContextSize;
use papermachine_protocol::WorkflowProgramSnapshot;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokio::sync::broadcast;

pub use catalog::CatalogFailure;
pub use catalog::CatalogProject;
pub use catalog::ProjectCatalog;
pub use database::Store;
pub use filesystem::ManagedFs;
pub use handle::STORE_QUEUE_CAPACITY;
pub use handle::StoreHandle;
pub use project_changes::ProjectEntitySnapshot;
pub use project_changes::ProjectSnapshotPage;

pub(crate) use database::ProjectChange;
pub use project_home::PROJECT_HOME_MEDIA_TYPE;
pub use project_home::PROJECT_HOME_ROLE;
pub use project_home::PROJECT_HOME_SOURCE_MEDIA_TYPE;
pub use project_home::PROJECT_HOME_SOURCE_ROLE;
pub use project_home::PublishedProjectHome;

#[derive(Clone, Debug)]
pub struct NewSession {
    pub project_id: ProjectId,
    pub program: WorkflowProgramSnapshot,
    pub title: String,
    pub request: String,
    pub instructions: String,
    pub trigger: SessionTrigger,
    pub params: Value,
    pub default_model: String,
    pub access: AccessPreset,
    pub enabled_skills: Vec<String>,
    pub agent_access_overrides: BTreeMap<String, AccessPreset>,
}

#[derive(Clone, Debug)]
pub struct NewActionInvocation {
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub action_name: String,
    pub contract: String,
    pub arguments: Value,
    pub input: String,
    pub source: ActionSource,
    pub requested_tools: Vec<String>,
    pub tools_enabled: bool,
    pub web_search_context_size: Option<WebSearchContextSize>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub response_format: Option<ModelResponseFormat>,
}

#[derive(Clone, Debug)]
pub struct TurnContextCheckpoint {
    pub mutation: ModelContextMutation,
    pub usage: TokenUsage,
    pub completed_model_steps: u32,
    pub hosted_search_calls_used: u32,
    pub checkpoint_message: Option<String>,
    pub acknowledged_control_ids: Vec<ControlMessageId>,
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
    agent_rollout_locks: Arc<Mutex<HashMap<AgentId, Arc<Mutex<()>>>>>,
    rollout_sequences: Arc<Mutex<HashMap<AgentId, u64>>>,
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
        let (session_events, _) = broadcast::channel(4096);
        Ok(Self {
            artifact_root: Arc::new(artifact_root),
            rollout_root: Arc::new(rollout_root),
            agent_rollout_locks: Arc::new(Mutex::new(HashMap::new())),
            rollout_sequences: Arc::new(Mutex::new(HashMap::new())),
            session_events,
        })
    }

    fn agent_rollout_lock(&self, agent_id: AgentId) -> Result<Arc<Mutex<()>>, StoreError> {
        let mut locks = self
            .agent_rollout_locks
            .lock()
            .map_err(|_| StoreError::LockPoisoned)?;
        Ok(Arc::clone(
            locks
                .entry(agent_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        ))
    }

    fn publish_session(&self, event: SessionEvent) {
        let _ = self.session_events.send(event);
    }
}
