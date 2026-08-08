use crate::AccessPreset;
use crate::ModelResponseFormat;
use crate::ProjectId;
use crate::PromptSnapshot;
use crate::ReasoningEffort;
use crate::SessionId;
use crate::StepId;
use crate::TokenUsage;
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
    pub access: AccessPreset,
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
    /// Immutable Workspace and materialized authorization captured when this
    /// Turn is created.
    pub environment: TurnEnvironmentSnapshot,
    /// Internal Action policy used by finalization and JSON-repair Turns.
    /// Ordinary user and Workflow Turns enable tools according to `access`.
    #[serde(default = "default_tools_enabled")]
    pub tools_enabled: bool,
    /// Hosted web-search retrieval size for this Turn. `None` inherits the
    /// provider default.
    #[serde(default)]
    pub web_search_context_size: Option<WebSearchContextSize>,
    #[serde(default)]
    pub response_format: Option<ModelResponseFormat>,
    #[serde(default)]
    pub skill_snapshots: Vec<SkillSnapshot>,
    #[serde(default)]
    pub usage: TokenUsage,
    /// Durable Agent loop cursor used to continue an interrupted Turn without
    /// replaying completed model samples.
    #[serde(default)]
    pub completed_model_steps: u32,
    #[serde(default)]
    pub hosted_search_calls_used: u32,
    /// Last terminal assistant message checkpointed before the Turn status was
    /// committed. This closes the small crash window between model completion
    /// and `complete_turn`.
    #[serde(default)]
    pub checkpoint_message: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const fn default_tools_enabled() -> bool {
    true
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
    ExecutionUnknown,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffectDisposition {
    Pure,
    Idempotent,
    Reconcilable,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionState {
    Prepared,
    Executing,
    Completed,
    ExecutionUnknown,
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
    #[serde(default)]
    pub tool_call_id: Option<String>,
    pub effect_disposition: Option<ToolEffectDisposition>,
    pub execution_state: Option<ToolExecutionState>,
    pub status: StepStatus,
    pub input: Value,
    pub output: Option<Value>,
    #[serde(default)]
    pub usage: TokenUsage,
    pub duration_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
