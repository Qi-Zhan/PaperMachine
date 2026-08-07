//! Model-provider streaming used by the PaperMachine agent runtime.

mod openai;
mod providers;
mod scripted;

use async_trait::async_trait;
use futures::stream::BoxStream;
use papermachine_protocol::HostedTool;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelRequest;
use papermachine_protocol::TokenUsage;
use thiserror::Error;

pub use openai::DEFAULT_MODEL_CONTEXT_WINDOW;
pub use openai::DEFAULT_MODEL_REQUEST_TIMEOUT;
pub use openai::DEFAULT_MODEL_STREAM_IDLE_TIMEOUT;
pub use openai::OpenAiPromptCacheMode;
pub use openai::OpenAiReasoningEffort;
pub use openai::OpenAiResponsesClient;
pub use openai::OpenAiResponsesConfig;
pub use providers::ConfiguredModels;
pub use providers::ModelProfile;
pub use providers::ModelProviderInfo;
pub use providers::ModelRouter;
pub use scripted::ScriptedModelClient;

pub type ModelStream = BoxStream<'static, Result<ModelEvent, ModelError>>;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model configuration error: {0}")]
    Configuration(String),
    #[error("model request failed: {0}")]
    Transport(String),
    #[error("model provider returned HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("model stream error: {0}")]
    Stream(String),
    #[error("model provider error: {0}")]
    Provider(String),
    #[error("model provider response was incomplete: {reason}")]
    IncompleteResponse { reason: String, usage: TokenUsage },
    #[error("scripted model has no response remaining")]
    ScriptExhausted,
}

impl ModelError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Stream(_) => true,
            Self::Http { status, .. } => *status == 429 || *status >= 500,
            Self::IncompleteResponse { reason, .. } => reason == "max_output_tokens",
            Self::Configuration(_) | Self::Provider(_) | Self::ScriptExhausted => false,
        }
    }

    pub const fn incomplete_usage(&self) -> Option<TokenUsage> {
        match self {
            Self::IncompleteResponse { usage, .. } => Some(*usage),
            _ => None,
        }
    }

    pub fn is_output_limit(&self) -> bool {
        matches!(
            self,
            Self::IncompleteResponse { reason, .. } if reason == "max_output_tokens"
        )
    }
}

#[async_trait]
pub trait ModelClient: Send + Sync {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError>;

    async fn close_transport_session(&self, _session_key: &str) {}

    fn model_context_window(&self, _model: &str) -> Option<usize> {
        None
    }

    fn supports_hosted_tool(&self, _model: &str, _tool: HostedTool) -> bool {
        false
    }
}
