use crate::ActionAttempt;
use crate::AgentId;
use crate::AgentInputId;
use crate::ModelInputItem;
use crate::TokenUsage;
use crate::Turn;
use crate::TurnId;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

pub const AGENT_ROLLOUT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AgentRolloutStatus {
    pub version: u32,
    pub last_sequence: u64,
    pub projected_sequence: u64,
}

/// One durable, monotonically sequenced fact in an Agent rollout.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AgentRolloutRecord {
    pub version: u32,
    pub agent_id: AgentId,
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
    pub item: AgentRolloutItem,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextReplacementReason {
    Compaction,
    Trim,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelContextMutation {
    Unchanged,
    Append {
        items: Vec<ModelInputItem>,
    },
    Replace {
        items: Vec<ModelInputItem>,
        reason: ContextReplacementReason,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentRolloutItem {
    TurnCreated {
        turn: Turn,
        action_attempt: ActionAttempt,
    },
    ContextCheckpoint {
        turn_id: TurnId,
        mutation: ModelContextMutation,
        usage: TokenUsage,
        completed_model_steps: u32,
        hosted_search_calls_used: u32,
        checkpoint_message: Option<String>,
        acknowledged_agent_input_ids: Vec<AgentInputId>,
    },
    TurnUpdated {
        turn: Turn,
        acknowledged_agent_input_ids: Vec<AgentInputId>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentRolloutState {
    pub committed_context: Vec<ModelInputItem>,
    pub active_turn: Option<ActiveTurnRolloutState>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveTurnRolloutState {
    pub turn_id: TurnId,
    pub context: Vec<ModelInputItem>,
    pub has_checkpoint: bool,
    pub usage: TokenUsage,
    pub completed_model_steps: u32,
    pub hosted_search_calls_used: u32,
    pub checkpoint_message: Option<String>,
}
