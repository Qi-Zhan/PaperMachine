//! A small Responses API client adapted from the streaming model used by
//! OpenAI Codex. PaperMachine keeps only text, function calls, usage, HTTP
//! retry, and SSE handling.

use crate::ModelClient;
use crate::ModelError;
use crate::ModelStream;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::SinkExt;
use futures::StreamExt;
use futures::stream;
use papermachine_protocol::MaxToolCallsMode;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelRequest;
use papermachine_protocol::ModelRequestMetadata;
use papermachine_protocol::ModelToolChoice;
use papermachine_protocol::ModelTransport;
use papermachine_protocol::PromptCacheMode;
use papermachine_protocol::PromptCacheStrategy;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::TokenUsage;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue as WebSocketHeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::header::USER_AGENT as WEBSOCKET_USER_AGENT;
use url::Url;

const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/responses";
pub const DEFAULT_MODEL_CONTEXT_WINDOW: usize = 128_000;
pub const DEFAULT_MODEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_MODEL_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const WEBSOCKET_SESSION_TTL: Duration = Duration::from_secs(60 * 60);
const WEBSOCKET_FALLBACK_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_WEBSOCKET_SESSIONS: usize = 64;
const WEBSOCKET_EVENT_BUFFER: usize = 128;
const RESPONSES_WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
const PROMPT_CACHE_CAPABILITY_PROBE_KEY: &str = "papermachine:cache-capability:v1";

fn request_retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    let base_ms = 1_000_u64.saturating_mul(1_u64 << shift);
    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_millis()) % 501)
        .unwrap_or_default();
    Duration::from_millis(base_ms.saturating_add(jitter_ms))
}

type ResponsesWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl OpenAiReasoningEffort {
    const fn as_str(self) -> &'static str {
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

impl FromStr for OpenAiReasoningEffort {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            other => Err(ModelError::Configuration(format!(
                "unsupported reasoning effort {other:?}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiPromptCacheMode {
    #[default]
    Auto,
    Implicit,
    Explicit,
}

impl FromStr for OpenAiPromptCacheMode {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "implicit" => Ok(Self::Implicit),
            "explicit" => Ok(Self::Explicit),
            other => Err(ModelError::Configuration(format!(
                "unsupported prompt cache mode {other:?}"
            ))),
        }
    }
}

#[derive(Clone)]
pub struct OpenAiResponsesConfig {
    pub provider_id: String,
    pub endpoint: Url,
    pub api_key: String,
    pub organization: Option<String>,
    pub project: Option<String>,
    pub max_request_retries: u32,
    pub request_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub reasoning_effort: Option<OpenAiReasoningEffort>,
    pub store_responses: bool,
    pub responses_websockets: bool,
    pub prompt_cache_mode: OpenAiPromptCacheMode,
}

impl fmt::Debug for OpenAiResponsesConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesConfig")
            .field("provider_id", &self.provider_id)
            .field("endpoint", &self.endpoint)
            .field("api_key", &"<redacted>")
            .field("organization", &self.organization)
            .field("project", &self.project)
            .field("max_request_retries", &self.max_request_retries)
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("store_responses", &self.store_responses)
            .field("responses_websockets", &self.responses_websockets)
            .field("prompt_cache_mode", &self.prompt_cache_mode)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct CodexOpenAiSettings {
    pub client: OpenAiResponsesConfig,
    pub model: String,
    pub model_context_window: usize,
}

impl OpenAiResponsesConfig {
    pub fn from_env() -> Result<Self, ModelError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| ModelError::Configuration("OPENAI_API_KEY is not set".to_string()))?;
        let endpoint = match std::env::var("OPENAI_RESPONSES_ENDPOINT") {
            Ok(endpoint) => parse_endpoint(&endpoint)?,
            Err(_) => match std::env::var("OPENAI_BASE_URL") {
                Ok(base_url) => responses_endpoint(&base_url)?,
                Err(_) => parse_endpoint(DEFAULT_ENDPOINT)?,
            },
        };
        let reasoning_effort = std::env::var("OPENAI_REASONING_EFFORT")
            .ok()
            .map(|value| value.parse())
            .transpose()?;
        let store_responses = std::env::var("OPENAI_STORE_RESPONSES")
            .ok()
            .map(|value| parse_bool("OPENAI_STORE_RESPONSES", &value))
            .transpose()?
            .unwrap_or(false);
        let responses_websockets = responses_websockets_from_env()?;
        let prompt_cache_mode = prompt_cache_mode_from_env()?.unwrap_or_default();

        Ok(Self {
            provider_id: "openai".to_string(),
            endpoint,
            api_key,
            organization: std::env::var("OPENAI_ORG_ID").ok(),
            project: std::env::var("OPENAI_PROJECT_ID").ok(),
            max_request_retries: 2,
            request_timeout: timeout_from_env(
                "OPENAI_REQUEST_TIMEOUT_SECONDS",
                DEFAULT_MODEL_REQUEST_TIMEOUT,
            )?,
            stream_idle_timeout: timeout_from_env(
                "OPENAI_STREAM_IDLE_TIMEOUT_SECONDS",
                DEFAULT_MODEL_STREAM_IDLE_TIMEOUT,
            )?,
            reasoning_effort,
            store_responses,
            responses_websockets,
            prompt_cache_mode,
        })
    }

    pub fn from_codex_home(codex_home: &Path) -> Result<CodexOpenAiSettings, ModelError> {
        let config_path = codex_home.join("config.toml");
        let auth_path = codex_home.join("auth.json");
        let config_text = fs::read_to_string(&config_path).map_err(|error| {
            ModelError::Configuration(format!("failed to read {}: {error}", config_path.display()))
        })?;
        let config: CodexConfigFile = toml::from_str(&config_text).map_err(|error| {
            ModelError::Configuration(format!(
                "failed to parse {}: {error}",
                config_path.display()
            ))
        })?;
        if config.model_provider.as_deref().unwrap_or("openai") != "openai" {
            return Err(ModelError::Configuration(
                "Codex model_provider must be openai".to_string(),
            ));
        }
        let auth_text = fs::read_to_string(&auth_path).map_err(|error| {
            ModelError::Configuration(format!("failed to read {}: {error}", auth_path.display()))
        })?;
        let auth: CodexAuthFile = serde_json::from_str(&auth_text).map_err(|error| {
            ModelError::Configuration(format!("failed to parse {}: {error}", auth_path.display()))
        })?;
        let api_key = auth
            .openai_api_key
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| {
                ModelError::Configuration(format!(
                    "{} does not contain a non-empty OPENAI_API_KEY",
                    auth_path.display()
                ))
            })?;
        let endpoint = match config.openai_base_url {
            Some(base_url) => responses_endpoint(&base_url)?,
            None => parse_endpoint(DEFAULT_ENDPOINT)?,
        };
        let model = config
            .model
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| "gpt-5.2".to_string());
        let model_context_window = config
            .model_context_window
            .unwrap_or(DEFAULT_MODEL_CONTEXT_WINDOW);
        if model_context_window < 4_096 {
            return Err(ModelError::Configuration(
                "model_context_window must be at least 4096".to_string(),
            ));
        }

        Ok(CodexOpenAiSettings {
            client: Self {
                provider_id: "codex-openai".to_string(),
                endpoint,
                api_key,
                organization: None,
                project: None,
                max_request_retries: 2,
                request_timeout: timeout_from_env(
                    "OPENAI_REQUEST_TIMEOUT_SECONDS",
                    DEFAULT_MODEL_REQUEST_TIMEOUT,
                )?,
                stream_idle_timeout: timeout_from_env(
                    "OPENAI_STREAM_IDLE_TIMEOUT_SECONDS",
                    DEFAULT_MODEL_STREAM_IDLE_TIMEOUT,
                )?,
                reasoning_effort: config.model_reasoning_effort,
                store_responses: !config.disable_response_storage.unwrap_or(true),
                responses_websockets: responses_websockets_from_env()?,
                prompt_cache_mode: prompt_cache_mode_from_env()?
                    .or(config.prompt_cache_mode)
                    .unwrap_or_default(),
            },
            model,
            model_context_window,
        })
    }
}

#[derive(Debug, Deserialize)]
struct CodexConfigFile {
    model_provider: Option<String>,
    openai_base_url: Option<String>,
    model: Option<String>,
    model_reasoning_effort: Option<OpenAiReasoningEffort>,
    disable_response_storage: Option<bool>,
    model_context_window: Option<usize>,
    prompt_cache_mode: Option<OpenAiPromptCacheMode>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
}

fn parse_endpoint(value: &str) -> Result<Url, ModelError> {
    value
        .parse::<Url>()
        .map_err(|error| ModelError::Configuration(format!("invalid OpenAI endpoint: {error}")))
}

pub(crate) fn responses_endpoint(base_url: &str) -> Result<Url, ModelError> {
    let parsed = parse_endpoint(base_url)?;
    if parsed.path().trim_end_matches('/').ends_with("/responses") {
        return Ok(parsed);
    }
    let mut directory = parsed;
    let path = format!("{}/", directory.path().trim_end_matches('/'));
    directory.set_path(&path);
    directory
        .join("responses")
        .map_err(|error| ModelError::Configuration(format!("invalid OpenAI base URL: {error}")))
}

