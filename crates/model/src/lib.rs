//! Model-provider streaming used by the PaperMachine agent runtime.

mod openai;
mod openai_chat;
mod providers;
mod scripted;

use async_trait::async_trait;
use futures::stream::BoxStream;
use papermachine_protocol::HostedTool;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelRequest;
use papermachine_protocol::ModelRouteCapabilities;
use papermachine_protocol::ModelRouteSnapshot;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::TokenUsage;
use sha2::Digest;
use sha2::Sha256;
use thiserror::Error;

pub use openai::DEFAULT_MODEL_CONTEXT_WINDOW;
pub use openai::DEFAULT_MODEL_REQUEST_TIMEOUT;
pub use openai::DEFAULT_MODEL_STREAM_IDLE_TIMEOUT;
pub use openai::OpenAiPromptCacheMode;
pub use openai::OpenAiReasoningEffort;
pub use openai::OpenAiResponsesClient;
pub use openai::OpenAiResponsesConfig;
pub use openai_chat::OpenAiChatClient;
pub use openai_chat::OpenAiChatCompatibility;
pub use openai_chat::OpenAiChatConfig;
pub use providers::ConfiguredModels;
pub use providers::ModelApi;
pub use providers::ModelCapability;
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

    fn resolve_route_snapshot(
        &self,
        profile: &str,
        reasoning_effort: Option<ReasoningEffort>,
        fallback_context_window: usize,
    ) -> Result<ModelRouteSnapshot, ModelError> {
        let capabilities = ModelRouteCapabilities {
            hosted_web_search: self.supports_hosted_tool(profile, HostedTool::WebSearch),
        };
        let context_window = self
            .model_context_window(profile)
            .unwrap_or(fallback_context_window);
        let config_sha256 = hash_route_config(&serde_json::json!({
            "profile": profile,
            "provider": "direct",
            "upstream_model": profile,
            "context_window": context_window,
            "capabilities": capabilities,
            "reasoning_effort": reasoning_effort,
        }))?;
        let snapshot = ModelRouteSnapshot {
            profile: profile.to_string(),
            provider: "direct".to_string(),
            upstream_model: profile.to_string(),
            context_window,
            capabilities,
            reasoning_effort,
            config_sha256,
        };
        snapshot.validate().map_err(ModelError::Configuration)?;
        Ok(snapshot)
    }

    fn validate_route_snapshot(
        &self,
        snapshot: &ModelRouteSnapshot,
        fallback_context_window: usize,
    ) -> Result<(), ModelError> {
        snapshot.validate().map_err(ModelError::Configuration)?;
        let current = self.resolve_route_snapshot(
            &snapshot.profile,
            snapshot.reasoning_effort,
            fallback_context_window,
        )?;
        if current != *snapshot {
            return Err(ModelError::Configuration(format!(
                "model route configuration changed for profile {:?}",
                snapshot.profile
            )));
        }
        Ok(())
    }
}

pub(crate) fn hash_route_config(value: &serde_json::Value) -> Result<String, ModelError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| ModelError::Configuration(error.to_string()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}
