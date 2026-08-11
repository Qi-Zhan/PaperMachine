//! OpenAI-compatible Chat Completions transport for providers whose native
//! agent API is not the Responses API. PaperMachine normalizes the wire
//! response into the same durable `ModelEvent` stream used by every Agent.

use crate::ModelClient;
use crate::ModelError;
use crate::ModelStream;
use crate::OpenAiReasoningEffort;
use async_trait::async_trait;
use eventsource_stream::Eventsource;
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
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::TokenUsage;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use url::Url;

const CHAT_EVENT_BUFFER: usize = 128;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiChatCompatibility {
    #[default]
    Generic,
    Deepseek,
    Glm,
}

impl OpenAiChatCompatibility {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Deepseek => "deepseek",
            Self::Glm => "glm",
        }
    }
}

#[derive(Clone)]
pub struct OpenAiChatConfig {
    pub provider_id: String,
    pub endpoint: Url,
    pub api_key: String,
    pub organization: Option<String>,
    pub project: Option<String>,
    pub max_request_retries: u32,
    pub request_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub reasoning_effort: Option<OpenAiReasoningEffort>,
    pub compatibility: OpenAiChatCompatibility,
}

impl fmt::Debug for OpenAiChatConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiChatConfig")
            .field("provider_id", &self.provider_id)
            .field("endpoint", &self.endpoint)
            .field("api_key", &"<redacted>")
            .field("organization", &self.organization)
            .field("project", &self.project)
            .field("max_request_retries", &self.max_request_retries)
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("compatibility", &self.compatibility)
            .finish()
    }
}

pub(crate) fn chat_completions_endpoint(base_url: &str) -> Result<Url, ModelError> {
    let parsed = base_url
        .parse::<Url>()
        .map_err(|error| ModelError::Configuration(format!("invalid model base URL: {error}")))?;
    if parsed
        .path()
        .trim_end_matches('/')
        .ends_with("/chat/completions")
    {
        return Ok(parsed);
    }
    let mut directory = parsed;
    let path = format!("{}/", directory.path().trim_end_matches('/'));
    directory.set_path(&path);
    directory.join("chat/completions").map_err(|error| {
        ModelError::Configuration(format!("invalid Chat Completions base URL: {error}"))
    })
}

#[derive(Clone)]
pub struct OpenAiChatClient {
    http: reqwest::Client,
    config: OpenAiChatConfig,
}

impl OpenAiChatClient {
    pub fn new(config: OpenAiChatConfig) -> Result<Self, ModelError> {
        if config.api_key.trim().is_empty() {
            return Err(ModelError::Configuration(
                "model provider API key must not be empty".to_string(),
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
        Ok(Self { http, config })
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
}

#[async_trait]
impl ModelClient for OpenAiChatClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        let upstream_model = request.model.clone();
        let prompt_cache_key = request.prompt_cache.as_ref().map(|cache| cache.key.clone());
        let body = request_body(&request, &self.config)?;
        let response = self.request(&body).await?;
        let (sender, receiver) = mpsc::channel(CHAT_EVENT_BUFFER);
        let idle_timeout = self.config.stream_idle_timeout;
        tokio::spawn(async move {
            forward_chat_stream(response, idle_timeout, sender).await;
        });
        let metadata = ModelEvent::RequestMetadata {
            metadata: ModelRequestMetadata {
                provider: Some(self.config.provider_id.clone()),
                api: Some("open_ai_chat_completions".to_string()),
                model_profile: None,
                upstream_model: Some(upstream_model),
                transport: ModelTransport::HttpSse,
                prompt_cache_mode: PromptCacheMode::Implicit,
                prompt_cache_key,
                prompt_cache_breakpoint: false,
                used_previous_response_id: false,
                continuation_miss_reason: Some("chat_completions_stateless".to_string()),
                websocket_fallback_reason: None,
            },
        };
        Ok(stream::once(async move { Ok(metadata) })
            .chain(ReceiverStream::new(receiver))
            .boxed())
    }
}

fn request_retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    let base_ms = 1_000_u64.saturating_mul(1_u64 << shift);
    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::from(duration.subsec_millis()) % 501)
        .unwrap_or_default();
    Duration::from_millis(base_ms.saturating_add(jitter_ms))
}

