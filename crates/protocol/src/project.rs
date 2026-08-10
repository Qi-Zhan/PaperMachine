use crate::ActionInvocationId;
use crate::AgentId;
use crate::ArtifactId;
use crate::ProjectId;
use crate::SessionId;
use crate::WorkspaceId;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkspaceAttachment {
    pub id: WorkspaceId,
    /// Monotonically increases whenever the user changes the attached path.
    pub revision: u64,
    /// Canonical absolute user-owned filesystem directory.
    pub path: String,
}

impl WorkspaceAttachment {
    pub fn single(path: impl Into<String>) -> Self {
        Self {
            id: WorkspaceId::new(),
            revision: 1,
            path: path.into(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.revision == 0 {
            return Err("Workspace revision must be positive".to_string());
        }
        if self.path.trim().is_empty() {
            return Err("Workspace path must not be empty".to_string());
        }
        if !Path::new(&self.path).is_absolute() {
            return Err("Workspace path must be absolute".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkspaceSelection {
    pub path: String,
}

impl WorkspaceSelection {
    pub fn single(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// User-owned filesystem attached to this managed Project. PaperMachine
    /// runtime state is stored separately and is never placed in this path.
    pub workspace: WorkspaceAttachment,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Paper,
    Source,
    Code,
    Dataset,
    Experiment,
    Log,
    Figure,
    Report,
    Metric,
    Other,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Artifact {
    pub id: ArtifactId,
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub action_invocation_id: Option<ActionInvocationId>,
    pub kind: ArtifactKind,
    pub name: String,
    pub media_type: String,
    pub relative_path: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub metadata: Value,
    pub created_at: DateTime<Utc>,
}

/// Canonical Project home revision stored in PaperMachine-managed state.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProjectHome {
    pub project_id: ProjectId,
    pub artifact_id: ArtifactId,
    pub source_artifact_id: ArtifactId,
    pub revision: String,
    pub updated_at: DateTime<Utc>,
}
