use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Developer,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelInputItem {
    Message {
        role: MessageRole,
        content: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: Value,
    },
    ResponseItem {
        item: Value,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub supports_parallel: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedTool {
    WebSearch,
}

impl HostedTool {
    pub const fn name(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchContextSize {
    Low,
    Medium,
    High,
}

impl WebSearchContextSize {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelToolChoice {
    #[default]
    Auto,
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheStrategy {
    #[default]
    Auto,
    Implicit,
    Explicit,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PromptCacheConfig {
    /// Stable identity of the rendered prompt prefix, shared by requests with
    /// identical instructions, tools, and response schema.
    pub key: String,
    #[serde(default)]
    pub strategy: PromptCacheStrategy,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    Implicit,
    Explicit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTransport {
    HttpSse,
    ResponsesWebsocket,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxToolCallsMode {
    NotRequested,
    ProviderEnforced,
    ProviderViolated,
    RuntimeFallback,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ModelRequestMetadata {
    /// PaperMachine provider identifier selected by the model profile.
    #[serde(default)]
    pub provider: Option<String>,
    /// User-facing PaperMachine model profile. This can differ from the
    /// provider's model identifier.
    #[serde(default)]
    pub model_profile: Option<String>,
    /// Concrete model identifier sent to the provider.
    #[serde(default)]
    pub upstream_model: Option<String>,
    pub transport: ModelTransport,
    pub prompt_cache_mode: PromptCacheMode,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_breakpoint: bool,
    /// Whether the provider accepted and honored the Responses API
    /// `max_tool_calls` request property. ProviderViolated means the endpoint
    /// accepted the field but emitted more hosted calls than requested;
    /// subsequent requests fall back to runtime accounting. RuntimeFallback
    /// means PaperMachine omitted the field because the endpoint rejected or
    /// previously violated it.
    pub max_tool_calls_mode: MaxToolCallsMode,
    pub used_previous_response_id: bool,
    pub continuation_miss_reason: Option<String>,
    pub websocket_fallback_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ModelRequest {
    pub model: String,
    /// Optional per-request override of the provider's default reasoning
    /// effort. This is useful when orchestration and research actions need
    /// different compute policies inside one workflow.
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    pub instructions: String,
    pub input: Vec<ModelInputItem>,
    /// Provider prompt-cache routing and breakpoint strategy.
    #[serde(default)]
    pub prompt_cache: Option<PromptCacheConfig>,
    /// Session-scoped key used to retain an incremental model transport connection.
    #[serde(default)]
    pub transport_session_key: Option<String>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    #[serde(default)]
    pub hosted_tools: Vec<HostedTool>,
    /// Amount of retrieved context attached to each hosted web-search call.
    /// `None` leaves the provider default unchanged.
    #[serde(default)]
    pub web_search_context_size: Option<WebSearchContextSize>,
    #[serde(default = "default_true")]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub tool_choice: ModelToolChoice,
    /// Maximum number of provider-hosted built-in tool calls that may be
    /// processed while producing this response.
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub response_format: Option<ModelResponseFormat>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ModelResponseFormat {
    pub name: String,
    pub schema: Value,
    #[serde(default = "default_true")]
    pub strict: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ModelToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
}

impl TokenUsage {
    pub const fn total_tokens(self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Tokens that were not served from the provider's prompt cache.
    ///
    /// Output tokens are included because they are always newly generated.
    pub const fn uncached_tokens(self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cached_input_tokens)
            .saturating_add(self.output_tokens)
    }

    pub fn saturating_add_assign(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(other.cache_write_input_tokens);
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    RequestMetadata { metadata: ModelRequestMetadata },
    OutputTextDelta { delta: String },
    ToolCallCompleted { call: ModelToolCall },
    ResponseItemCompleted { item: Value },
    Completed { usage: TokenUsage },
}
