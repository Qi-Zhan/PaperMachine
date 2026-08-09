use crate::AccessPreset;
use crate::ModelResponseFormat;
use crate::ProjectId;
use crate::PromptSnapshot;
use crate::ReasoningEffort;
use crate::SessionId;
use crate::StepId;
use crate::TokenUsage;
use crate::ToolSetSnapshot;
use crate::TurnEnvironmentSnapshot;
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
    Paused,
    Failed,
    Archived,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOrigin {
    User,
    Workflow,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Session {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub title: String,
    /// User-configurable system prompt. Project, Workflow, Agent, skill, and
    /// runtime layers are snapshotted separately for each Turn.
    pub system_prompt: String,
    pub model: String,
    pub access: AccessPreset,
    pub status: SessionStatus,
    pub enabled_skills: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Running,
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
    pub reasoning_effort: Option<ReasoningEffort>,
    pub prompt: PromptSnapshot,
    /// Immutable Workspace and materialized authorization captured when this
    /// Turn is created.
    pub environment: TurnEnvironmentSnapshot,
    /// Exact host-materialized local tool surface for this Turn. Hosted tools
    /// remain a separate provider capability.
    pub tool_set: ToolSetSnapshot,
    /// Internal Action policy used by finalization and JSON-repair Turns.
    pub tools_enabled: bool,
    /// Hosted web-search retrieval size for this Turn. `None` inherits the
    /// provider default.
    pub web_search_context_size: Option<WebSearchContextSize>,
    pub response_format: Option<ModelResponseFormat>,
    pub skill_snapshots: Vec<SkillSnapshot>,
    pub usage: TokenUsage,
    /// Durable Agent loop cursor used to continue an interrupted Turn without
    /// replaying completed model samples.
    pub completed_model_steps: u32,
    pub hosted_search_calls_used: u32,
    /// Last terminal assistant message checkpointed before the Turn status was
    /// committed. This closes the small crash window between model completion
    /// and `complete_turn`.
    pub checkpoint_message: Option<String>,
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
    Aborted,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AgentStep {
    pub id: StepId,
    pub turn_id: TurnId,
    pub sequence: u32,
    pub kind: StepKind,
    pub name: String,
    /// Provider/tool-loop call ID for local tool Steps. Model and system Steps
    /// leave this empty.
    pub tool_call_id: Option<String>,
    pub status: StepStatus,
    pub input: Value,
    pub output: Option<Value>,
    pub usage: TokenUsage,
    pub duration_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