fn request_body(request: &ModelRequest, config: &OpenAiChatConfig) -> Result<Value, ModelError> {
    if !request.hosted_tools.is_empty() {
        return Err(ModelError::Configuration(format!(
            "provider {:?} does not support hosted tools through Chat Completions",
            config.provider_id
        )));
    }
    let mut body = json!({
        "model": request.model,
        "messages": chat_messages(request)?,
        "stream": true,
    });
    let tools = if request.tool_choice == ModelToolChoice::None {
        Vec::new()
    } else {
        request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect::<Vec<_>>()
    };
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!(match request.tool_choice {
            ModelToolChoice::Auto => "auto",
            ModelToolChoice::None => "none",
        });
        if config.compatibility == OpenAiChatCompatibility::Generic {
            body["parallel_tool_calls"] = Value::Bool(request.parallel_tool_calls);
        }
        if config.compatibility == OpenAiChatCompatibility::Glm {
            body["tool_stream"] = Value::Bool(true);
        }
    }
    if config.compatibility != OpenAiChatCompatibility::Glm {
        body["stream_options"] = json!({"include_usage": true});
    }
    let reasoning_effort = request
        .reasoning_effort
        .or_else(|| config.reasoning_effort.map(protocol_reasoning_effort));
    if let Some(reasoning_effort) = reasoning_effort {
        match config.compatibility {
            OpenAiChatCompatibility::Generic => {
                body["reasoning_effort"] = json!(reasoning_effort.as_str());
            }
            OpenAiChatCompatibility::Deepseek | OpenAiChatCompatibility::Glm => {
                if reasoning_effort == ReasoningEffort::None {
                    body["thinking"] = json!({"type": "disabled"});
                } else {
                    body["thinking"] = json!({"type": "enabled"});
                    body["reasoning_effort"] = json!(reasoning_effort.as_str());
                }
            }
        }
    }
    if let Some(format) = &request.response_format {
        body["response_format"] = match config.compatibility {
            OpenAiChatCompatibility::Generic => json!({
                "type": "json_schema",
                "json_schema": {
                    "name": format.name,
                    "schema": format.schema,
                    "strict": format.strict,
                }
            }),
            OpenAiChatCompatibility::Deepseek | OpenAiChatCompatibility::Glm => {
                json!({"type": "json_object"})
            }
        };
    }
    Ok(body)
}

fn chat_messages(request: &ModelRequest) -> Result<Vec<Value>, ModelError> {
    let mut messages = Vec::new();
    if !request.instructions.trim().is_empty() {
        messages.push(json!({"role": "system", "content": request.instructions}));
    }
    let mut pending_reasoning = None;
    for item in &request.input {
        match item {
            ModelInputItem::Message { role, content } => {
                let assistant = matches!(role, MessageRole::Assistant);
                if !assistant {
                    flush_pending_reasoning(&mut messages, &mut pending_reasoning);
                }
                let role = match role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Developer => "system",
                };
                let mut message = json!({"role": role, "content": content});
                if assistant && let Some(reasoning) = pending_reasoning.take() {
                    message["reasoning_content"] = Value::String(reasoning);
                }
                messages.push(message);
            }
            ModelInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => append_tool_call(
                &mut messages,
                &mut pending_reasoning,
                call_id,
                name,
                arguments,
            ),
            ModelInputItem::FunctionCallOutput { call_id, output } => {
                flush_pending_reasoning(&mut messages, &mut pending_reasoning);
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": tool_output_content(output),
                }));
            }
            ModelInputItem::ResponseItem { item } => match item_type(item) {
                "reasoning" => {
                    if let Some(reasoning) = response_item_text(item) {
                        pending_reasoning
                            .get_or_insert_with(String::new)
                            .push_str(&reasoning);
                    }
                }
                "message" => {
                    if let Some(content) = response_item_text(item) {
                        let mut message = json!({"role": "assistant", "content": content});
                        if let Some(reasoning) = pending_reasoning.take() {
                            message["reasoning_content"] = Value::String(reasoning);
                        }
                        messages.push(message);
                    }
                }
                "function_call" => {
                    let call_id = required_item_string(item, "call_id")?;
                    let name = required_item_string(item, "name")?;
                    let arguments = required_item_string(item, "arguments")?;
                    append_tool_call(
                        &mut messages,
                        &mut pending_reasoning,
                        call_id,
                        name,
                        arguments,
                    );
                }
                _ => {}
            },
        }
    }
    flush_pending_reasoning(&mut messages, &mut pending_reasoning);
    Ok(messages)
}

