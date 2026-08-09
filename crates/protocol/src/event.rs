use crate::ActionAttemptId;
use crate::ActionInvocationId;
use crate::ActionStatus;
use crate::AgentInstanceId;
use crate::ControlMessageId;
use crate::ControlMessageKind;
use crate::EventId;
use crate::HumanRequestId;
use crate::ProjectId;
use crate::SessionId;
use crate::SessionStatus;
use crate::StepId;
use crate::TurnId;
use crate::TurnStatus;
use crate::WorkflowId;
use crate::WorkflowStatus;
use crate::WorkflowUsage;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkflowEvent {
    pub id: EventId,
    pub sequence: u64,
    pub project_id: ProjectId,
    pub workflow_id: WorkflowId,
    pub occurred_at: DateTime<Utc>,
    #[serde(flatten)]
    pub payload: WorkflowEventPayload,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowEventPayload {
    WorkflowCreated {
        request: String,
        program_slug: String,
        source_sha256: String,
    },
    WorkflowStatusChanged {
        status: WorkflowStatus,
        reason: Option<String>,
    },
    ParticipantCreated {
        agent_instance_id: AgentInstanceId,
        session_id: SessionId,
        name: String,
        role: String,
    },
    ActionChanged {
        action_invocation_id: ActionInvocationId,
        action_attempt_id: Option<ActionAttemptId>,
        agent_instance_id: AgentInstanceId,
        action_name: String,
        status: ActionStatus,
        error: Option<String>,
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
    UsageUpdated {
        usage: WorkflowUsage,
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
    SessionCreated,
    SessionStatusChanged {
        status: SessionStatus,
        reason: Option<String>,
    },
    TurnCreated,
    TurnStatusChanged {
        status: TurnStatus,
        error: Option<String>,
    },
    AssistantMessageDelta {
        delta: String,
    },
    AssistantMessageReset,
    AssistantMessageCompleted,
    ModelStepStarted,
    ModelStepCompleted,
    ModelStepFailed,
    ToolCallStarted,
    ToolCallCompleted,
    HostedToolCompleted,
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
        workflow_id: WorkflowId,
        agent_instance_id: AgentInstanceId,
    },
    HumanRequestOpened {
        workflow_id: WorkflowId,
        human_request_id: HumanRequestId,
    },
    HumanRequestResolved {
        workflow_id: WorkflowId,
        human_request_id: HumanRequestId,
    },
    ControlMessageApplied {
        workflow_id: WorkflowId,
        control_message_id: ControlMessageId,
        kind: ControlMessageKind,
    },
    Warning {
        message: String,
    },
}
