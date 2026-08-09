use crate::ActionAttempt;
use crate::ControlMessageId;
use crate::ModelInputItem;
use crate::SessionId;
use crate::TokenUsage;
use crate::Turn;
use crate::TurnId;
use chrono::DateTime;
use chrono::Utc;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

pub const SESSION_ROLLOUT_VERSION: u32 = 3;

/// Observable relationship between the canonical JSONL rollout and its
/// SQLite query projection.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionRolloutStatus {
    pub version: u32,
    pub last_sequence: u64,
    pub projected_sequence: u64,
}

/// One durable, monotonically sequenced fact in a Session rollout.
///
/// Rollouts are the canonical source for model context. SQLite stores the
/// query projection and the last applied rollout sequence.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionRolloutRecord {
    pub version: u32,
    pub session_id: SessionId,
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
    pub item: SessionRolloutItem,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextReplacementReason {
    Compaction,
    Trim,
}

/// A checkpoint either extends the prior context or explicitly replaces it.
/// Replacement is reserved for bounded-history operations such as compaction
/// and trimming; prior rollout records remain immutable.
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
pub enum SessionRolloutItem {
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
        acknowledged_control_ids: Vec<ControlMessageId>,
    },
    TurnUpdated {
        turn: Turn,
        acknowledged_control_ids: Vec<ControlMessageId>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRolloutState {
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
