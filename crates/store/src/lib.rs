//! Durable metadata, append-only Workflow events, and content-addressed artifacts.

mod artifact;
mod database;

use papermachine_protocol::AgentAccessProfile;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionEvent;
use papermachine_protocol::SessionId;
use papermachine_protocol::WorkflowEvent;
use papermachine_protocol::WorkflowLaunchContext;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowTrigger;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::broadcast;

pub use database::Store;

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
    pub access: AgentAccessProfile,
    pub enabled_skills: Vec<String>,
    pub launch_context: WorkflowLaunchContext,
    pub agent_access_overrides: BTreeMap<String, AgentAccessProfile>,
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
    workflow_events: broadcast::Sender<WorkflowEvent>,
    session_events: broadcast::Sender<SessionEvent>,
}

impl StoreShared {
    fn new(artifact_root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let artifact_root = artifact_root.as_ref().to_path_buf();
        std::fs::create_dir_all(&artifact_root)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let (workflow_events, _) = broadcast::channel(4096);
        let (session_events, _) = broadcast::channel(4096);
        Ok(Self {
            artifact_root: Arc::new(artifact_root),
            workflow_events,
            session_events,
        })
    }

    fn publish_workflow(&self, event: WorkflowEvent) {
        let _ = self.workflow_events.send(event);
    }

    fn publish_session(&self, event: SessionEvent) {
        let _ = self.session_events.send(event);
    }
}