fn parse_bool(name: &str, value: &str) -> Result<bool, ModelError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ModelError::Configuration(format!(
            "{name} must be true or false"
        ))),
    }
}

fn responses_websockets_from_env() -> Result<bool, ModelError> {
    std::env::var("PAPERMACHINE_RESPONSES_WEBSOCKETS")
        .ok()
        .map(|value| parse_bool("PAPERMACHINE_RESPONSES_WEBSOCKETS", &value))
        .transpose()
        .map(|value| value.unwrap_or(true))
}

fn prompt_cache_mode_from_env() -> Result<Option<OpenAiPromptCacheMode>, ModelError> {
    std::env::var("PAPERMACHINE_PROMPT_CACHE_MODE")
        .ok()
        .map(|value| value.parse())
        .transpose()
}

fn timeout_from_env(name: &str, default: Duration) -> Result<Duration, ModelError> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
    let seconds = value.trim().parse::<u64>().map_err(|_| {
        ModelError::Configuration(format!(
            "{name} must be a positive integer number of seconds"
        ))
    })?;
    if seconds == 0 {
        return Err(ModelError::Configuration(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(Duration::from_secs(seconds))
}

#[derive(Clone)]
pub struct OpenAiResponsesClient {
    http: reqwest::Client,
    config: OpenAiResponsesConfig,
    websocket_sessions: Arc<Mutex<HashMap<String, WebsocketSessionState>>>,
    websocket_fallback_sessions: Arc<Mutex<HashMap<String, Instant>>>,
    prompt_cache_capabilities: Arc<Mutex<HashMap<String, Arc<OnceCell<PromptCacheCapability>>>>>,
    max_tool_calls_capabilities: Arc<Mutex<HashMap<String, Arc<OnceCell<MaxToolCallsCapability>>>>>,
    max_tool_calls_violations: Arc<Mutex<HashSet<String>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptCacheCapability {
    Supported,
    Unsupported,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaxToolCallsCapability {
    Supported,
    Unsupported,
    Indeterminate,
}

struct WebsocketSessionState {
    connection: ResponsesWebSocket,
    last_request_properties: Value,
    last_request_input: Vec<Value>,
    last_response_id: String,
    last_response_items: Vec<Value>,
    last_used: Instant,
}

struct MaxToolCallsObservationState {
    stream: ModelStream,
    model: String,
    limit: u32,
    hosted_calls: u32,
    latest_metadata: Option<ModelRequestMetadata>,
    pending: VecDeque<Result<ModelEvent, ModelError>>,
    violations: Arc<Mutex<HashSet<String>>>,
    reported: bool,
}

impl OpenAiResponsesClient {
    pub fn new(config: OpenAiResponsesConfig) -> Result<Self, ModelError> {
        if config.api_key.trim().is_empty() {
            return Err(ModelError::Configuration(
                "OpenAI API key must not be empty".to_string(),
            ));
        }

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("papermachine/0.1"));
        if let Some(organization) = config.organization.as_deref() {
            headers.insert(
                "OpenAI-Organization",
                HeaderValue::from_str(organization)
                    .map_err(|error| ModelError::Configuration(error.to_string()))?,
            );
        }
        if let Some(project) = config.project.as_deref() {
            headers.insert(
                "OpenAI-Project",
                HeaderValue::from_str(project)
                    .map_err(|error| ModelError::Configuration(error.to_string()))?,
            );
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(20))
            .build()
            .map_err(|error| ModelError::Configuration(error.to_string()))?;
        Ok(Self {
            http,
            config,
            websocket_sessions: Arc::new(Mutex::new(HashMap::new())),
            websocket_fallback_sessions: Arc::new(Mutex::new(HashMap::new())),
            prompt_cache_capabilities: Arc::new(Mutex::new(HashMap::new())),
            max_tool_calls_capabilities: Arc::new(Mutex::new(HashMap::new())),
            max_tool_calls_violations: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    async fn resolve_prompt_cache_mode(&self, request: &ModelRequest) -> PromptCacheMode {
        let request_strategy = request
            .prompt_cache
            .as_ref()
            .map(|cache| cache.strategy)
            .unwrap_or(PromptCacheStrategy::Implicit);
        let configured = match request_strategy {
            PromptCacheStrategy::Implicit => OpenAiPromptCacheMode::Implicit,
            PromptCacheStrategy::Explicit => OpenAiPromptCacheMode::Explicit,
            PromptCacheStrategy::Auto => self.config.prompt_cache_mode,
        };
        match configured {
            OpenAiPromptCacheMode::Implicit => PromptCacheMode::Implicit,
            OpenAiPromptCacheMode::Explicit => PromptCacheMode::Explicit,
            OpenAiPromptCacheMode::Auto => {
                match self.prompt_cache_capability(&request.model).await {
                    PromptCacheCapability::Supported => PromptCacheMode::Explicit,
                    PromptCacheCapability::Unsupported | PromptCacheCapability::Indeterminate => {
                        PromptCacheMode::Implicit
                    }
                }
            }
        }
    }

    async fn prompt_cache_capability(&self, model: &str) -> PromptCacheCapability {
        let cell = {
            let mut capabilities = self.prompt_cache_capabilities.lock().await;
            Arc::clone(
                capabilities
                    .entry(model.to_string())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        *cell
            .get_or_init(|| async { self.probe_prompt_cache_breakpoint(model).await })
            .await
    }

    async fn probe_prompt_cache_breakpoint(&self, model: &str) -> PromptCacheCapability {
        let marked_status = match self
            .send_probe_request(&prompt_cache_probe_body(model, true))
            .await
        {
            Ok(status) if status.is_success() => {
                tracing::info!(model, "provider supports explicit prompt-cache breakpoints");
                return PromptCacheCapability::Supported;
            }
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(model, error = %error, "prompt-cache capability probe was inconclusive; using implicit caching");
                return PromptCacheCapability::Indeterminate;
            }
        };

        if matches!(
            marked_status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        ) {
            tracing::warn!(
                model,
                status = marked_status.as_u16(),
                "prompt-cache capability probe could not complete; using implicit caching"
            );
            return PromptCacheCapability::Indeterminate;
        }

        match self
            .send_probe_request(&prompt_cache_probe_body(model, false))
            .await
        {
            Ok(status) if status.is_success() => {
                tracing::warn!(
                    model,
                    breakpoint_status = marked_status.as_u16(),
                    "provider rejected explicit prompt-cache breakpoints; using implicit caching"
                );
                PromptCacheCapability::Unsupported
            }
            Ok(status) => {
                tracing::warn!(
                    model,
                    breakpoint_status = marked_status.as_u16(),
                    control_status = status.as_u16(),
                    "prompt-cache capability probe and control both failed; using implicit caching"
                );
                PromptCacheCapability::Indeterminate
            }
            Err(error) => {
                tracing::warn!(
                    model,
                    breakpoint_status = marked_status.as_u16(),
                    error = %error,
                    "prompt-cache control probe failed; using implicit caching"
                );
                PromptCacheCapability::Indeterminate
            }
        }
    }

    async fn send_probe_request(&self, body: &Value) -> Result<StatusCode, ModelError> {
        let request = self
            .http
            .post(self.config.endpoint.clone())
            .bearer_auth(&self.config.api_key)
            .json(body)
            .send();
        match tokio::time::timeout(self.config.request_timeout, request).await {
            Ok(Ok(response)) => Ok(response.status()),
            Ok(Err(error)) => Err(ModelError::Transport(error.to_string())),
            Err(_) => Err(ModelError::Transport(format!(
                "provider response headers timed out after {} seconds",
                self.config.request_timeout.as_secs()
            ))),
        }
    }

    async fn resolve_max_tool_calls(
        &self,
        mut request: ModelRequest,
    ) -> (ModelRequest, MaxToolCallsMode) {
        if request.max_tool_calls.is_none() {
            return (request, MaxToolCallsMode::NotRequested);
        }
        if self
            .max_tool_calls_violations
            .lock()
            .await
            .contains(&request.model)
        {
            request.max_tool_calls = None;
            return (request, MaxToolCallsMode::RuntimeFallback);
        }
        let capability = self.max_tool_calls_capability(&request.model).await;
        match capability {
            MaxToolCallsCapability::Supported => (request, MaxToolCallsMode::ProviderEnforced),
            MaxToolCallsCapability::Unsupported | MaxToolCallsCapability::Indeterminate => {
                request.max_tool_calls = None;
                (request, MaxToolCallsMode::RuntimeFallback)
            }
        }
    }

    fn observe_max_tool_calls(
        &self,
        stream: ModelStream,
        model: String,
        limit: Option<u32>,
        mode: MaxToolCallsMode,
    ) -> ModelStream {
        let Some(limit) = limit.filter(|_| mode == MaxToolCallsMode::ProviderEnforced) else {
            return stream;
        };
        let state = MaxToolCallsObservationState {
            stream,
            model,
            limit,
            hosted_calls: 0,
            latest_metadata: None,
            pending: VecDeque::new(),
            violations: Arc::clone(&self.max_tool_calls_violations),
            reported: false,
        };
        stream::unfold(state, |mut state| async move {
            if let Some(event) = state.pending.pop_front() {
                return Some((event, state));
            }
            let event = state.stream.next().await?;
            match &event {
                Ok(ModelEvent::RequestMetadata { metadata }) => {
                    state.latest_metadata = Some(metadata.clone());
                }
                Ok(ModelEvent::ResponseItemCompleted { item })
                    if item.get("type").and_then(Value::as_str) == Some("web_search_call") =>
                {
                    state.hosted_calls = state.hosted_calls.saturating_add(1);
                }
                Ok(ModelEvent::Completed { .. })
                    if !state.reported && state.hosted_calls > state.limit =>
                {
                    state.reported = true;
                    state.violations.lock().await.insert(state.model.clone());
                    tracing::warn!(
                        model = state.model,
                        requested = state.limit,
                        observed = state.hosted_calls,
                        "provider accepted but violated max_tool_calls; using runtime fallback"
                    );
                    if let Some(mut metadata) = state.latest_metadata.clone() {
                        metadata.max_tool_calls_mode = MaxToolCallsMode::ProviderViolated;
                        state.pending.push_back(event);
                        return Some((Ok(ModelEvent::RequestMetadata { metadata }), state));
                    }
                }
                _ => {}
            }
            Some((event, state))
        })
        .boxed()
    }

    async fn max_tool_calls_capability(&self, model: &str) -> MaxToolCallsCapability {
        let cell = {
            let mut capabilities = self.max_tool_calls_capabilities.lock().await;
            Arc::clone(
                capabilities
                    .entry(model.to_string())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        *cell
            .get_or_init(|| async { self.probe_max_tool_calls(model).await })
            .await
    }

    async fn probe_max_tool_calls(&self, model: &str) -> MaxToolCallsCapability {
        let marked_status = match self
            .send_probe_request(&max_tool_calls_probe_body(model, true))
            .await
        {
            Ok(status) if status.is_success() => {
                tracing::info!(model, "provider supports max_tool_calls");
                return MaxToolCallsCapability::Supported;
            }
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(model, error = %error, "max_tool_calls capability probe was inconclusive; using runtime fallback");
                return MaxToolCallsCapability::Indeterminate;
            }
        };

        if matches!(
            marked_status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
        ) {
            tracing::warn!(
                model,
                status = marked_status.as_u16(),
                "max_tool_calls capability probe could not complete; using runtime fallback"
            );
            return MaxToolCallsCapability::Indeterminate;
        }

        match self
            .send_probe_request(&max_tool_calls_probe_body(model, false))
            .await
        {
            Ok(status) if status.is_success() => {
                tracing::warn!(
                    model,
                    max_tool_calls_status = marked_status.as_u16(),
                    "provider rejected max_tool_calls; enforcing the search limit between model samples"
                );
                MaxToolCallsCapability::Unsupported
            }
            Ok(status) => {
                tracing::warn!(
                    model,
                    max_tool_calls_status = marked_status.as_u16(),
                    control_status = status.as_u16(),
                    "max_tool_calls capability probe and control both failed; using runtime fallback"
                );
                MaxToolCallsCapability::Indeterminate
            }
            Err(error) => {
                tracing::warn!(
                    model,
                    max_tool_calls_status = marked_status.as_u16(),
                    error = %error,
                    "max_tool_calls control probe failed; using runtime fallback"
                );
                MaxToolCallsCapability::Indeterminate
            }
        }
    }

    async fn request(&self, body: &Value) -> Result<reqwest::Response, ModelError> {
        let mut attempt = 0;
        loop {
            let request = self
                .http
                .post(self.config.endpoint.clone())
                .bearer_auth(&self.config.api_key)
                .json(body)
                .send();
            let response = match tokio::time::timeout(self.config.request_timeout, request).await {
                Ok(Ok(response)) => response,
                Ok(Err(_)) | Err(_) if attempt < self.config.max_request_retries => {
                    attempt += 1;
                    tokio::time::sleep(request_retry_delay(attempt)).await;
                    continue;
                }
                Ok(Err(error)) => return Err(ModelError::Transport(error.to_string())),
                Err(_) => {
                    return Err(ModelError::Transport(format!(
                        "provider response headers timed out after {} seconds",
                        self.config.request_timeout.as_secs()
                    )));
                }
            };

            if response.status().is_success() {
                return Ok(response);
            }

            let status = response.status();
            let should_retry = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            let message = match tokio::time::timeout(
                self.config.stream_idle_timeout,
                response.text(),
            )
            .await
            {
                Ok(Ok(message)) => message,
                Ok(Err(_)) => "unable to read provider response".to_string(),
                Err(_) => "timed out reading provider error response".to_string(),
            };
            if should_retry && attempt < self.config.max_request_retries {
                attempt += 1;
                tokio::time::sleep(request_retry_delay(attempt)).await;
                continue;
            }
            return Err(ModelError::Http {
                status: status.as_u16(),
                message,
            });
        }
    }

    async fn stream_http(
        &self,
        request: ModelRequest,
        prompt_cache_mode: PromptCacheMode,
        max_tool_calls_mode: MaxToolCallsMode,
        websocket_fallback_reason: Option<String>,
    ) -> Result<ModelStream, ModelError> {
        let upstream_model = request.model.clone();
        let max_tool_calls = request.max_tool_calls;
        let body = request_body(&request, &self.config, prompt_cache_mode);
        let response = self.request(&body).await?;
        let idle_timeout = self.config.stream_idle_timeout;
        let source = Box::pin(response.bytes_stream().eventsource());
        let stream = stream::unfold(Some(source), move |state| async move {
            let mut source = state?;
            match tokio::time::timeout(idle_timeout, source.next()).await {
                Ok(Some(Ok(event))) => Some((Ok(event), Some(source))),
                Ok(Some(Err(error))) => Some((Err(ModelError::Stream(error.to_string())), None)),
                Ok(None) => None,
                Err(_) => Some((
                    Err(ModelError::Stream(format!(
                        "provider stream was idle for {} seconds",
                        idle_timeout.as_secs()
                    ))),
                    None,
                )),
            }
        })
        .filter_map(|event| async move {
            match event {
                Ok(event) => parse_event_data(&event.data).transpose(),
                Err(error) => Some(Err(error)),
            }
        })
        .boxed();
        let metadata = ModelEvent::RequestMetadata {
            metadata: ModelRequestMetadata {
                provider: Some(self.config.provider_id.clone()),
                model_profile: None,
                upstream_model: Some(upstream_model.clone()),
                transport: ModelTransport::HttpSse,
                prompt_cache_mode,
                prompt_cache_key: request.prompt_cache.map(|cache| cache.key),
                prompt_cache_breakpoint: prompt_cache_mode == PromptCacheMode::Explicit,
                max_tool_calls_mode,
                used_previous_response_id: false,
                continuation_miss_reason: Some("http_transport".to_string()),
                websocket_fallback_reason,
            },
        };
        let stream = stream::once(async move { Ok(metadata) })
            .chain(stream)
            .boxed();
        Ok(
            self.observe_max_tool_calls(
                stream,
                upstream_model,
                max_tool_calls,
                max_tool_calls_mode,
            ),
        )
    }

    async fn connect_websocket(&self, session_key: &str) -> Result<ResponsesWebSocket, ModelError> {
        let endpoint = websocket_endpoint(&self.config.endpoint)?;
        let mut request = endpoint
            .as_str()
            .into_client_request()
            .map_err(|error| ModelError::Transport(error.to_string()))?;
        let headers = request.headers_mut();
        headers.insert(
            AUTHORIZATION,
            WebSocketHeaderValue::from_str(&format!("Bearer {}", self.config.api_key))
                .map_err(|error| ModelError::Configuration(error.to_string()))?,
        );
        headers.insert(
            WEBSOCKET_USER_AGENT,
            WebSocketHeaderValue::from_static("papermachine/0.1"),
        );
        headers.insert(
            "OpenAI-Beta",
            WebSocketHeaderValue::from_static(RESPONSES_WEBSOCKET_BETA),
        );
        if let Ok(value) = WebSocketHeaderValue::from_str(session_key) {
            headers.insert("x-client-request-id", value);
        }
        if let Some(organization) = self.config.organization.as_deref() {
            headers.insert(
                "OpenAI-Organization",
                WebSocketHeaderValue::from_str(organization)
                    .map_err(|error| ModelError::Configuration(error.to_string()))?,
            );
        }
        if let Some(project) = self.config.project.as_deref() {
            headers.insert(
                "OpenAI-Project",
                WebSocketHeaderValue::from_str(project)
                    .map_err(|error| ModelError::Configuration(error.to_string()))?,
            );
        }

        match tokio::time::timeout(WEBSOCKET_CONNECT_TIMEOUT, connect_async(request)).await {
            Ok(Ok((connection, _response))) => Ok(connection),
            Ok(Err(error)) => Err(ModelError::Transport(format!(
                "Responses WebSocket handshake failed: {error}"
            ))),
            Err(_) => Err(ModelError::Transport(format!(
                "Responses WebSocket handshake timed out after {} seconds",
                WEBSOCKET_CONNECT_TIMEOUT.as_secs()
            ))),
        }
    }

    async fn stream_websocket(
        &self,
        request: ModelRequest,
        prompt_cache_mode: PromptCacheMode,
        max_tool_calls_mode: MaxToolCallsMode,
    ) -> Result<ModelStream, ModelError> {
        let upstream_model = request.model.clone();
        let max_tool_calls = request.max_tool_calls;
        let session_key = request
            .transport_session_key
            .as_deref()
            .ok_or_else(|| ModelError::Configuration("missing transport session key".to_string()))?
            .to_string();
        let mut state = match self.websocket_sessions.lock().await.remove(&session_key) {
            Some(state) => state,
            None => WebsocketSessionState {
                connection: self.connect_websocket(&session_key).await?,
                last_request_properties: Value::Null,
                last_request_input: Vec::new(),
                last_response_id: String::new(),
                last_response_items: Vec::new(),
                last_used: Instant::now(),
            },
        };

        let mut body = websocket_request_body(&request, &self.config, prompt_cache_mode);
        let full_input = body
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let request_properties = websocket_request_properties(&body);
        let continuation = websocket_continuation(&state, &request_properties, &full_input);
        if let Some(incremental_input) = continuation.incremental_input.as_ref() {
            body["previous_response_id"] = json!(state.last_response_id);
            body["input"] = Value::Array(incremental_input.clone());
        }

        state
            .connection
            .send(Message::Text(body.to_string().into()))
            .await
            .map_err(|error| {
                ModelError::Transport(format!(
                    "failed to send Responses WebSocket request: {error}"
                ))
            })?;

        let idle_timeout = self.config.stream_idle_timeout;
        let sessions = Arc::clone(&self.websocket_sessions);
        let fallback_sessions = Arc::clone(&self.websocket_fallback_sessions);
        let (sender, receiver) = mpsc::channel(WEBSOCKET_EVENT_BUFFER);
        tokio::spawn(async move {
            stream_websocket_events(
                state,
                WebsocketEventContext {
                    session_key,
                    request_properties,
                    full_input,
                    idle_timeout,
                    sessions,
                    fallback_sessions,
                    sender,
                },
            )
            .await;
        });
        let metadata = ModelEvent::RequestMetadata {
            metadata: ModelRequestMetadata {
                provider: Some(self.config.provider_id.clone()),
                model_profile: None,
                upstream_model: Some(upstream_model.clone()),
                transport: ModelTransport::ResponsesWebsocket,
                prompt_cache_mode,
                prompt_cache_key: request.prompt_cache.map(|cache| cache.key),
                prompt_cache_breakpoint: prompt_cache_mode == PromptCacheMode::Explicit,
                max_tool_calls_mode,
                used_previous_response_id: continuation.incremental_input.is_some(),
                continuation_miss_reason: continuation.miss_reason.map(str::to_string),
                websocket_fallback_reason: None,
            },
        };
        let stream = stream::once(async move { Ok(metadata) })
            .chain(ReceiverStream::new(receiver))
            .boxed();
        Ok(
            self.observe_max_tool_calls(
                stream,
                upstream_model,
                max_tool_calls,
                max_tool_calls_mode,
            ),
        )
    }
}

#[async_trait]
impl ModelClient for OpenAiResponsesClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        let prompt_cache_mode = self.resolve_prompt_cache_mode(&request).await;
        let (request, max_tool_calls_mode) = self.resolve_max_tool_calls(request).await;
        let transport_session_key = request.transport_session_key.clone();
        // The Responses WebSocket beta (including otherwise compatible
        // proxies) does not consistently accept max_output_tokens. A bounded
        // one-shot action can safely use HTTP SSE; multi-sample research
        // actions omit the cap so they retain incremental WebSocket state.
        let output_limit_requires_http = request.max_output_tokens.is_some();
        let session_uses_http_fallback = match transport_session_key.as_deref() {
            Some(session_key) => {
                let mut fallback_sessions = self.websocket_fallback_sessions.lock().await;
                fallback_sessions
                    .retain(|_, failed_at| failed_at.elapsed() < WEBSOCKET_FALLBACK_TTL);
                fallback_sessions.contains_key(session_key)
            }
            None => false,
        };
        let mut websocket_fallback_reason = if output_limit_requires_http {
            Some("max_output_tokens_requires_http".to_string())
        } else {
            session_uses_http_fallback.then(|| "session_in_http_fallback_ttl".to_string())
        };
        if self.config.responses_websockets
            && transport_session_key.is_some()
            && !session_uses_http_fallback
            && !output_limit_requires_http
        {
            match self
                .stream_websocket(request.clone(), prompt_cache_mode, max_tool_calls_mode)
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(error) => {
                    websocket_fallback_reason = Some(error.to_string());
                    if let Some(session_key) = transport_session_key.as_deref() {
                        self.websocket_fallback_sessions
                            .lock()
                            .await
                            .insert(session_key.to_string(), Instant::now());
                    }
                    tracing::warn!(error = %error, "falling back from Responses WebSocket to HTTP SSE");
                }
            }
        }
        self.stream_http(
            request,
            prompt_cache_mode,
            max_tool_calls_mode,
            websocket_fallback_reason,
        )
        .await
    }

    async fn close_transport_session(&self, session_key: &str) {
        let state = self.websocket_sessions.lock().await.remove(session_key);
        self.websocket_fallback_sessions
            .lock()
            .await
            .remove(session_key);
        if let Some(mut state) = state {
            let _ = state.connection.close(None).await;
        }
    }
}

struct WebsocketEventContext {
    session_key: String,
    request_properties: Value,
    full_input: Vec<Value>,
    idle_timeout: Duration,
    sessions: Arc<Mutex<HashMap<String, WebsocketSessionState>>>,
    fallback_sessions: Arc<Mutex<HashMap<String, Instant>>>,
    sender: mpsc::Sender<Result<ModelEvent, ModelError>>,
}

fn websocket_endpoint(endpoint: &Url) -> Result<Url, ModelError> {
    let mut endpoint = endpoint.clone();
    let scheme = match endpoint.scheme() {
        "http" => "ws",
        "https" => "wss",
        scheme => {
            return Err(ModelError::Configuration(format!(
                "cannot derive Responses WebSocket URL from {scheme:?} endpoint"
            )));
        }
    };
    endpoint.set_scheme(scheme).map_err(|_| {
        ModelError::Configuration("failed to derive Responses WebSocket URL".to_string())
    })?;
    Ok(endpoint)
}

fn websocket_request_body(
    request: &ModelRequest,
    config: &OpenAiResponsesConfig,
    prompt_cache_mode: PromptCacheMode,
) -> Value {
    let mut body = request_body(request, config, prompt_cache_mode);
    if let Some(object) = body.as_object_mut() {
        object.remove("stream");
        object.remove("background");
        object.insert("type".to_string(), json!("response.create"));
    }
    body
}

fn websocket_request_properties(body: &Value) -> Value {
    let mut properties = body.clone();
    if let Some(object) = properties.as_object_mut() {
        object.remove("input");
        object.remove("type");
        object.remove("previous_response_id");
        // These control only the next response. They do not change the input
        // chain referenced by previous_response_id.
        object.remove("tool_choice");
        object.remove("parallel_tool_calls");
        object.remove("max_tool_calls");
    }
    properties
}

struct WebsocketContinuationDecision {
    incremental_input: Option<Vec<Value>>,
    miss_reason: Option<&'static str>,
}

fn websocket_continuation(
    state: &WebsocketSessionState,
    request_properties: &Value,
    full_input: &[Value],
) -> WebsocketContinuationDecision {
    if state.last_response_id.is_empty() {
        return WebsocketContinuationDecision {
            incremental_input: None,
            miss_reason: Some("no_previous_response"),
        };
    }
    if state.last_request_properties != *request_properties {
        return WebsocketContinuationDecision {
            incremental_input: None,
            miss_reason: Some("request_properties_changed"),
        };
    }
    let Some(baseline_len) = state
        .last_request_input
        .len()
        .checked_add(state.last_response_items.len())
    else {
        return WebsocketContinuationDecision {
            incremental_input: None,
            miss_reason: Some("history_length_overflow"),
        };
    };
    if full_input.len() <= baseline_len {
        return WebsocketContinuationDecision {
            incremental_input: None,
            miss_reason: Some("history_not_extended"),
        };
    }
    let baseline_matches = state
        .last_request_input
        .iter()
        .chain(&state.last_response_items)
        .zip(full_input.iter().take(baseline_len))
        .all(|(previous, current)| previous == current);
    if !baseline_matches {
        return WebsocketContinuationDecision {
            incremental_input: None,
            miss_reason: Some("history_prefix_mismatch"),
        };
    }
    WebsocketContinuationDecision {
        incremental_input: Some(full_input[baseline_len..].to_vec()),
        miss_reason: None,
    }
}

async fn stream_websocket_events(mut state: WebsocketSessionState, context: WebsocketEventContext) {
    let WebsocketEventContext {
        session_key,
        request_properties,
        full_input,
        idle_timeout,
        sessions,
        fallback_sessions,
        sender,
    } = context;
    let mut response_id = String::new();
    let mut response_items = Vec::new();
    loop {
        let next_message = tokio::select! {
            _ = sender.closed() => return,
            result = tokio::time::timeout(idle_timeout, state.connection.next()) => result,
        };
        let message = match next_message {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(error))) => {
                fail_websocket_stream(
                    &sender,
                    &fallback_sessions,
                    &session_key,
                    ModelError::Stream(format!("Responses WebSocket receive failed: {error}")),
                )
                .await;
                return;
            }
            Ok(None) => {
                fail_websocket_stream(
                    &sender,
                    &fallback_sessions,
                    &session_key,
                    ModelError::Stream(
                        "Responses WebSocket closed before response.completed".to_string(),
                    ),
                )
                .await;
                return;
            }
            Err(_) => {
                fail_websocket_stream(
                    &sender,
                    &fallback_sessions,
                    &session_key,
                    ModelError::Stream(format!(
                        "Responses WebSocket was idle for {} seconds",
                        idle_timeout.as_secs()
                    )),
                )
                .await;
                return;
            }
        };

        let data = match message {
            Message::Text(text) => text.to_string(),
            Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(data) => data,
                Err(error) => {
                    fail_websocket_stream(
                        &sender,
                        &fallback_sessions,
                        &session_key,
                        ModelError::Stream(format!(
                            "Responses WebSocket returned invalid UTF-8: {error}"
                        )),
                    )
                    .await;
                    return;
                }
            },
            Message::Ping(payload) => {
                if let Err(error) = state.connection.send(Message::Pong(payload)).await {
                    fail_websocket_stream(
                        &sender,
                        &fallback_sessions,
                        &session_key,
                        ModelError::Stream(format!("Responses WebSocket pong failed: {error}")),
                    )
                    .await;
                    return;
                }
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => {
                fail_websocket_stream(
                    &sender,
                    &fallback_sessions,
                    &session_key,
                    ModelError::Stream(
                        "Responses WebSocket closed before response.completed".to_string(),
                    ),
                )
                .await;
                return;
            }
            Message::Frame(_) => continue,
        };

        let value: Value = match serde_json::from_str(&data) {
            Ok(value) => value,
            Err(error) => {
                fail_websocket_stream(
                    &sender,
                    &fallback_sessions,
                    &session_key,
                    ModelError::Stream(format!("invalid provider event JSON: {error}")),
                )
                .await;
                return;
            }
        };
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(id) = value.pointer("/response/id").and_then(Value::as_str) {
            response_id = id.to_string();
        }
        if event_type == "response.output_item.done"
            && let Some(item) = value.get("item")
        {
            response_items.push(sanitize_response_item(item));
        }

        let event = match parse_event_data(&data) {
            Ok(event) => event,
            Err(error) => {
                if matches!(error, ModelError::Stream(_) | ModelError::Transport(_)) {
                    fallback_sessions
                        .lock()
                        .await
                        .insert(session_key.clone(), Instant::now());
                }
                let _ = sender.send(Err(error)).await;
                return;
            }
        };
        if event_type == "response.completed" {
            if response_id.is_empty() {
                fail_websocket_stream(
                    &sender,
                    &fallback_sessions,
                    &session_key,
                    ModelError::Stream(
                        "Responses WebSocket completion omitted response.id".to_string(),
                    ),
                )
                .await;
                return;
            }
            state.last_request_properties = request_properties;
            state.last_request_input = full_input;
            state.last_response_id = response_id;
            state.last_response_items = response_items;
            state.last_used = Instant::now();
            store_websocket_state(&sessions, session_key, state).await;
            if let Some(event) = event {
                let _ = sender.send(Ok(event)).await;
            }
            return;
        }
        if let Some(event) = event
            && sender.send(Ok(event)).await.is_err()
        {
            return;
        }
    }
}

async fn fail_websocket_stream(
    sender: &mpsc::Sender<Result<ModelEvent, ModelError>>,
    fallback_sessions: &Mutex<HashMap<String, Instant>>,
    session_key: &str,
    error: ModelError,
) {
    fallback_sessions
        .lock()
        .await
        .insert(session_key.to_string(), Instant::now());
    let _ = sender.send(Err(error)).await;
}

async fn store_websocket_state(
    sessions: &Mutex<HashMap<String, WebsocketSessionState>>,
    session_key: String,
    state: WebsocketSessionState,
) {
    let mut sessions = sessions.lock().await;
    sessions.retain(|_, state| state.last_used.elapsed() < WEBSOCKET_SESSION_TTL);
    if sessions.len() >= MAX_WEBSOCKET_SESSIONS
        && let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, state)| state.last_used)
            .map(|(key, _)| key.clone())
    {
        sessions.remove(&oldest);
    }
    sessions.insert(session_key, state);
}

fn prompt_cache_probe_body(model: &str, marked: bool) -> Value {
    let mut content = json!({
        "type": "input_text",
        "text": "PaperMachine prompt-cache capability probe."
    });
    if marked {
        content["prompt_cache_breakpoint"] = json!({"mode": "explicit"});
    }
    let mut body = json!({
        "model": model,
        "instructions": "",
        "input": [
            {
                "role": "developer",
                "content": [content]
            },
            {
                "role": "user",
                "content": [{"type": "input_text", "text": "Return OK."}]
            }
        ],
        "prompt_cache_key": PROMPT_CACHE_CAPABILITY_PROBE_KEY,
        "store": false,
        "stream": false,
        "max_output_tokens": 16
    });
    if marked {
        body["prompt_cache_options"] = json!({"mode": "explicit"});
    }
    body
}

fn max_tool_calls_probe_body(model: &str, marked: bool) -> Value {
    let mut body = json!({
        "model": model,
        "input": "Reply with OK only.",
        "tools": [{"type": "web_search"}],
        "tool_choice": "none",
        "store": false,
        "stream": false,
        "max_output_tokens": 128,
    });
    if marked {
        body["max_tool_calls"] = json!(1);
    }
    body
}

fn request_body(
    request: &ModelRequest,
    config: &OpenAiResponsesConfig,
    prompt_cache_mode: PromptCacheMode,
) -> Value {
    let mut input = request
        .input
        .iter()
        .map(model_input_json)
        .collect::<Vec<_>>();
    let instructions = match prompt_cache_mode {
        PromptCacheMode::Implicit => request.instructions.clone(),
        PromptCacheMode::Explicit => {
            let stable_instructions = if request.instructions.trim().is_empty() {
                "Follow the user's request."
            } else {
                request.instructions.as_str()
            };
            input.insert(
                0,
                json!({
                    "role": "developer",
                    "content": [{
                        "type": "input_text",
                        "text": stable_instructions,
                        "prompt_cache_breakpoint": {"mode": "explicit"}
                    }]
                }),
            );
            String::new()
        }
    };
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": false,
            })
        })
        .chain(request.hosted_tools.iter().map(|tool| match tool {
            papermachine_protocol::HostedTool::WebSearch => {
                let mut definition = json!({"type": "web_search"});
                if let Some(size) = request.web_search_context_size {
                    definition["search_context_size"] = json!(size.as_str());
                }
                definition
            }
        }))
        .collect::<Vec<_>>();

    let mut body = json!({
        "model": request.model,
        "instructions": instructions,
        "input": input,
        "tools": tools,
        "tool_choice": match request.tool_choice {
            ModelToolChoice::Auto => "auto",
            ModelToolChoice::None => "none",
        },
        "parallel_tool_calls": request.parallel_tool_calls,
        "store": config.store_responses,
        "stream": true,
    });
    if let Some(prompt_cache) = &request.prompt_cache {
        body["prompt_cache_key"] = json!(prompt_cache.key);
    }
    if let Some(max_tool_calls) = request.max_tool_calls {
        body["max_tool_calls"] = json!(max_tool_calls);
    }
    if prompt_cache_mode == PromptCacheMode::Explicit {
        body["prompt_cache_options"] = json!({"mode": "explicit"});
    }
    let reasoning_effort = request
        .reasoning_effort
        .map(ReasoningEffort::as_str)
        .or_else(|| config.reasoning_effort.map(OpenAiReasoningEffort::as_str));
    if let Some(reasoning_effort) = reasoning_effort {
        body["reasoning"] = json!({"effort": reasoning_effort});
    }
    if let Some(format) = &request.response_format {
        body["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": format.name,
                "schema": format.schema,
                "strict": format.strict,
            }
        });
    }
    if let Some(max_output_tokens) = request.max_output_tokens {
        body["max_output_tokens"] = json!(max_output_tokens);
    }
    body
}

