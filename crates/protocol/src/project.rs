use crate::ActionAttemptId;
use crate::ActionInvocationId;
use crate::AgentInstanceId;
use crate::ArtifactId;
use crate::ChannelId;
use crate::ControlMessageId;
use crate::HumanRequestId;
use crate::ProjectId;
use crate::RelationId;
use crate::SessionId;
use crate::SignalId;
use crate::TaskScopeId;
use crate::TeamId;
use crate::TimerId;
use crate::TokenUsage;
use crate::TurnId;
use crate::WorkflowId;
use crate::WorkflowProgramSnapshot;
use crate::WorkspaceId;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkspaceAttachment {
    pub id: WorkspaceId,
    /// Monotonically increases whenever the user changes the attached roots.
    pub revision: u64,
    /// Canonical absolute user-owned filesystem roots. The current UI selects
    /// one root, while the runtime representation deliberately supports more.
    pub roots: Vec<String>,
    /// Index into `roots` used as the default cwd for relative tool paths.
    pub primary_root: usize,
}

impl WorkspaceAttachment {
    pub fn single(path: impl Into<String>) -> Self {
        Self {
            id: WorkspaceId::new(),
            revision: 1,
            roots: vec![path.into()],
            primary_root: 0,
        }
    }

    pub fn primary_path(&self) -> Option<&str> {
        self.roots.get(self.primary_root).map(String::as_str)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.roots.is_empty() {
            return Err("Workspace must contain at least one root".to_string());
        }
        if self.primary_root >= self.roots.len() {
            return Err("Workspace primary_root is outside roots".to_string());
        }
        if self.revision == 0 {
            return Err("Workspace revision must be positive".to_string());
        }
        if self.roots.iter().any(|root| root.trim().is_empty()) {
            return Err("Workspace roots must not be empty".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkspaceSelection {
    pub roots: Vec<String>,
    #[serde(default)]
    pub primary_root: usize,
}

impl WorkspaceSelection {
    pub fn single(path: impl Into<String>) -> Self {
        Self {
            roots: vec![path.into()],
            primary_root: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Created,
    Running,
    WaitingForUser,
    WaitingForTimer,
    WaitingForSignal,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub description: String,
    /// User-owned filesystem attached to this managed Project. PaperMachine
    /// runtime state is stored separately and is never placed in these roots.
    pub workspace: WorkspaceAttachment,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowContextMode {
    #[default]
    Fresh,
    ProjectSnapshot,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowLaunchContext {
    pub mode: WorkflowContextMode,
    pub snapshot: Option<Value>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTriggerKind {
    /// A person launched the Run from an existing Session.
    User,
    /// Another durable Workflow launched this Run.
    Workflow,
    /// A scheduler launched this Run rather than merely waking an existing Run.
    Timer,
    /// A person or API client launched the Run directly from a Project.
    #[default]
    Manual,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowTrigger {
    pub kind: WorkflowTriggerKind,
    pub source_workflow_id: Option<WorkflowId>,
    pub source_session_id: Option<SessionId>,
    pub source_timer_id: Option<TimerId>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Workflow {
    pub id: WorkflowId,
    pub project_id: ProjectId,
    /// Optional Session from which the user started this Workflow. Project-level
    /// and built-in background Workflows do not need one.
    pub started_from_session_id: Option<SessionId>,
    pub program: WorkflowProgramSnapshot,
    /// Immutable per-Run request. The Workflow program decides which Agents
    /// receive it; the runtime never promotes it into system instructions.
    pub request: String,
    /// Optional high-priority instructions for this Run. This is not the user
    /// request and should remain stable across its Agent Sessions.
    pub instructions: String,
    /// Durable provenance for why and from where this Run was launched.
    pub trigger: WorkflowTrigger,
    pub default_model: String,
    #[serde(default)]
    pub access: crate::AccessPreset,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    /// Immutable Project state captured when this Workflow was launched. It is
    /// exposed as `ctx.context`; the Workflow must explicitly pass any relevant
    /// data to an Agent Action.
    #[serde(default)]
    pub launch_context: WorkflowLaunchContext,
    /// Per-run overrides keyed by Python Agent class name. The Workflow access
    /// profile remains the hard upper bound.
    #[serde(default)]
    pub agent_access_overrides: BTreeMap<String, crate::AccessPreset>,
    pub status: WorkflowStatus,
    pub params: Value,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub attention_required: bool,
    #[serde(default)]
    pub usage: WorkflowUsage,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEffectStatus {
    Started,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowEffect {
    pub workflow_id: WorkflowId,
    /// Deterministic logical path assigned by the Python DSL runtime.
    pub key: String,
    pub kind: String,
    pub request_sha256: String,
    pub payload: Value,
    pub status: WorkflowEffectStatus,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowUsage {
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
    Retired,
    Failed,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowParticipant {
    pub id: AgentInstanceId,
    pub workflow_id: WorkflowId,
    pub session_id: SessionId,
    pub class_name: String,
    pub name: String,
    pub role: String,
    pub system_prompt: String,
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
    pub workflow_id: WorkflowId,
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
    pub workflow_id: WorkflowId,
    pub task_scope_id: Option<TaskScopeId>,
    pub agent_instance_id: AgentInstanceId,
    pub session_id: SessionId,
    pub action_name: String,
    /// Stable Action method contract (normally its Python docstring/prompt).
    /// Concrete arguments remain separate and become the Workflow-origin Turn
    /// data only when the program invokes this Action.
    pub contract: String,
    pub arguments: Value,
    /// Direct HumanRequest whose answered string became this Action Turn's
    /// user message. `None` means the Turn was dispatched as workflow work.
    #[serde(default)]
    pub source_human_request_id: Option<HumanRequestId>,
    pub status: ActionStatus,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ActionAttempt {
    pub id: ActionAttemptId,
    pub workflow_id: WorkflowId,
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
    pub workflow_id: WorkflowId,
    pub name: String,
    pub member_ids: Vec<AgentInstanceId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AgentRelation {
    pub id: RelationId,
    pub workflow_id: WorkflowId,
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
    pub workflow_id: WorkflowId,
    pub name: String,
    pub interval_ms: u64,
    pub policy: TimerPolicy,
    pub status: TimerStatus,
    pub fire_count: u32,
    pub next_fire_at: DateTime<Utc>,
    pub last_fired_at: Option<DateTime<Utc>>,
    /// Lets an interrupted `wait_timer` effect observe the same durable fire
    /// instead of incrementing the timer twice during replay.
    pub last_fire_effect_key: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowChannel {
    pub id: ChannelId,
    pub workflow_id: WorkflowId,
    pub name: String,
    pub schema: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowSignal {
    pub id: SignalId,
    pub workflow_id: WorkflowId,
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
    pub workflow_id: WorkflowId,
    pub session_id: SessionId,
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
    pub workflow_id: WorkflowId,
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
    pub project_id: ProjectId,
    pub workflow_id: WorkflowId,
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
