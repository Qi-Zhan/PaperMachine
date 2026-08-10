use crate::ActionAttemptId;
use crate::ActionInvocationId;
use crate::ActionStatus;
use crate::AgentId;
use crate::AgentInputId;
use crate::AgentInputKind;
use crate::EventId;
use crate::HumanRequestId;
use crate::ProjectId;
use crate::SessionId;
use crate::SessionStatus;
use crate::SessionUsage;
use crate::StepId;
use crate::TurnId;
use crate::TurnStatus;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// The single durable event stream for a Session and all of its Agents.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionEvent {
    pub id: EventId,
    pub sequence: u64,
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
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
        request: String,
        program_slug: String,
        source_sha256: String,
    },
    SessionChanged {
        status: SessionStatus,
        reason: Option<String>,
    },
    AgentCreated {
        name: String,
        role: String,
    },
    ActionChanged {
        action_invocation_id: ActionInvocationId,
        action_attempt_id: Option<ActionAttemptId>,
        action_name: String,
        status: ActionStatus,
        error: Option<String>,
    },
    HumanRequestOpened {
        human_request_id: HumanRequestId,
        question: String,
    },
    HumanRequestResolved {
        human_request_id: HumanRequestId,
    },
    AgentInputQueued {
        agent_input_id: AgentInputId,
        kind: AgentInputKind,
    },
    AgentInputApplied {
        agent_input_id: AgentInputId,
        kind: AgentInputKind,
    },
    UsageUpdated {
        usage: SessionUsage,
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
    Warning {
        message: String,
    },
}