fn model_input_json(item: &ModelInputItem) -> Value {
    match item {
        ModelInputItem::Message { role, content } => {
            let (role, content_type) = match role {
                MessageRole::User => ("user", "input_text"),
                MessageRole::Developer => ("developer", "input_text"),
                MessageRole::Assistant => ("assistant", "output_text"),
            };
            let content_block = json!({ "type": content_type, "text": content });
            json!({
                "role": role,
                "content": [content_block],
            })
        }
        ModelInputItem::FunctionCall {
            call_id,
            name,
            arguments,
        } => json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments,
        }),
        ModelInputItem::FunctionCallOutput { call_id, output } => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output.to_string(),
        }),
        ModelInputItem::ResponseItem { item } => item.clone(),
    }
}

fn parse_event_data(data: &str) -> Result<Option<ModelEvent>, ModelError> {
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(data)
        .map_err(|error| ModelError::Stream(format!("invalid SSE JSON: {error}")))?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match event_type {
        "response.output_text.delta" => {
            Ok(value.get("delta").and_then(Value::as_str).map(|delta| {
                ModelEvent::OutputTextDelta {
                    delta: delta.to_string(),
                }
            }))
        }
        "response.output_item.done" => parse_output_item(value.get("item")),
        "response.completed" => Ok(Some(ModelEvent::Completed {
            usage: parse_usage(
                value
                    .get("response")
                    .and_then(|response| response.get("usage")),
            ),
        })),
        "response.failed" | "response.incomplete" => {
            let reason = value
                .pointer("/response/error/message")
                .or_else(|| value.pointer("/response/incomplete_details/reason"))
                .and_then(Value::as_str)
                .unwrap_or(event_type);
            Err(ModelError::IncompleteResponse {
                reason: reason.to_string(),
                usage: parse_usage(value.pointer("/response/usage")),
            })
        }
        "error" => {
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error");
            Err(ModelError::Provider(message.to_string()))
        }
        _ => Ok(None),
    }
}

