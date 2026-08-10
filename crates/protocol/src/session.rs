use crate::AccessPreset;
use crate::ActionAttemptId;
use crate::ActionInvocationId;
use crate::AgentId;
use crate::ControlMessageId;
use crate::HumanRequestId;
use crate::ModelResponseFormat;
use crate::ModelRouteSnapshot;
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
use crate::WorkflowProgramSnapshot;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Running,
    WaitingForInput,
    WaitingForDeadline,
    Paused,
    Closing,
    Completed,
    Failed,
    Cancelled,
}

impl SessionStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTriggerKind {
    /// A person launched the Session from another Session.
    User,
    /// A person or API client launched the Session directly from a Project.
    #[default]
    Manual,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionTrigger {
    pub kind: SessionTriggerKind,
    pub source_session_id: Option<SessionId>,
}

/// One durable runtime instance of a WorkflowProgram.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Session {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub program: WorkflowProgramSnapshot,
    pub title: String,
    /// Immutable request supplied when this Session was launched.
    pub request: String,
    /// Optional high-priority instructions shared by its Agents.
    pub instructions: String,
    pub trigger: SessionTrigger,
    pub default_model: String,
    pub access: AccessPreset,
    pub enabled_skills: Vec<String>,
    /// Per-Session overrides keyed by Python Agent class name. Session access
    /// remains the hard upper bound.
    pub agent_access_overrides: BTreeMap<String, AccessPreset>,
    pub status: SessionStatus,
    /// Final status selected when `status == Closing`; otherwise `None`.
    pub closing_status: Option<SessionStatus>,
    pub params: Value,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub attention_required: bool,
    pub usage: SessionUsage,
    /// Archival is presentation/lifecycle metadata, not an execution status.
    pub archived_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEffectStatus {
    Started,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionEffect {
    pub session_id: SessionId,
    /// Deterministic logical path assigned by the Python DSL runtime.
    pub key: String,
    pub kind: String,
    pub request_sha256: String,
    pub payload: Value,
    pub status: SessionEffectStatus,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionUsage {
    pub agents_created: u32,
    pub actions_started: u32,
    pub actions_completed: u32,
    pub action_steps: u32,
    pub hosted_search_calls: u32,
    pub tokens: TokenUsage,
    pub wall_time_seconds: u64,
    pub estimated_cost_usd: Option<f64>,
}

/// One model identity owned by a Session. Each Agent has an independent
/// durable rollout, prompt, model route policy, access ceiling, and skills.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Agent {
    pub id: AgentId,
    pub session_id: SessionId,
    pub class_name: String,
    pub name: String,
    pub role: String,
    pub system_prompt: String,
    pub model: String,
    pub access: AccessPreset,
    pub skills: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Scheduled,
    Running,
    Completed,
    Failed,
    Interrupted,
    Cancelled,
}

impl ActionStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Interrupted | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ActionSource {
    Workflow,
    HumanRequest { request_id: HumanRequestId },
    Agent { sender_agent_id: AgentId },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ActionInvocation {
    pub id: ActionInvocationId,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub action_name: String,
    /// Stable Action method contract (normally its Python docstring/prompt).
    pub contract: String,
    pub arguments: Value,
    /// Exact user-role input fixed when the Action is admitted.
    pub input: String,
    pub source: ActionSource,
    pub requested_tools: Vec<String>,
    pub tools_enabled: bool,
    pub web_search_context_size: Option<WebSearchContextSize>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub response_format: Option<ModelResponseFormat>,
    pub status: ActionStatus,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ActionAttempt {
    pub id: ActionAttemptId,
    pub invocation_id: ActionInvocationId,
    pub number: u32,
    pub turn_id: Option<TurnId>,
    pub status: ActionStatus,
    pub guidance: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanRequestStatus {
    Open,
    Answered,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct HumanRequest {
    pub id: HumanRequestId,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub question: String,
    pub response_schema: Value,
    pub status: HumanRequestStatus,
    pub answer: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMessageKind {
    Guide,
    Interrupt,
    Finish,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMessageStatus {
    Pending,
    Claimed,
    Applied,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ControlMessage {
    pub id: ControlMessageId,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub action_invocation_id: Option<ActionInvocationId>,
    pub kind: ControlMessageKind,
    pub content: String,
    pub status: ControlMessageStatus,
    pub created_at: DateTime<Utc>,
    pub claimed_turn_id: Option<TurnId>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub applied_at: Option<DateTime<Utc>>,
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
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Turn {
    pub id: TurnId,
    pub agent_id: AgentId,
    pub status: TurnStatus,
    pub input: String,
    pub output: Option<String>,
    pub model_route: ModelRouteSnapshot,
    pub prompt: PromptSnapshot,
    pub environment: TurnEnvironmentSnapshot,
    pub tool_set: ToolSetSnapshot,
    pub tools_enabled: bool,
    pub web_search_context_size: Option<WebSearchContextSize>,
    pub response_format: Option<ModelResponseFormat>,
    pub skill_snapshots: Vec<SkillSnapshot>,
    pub usage: TokenUsage,
    pub completed_model_steps: u32,
    pub hosted_search_calls_used: u32,
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
    pub tool_call_id: Option<String>,
    pub status: StepStatus,
    pub input: Value,
    pub output: Option<Value>,
    pub usage: TokenUsage,
    pub duration_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