fn flush_pending_reasoning(messages: &mut Vec<Value>, pending_reasoning: &mut Option<String>) {
    if let Some(reasoning) = pending_reasoning.take() {
        messages.push(json!({
            "role": "assistant",
            "content": Value::Null,
            "reasoning_content": reasoning,
        }));
    }
}

fn append_tool_call(
    messages: &mut Vec<Value>,
    pending_reasoning: &mut Option<String>,
    call_id: &str,
    name: &str,
    arguments: &str,
) {
    let can_append = messages
        .last()
        .is_some_and(|message| message.get("role").and_then(Value::as_str) == Some("assistant"));
    if !can_append {
        let mut message = json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [],
        });
        if let Some(reasoning) = pending_reasoning.take() {
            message["reasoning_content"] = Value::String(reasoning);
        }
        messages.push(message);
    } else if let Some(reasoning) = pending_reasoning.take()
        && messages
            .last()
            .and_then(|message| message.get("reasoning_content"))
            .is_none()
        && let Some(message) = messages.last_mut()
    {
        message["reasoning_content"] = Value::String(reasoning);
    }
    let message = messages
        .last_mut()
        .expect("assistant message must exist before appending a tool call");
    if !message.get("tool_calls").is_some_and(Value::is_array) {
        message["tool_calls"] = Value::Array(Vec::new());
    }
    message["tool_calls"]
        .as_array_mut()
        .expect("tool_calls was initialized as an array")
        .push(json!({
            "id": call_id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": replayable_tool_arguments(arguments),
            }
        }));
}

fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn required_item_string<'a>(item: &'a Value, key: &str) -> Result<&'a str, ModelError> {
    item.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ModelError::Configuration(format!("response item missing {key}")))
}

fn response_item_text(item: &Value) -> Option<String> {
    if let Some(content) = item.get("content").and_then(Value::as_str) {
        return (!content.is_empty()).then(|| content.to_string());
    }
    let content = item.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|part| {
            part.get("text")
                .or_else(|| part.get("content"))
                .and_then(Value::as_str)
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn replayable_tool_arguments(arguments: &str) -> &str {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(_)) => arguments,
        _ => "{}",
    }
}

fn tool_output_content(output: &Value) -> String {
    output
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| output.to_string())
}

#[derive(Default)]
struct PartialToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct ChatStreamState {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<u64, PartialToolCall>,
    usage: TokenUsage,
    terminal_finish: bool,
}