fn parse_output_item(item: Option<&Value>) -> Result<Option<ModelEvent>, ModelError> {
    let Some(item) = item else {
        return Ok(None);
    };
    if item.get("type").and_then(Value::as_str) == Some("function_call") {
        let field = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| ModelError::Stream(format!("function call missing {key}")))
        };
        let _ = field("call_id")?;
        let _ = field("name")?;
        let _ = field("arguments")?;
    }
    Ok(Some(ModelEvent::ResponseItemCompleted {
        item: sanitize_response_item(item),
    }))
}

fn sanitize_response_item(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(object) = item.as_object_mut() {
        object.remove("id");
        object.remove("status");
    }
    item
}

fn parse_usage(usage: Option<&Value>) -> TokenUsage {
    let read = |key: &str| {
        usage
            .and_then(|value| value.get(key))
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    let cached_input_tokens = usage
        .and_then(|value| value.pointer("/input_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cache_write_input_tokens = usage
        .and_then(|value| value.pointer("/input_tokens_details/cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    TokenUsage {
        input_tokens: read("input_tokens"),
        output_tokens: read("output_tokens"),
        cached_input_tokens,
        cache_write_input_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::body::Body;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::StatusCode as AxumStatusCode;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::post;
    use futures::TryStreamExt;
    use papermachine_protocol::ModelResponseFormat;
    use papermachine_protocol::ToolDefinition;
    use std::convert::Infallible;
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    fn test_config() -> OpenAiResponsesConfig {
        OpenAiResponsesConfig {
            provider_id: "test-openai".to_string(),
            endpoint: Url::parse(DEFAULT_ENDPOINT).expect("default endpoint should parse"),
            api_key: "test-key".to_string(),
            organization: None,
            project: None,
            max_request_retries: 0,
            request_timeout: DEFAULT_MODEL_REQUEST_TIMEOUT,
            stream_idle_timeout: DEFAULT_MODEL_STREAM_IDLE_TIMEOUT,
            reasoning_effort: Some(OpenAiReasoningEffort::Medium),
            store_responses: false,
            responses_websockets: true,
            prompt_cache_mode: OpenAiPromptCacheMode::Implicit,
        }
    }

    #[test]
    fn request_body_contains_function_tools_and_history() {
        let request = ModelRequest {
            model: "gpt-5.6-sol".to_string(),
            reasoning_effort: Some(ReasoningEffort::High),
            instructions: "research carefully".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "question".to_string(),
            }],
            prompt_cache: Some(papermachine_protocol::PromptCacheConfig {
                key: "prefix-123".to_string(),
                strategy: PromptCacheStrategy::Auto,
            }),
            transport_session_key: Some("turn-123".to_string()),
            tools: vec![ToolDefinition {
                name: "run_program".to_string(),
                description: "Run a program".to_string(),
                input_schema: json!({"type": "object"}),
                supports_parallel: false,
            }],
            hosted_tools: vec![papermachine_protocol::HostedTool::WebSearch],
            web_search_context_size: Some(papermachine_protocol::WebSearchContextSize::Low),
            parallel_tool_calls: true,
            tool_choice: ModelToolChoice::Auto,
            max_tool_calls: Some(12),
            max_output_tokens: Some(1000),
            response_format: Some(ModelResponseFormat {
                name: "research_result".to_string(),
                schema: json!({
                    "type": "object",
                    "properties": {"result": {"type": "string"}},
                    "required": ["result"],
                    "additionalProperties": false
                }),
                strict: true,
            }),
        };

        let body = request_body(&request, &test_config(), PromptCacheMode::Implicit);
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["prompt_cache_key"], "prefix-123");
        assert!(body.get("prompt_cache_options").is_none());
        assert!(
            body["input"][0]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none()
        );
        assert_eq!(body["tools"][0]["name"], "run_program");
        assert_eq!(body["tools"][1]["type"], "web_search");
        assert_eq!(body["tools"][1]["search_context_size"], "low");
        assert_eq!(body["max_tool_calls"], 12);
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["max_output_tokens"], 1000);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["store"], false);
    }

    #[test]
    fn implicit_mode_uses_provider_managed_prompt_caching() {
        let request = ModelRequest {
            model: "gpt-5.5".to_string(),
            reasoning_effort: None,
            instructions: "research carefully".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "question".to_string(),
            }],
            prompt_cache: Some(papermachine_protocol::PromptCacheConfig {
                key: "prefix-123".to_string(),
                strategy: PromptCacheStrategy::Auto,
            }),
            transport_session_key: Some("turn-123".to_string()),
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: true,
            tool_choice: ModelToolChoice::Auto,
            max_tool_calls: None,
            max_output_tokens: None,
            response_format: None,
        };

        let body = request_body(&request, &test_config(), PromptCacheMode::Implicit);
        assert!(body.get("prompt_cache_options").is_none());
        assert!(
            body["input"][0]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none()
        );
    }

    #[test]
    fn explicit_mode_marks_only_the_stable_instruction_prefix() {
        let request = ModelRequest {
            model: "gpt-5.6-sol".to_string(),
            reasoning_effort: None,
            instructions: "research carefully".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "question".to_string(),
            }],
            prompt_cache: Some(papermachine_protocol::PromptCacheConfig {
                key: "prefix-123".to_string(),
                strategy: PromptCacheStrategy::Explicit,
            }),
            transport_session_key: Some("turn-123".to_string()),
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: false,
            tool_choice: ModelToolChoice::None,
            max_tool_calls: None,
            max_output_tokens: None,
            response_format: None,
        };

        let body = request_body(&request, &test_config(), PromptCacheMode::Explicit);
        assert_eq!(body["instructions"], "");
        assert_eq!(body["prompt_cache_key"], "prefix-123");
        assert_eq!(body["prompt_cache_options"]["mode"], "explicit");
        assert_eq!(body["input"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(
            body["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
            "explicit"
        );
        assert!(
            body["input"][1]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none()
        );
        assert_eq!(body["tool_choice"], "none");
    }

    #[test]
    fn capability_probe_differs_only_by_explicit_cache_fields() {
        let marked = prompt_cache_probe_body("gpt-test", true);
        let control = prompt_cache_probe_body("gpt-test", false);

        assert_eq!(
            marked["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
            "explicit"
        );
        assert_eq!(marked["prompt_cache_options"]["mode"], "explicit");
        assert!(
            control["input"][0]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none()
        );
        assert!(control.get("prompt_cache_options").is_none());
        assert_eq!(marked["prompt_cache_key"], control["prompt_cache_key"]);
    }

    #[test]
    fn max_tool_calls_probe_differs_only_by_the_optional_limit() {
        let marked = max_tool_calls_probe_body("gpt-test", true);
        let control = max_tool_calls_probe_body("gpt-test", false);

        assert_eq!(marked["max_tool_calls"], 1);
        assert!(control.get("max_tool_calls").is_none());
        let mut marked_without_limit = marked;
        marked_without_limit
            .as_object_mut()
            .expect("probe body should be an object")
            .remove("max_tool_calls");
        assert_eq!(marked_without_limit, control);
    }

    #[test]
    fn websocket_continuation_ignores_per_response_tool_call_limits() {
        let first = json!({
            "type": "response.create",
            "input": [{"role": "user", "content": "question"}],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "max_tool_calls": 12,
        });
        let second = json!({
            "type": "response.create",
            "input": [{"role": "user", "content": "question"}],
            "tool_choice": "none",
            "parallel_tool_calls": false,
            "max_tool_calls": 3,
        });

        assert_eq!(
            websocket_request_properties(&first),
            websocket_request_properties(&second)
        );
    }

    #[derive(Clone, Default)]
    struct CacheProbeServerState {
        requests: Arc<Mutex<Vec<Value>>>,
    }

    async fn reject_explicit_cache_then_stream(
        State(state): State<CacheProbeServerState>,
        Json(body): Json<Value>,
    ) -> Response {
        let marked = body
            .pointer("/input/0/content/0/prompt_cache_breakpoint")
            .is_some();
        state.requests.lock().await.push(body.clone());
        if marked {
            return (AxumStatusCode::BAD_GATEWAY, "unsupported cache breakpoint").into_response();
        }
        if body.get("stream").and_then(Value::as_bool) == Some(false) {
            return Json(json!({"id": "probe-control"})).into_response();
        }
        let events = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1,\"input_tokens_details\":{\"cached_tokens\":0}}}}\n\n"
        );
        Response::builder()
            .status(AxumStatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(events))
            .expect("test SSE response should build")
    }

    async fn stream_past_response_header_timeout() -> Response {
        let chunks = stream::unfold(0_u8, |state| async move {
            match state {
                0 => Some((
                    Ok::<Bytes, Infallible>(Bytes::from_static(
                        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"still \"}\n\n",
                    )),
                    1,
                )),
                1 => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Some((
                        Ok::<Bytes, Infallible>(Bytes::from_static(
                            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"streaming\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":0}}}}\n\n",
                        )),
                        2,
                    ))
                }
                _ => None,
            }
        });
        Response::builder()
            .status(AxumStatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(chunks))
            .expect("test SSE response should build")
    }

    #[tokio::test]
    async fn response_header_timeout_does_not_cap_an_active_sse_stream() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should have an address");
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/responses", post(stream_past_response_header_timeout)),
            )
            .await
            .expect("test HTTP server should run");
        });

        let mut config = test_config();
        config.endpoint = Url::parse(&format!("http://{address}/v1/responses"))
            .expect("test endpoint should parse");
        config.responses_websockets = false;
        config.request_timeout = Duration::from_millis(100);
        config.stream_idle_timeout = Duration::from_millis(500);
        let client = OpenAiResponsesClient::new(config).expect("test client should build");
        let request = ModelRequest {
            model: "gpt-test".to_string(),
            reasoning_effort: None,
            instructions: "answer".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "question".to_string(),
            }],
            prompt_cache: None,
            transport_session_key: None,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: false,
            tool_choice: ModelToolChoice::None,
            max_tool_calls: None,
            max_output_tokens: None,
            response_format: None,
        };

        let events = client
            .stream(request)
            .await
            .expect("SSE response headers should arrive")
            .try_collect::<Vec<_>>()
            .await
            .expect("active SSE stream should outlive the response-header timeout");

        assert!(events.iter().any(|event| {
            matches!(event, ModelEvent::OutputTextDelta { delta } if delta == "streaming")
        }));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelEvent::Completed { .. }))
        );

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn auto_cache_mode_falls_back_when_provider_rejects_breakpoints() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should have an address");
        let state = CacheProbeServerState::default();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/responses", post(reject_explicit_cache_then_stream))
                    .with_state(server_state),
            )
            .await
            .expect("test HTTP server should run");
        });

        let mut config = test_config();
        config.endpoint = Url::parse(&format!("http://{address}/v1/responses"))
            .expect("test endpoint should parse");
        config.responses_websockets = false;
        config.prompt_cache_mode = OpenAiPromptCacheMode::Auto;
        let client = OpenAiResponsesClient::new(config).expect("test client should build");
        let request = ModelRequest {
            model: "gpt-test".to_string(),
            reasoning_effort: None,
            instructions: "stable instructions".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "dynamic question".to_string(),
            }],
            prompt_cache: Some(papermachine_protocol::PromptCacheConfig {
                key: "stable-prefix".to_string(),
                strategy: PromptCacheStrategy::Auto,
            }),
            transport_session_key: None,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: false,
            tool_choice: ModelToolChoice::Auto,
            max_tool_calls: None,
            max_output_tokens: None,
            response_format: None,
        };
        let events = client
            .stream(request)
            .await
            .expect("fallback stream should start")
            .try_collect::<Vec<_>>()
            .await
            .expect("fallback stream should complete");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    transport: ModelTransport::HttpSse,
                    prompt_cache_mode: PromptCacheMode::Implicit,
                    prompt_cache_breakpoint: false,
                    ..
                }
            }
        )));

        let requests = state.requests.lock().await.clone();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0]
                .pointer("/input/0/content/0/prompt_cache_breakpoint")
                .is_some()
        );
        assert_eq!(requests[1]["stream"], false);
        assert_eq!(requests[2]["stream"], true);
        assert_eq!(requests[2]["instructions"], "stable instructions");
        assert!(requests[2].get("prompt_cache_options").is_none());
        assert!(
            requests[2]["input"][0]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none()
        );

        server.abort();
        let _ = server.await;
    }

    async fn reject_max_tool_calls_then_stream(
        State(state): State<CacheProbeServerState>,
        Json(body): Json<Value>,
    ) -> Response {
        state.requests.lock().await.push(body.clone());
        if body.get("stream").and_then(Value::as_bool) == Some(false) {
            if body.get("max_tool_calls").is_some() {
                return (AxumStatusCode::BAD_GATEWAY, "unsupported max_tool_calls").into_response();
            }
            return Json(json!({"id": "probe-control"})).into_response();
        }
        let events = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1,\"input_tokens_details\":{\"cached_tokens\":0}}}}\n\n"
        );
        Response::builder()
            .status(AxumStatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(events))
            .expect("test SSE response should build")
    }

    async fn accept_but_violate_max_tool_calls(
        State(state): State<CacheProbeServerState>,
        Json(body): Json<Value>,
    ) -> Response {
        state.requests.lock().await.push(body.clone());
        if body.get("stream").and_then(Value::as_bool) == Some(false) {
            return Json(json!({"id": "probe-accepted"})).into_response();
        }
        let events = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":1,\"input_tokens_details\":{\"cached_tokens\":0}}}}\n\n"
        );
        Response::builder()
            .status(AxumStatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(events))
            .expect("test SSE response should build")
    }

    #[tokio::test]
    async fn max_tool_calls_falls_back_when_provider_rejects_the_field() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should have an address");
        let state = CacheProbeServerState::default();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/responses", post(reject_max_tool_calls_then_stream))
                    .with_state(server_state),
            )
            .await
            .expect("test HTTP server should run");
        });

        let mut config = test_config();
        config.endpoint = Url::parse(&format!("http://{address}/v1/responses"))
            .expect("test endpoint should parse");
        config.responses_websockets = false;
        let client = OpenAiResponsesClient::new(config).expect("test client should build");
        let request = ModelRequest {
            model: "gpt-test".to_string(),
            reasoning_effort: None,
            instructions: "research carefully".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "question".to_string(),
            }],
            prompt_cache: None,
            transport_session_key: None,
            tools: Vec::new(),
            hosted_tools: vec![papermachine_protocol::HostedTool::WebSearch],
            web_search_context_size: None,
            parallel_tool_calls: true,
            tool_choice: ModelToolChoice::Auto,
            max_tool_calls: Some(4),
            max_output_tokens: None,
            response_format: None,
        };
        let events = client
            .stream(request)
            .await
            .expect("fallback stream should start")
            .try_collect::<Vec<_>>()
            .await
            .expect("fallback stream should complete");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    max_tool_calls_mode: MaxToolCallsMode::RuntimeFallback,
                    ..
                }
            }
        )));

        let requests = state.requests.lock().await.clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["max_tool_calls"], 1);
        assert!(requests[1].get("max_tool_calls").is_none());
        assert!(requests[2].get("max_tool_calls").is_none());
        assert_eq!(requests[2]["stream"], true);

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn max_tool_calls_falls_back_after_provider_accepts_but_violates_limit() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should have an address");
        let state = CacheProbeServerState::default();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/responses", post(accept_but_violate_max_tool_calls))
                    .with_state(server_state),
            )
            .await
            .expect("test HTTP server should run");
        });

        let mut config = test_config();
        config.endpoint = Url::parse(&format!("http://{address}/v1/responses"))
            .expect("test endpoint should parse");
        config.responses_websockets = false;
        let client = OpenAiResponsesClient::new(config).expect("test client should build");
        let request = ModelRequest {
            model: "gpt-test".to_string(),
            reasoning_effort: None,
            instructions: "research carefully".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "question".to_string(),
            }],
            prompt_cache: None,
            transport_session_key: None,
            tools: Vec::new(),
            hosted_tools: vec![papermachine_protocol::HostedTool::WebSearch],
            web_search_context_size: None,
            parallel_tool_calls: true,
            tool_choice: ModelToolChoice::Auto,
            max_tool_calls: Some(1),
            max_output_tokens: None,
            response_format: None,
        };
        let first = client
            .stream(request.clone())
            .await
            .expect("first stream should start")
            .try_collect::<Vec<_>>()
            .await
            .expect("first stream should complete");
        assert!(first.iter().any(|event| matches!(
            event,
            ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    max_tool_calls_mode: MaxToolCallsMode::ProviderViolated,
                    ..
                }
            }
        )));

        let second = client
            .stream(request)
            .await
            .expect("fallback stream should start")
            .try_collect::<Vec<_>>()
            .await
            .expect("fallback stream should complete");
        assert!(second.iter().any(|event| matches!(
            event,
            ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    max_tool_calls_mode: MaxToolCallsMode::RuntimeFallback,
                    ..
                }
            }
        )));

        let requests = state.requests.lock().await.clone();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["max_tool_calls"], 1);
        assert_eq!(requests[1]["max_tool_calls"], 1);
        assert!(requests[2].get("max_tool_calls").is_none());

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn output_limited_request_uses_http_without_attempting_websocket() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should have an address");
        let state = CacheProbeServerState::default();
        let server_state = state.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/responses", post(reject_explicit_cache_then_stream))
                    .with_state(server_state),
            )
            .await
            .expect("test HTTP server should run");
        });

        let mut config = test_config();
        config.endpoint = Url::parse(&format!("http://{address}/v1/responses"))
            .expect("test endpoint should parse");
        config.responses_websockets = true;
        let client = OpenAiResponsesClient::new(config).expect("test client should build");
        let request = ModelRequest {
            model: "gpt-test".to_string(),
            reasoning_effort: Some(ReasoningEffort::Medium),
            instructions: "plan briefly".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "question".to_string(),
            }],
            prompt_cache: None,
            transport_session_key: Some("session-with-limit".to_string()),
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: false,
            tool_choice: ModelToolChoice::None,
            max_tool_calls: None,
            max_output_tokens: Some(4_096),
            response_format: None,
        };
        let events = client
            .stream(request)
            .await
            .expect("HTTP stream should start")
            .try_collect::<Vec<_>>()
            .await
            .expect("HTTP stream should complete");
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    transport: ModelTransport::HttpSse,
                    websocket_fallback_reason: Some(reason),
                    ..
                }
            } if reason == "max_output_tokens_requires_http"
        )));
        let requests = state.requests.lock().await.clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["max_output_tokens"], 4_096);

        server.abort();
        let _ = server.await;
    }

    #[test]
    fn parses_streamed_tool_call_and_usage() {
        let tool = parse_event_data(
            &json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "read_file",
                    "arguments": "{\"path\":\"paper.md\"}"
                }
            })
            .to_string(),
        )
        .expect("tool event should parse")
        .expect("tool event should be emitted");
        let ModelEvent::ResponseItemCompleted { item } = tool else {
            panic!("expected a replayable response item");
        };
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "call-1");

        let completed = parse_event_data(
            &json!({
                "type": "response.completed",
                "response": {
                    "usage": {
                        "input_tokens": 20,
                        "output_tokens": 5,
                        "input_tokens_details": {
                            "cached_tokens": 8,
                            "cache_write_tokens": 12
                        }
                    }
                }
            })
            .to_string(),
        )
        .expect("completion should parse")
        .expect("completion should be emitted");
        assert_eq!(
            completed,
            ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 5,
                    cached_input_tokens: 8,
                    cache_write_input_tokens: 12,
                }
            }
        );
    }

    #[test]
    fn incomplete_response_preserves_usage_for_retry_accounting() {
        let error = parse_event_data(
            &json!({
                "type": "response.incomplete",
                "response": {
                    "incomplete_details": {"reason": "max_output_tokens"},
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 32768,
                        "input_tokens_details": {"cached_tokens": 8}
                    }
                }
            })
            .to_string(),
        )
        .expect_err("incomplete response should stop the sample");
        assert!(matches!(
            error,
            ModelError::IncompleteResponse {
                reason,
                usage: TokenUsage {
                    input_tokens: 12,
                    output_tokens: 32_768,
                    cached_input_tokens: 8,
                    ..
                }
            } if reason == "max_output_tokens"
        ));
    }

    #[tokio::test]
    async fn websocket_continuation_sends_only_new_input_items() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should have an address");
        let server = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.expect("test client should connect");
            let mut websocket = accept_async(connection)
                .await
                .expect("test websocket handshake should succeed");

            let first = websocket
                .next()
                .await
                .expect("first request should arrive")
                .expect("first websocket message should be valid");
            let Message::Text(first) = first else {
                panic!("first request should be text");
            };
            let first: Value =
                serde_json::from_str(&first).expect("first request should contain JSON");
            assert_eq!(first["type"], "response.create");
            assert_eq!(first["input"].as_array().map(Vec::len), Some(1));
            assert_eq!(first["tool_choice"], "auto");
            assert!(first.get("previous_response_id").is_none());
            assert!(first.get("stream").is_none());

            let call_item = json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "read_file",
                "arguments": "{\"path\":\"paper.md\"}"
            });
            websocket
                .send(Message::Text(
                    json!({
                        "type": "response.output_item.done",
                        "item": call_item,
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("first output item should send");
            websocket
                .send(Message::Text(completed_event("resp-1").to_string().into()))
                .await
                .expect("first completion should send");

            let second = websocket
                .next()
                .await
                .expect("second request should arrive")
                .expect("second websocket message should be valid");
            let Message::Text(second) = second else {
                panic!("second request should be text");
            };
            let second: Value =
                serde_json::from_str(&second).expect("second request should contain JSON");
            assert_eq!(second["previous_response_id"], "resp-1");
            assert_eq!(second["input"].as_array().map(Vec::len), Some(1));
            assert_eq!(second["input"][0]["type"], "function_call_output");
            assert_eq!(second["input"][0]["call_id"], "call-1");
            assert_eq!(second["tool_choice"], "none");

            websocket
                .send(Message::Text(completed_event("resp-2").to_string().into()))
                .await
                .expect("second completion should send");
            let close = websocket
                .next()
                .await
                .expect("client should close the turn");
            assert!(matches!(close, Ok(Message::Close(_))));
        });

        let mut config = test_config();
        config.endpoint = Url::parse(&format!("http://{address}/v1/responses"))
            .expect("test endpoint should parse");
        let client = OpenAiResponsesClient::new(config).expect("test client should build");
        let first_request = ModelRequest {
            model: "gpt-5.6-sol".to_string(),
            reasoning_effort: None,
            instructions: "Use tools carefully".to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: "Read the paper".to_string(),
            }],
            prompt_cache: Some(papermachine_protocol::PromptCacheConfig {
                key: "prefix-123".to_string(),
                strategy: PromptCacheStrategy::Auto,
            }),
            transport_session_key: Some("turn-123".to_string()),
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: true,
            tool_choice: ModelToolChoice::Auto,
            max_tool_calls: None,
            max_output_tokens: None,
            response_format: None,
        };
        let first_events = client
            .stream(first_request.clone())
            .await
            .expect("first stream should start")
            .try_collect::<Vec<_>>()
            .await
            .expect("first stream should complete");
        let call_item = first_events
            .iter()
            .find_map(|event| match event {
                ModelEvent::ResponseItemCompleted { item } => Some(item.clone()),
                _ => None,
            })
            .expect("first response should contain a call item");

        let mut second_request = first_request;
        second_request
            .input
            .push(ModelInputItem::ResponseItem { item: call_item });
        second_request
            .input
            .push(ModelInputItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: json!({"content": "paper text"}),
            });
        second_request.tool_choice = ModelToolChoice::None;
        let second_events = client
            .stream(second_request)
            .await
            .expect("second stream should start")
            .try_collect::<Vec<_>>()
            .await
            .expect("second stream should complete");
        assert!(second_events.iter().any(|event| matches!(
            event,
            ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    transport: ModelTransport::ResponsesWebsocket,
                    prompt_cache_mode: PromptCacheMode::Implicit,
                    used_previous_response_id: true,
                    continuation_miss_reason: None,
                    ..
                }
            }
        )));
        client.close_transport_session("turn-123").await;

        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("test server should finish")
            .expect("test server should not panic");
    }

    fn completed_event(response_id: &str) -> Value {
        json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": {
                    "input_tokens": 20,
                    "output_tokens": 5,
                    "input_tokens_details": {
                        "cached_tokens": 0,
                        "cache_write_tokens": 20
                    }
                }
            }
        })
    }

    #[test]
    fn codex_settings_resolve_base_url_model_and_redact_key() {
        let directory = tempdir().expect("temporary Codex home should be created");
        fs::write(
            directory.path().join("config.toml"),
            r#"model_provider = "openai"
openai_base_url = "http://127.0.0.1:9876/api"
model = "gpt-test"
model_reasoning_effort = "medium"
disable_response_storage = true
model_context_window = 1000000
"#,
        )
        .expect("Codex config should be written");
        fs::write(
            directory.path().join("auth.json"),
            r#"{"OPENAI_API_KEY":"secret-test-key"}"#,
        )
        .expect("Codex auth should be written");

        let settings = OpenAiResponsesConfig::from_codex_home(directory.path())
            .expect("Codex settings should load");
        assert_eq!(
            settings.client.endpoint.as_str(),
            "http://127.0.0.1:9876/api/responses"
        );
        assert_eq!(settings.model, "gpt-test");
        assert_eq!(settings.model_context_window, 1_000_000);
        assert_eq!(
            settings.client.reasoning_effort,
            Some(OpenAiReasoningEffort::Medium)
        );
        assert!(!settings.client.store_responses);
        let debug = format!("{:?}", settings.client);
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-test-key"));
    }
}
