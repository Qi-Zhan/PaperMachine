use crate::ActionAttemptId;
use crate::ActionInvocationId;
use crate::AgentInstanceId;
use crate::ArtifactId;
use crate::ChannelId;
use crate::ControlMessageId;
use crate::HumanRequestId;
use crate::RelationId;
use crate::ResearchId;
use crate::SessionId;
use crate::SignalId;
use crate::TaskScopeId;
use crate::TeamId;
use crate::TimerId;
use crate::TokenUsage;
use crate::TurnId;
use crate::WorkflowRunId;
use crate::WorkflowSnapshot;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Created,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowRunStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Research {
    pub id: ResearchId,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowRun {
    pub id: WorkflowRunId,
    pub research_id: ResearchId,
    pub origin_session_id: SessionId,
    pub workflow: WorkflowSnapshot,
    pub objective: String,
    pub status: WorkflowRunStatus,
    pub input: Value,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub attention_required: bool,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub usage: BudgetUsage,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Budget {
    pub max_agents: u32,
    pub max_concurrent_actions: u32,
    pub max_action_steps: u32,
    pub max_total_tokens: Option<u64>,
    #[serde(default)]
    pub max_uncached_tokens: Option<u64>,
    #[serde(default)]
    pub max_hosted_search_calls: Option<u32>,
    pub max_wall_time_seconds: Option<u64>,
    pub max_cost_usd: Option<f64>,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_agents: 12,
            max_concurrent_actions: 4,
            max_action_steps: 32,
            max_total_tokens: Some(2_000_000),
            max_uncached_tokens: Some(500_000),
            max_hosted_search_calls: Some(64),
            max_wall_time_seconds: Some(14_400),
            max_cost_usd: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct BudgetUsage {
    pub agents_created: u32,
    pub actions_started: u32,
    pub actions_completed: u32,
    pub action_steps: u32,
    pub timer_fires: u32,
    #[serde(default)]
    pub hosted_search_calls: u32,
    pub tokens: TokenUsage,
    pub wall_time_seconds: u64,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantStatus {
    Active,
    WaitingForHuman,
    Retired,
    Failed,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowParticipant {
    pub id: AgentInstanceId,
    pub workflow_run_id: WorkflowRunId,
    pub session_id: SessionId,
    pub class_name: String,
    pub name: String,
    pub role: String,
    pub instructions: String,
    pub model: String,
    #[serde(default)]
    pub skills: Vec<String>,
    pub status: ParticipantStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskScopeStatus {
    Open,
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct TaskScope {
    pub id: TaskScopeId,
    pub workflow_run_id: WorkflowRunId,
    pub parent_id: Option<TaskScopeId>,
    pub name: String,
    pub objective: String,
    pub status: TaskScopeStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Scheduled,
    Running,
    WaitingForHuman,
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
pub struct ActionInvocation {
    pub id: ActionInvocationId,
    pub workflow_run_id: WorkflowRunId,
    pub task_scope_id: Option<TaskScopeId>,
    pub agent_instance_id: AgentInstanceId,
    pub session_id: SessionId,
    pub action_name: String,
    pub objective: String,
    pub arguments: Value,
    pub status: ActionStatus,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ActionAttempt {
    pub id: ActionAttemptId,
    pub workflow_run_id: WorkflowRunId,
    pub invocation_id: ActionInvocationId,
    pub number: u32,
    pub turn_id: Option<TurnId>,
    pub status: ActionStatus,
    pub guidance: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowTeam {
    pub id: TeamId,
    pub workflow_run_id: WorkflowRunId,
    pub name: String,
    pub member_ids: Vec<AgentInstanceId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AgentRelation {
    pub id: RelationId,
    pub workflow_run_id: WorkflowRunId,
    pub source_agent_id: AgentInstanceId,
    pub target_agent_id: AgentInstanceId,
    pub kind: String,
    pub instructions: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerPolicy {
    Coalesce,
    Skip,
    Queue,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerStatus {
    Active,
    Paused,
    Completed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowTimer {
    pub id: TimerId,
    pub workflow_run_id: WorkflowRunId,
    pub name: String,
    pub interval_ms: u64,
    pub policy: TimerPolicy,
    pub status: TimerStatus,
    pub fire_count: u32,
    pub next_fire_at: DateTime<Utc>,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowChannel {
    pub id: ChannelId,
    pub workflow_run_id: WorkflowRunId,
    pub name: String,
    pub schema: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowSignal {
    pub id: SignalId,
    pub workflow_run_id: WorkflowRunId,
    pub channel_id: ChannelId,
    pub sender_agent_id: Option<AgentInstanceId>,
    pub sequence: u64,
    pub value: Value,
    pub created_at: DateTime<Utc>,
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
    pub workflow_run_id: WorkflowRunId,
    pub action_invocation_id: Option<ActionInvocationId>,
    pub action_attempt_id: Option<ActionAttemptId>,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
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
    Applied,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ControlMessage {
    pub id: ControlMessageId,
    pub workflow_run_id: WorkflowRunId,
    pub session_id: SessionId,
    pub action_invocation_id: Option<ActionInvocationId>,
    pub kind: ControlMessageKind,
    pub content: String,
    pub status: ControlMessageStatus,
    pub created_at: DateTime<Utc>,
    pub applied_at: Option<DateTime<Utc>>,
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
    pub research_id: ResearchId,
    pub workflow_run_id: WorkflowRunId,
    pub session_id: Option<SessionId>,
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