async fn forward_chat_stream(
    response: reqwest::Response,
    idle_timeout: Duration,
    sender: mpsc::Sender<Result<ModelEvent, ModelError>>,
) {
    let mut source = response.bytes_stream().eventsource();
    let mut state = ChatStreamState::default();
    loop {
        let next = tokio::time::timeout(idle_timeout, source.next()).await;
        let data = match next {
            Ok(Some(Ok(event))) => event.data,
            Ok(Some(Err(error))) => {
                let _ = sender
                    .send(Err(ModelError::Stream(error.to_string())))
                    .await;
                return;
            }
            Ok(None) => {
                let _ = sender
                    .send(Err(ModelError::Stream(
                        "Chat Completions stream ended before data: [DONE]".to_string(),
                    )))
                    .await;
                return;
            }
            Err(_) => {
                let _ = sender
                    .send(Err(ModelError::Stream(format!(
                        "provider stream was idle for {} seconds",
                        idle_timeout.as_secs()
                    ))))
                    .await;
                return;
            }
        };
        if data.trim().is_empty() {
            continue;
        }
        if data.trim() == "[DONE]" {
            match state.finish() {
                Ok(events) => {
                    for event in events {
                        if sender.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                }
            }
            return;
        }
        let events = match state.consume(&data) {
            Ok(events) => events,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        };
        for event in events {
            if sender.send(Ok(event)).await.is_err() {
                return;
            }
        }
    }
}

impl ChatStreamState {
    fn consume(&mut self, data: &str) -> Result<Vec<ModelEvent>, ModelError> {
        let value: Value = serde_json::from_str(data)
            .map_err(|error| ModelError::Stream(format!("invalid Chat SSE JSON: {error}")))?;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error");
            return Err(ModelError::Provider(message.to_string()));
        }
        if value.get("usage").is_some_and(|usage| !usage.is_null()) {
            self.usage = parse_usage(value.get("usage"));
        }
        let mut events = Vec::new();
        let choices = value
            .get("choices")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if choices.len() > 1 {
            return Err(ModelError::Stream(
                "Chat Completions returned more than one choice".to_string(),
            ));
        }
        for choice in choices {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                self.text.push_str(content);
                events.push(ModelEvent::OutputTextDelta {
                    delta: content.to_string(),
                });
            }
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
                self.reasoning.push_str(reasoning);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (fallback_index, call) in calls.iter().enumerate() {
                    let index = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(fallback_index as u64);
                    let partial = self.tool_calls.entry(index).or_default();
                    if let Some(call_id) = call.get("id").and_then(Value::as_str)
                        && !call_id.is_empty()
                    {
                        partial.call_id = call_id.to_string();
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        partial.name.push_str(name);
                    }
                    if let Some(arguments) =
                        call.pointer("/function/arguments").and_then(Value::as_str)
                    {
                        partial.arguments.push_str(arguments);
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                match reason {
                    "stop" | "tool_calls" => self.terminal_finish = true,
                    "length" => {
                        return Err(ModelError::IncompleteResponse {
                            reason: "max_output_tokens".to_string(),
                            usage: self.usage,
                        });
                    }
                    "insufficient_system_resource" => {
                        return Err(ModelError::Stream(
                            "provider stopped for insufficient system resources".to_string(),
                        ));
                    }
                    other => {
                        return Err(ModelError::Provider(format!(
                            "provider stopped with finish_reason {other:?}"
                        )));
                    }
                }
            }
        }
        Ok(events)
    }

    fn finish(self) -> Result<Vec<ModelEvent>, ModelError> {
        if !self.terminal_finish {
            return Err(ModelError::Stream(
                "Chat Completions stream reached data: [DONE] without a terminal finish_reason"
                    .to_string(),
            ));
        }
        let mut events = Vec::new();
        if !self.reasoning.is_empty() {
            events.push(ModelEvent::ResponseItemCompleted {
                item: json!({"type": "reasoning", "content": self.reasoning}),
            });
        }
        if !self.text.is_empty() {
            events.push(ModelEvent::ResponseItemCompleted {
                item: json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": self.text}],
                }),
            });
        }
        for (_, call) in self.tool_calls {
            if call.call_id.trim().is_empty() || call.name.trim().is_empty() {
                return Err(ModelError::Stream(
                    "streamed function call omitted its id or name".to_string(),
                ));
            }
            events.push(ModelEvent::ResponseItemCompleted {
                item: json!({
                    "type": "function_call",
                    "call_id": call.call_id,
                    "name": call.name,
                    "arguments": call.arguments,
                }),
            });
        }
        events.push(ModelEvent::Completed { usage: self.usage });
        Ok(events)
    }
}

