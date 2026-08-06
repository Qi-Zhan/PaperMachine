use crate::ModelInputItem;
use crate::ModelResponseFormat;
use crate::ProjectId;
use crate::PromptSnapshot;
use crate::ReasoningEffort;
use crate::SessionId;
use crate::StepId;
use crate::TokenUsage;
use crate::TurnId;
use crate::WebSearchContextSize;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Ready,
    Running,
    WaitingForHuman,
    Paused,
    Failed,
    Archived,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    User,
    WorkflowAgent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOrigin {
    User,
    Workflow,
}

/// User-facing access profiles. The runtime expands these presets into
/// concrete file, command, and network capabilities at every enforcement
/// boundary.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentAccessProfile {
    ModelOnly,
    ReadOnly,
    Workspace,
    #[default]
    Research,
    FullAccess,
}

impl AgentAccessProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelOnly => "model_only",
            Self::ReadOnly => "read_only",
            Self::Workspace => "workspace",
            Self::Research => "research",
            Self::FullAccess => "full_access",
        }
    }

    pub const fn allows_workspace_read(self) -> bool {
        !matches!(self, Self::ModelOnly)
    }

    pub const fn allows_workspace_write(self) -> bool {
        matches!(self, Self::Workspace | Self::Research | Self::FullAccess)
    }

    pub const fn allows_sandboxed_command(self) -> bool {
        matches!(self, Self::Workspace | Self::Research)
    }

    pub const fn allows_research_network(self) -> bool {
        matches!(self, Self::Research | Self::FullAccess)
    }

    pub const fn is_unrestricted(self) -> bool {
        matches!(self, Self::FullAccess)
    }
}

impl std::fmt::Display for AgentAccessProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Session {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub origin: SessionOrigin,
    pub title: String,
    /// User-configurable system prompt. Project, Workflow, Agent, skill, and
    /// runtime layers are snapshotted separately for each Turn.
    pub system_prompt: String,
    pub model: String,
    #[serde(default)]
    pub access: AgentAccessProfile,
    pub status: SessionStatus,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Running,
    WaitingForHuman,
    Paused,
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

impl TurnStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Interrupted | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SkillSnapshot {
    pub slug: String,
    pub sha256: String,
    pub relative_path: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub status: TurnStatus,
    pub origin: TurnOrigin,
    pub input: String,
    pub output: Option<String>,
    pub model: String,
    /// Per-Turn model compute policy. `None` inherits the server/provider
    /// default.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    pub prompt: PromptSnapshot,
    /// Access snapshot captured when this Turn is created.
    #[serde(default)]
    pub access: AgentAccessProfile,
    pub max_steps: u32,
    /// Per-Turn limit for provider-hosted web search calls. `None` leaves the
    /// provider limit unset; zero disables hosted search for this Turn.
    #[serde(default)]
    pub max_search_calls: Option<u32>,
    /// Hosted web-search retrieval size for this Turn. `None` inherits the
    /// provider default.
    #[serde(default)]
    pub web_search_context_size: Option<WebSearchContextSize>,
    /// Per-response output ceiling for model samples in this Turn.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub response_format: Option<ModelResponseFormat>,
    #[serde(default)]
    pub skill_snapshots: Vec<SkillSnapshot>,
    #[serde(default)]
    pub history: Vec<ModelInputItem>,
    #[serde(default)]
    pub usage: TokenUsage,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Model,
    Tool,
    Workflow,
    System,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AgentStep {
    pub id: StepId,
    pub turn_id: TurnId,
    pub sequence: u32,
    pub kind: StepKind,
    pub name: String,
    pub status: StepStatus,
    pub input: Value,
    pub output: Option<Value>,
    #[serde(default)]
    pub usage: TokenUsage,
    pub duration_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
