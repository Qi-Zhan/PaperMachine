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
use std::fmt;
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OpenAiPromptCacheMode {
    #[default]
    Auto,
    Implicit,
    Explicit,
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

#[derive(Clone)]
pub struct OpenAiResponsesClient {
    http: reqwest::Client,
    config: OpenAiResponsesConfig,
    websocket_sessions: Arc<Mutex<HashMap<String, WebsocketSessionState>>>,
    websocket_fallback_sessions: Arc<Mutex<HashMap<String, Instant>>>,
    prompt_cache_capabilities: Arc<Mutex<HashMap<String, Arc<OnceCell<PromptCacheCapability>>>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptCacheCapability {
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
        websocket_fallback_reason: Option<String>,
    ) -> Result<ModelStream, ModelError> {
        let upstream_model = request.model.clone();
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
                used_previous_response_id: false,
                continuation_miss_reason: Some("http_transport".to_string()),
                websocket_fallback_reason,
            },
        };
        let stream = stream::once(async move { Ok(metadata) })
            .chain(stream)
            .boxed();
        Ok(stream)
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
    ) -> Result<ModelStream, ModelError> {
        let upstream_model = request.model.clone();
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
                used_previous_response_id: continuation.incremental_input.is_some(),
                continuation_miss_reason: continuation.miss_reason.map(str::to_string),
                websocket_fallback_reason: None,
            },
        };
        let stream = stream::once(async move { Ok(metadata) })
            .chain(ReceiverStream::new(receiver))
            .boxed();
        Ok(stream)
    }
}

#[async_trait]
impl ModelClient for OpenAiResponsesClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        let prompt_cache_mode = self.resolve_prompt_cache_mode(&request).await;
        let transport_session_key = request.transport_session_key.clone();
        let session_uses_http_fallback = match transport_session_key.as_deref() {
            Some(session_key) => {
                let mut fallback_sessions = self.websocket_fallback_sessions.lock().await;
                fallback_sessions
                    .retain(|_, failed_at| failed_at.elapsed() < WEBSOCKET_FALLBACK_TTL);
                fallback_sessions.contains_key(session_key)
            }
            None => false,
        };
        let mut websocket_fallback_reason =
            session_uses_http_fallback.then(|| "session_in_http_fallback_ttl".to_string());
        if self.config.responses_websockets
            && transport_session_key.is_some()
            && !session_uses_http_fallback
        {
            match self
                .stream_websocket(request.clone(), prompt_cache_mode)
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
        self.stream_http(request, prompt_cache_mode, websocket_fallback_reason)
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
        "stream": false
    });
    if marked {
        body["prompt_cache_options"] = json!({"mode": "explicit"});
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
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    fn test_config() -> OpenAiResponsesConfig {
        OpenAiResponsesConfig {
            provider_id: "test-openai".to_string(),
            endpoint: Url::parse("https://api.openai.com/v1/responses")
                .expect("test endpoint should parse"),
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
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
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
    fn websocket_continuation_ignores_per_response_tool_choice() {
        let first = json!({
            "type": "response.create",
            "input": [{"role": "user", "content": "question"}],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });
        let second = json!({
            "type": "response.create",
            "input": [{"role": "user", "content": "question"}],
            "tool_choice": "none",
            "parallel_tool_calls": false,
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
}