fn parse_usage(usage: Option<&Value>) -> TokenUsage {
    let read = |key: &str| {
        usage
            .and_then(|value| value.get(key))
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    let cached_input_tokens = usage
        .and_then(|value| {
            value
                .get("prompt_cache_hit_tokens")
                .or_else(|| value.pointer("/prompt_tokens_details/cached_tokens"))
        })
        .and_then(Value::as_u64)
        .unwrap_or_default();
    TokenUsage {
        input_tokens: read("prompt_tokens"),
        output_tokens: read("completion_tokens"),
        cached_input_tokens,
        cache_write_input_tokens: 0,
    }
}

const fn protocol_reasoning_effort(value: OpenAiReasoningEffort) -> ReasoningEffort {
    match value {
        OpenAiReasoningEffort::None => ReasoningEffort::None,
        OpenAiReasoningEffort::Low => ReasoningEffort::Low,
        OpenAiReasoningEffort::Medium => ReasoningEffort::Medium,
        OpenAiReasoningEffort::High => ReasoningEffort::High,
        OpenAiReasoningEffort::Xhigh => ReasoningEffort::Xhigh,
        OpenAiReasoningEffort::Max => ReasoningEffort::Max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::post;
    use futures::TryStreamExt;
    use papermachine_protocol::ModelResponseFormat;
    use papermachine_protocol::ToolDefinition;
    use tokio::net::TcpListener;

    fn sample_config(compatibility: OpenAiChatCompatibility) -> OpenAiChatConfig {
        OpenAiChatConfig {
            provider_id: "test-provider".to_string(),
            endpoint: Url::parse("https://models.example.test/v1/chat/completions")
                .expect("test endpoint should parse"),
            api_key: "secret".to_string(),
            organization: None,
            project: None,
            max_request_retries: 0,
            request_timeout: Duration::from_secs(30),
            stream_idle_timeout: Duration::from_secs(30),
            reasoning_effort: Some(OpenAiReasoningEffort::High),
            compatibility,
        }
    }

    fn sample_request() -> ModelRequest {
        ModelRequest {
            model: "upstream-model".to_string(),
            reasoning_effort: None,
            instructions: "system rules".to_string(),
            input: vec![
                ModelInputItem::Message {
                    role: MessageRole::User,
                    content: "question".to_string(),
                },
                ModelInputItem::ResponseItem {
                    item: json!({
                        "type": "reasoning",
                        "content": [{"type": "reasoning_text", "text": "thought"}],
                    }),
                },
                ModelInputItem::ResponseItem {
                    item: json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "checking"}],
                    }),
                },
                ModelInputItem::ResponseItem {
                    item: json!({
                        "type": "function_call",
                        "call_id": "call-1",
                        "name": "read_file",
                        "arguments": "{\"path\":\"notes.md\"}",
                    }),
                },
                ModelInputItem::FunctionCallOutput {
                    call_id: "call-1".to_string(),
                    output: json!("file contents"),
                },
            ],
            prompt_cache: None,
            transport_session_key: None,
            tools: vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                }),
                supports_parallel: true,
            }],
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: true,
            tool_choice: ModelToolChoice::Auto,
            response_format: Some(ModelResponseFormat {
                name: "decision".to_string(),
                schema: json!({
                    "type": "object",
                    "properties": {"status": {"type": "string"}},
                    "required": ["status"],
                }),
                strict: true,
            }),
        }
    }

    #[test]
    fn derives_chat_completions_endpoint() {
        assert_eq!(
            chat_completions_endpoint("https://api.example.test/v1")
                .expect("base URL should resolve")
                .as_str(),
            "https://api.example.test/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_endpoint("https://api.example.test/v1/chat/completions")
                .expect("complete endpoint should stay unchanged")
                .as_str(),
            "https://api.example.test/v1/chat/completions"
        );
    }

    #[test]
    fn deepseek_request_preserves_history_tools_and_reasoning() {
        let body = request_body(
            &sample_request(),
            &sample_config(OpenAiChatCompatibility::Deepseek),
        )
        .expect("request should render");

        assert_eq!(body["model"], "upstream-model");
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "system rules"})
        );
        assert_eq!(
            body["messages"][1],
            json!({"role": "user", "content": "question"})
        );
        assert_eq!(body["messages"][2]["content"], "checking");
        assert_eq!(body["messages"][2]["reasoning_content"], "thought");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call-1");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"notes.md\"}"
        );
        assert_eq!(body["messages"][3]["content"], "file contents");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["tool_choice"], "auto");
        assert!(body.get("parallel_tool_calls").is_none());
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
    }

    #[test]
    fn reasoning_only_history_is_not_dropped_before_retry_guidance() {
        let mut request = sample_request();
        request.input = vec![
            ModelInputItem::ResponseItem {
                item: json!({"type": "reasoning", "content": "partial thought"}),
            },
            ModelInputItem::Message {
                role: MessageRole::User,
                content: "Please return a final answer.".to_string(),
            },
        ];
        let messages = chat_messages(&request).expect("history should render");

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["reasoning_content"], "partial thought");
        assert!(messages[1]["content"].is_null());
        assert_eq!(messages[2]["role"], "user");
    }

    #[test]
    fn generic_request_keeps_json_schema_and_parallel_setting() {
        let body = request_body(
            &sample_request(),
            &sample_config(OpenAiChatCompatibility::Generic),
        )
        .expect("request should render");

        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "decision");
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["required"],
            json!(["status"])
        );
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn glm_request_uses_tool_stream_without_openai_stream_options() {
        let body = request_body(
            &sample_request(),
            &sample_config(OpenAiChatCompatibility::Glm),
        )
        .expect("request should render");

        assert_eq!(body["tool_stream"], true);
        assert!(body.get("stream_options").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
    }

    #[test]
    fn stream_state_normalizes_reasoning_text_tool_calls_and_usage() {
        let mut state = ChatStreamState::default();
        let deltas = state
            .consume(
                r#"{"choices":[{"delta":{"reasoning_content":"think ","content":"hello "},"finish_reason":null}]}"#,
            )
            .expect("first delta should parse");
        assert_eq!(
            deltas,
            vec![ModelEvent::OutputTextDelta {
                delta: "hello ".to_string()
            }]
        );
        state
            .consume(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-7","function":{"name":"read_","arguments":"{\"pa"}}]},"finish_reason":null}]}"#,
            )
            .expect("first tool delta should parse");
        state
            .consume(
                r#"{"choices":[{"delta":{"reasoning_content":"carefully","content":"world","tool_calls":[{"index":0,"function":{"name":"file","arguments":"th\":\"x\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            )
            .expect("terminal delta should parse");
        state
            .consume(
                r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":3}}}"#,
            )
            .expect("usage delta should parse");

        assert_eq!(
            state.finish().expect("finished stream should normalize"),
            vec![
                ModelEvent::ResponseItemCompleted {
                    item: json!({"type": "reasoning", "content": "think carefully"}),
                },
                ModelEvent::ResponseItemCompleted {
                    item: json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello world"}],
                    }),
                },
                ModelEvent::ResponseItemCompleted {
                    item: json!({
                        "type": "function_call",
                        "call_id": "call-7",
                        "name": "read_file",
                        "arguments": "{\"path\":\"x\"}",
                    }),
                },
                ModelEvent::Completed {
                    usage: TokenUsage {
                        input_tokens: 12,
                        output_tokens: 5,
                        cached_input_tokens: 3,
                        cache_write_input_tokens: 0,
                    },
                },
            ]
        );
    }

    #[test]
    fn output_limit_preserves_usage() {
        let mut state = ChatStreamState::default();
        let error = state
            .consume(
                r#"{"choices":[{"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":20,"completion_tokens":8}}"#,
            )
            .expect_err("length finish should be recoverable");
        assert!(matches!(
            error,
            ModelError::IncompleteResponse {
                reason,
                usage: TokenUsage {
                    input_tokens: 20,
                    output_tokens: 8,
                    ..
                }
            } if reason == "max_output_tokens"
        ));
    }

    #[tokio::test]
    async fn streams_sse_without_an_external_provider_request() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from(concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":1}}\n\n",
                        "data: [DONE]\n\n",
                    )))
                    .expect("test response should build")
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("test listener should have an address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test server should run");
        });
        let mut config = sample_config(OpenAiChatCompatibility::Generic);
        config.endpoint = Url::parse(&format!("http://{address}/v1/chat/completions"))
            .expect("test endpoint should parse");
        let client = OpenAiChatClient::new(config).expect("client should build");
        let mut request = sample_request();
        request.input.clear();
        request.tools.clear();
        request.response_format = None;
        let events = client
            .stream(request)
            .await
            .expect("stream should start")
            .try_collect::<Vec<_>>()
            .await
            .expect("stream should finish");
        server.abort();
        let _ = server.await;

        assert!(matches!(
            events.first(),
            Some(ModelEvent::RequestMetadata {
                metadata: ModelRequestMetadata {
                    provider: Some(provider),
                    api: Some(api),
                    transport: ModelTransport::HttpSse,
                    ..
                }
            }) if provider == "test-provider" && api == "open_ai_chat_completions"
        ));
        assert!(events.contains(&ModelEvent::OutputTextDelta {
            delta: "hello".to_string(),
        }));
        assert!(events.contains(&ModelEvent::Completed {
            usage: TokenUsage {
                input_tokens: 4,
                output_tokens: 1,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
        }));
    }
}
