use crate::ActionAttemptId;
use crate::ActionInvocationId;
use crate::ActionStatus;
use crate::AgentInstanceId;
use crate::BudgetUsage;
use crate::ChannelId;
use crate::ControlMessageId;
use crate::ControlMessageKind;
use crate::EventId;
use crate::HumanRequestId;
use crate::ModelToolCall;
use crate::ResearchId;
use crate::SessionId;
use crate::SessionStatus;
use crate::SignalId;
use crate::StepId;
use crate::TaskScopeId;
use crate::TeamId;
use crate::TimerId;
use crate::TokenUsage;
use crate::TurnId;
use crate::TurnStatus;
use crate::WorkflowRunId;
use crate::WorkflowRunStatus;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowRunEvent {
    pub id: EventId,
    pub sequence: u64,
    pub research_id: ResearchId,
    pub workflow_run_id: WorkflowRunId,
    pub occurred_at: DateTime<Utc>,
    #[serde(flatten)]
    pub payload: WorkflowRunEventPayload,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowRunEventPayload {
    WorkflowRunCreated {
        objective: String,
        workflow_slug: String,
        workflow_version: String,
        source_sha256: String,
    },
    WorkflowRunStatusChanged {
        status: WorkflowRunStatus,
        reason: Option<String>,
    },
    ParticipantCreated {
        agent_instance_id: AgentInstanceId,
        session_id: SessionId,
        name: String,
        role: String,
    },
    ParticipantRetired {
        agent_instance_id: AgentInstanceId,
    },
    TeamChanged {
        team_id: TeamId,
        member_ids: Vec<AgentInstanceId>,
    },
    RelationChanged {
        source_agent_id: AgentInstanceId,
        target_agent_id: AgentInstanceId,
        kind: String,
    },
    TaskScopeChanged {
        task_scope_id: TaskScopeId,
        status: String,
    },
    ActionChanged {
        action_invocation_id: ActionInvocationId,
        action_attempt_id: Option<ActionAttemptId>,
        agent_instance_id: AgentInstanceId,
        action_name: String,
        status: ActionStatus,
        error: Option<String>,
    },
    TimerChanged {
        timer_id: TimerId,
        status: String,
        fire_count: u32,
    },
    ChannelCreated {
        channel_id: ChannelId,
        name: String,
    },
    SignalPublished {
        channel_id: ChannelId,
        signal_id: SignalId,
        signal_sequence: u64,
    },
    HumanRequestOpened {
        human_request_id: HumanRequestId,
        session_id: SessionId,
        question: String,
    },
    HumanRequestResolved {
        human_request_id: HumanRequestId,
    },
    ControlMessageQueued {
        control_message_id: ControlMessageId,
        session_id: SessionId,
        kind: ControlMessageKind,
    },
    ControlMessageApplied {
        control_message_id: ControlMessageId,
    },
    BudgetUpdated {
        usage: BudgetUsage,
    },
    WorkflowCompleted {
        output: Value,
    },
    Warning {
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionEvent {
    pub id: EventId,
    pub sequence: u64,
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub step_id: Option<StepId>,
    pub occurred_at: DateTime<Utc>,
    #[serde(flatten)]
    pub payload: SessionEventPayload,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventPayload {
    SessionCreated {
        title: String,
    },
    SessionStatusChanged {
        status: SessionStatus,
        reason: Option<String>,
    },
    TurnCreated {
        input: String,
        model: String,
    },
    TurnStatusChanged {
        status: TurnStatus,
        error: Option<String>,
    },
    AgentStarted {
        objective: String,
        model: String,
    },
    AssistantMessageDelta {
        delta: String,
    },
    AssistantMessageReset,
    AssistantMessageCompleted {
        message: String,
    },
    ModelStepStarted {
        step: u32,
    },
    ModelStepCompleted {
        step: u32,
        usage: TokenUsage,
    },
    ModelStepFailed {
        step: u32,
        error: String,
        usage: TokenUsage,
    },
    ToolCallStarted {
        call: ModelToolCall,
    },
    ToolCallCompleted {
        call_id: String,
        tool_name: String,
        output: Value,
        duration_ms: u64,
        success: bool,
    },
    HostedToolCompleted {
        tool_name: String,
        input: Value,
    },
    ContextTrimmed {
        removed_items: usize,
    },
    ContextCompacted {
        before_tokens: usize,
        after_tokens: usize,
        removed_items: usize,
    },
    SamplingRetry {
        attempt: u32,
        error: String,
    },
    WorkflowAgentAttached {
        workflow_run_id: WorkflowRunId,
        agent_instance_id: AgentInstanceId,
        role: String,
    },
    HumanRequestOpened {
        workflow_run_id: WorkflowRunId,
        human_request_id: HumanRequestId,
        question: String,
    },
    HumanRequestResolved {
        workflow_run_id: WorkflowRunId,
        human_request_id: HumanRequestId,
    },
    ControlMessageApplied {
        workflow_run_id: WorkflowRunId,
        control_message_id: ControlMessageId,
        kind: ControlMessageKind,
        content: String,
    },
    Warning {
        message: String,
    },
}
