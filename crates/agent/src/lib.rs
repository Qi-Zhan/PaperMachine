//! The sampling, tool execution, and follow-up loop for one research agent.
//!
//! The high-level loop is adapted from OpenAI Codex's `run_turn`: sample the
//! model, execute every requested tool, append outputs, and sample again until
//! the model returns a terminal assistant message. PaperMachine deliberately
//! removes Codex-specific hooks, skills, MCP, plugins, approvals, and UI modes.

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::join_all;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_protocol::ActionAttemptId;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::HostedTool;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelRequest;
use papermachine_protocol::ModelRequestMetadata;
use papermachine_protocol::ModelResponseFormat;
use papermachine_protocol::ModelToolCall;
use papermachine_protocol::ModelToolChoice;
use papermachine_protocol::ProjectId;
use papermachine_protocol::PromptCacheConfig;
use papermachine_protocol::PromptCacheStrategy;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::SessionId;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::ToolEffectDisposition;
use papermachine_protocol::TurnEnvironmentSnapshot;
use papermachine_protocol::TurnId;
use papermachine_protocol::WebSearchContextSize;
use papermachine_protocol::WorkflowId;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolRegistry;
use papermachine_tools::model_visible_tool_result;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const DELTA_EVENT_CHUNK_BYTES: usize = 256;
const DEFAULT_OUTPUT_RESERVE_TOKENS: usize = 16_384;
const AUTO_COMPACT_PERCENT: usize = 90;
const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;
const COMPACTION_PROMPT: &str = "You are performing a context checkpoint compaction. Create a concise handoff summary for the model that will continue this research session. Include current progress and decisions, important constraints and user guidance, verified evidence with source URLs, unresolved questions, and concrete next steps. Preserve exact identifiers, quantities, and caveats that matter. Do not continue the research or call tools; return only the handoff summary.";
const SUMMARY_PREFIX: &str = "Another model worked on this research session and produced the following checkpoint. Use it to continue without repeating completed work:";
const CONTROL_FINISH_PROMPT: &str = "Do not call tools. Synthesize the best self-contained answer supported by the evidence already gathered, follow the control-plane instruction above, and state any remaining limitations.";
const PROMPT_CACHE_KEY_NAMESPACE: &str = "papermachine-session";
const MAX_TOOL_CALL_ID_BYTES: usize = 512;

#[derive(Clone, Debug)]
pub struct AgentTurnRequest {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub workflow_id: Option<WorkflowId>,
    pub action_invocation_id: Option<ActionInvocationId>,
    pub action_attempt_id: Option<ActionAttemptId>,
    pub sandbox_root: PathBuf,
    pub environment: TurnEnvironmentSnapshot,
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub instructions: String,
    pub objective: String,
    pub initial_history: Vec<ModelInputItem>,
    pub initial_usage: TokenUsage,
    pub completed_model_steps: u32,
    pub hosted_search_calls_used: u32,
    pub resume_current_turn: bool,
    pub checkpoint_message: Option<String>,
    pub hosted_tools: Vec<HostedTool>,
    pub web_search_context_size: Option<WebSearchContextSize>,
    pub tools_enabled: bool,
    pub response_format: Option<ModelResponseFormat>,
    pub model_context_window: usize,
}

impl AgentTurnRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_id: ProjectId,
        session_id: SessionId,
        turn_id: TurnId,
        environment: TurnEnvironmentSnapshot,
        sandbox_root: PathBuf,
        model: impl Into<String>,
        instructions: impl Into<String>,
        objective: impl Into<String>,
    ) -> Self {
        Self {
            project_id,
            session_id,
            turn_id,
            workflow_id: None,
            action_invocation_id: None,
            action_attempt_id: None,
            sandbox_root,
            environment,
            model: model.into(),
            reasoning_effort: None,
            instructions: instructions.into(),
            objective: objective.into(),
            initial_history: Vec::new(),
            initial_usage: TokenUsage::default(),
            completed_model_steps: 0,
            hosted_search_calls_used: 0,
            resume_current_turn: false,
            checkpoint_message: None,
            hosted_tools: vec![HostedTool::WebSearch],
            web_search_context_size: None,
            tools_enabled: true,
            response_format: None,
            model_context_window: papermachine_model::DEFAULT_MODEL_CONTEXT_WINDOW,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentTurnResult {
    pub final_message: String,
    pub history: Vec<ModelInputItem>,
    pub steps: u32,
    pub usage: TokenUsage,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    Started {
        objective: String,
        model: String,
    },
    MessageDelta {
        delta: String,
    },
    MessageReset,
    MessageCompleted {
        message: String,
    },
    ToolCallStarted {
        call: ModelToolCall,
        effect_disposition: ToolEffectDisposition,
    },
    ToolExecutionStarted {
        call_id: String,
    },
    ToolCallCompleted {
        call_id: String,
        tool_name: String,
        output: Value,
        duration_ms: u64,
        success: bool,
    },
    ModelStepStarted {
        step: u32,
        input: Value,
    },
    ModelStepCompleted {
        step: u32,
        output: Value,
        usage: TokenUsage,
        duration_ms: u64,
    },
    ModelStepFailed {
        step: u32,
        error: String,
        usage: TokenUsage,
        duration_ms: u64,
    },
    ContextTrimmed {
        removed_items: usize,
    },
    ContextCompactionStarted {
        before_tokens: usize,
    },
    ContextCompactionCompleted {
        before_tokens: usize,
        after_tokens: usize,
        removed_items: usize,
        summary: String,
        usage: TokenUsage,
        duration_ms: u64,
    },
    HostedToolCompleted {
        tool_name: String,
        input: Value,
        output: Value,
    },
    SamplingRetry {
        attempt: u32,
        error: String,
    },
    HistoryCheckpoint {
        history: Vec<ModelInputItem>,
        usage: TokenUsage,
        completed_model_steps: u32,
        hosted_search_calls_used: u32,
        message: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentCheckpoint {
    pub guidance: Vec<String>,
    pub interrupt: Option<String>,
    pub finish: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct AgentCheckpointContext {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub workflow_id: Option<WorkflowId>,
    pub action_invocation_id: Option<ActionInvocationId>,
    pub action_attempt_id: Option<ActionAttemptId>,
}

#[async_trait]
pub trait AgentControlPlane: Send + Sync {
    async fn checkpoint(
        &self,
        context: AgentCheckpointContext,
        cancellation: CancellationToken,
    ) -> Result<AgentCheckpoint, String>;
}

#[derive(Clone, Copy, Default)]
struct NoopControlPlane;

#[async_trait]
impl AgentControlPlane for NoopControlPlane {
    async fn checkpoint(
        &self,
        _context: AgentCheckpointContext,
        _cancellation: CancellationToken,
    ) -> Result<AgentCheckpoint, String> {
        Ok(AgentCheckpoint::default())
    }
}

#[async_trait]
pub trait AgentEventSink: Send + Sync {
    async fn emit(&self, event: AgentEvent) -> Result<(), String>;
}

#[derive(Clone, Default)]
pub struct RecordingAgentEventSink {
    events: Arc<Mutex<Vec<AgentEvent>>>,
}

impl RecordingAgentEventSink {
    pub async fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().await.clone()
    }
}

#[async_trait]
impl AgentEventSink for RecordingAgentEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), String> {
        self.events.lock().await.push(event);
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("agent run was cancelled")]
    Cancelled,
    #[error("agent model-step counter overflowed")]
    StepCounterOverflow,
    #[error("model stream ended without a completion event")]
    IncompleteModelStream,
    #[error("model completed without a message or tool call")]
    EmptyModelResponse,
    #[error("model returned an invalid tool-call identity: {0}")]
    InvalidToolCallIdentity(String),
    #[error(
        "model context window is too small: {window} tokens configured, {reserved} tokens reserved for instructions, tools, and output"
    )]
    ContextWindowTooSmall { window: usize, reserved: usize },
    #[error("turn context requires about {estimated} tokens but only {budget} are available")]
    ContextWindowExceeded { estimated: usize, budget: usize },
    #[error("failed to persist agent event: {0}")]
    EventSink(String),
    #[error("agent control plane failed: {0}")]
    Control(String),
    #[error("Turn interrupted by human: {0}")]
    Interrupted(String),
}

#[derive(Clone)]
pub struct AgentRuntime {
    model: Arc<dyn ModelClient>,
    tools: ToolRegistry,
    events: Arc<dyn AgentEventSink>,
    control: Arc<dyn AgentControlPlane>,
}

impl AgentRuntime {
    pub fn new(
        model: Arc<dyn ModelClient>,
        tools: ToolRegistry,
        events: Arc<dyn AgentEventSink>,
    ) -> Self {
        Self {
            model,
            tools,
            events,
            control: Arc::new(NoopControlPlane),
        }
    }

    pub fn with_control(mut self, control: Arc<dyn AgentControlPlane>) -> Self {
        self.control = control;
        self
    }

    pub async fn run(
        &self,
        request: AgentTurnRequest,
        cancellation: CancellationToken,
    ) -> Result<AgentTurnResult, AgentError> {
        let workspace = tokio::fs::canonicalize(&request.environment.cwd)
            .await
            .map_err(|error| ModelError::Configuration(error.to_string()))?;
        if !workspace.is_dir() {
            return Err(ModelError::Configuration(format!(
                "Session Workspace is not a directory: {}",
                workspace.display()
            ))
            .into());
        }
        self.events
            .emit(AgentEvent::Started {
                objective: request.objective.clone(),
                model: request.model.clone(),
            })
            .await
            .map_err(AgentError::EventSink)?;

        let mut history = request.initial_history.clone();
        if !request.resume_current_turn {
            history.push(ModelInputItem::Message {
                role: MessageRole::User,
                content: request.objective.clone(),
            });
        }
        let mut tool_call_ids = tool_call_ids_from_history(&history);
        let execution_gate = Arc::new(RwLock::new(()));
        let mut total_usage = request.initial_usage;
        let tool_definitions = if request.tools_enabled {
            self.tools.definitions()
        } else {
            Vec::new()
        };
        let hosted_tools = if request.tools_enabled
            && request.environment.authorization.network.hosted_web_search
        {
            request
                .hosted_tools
                .iter()
                .copied()
                .filter(|tool| self.model.supports_hosted_tool(&request.model, *tool))
                .collect()
        } else {
            Vec::new()
        };
        let model_instructions = request.instructions.clone();
        let history_budget = history_token_budget(&request, &tool_definitions)?;
        let compact_trigger = history_budget.saturating_mul(AUTO_COMPACT_PERCENT) / 100;
        let mut control_forced_final = false;
        let mut hosted_search_calls_used = request.hosted_search_calls_used;

        if let Some(message) = request.checkpoint_message.clone() {
            return Ok(AgentTurnResult {
                final_message: message,
                history,
                steps: request.completed_model_steps,
                usage: total_usage,
            });
        }

        let mut step = request
            .completed_model_steps
            .checked_add(1)
            .ok_or(AgentError::StepCounterOverflow)?;
        loop {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let checkpoint = self
                .control
                .checkpoint(
                    AgentCheckpointContext {
                        project_id: request.project_id,
                        session_id: request.session_id,
                        turn_id: request.turn_id,
                        workflow_id: request.workflow_id,
                        action_invocation_id: request.action_invocation_id,
                        action_attempt_id: request.action_attempt_id,
                    },
                    cancellation.clone(),
                )
                .await
                .map_err(AgentError::Control)?;
            if let Some(reason) = checkpoint.interrupt {
                return Err(AgentError::Interrupted(reason));
            }
            if let Some(instruction) = checkpoint.finish {
                control_forced_final = true;
                history.push(ModelInputItem::Message {
                    role: MessageRole::User,
                    content: format!(
                        "The action control plane requested that this action finish now:\n{instruction}"
                    ),
                });
            }
            history.extend(checkpoint.guidance.into_iter().map(|content| {
                ModelInputItem::Message {
                    role: MessageRole::User,
                    content: format!("Human guidance for this running action:\n{content}"),
                }
            }));
            let mut estimated = history.iter().map(estimate_input_item_tokens).sum();
            if estimated >= compact_trigger && history.len() > 2 {
                let before_tokens = estimated;
                let before_items = history.len();
                self.events
                    .emit(AgentEvent::ContextCompactionStarted { before_tokens })
                    .await
                    .map_err(AgentError::EventSink)?;
                let compact_started = Instant::now();
                let (compacted, summary, usage) = self
                    .compact_history(&request, &history, history_budget, &cancellation)
                    .await?;
                total_usage.saturating_add_assign(usage);
                history = compacted;
                estimated = history.iter().map(estimate_input_item_tokens).sum();
                self.events
                    .emit(AgentEvent::ContextCompactionCompleted {
                        before_tokens,
                        after_tokens: estimated,
                        removed_items: before_items.saturating_sub(history.len()),
                        summary,
                        usage,
                        duration_ms: compact_started
                            .elapsed()
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    })
                    .await
                    .map_err(AgentError::EventSink)?;
            }
            let final_sample = control_forced_final;
            let already_has_final_prompt = matches!(
                history.last(),
                Some(ModelInputItem::Message { role: MessageRole::User, content })
                    if content == CONTROL_FINISH_PROMPT
            );
            if final_sample && !already_has_final_prompt {
                history.push(ModelInputItem::Message {
                    role: MessageRole::User,
                    content: CONTROL_FINISH_PROMPT.to_string(),
                });
            }
            let removed_items = trim_history(&mut history, history_budget);
            if removed_items > 0 {
                self.events
                    .emit(AgentEvent::ContextTrimmed { removed_items })
                    .await
                    .map_err(AgentError::EventSink)?;
            }
            let estimated = history.iter().map(estimate_input_item_tokens).sum();
            if estimated > history_budget {
                return Err(AgentError::ContextWindowExceeded {
                    estimated,
                    budget: history_budget,
                });
            }

            self.events
                .emit(AgentEvent::HistoryCheckpoint {
                    history: history.clone(),
                    usage: total_usage,
                    completed_model_steps: step.saturating_sub(1),
                    hosted_search_calls_used,
                    message: None,
                })
                .await
                .map_err(AgentError::EventSink)?;

            let step_tools = if final_sample {
                Vec::new()
            } else {
                tool_definitions.clone()
            };
            let step_hosted_tools = if final_sample {
                Vec::new()
            } else {
                hosted_tools.clone()
            };
            let has_step_tools = !step_tools.is_empty() || !step_hosted_tools.is_empty();
            let mut model_request = ModelRequest {
                model: request.model.clone(),
                reasoning_effort: request.reasoning_effort,
                instructions: model_instructions.clone(),
                input: history.clone(),
                prompt_cache: None,
                transport_session_key: Some(request.session_id.to_string()),
                tools: step_tools,
                hosted_tools: step_hosted_tools,
                web_search_context_size: request.web_search_context_size,
                parallel_tool_calls: has_step_tools,
                tool_choice: if final_sample {
                    ModelToolChoice::None
                } else {
                    ModelToolChoice::Auto
                },
                response_format: request.response_format.clone(),
            };
            model_request.prompt_cache = Some(prompt_cache_config(&model_request));
            let model_step_input = json!({
                "model": request.model.as_str(),
                "reasoning_effort": request.reasoning_effort,
                "access": request.environment.authorization.preset,
                "authorization_sha256": request.environment.authorization_sha256,
                "history_items": history.len(),
                "estimated_history_tokens": estimated,
                "history_budget_tokens": history_budget,
                "prompt_cache_key": model_request.prompt_cache.as_ref().map(|cache| cache.key.as_str()),
                "prompt_cache_strategy": model_request.prompt_cache.as_ref().map(|cache| cache.strategy),
                "transport_session_key": model_request.transport_session_key,
                "transport_preference": "responses_websocket",
                "available_tools": model_request.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
                "available_hosted_tools": model_request.hosted_tools.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
                "web_search_context_size": request.web_search_context_size,
                "tools_enabled": request.tools_enabled,
                "hosted_search_calls_used": hosted_search_calls_used,
                "tool_choice": model_request.tool_choice,
                "final_sample": final_sample,
                "control_forced_final": control_forced_final,
            });
            self.events
                .emit(AgentEvent::ModelStepStarted {
                    step,
                    input: model_step_input,
                })
                .await
                .map_err(AgentError::EventSink)?;
            let sample_started = Instant::now();
            let mut retry = 0_u32;
            let mut retry_usage = TokenUsage::default();
            let mut output_limit_recovery_added = false;
            let mut empty_response_recovery_added = false;
            let SampledStep {
                message,
                calls,
                response_items,
                usage: mut step_usage,
                request_metadata,
            } = loop {
                let error = match self
                    .sample_model_step(model_request.clone(), &cancellation, true)
                    .await
                {
                    Ok(sample) if sample.message.is_empty() && sample.calls.is_empty() => {
                        retry_usage.saturating_add_assign(sample.usage);
                        AgentError::EmptyModelResponse
                    }
                    Ok(sample) => break sample,
                    Err(error) => {
                        if let AgentError::Model(model_error) = &error
                            && let Some(usage) = model_error.incomplete_usage()
                        {
                            retry_usage.saturating_add_assign(usage);
                        }
                        error
                    }
                };
                let output_limit = matches!(
                    &error,
                    AgentError::Model(model_error) if model_error.is_output_limit()
                );
                let empty_response = matches!(&error, AgentError::EmptyModelResponse);
                let retryable = match &error {
                    AgentError::Model(error) => error.is_retryable(),
                    AgentError::IncompleteModelStream | AgentError::EmptyModelResponse => true,
                    _ => false,
                };
                if !retryable || retry >= 2 {
                    if retry_usage != TokenUsage::default() {
                        self.events
                            .emit(AgentEvent::ModelStepFailed {
                                step,
                                error: error.to_string(),
                                usage: retry_usage,
                                duration_ms: sample_started
                                    .elapsed()
                                    .as_millis()
                                    .min(u128::from(u64::MAX))
                                    as u64,
                            })
                            .await
                            .map_err(AgentError::EventSink)?;
                    }
                    return Err(error);
                }
                retry += 1;
                if output_limit {
                    model_request.reasoning_effort = Some(ReasoningEffort::Low);
                    if !output_limit_recovery_added {
                        model_request.input.push(ModelInputItem::Message {
                            role: MessageRole::User,
                            content: "The previous provider response exhausted its output budget before completing. Restart from the original request, omit narrated analysis, and return a concise but complete result now.".to_string(),
                        });
                        output_limit_recovery_added = true;
                    }
                }
                if empty_response {
                    model_request.reasoning_effort = Some(ReasoningEffort::Low);
                    if !empty_response_recovery_added {
                        model_request.input.push(ModelInputItem::Message {
                            role: MessageRole::User,
                            content: "The previous provider response completed without emitting an answer. Return the requested final answer now. Be concise, do not narrate hidden reasoning, and satisfy the original output format exactly.".to_string(),
                        });
                        empty_response_recovery_added = true;
                    }
                }
                self.events
                    .emit(AgentEvent::MessageReset)
                    .await
                    .map_err(AgentError::EventSink)?;
                self.events
                    .emit(AgentEvent::SamplingRetry {
                        attempt: retry,
                        error: error.to_string(),
                    })
                    .await
                    .map_err(AgentError::EventSink)?;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(
                        250_u64.saturating_mul(1_u64 << retry),
                    )) => {}
                    _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                }
            };
            if let Err(error) = reserve_tool_call_ids(&calls, &mut tool_call_ids) {
                step_usage.saturating_add_assign(retry_usage);
                total_usage.saturating_add_assign(step_usage);
                self.events
                    .emit(AgentEvent::ModelStepFailed {
                        step,
                        error: error.to_string(),
                        usage: step_usage,
                        duration_ms: sample_started
                            .elapsed()
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    })
                    .await
                    .map_err(AgentError::EventSink)?;
                return Err(error);
            }
            step_usage.saturating_add_assign(retry_usage);
            total_usage.saturating_add_assign(step_usage);
            hosted_search_calls_used = hosted_search_calls_used.saturating_add(
                response_items
                    .iter()
                    .filter(|item| {
                        item.get("type").and_then(Value::as_str) == Some("web_search_call")
                    })
                    .count()
                    .min(u32::MAX as usize) as u32,
            );
            let model_step_output =
                inspectable_model_output(&message, &response_items, request_metadata.as_ref());
            // Persist provider-hosted tool actions before the model Step is
            // completed so search telemetry and the Session trace stay aligned.
            for item in &response_items {
                if item.get("type").and_then(Value::as_str) == Some("web_search_call") {
                    self.events
                        .emit(AgentEvent::HostedToolCompleted {
                            tool_name: HostedTool::WebSearch.name().to_string(),
                            input: item.get("action").cloned().unwrap_or(Value::Null),
                            output: item.clone(),
                        })
                        .await
                        .map_err(AgentError::EventSink)?;
                }
            }
            self.events
                .emit(AgentEvent::ModelStepCompleted {
                    step,
                    output: model_step_output,
                    usage: step_usage,
                    duration_ms: sample_started
                        .elapsed()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                })
                .await
                .map_err(AgentError::EventSink)?;

            let has_message_item = response_items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("message"));
            let replayed_call_ids = response_items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
                .filter_map(|item| item.get("call_id").and_then(Value::as_str))
                .map(str::to_string)
                .collect::<Vec<_>>();
            history.extend(
                response_items
                    .into_iter()
                    .map(|item| ModelInputItem::ResponseItem { item }),
            );
            if !message.is_empty() {
                if !has_message_item {
                    history.push(ModelInputItem::Message {
                        role: MessageRole::Assistant,
                        content: message.clone(),
                    });
                }
                self.events
                    .emit(AgentEvent::MessageCompleted {
                        message: message.clone(),
                    })
                    .await
                    .map_err(AgentError::EventSink)?;
            }

            for call in &calls {
                if !replayed_call_ids.contains(&call.call_id) {
                    history.push(ModelInputItem::FunctionCall {
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                }
                self.events
                    .emit(AgentEvent::ToolCallStarted {
                        call: call.clone(),
                        effect_disposition: self
                            .tools
                            .effect_disposition(&call.name)
                            .unwrap_or(ToolEffectDisposition::Unknown),
                    })
                    .await
                    .map_err(AgentError::EventSink)?;
            }

            self.events
                .emit(AgentEvent::HistoryCheckpoint {
                    history: history.clone(),
                    usage: total_usage,
                    completed_model_steps: step,
                    hosted_search_calls_used,
                    message: calls.is_empty().then_some(message.clone()),
                })
                .await
                .map_err(AgentError::EventSink)?;

            if calls.is_empty() {
                if message.is_empty() {
                    return Err(AgentError::EmptyModelResponse);
                }
                return Ok(AgentTurnResult {
                    final_message: message,
                    history,
                    steps: step,
                    usage: total_usage,
                });
            }

            for call in &calls {
                self.events
                    .emit(AgentEvent::ToolExecutionStarted {
                        call_id: call.call_id.clone(),
                    })
                    .await
                    .map_err(AgentError::EventSink)?;
            }

            let futures = calls.into_iter().map(|call| {
                self.execute_tool(
                    &request,
                    call,
                    cancellation.clone(),
                    Arc::clone(&execution_gate),
                )
            });
            for outcome in join_all(futures).await {
                self.events
                    .emit(outcome.event)
                    .await
                    .map_err(AgentError::EventSink)?;
                history.push(ModelInputItem::FunctionCallOutput {
                    call_id: outcome.call_id,
                    output: outcome.output,
                });
                self.events
                    .emit(AgentEvent::HistoryCheckpoint {
                        history: history.clone(),
                        usage: total_usage,
                        completed_model_steps: step,
                        hosted_search_calls_used,
                        message: None,
                    })
                    .await
                    .map_err(AgentError::EventSink)?;
            }
            step = step.checked_add(1).ok_or(AgentError::StepCounterOverflow)?;
        }
    }

    async fn sample_model_step(
        &self,
        request: ModelRequest,
        cancellation: &CancellationToken,
        emit_message_events: bool,
    ) -> Result<SampledStep, AgentError> {
        let mut stream = tokio::select! {
            result = self.model.stream(request) => result?,
            _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
        };
        let mut message = String::new();
        let mut pending_delta = String::new();
        let mut calls = Vec::new();
        let mut response_items = Vec::new();
        let mut request_metadata = None;
        let mut usage = TokenUsage::default();
        let mut completed = false;
        loop {
            let next = tokio::select! {
                next = stream.next() => next,
                _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
            };
            let Some(event) = next else {
                break;
            };
            match event? {
                ModelEvent::RequestMetadata { metadata } => {
                    request_metadata = Some(metadata);
                }
                ModelEvent::OutputTextDelta { delta } => {
                    message.push_str(&delta);
                    if emit_message_events {
                        pending_delta.push_str(&delta);
                    }
                    if emit_message_events && pending_delta.len() >= DELTA_EVENT_CHUNK_BYTES {
                        self.events
                            .emit(AgentEvent::MessageDelta {
                                delta: std::mem::take(&mut pending_delta),
                            })
                            .await
                            .map_err(AgentError::EventSink)?;
                    }
                }
                ModelEvent::ToolCallCompleted { call } => calls.push(call),
                ModelEvent::ResponseItemCompleted { item } => {
                    if let Some(call) = tool_call_from_response_item(&item)? {
                        calls.push(call);
                    }
                    response_items.push(item);
                }
                ModelEvent::Completed { usage: event_usage } => {
                    usage = event_usage;
                    completed = true;
                }
            }
        }
        if !completed {
            return Err(AgentError::IncompleteModelStream);
        }
        if emit_message_events && !pending_delta.is_empty() {
            self.events
                .emit(AgentEvent::MessageDelta {
                    delta: pending_delta,
                })
                .await
                .map_err(AgentError::EventSink)?;
        }
        Ok(SampledStep {
            message,
            calls,
            response_items,
            usage,
            request_metadata,
        })
    }

    async fn compact_history(
        &self,
        request: &AgentTurnRequest,
        history: &[ModelInputItem],
        history_budget: usize,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<ModelInputItem>, String, TokenUsage), AgentError> {
        let mut input = history.to_vec();
        let prompt_tokens = estimate_text_tokens(COMPACTION_PROMPT).saturating_add(8);
        trim_history(
            &mut input,
            history_budget.saturating_sub(prompt_tokens).max(1),
        );
        input.push(ModelInputItem::Message {
            role: MessageRole::User,
            content: COMPACTION_PROMPT.to_string(),
        });
        let mut model_request = ModelRequest {
            model: request.model.clone(),
            reasoning_effort: request.reasoning_effort,
            instructions: request.instructions.clone(),
            input,
            prompt_cache: None,
            transport_session_key: Some(request.session_id.to_string()),
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: false,
            tool_choice: ModelToolChoice::Auto,
            response_format: None,
        };
        model_request.prompt_cache = Some(prompt_cache_config(&model_request));
        let sampled = self
            .sample_model_step(model_request, cancellation, false)
            .await?;
        let summary = sampled.message.trim().to_string();
        if summary.is_empty() {
            return Err(AgentError::EmptyModelResponse);
        }
        let user_message_budget = COMPACT_USER_MESSAGE_MAX_TOKENS.min(history_budget / 3);
        let compacted = build_compacted_history(history, &summary, user_message_budget);
        Ok((compacted, summary, sampled.usage))
    }

    async fn execute_tool(
        &self,
        request: &AgentTurnRequest,
        call: ModelToolCall,
        cancellation: CancellationToken,
        execution_gate: Arc<RwLock<()>>,
    ) -> ToolOutcome {
        let started = Instant::now();
        let parsed_arguments = serde_json::from_str::<Value>(&call.arguments);
        let result = match parsed_arguments {
            Ok(arguments) => {
                let context = ToolContext {
                    project_id: request.project_id,
                    session_id: request.session_id,
                    turn_id: request.turn_id,
                    workflow_id: request.workflow_id,
                    action_invocation_id: request.action_invocation_id,
                    action_attempt_id: request.action_attempt_id,
                    effect_id: call.call_id.clone(),
                    sandbox_root: request.sandbox_root.clone(),
                    authorization: request.environment.authorization.clone(),
                    cancellation,
                };
                if self.tools.supports_parallel(&call.name).unwrap_or(false) {
                    let _guard = execution_gate.read().await;
                    self.tools.execute(&call.name, context, arguments).await
                } else {
                    let _guard = execution_gate.write().await;
                    self.tools.execute(&call.name, context, arguments).await
                }
            }
            Err(error) => Err(papermachine_tools::ToolError::InvalidArguments {
                tool: call.name.clone(),
                message: error.to_string(),
            }),
        };

        let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let (output, success) = model_visible_tool_result(result);
        ToolOutcome {
            call_id: call.call_id.clone(),
            output: output.clone(),
            event: AgentEvent::ToolCallCompleted {
                call_id: call.call_id,
                tool_name: call.name,
                output,
                duration_ms,
                success,
            },
        }
    }
}

struct ToolOutcome {
    call_id: String,
    output: Value,
    event: AgentEvent,
}

struct SampledStep {
    message: String,
    calls: Vec<ModelToolCall>,
    response_items: Vec<Value>,
    usage: TokenUsage,
    request_metadata: Option<ModelRequestMetadata>,
}

fn inspectable_model_output(
    message: &str,
    response_items: &[Value],
    request_metadata: Option<&ModelRequestMetadata>,
) -> Value {
    let items = response_items
        .iter()
        .map(|item| {
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            match item_type {
                "reasoning" => json!({
                    "type": "reasoning",
                    "summary": item.get("summary").cloned().unwrap_or_else(|| json!([])),
                }),
                "message" => json!({
                    "type": "message",
                    "content": item.get("content").cloned().unwrap_or(Value::Null),
                }),
                "function_call" => json!({
                    "type": "function_call",
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": item.get("arguments").cloned().unwrap_or(Value::Null),
                }),
                other => json!({"type": other}),
            }
        })
        .collect::<Vec<_>>();
    json!({
        "assistant_message": if message.is_empty() { Value::Null } else { Value::String(message.to_string()) },
        "response_items": items,
        "request": request_metadata,
    })
}

fn prompt_cache_config(request: &ModelRequest) -> PromptCacheConfig {
    let stable_prefix = json!({
        "namespace": PROMPT_CACHE_KEY_NAMESPACE,
        // This is a routing/cache-affinity key, not a content digest. Keep it
        // stable across actions, response schemas, tools, and compaction in one
        // durable Session; the provider still matches reusable content by the
        // actual prompt prefix. Session scoping prevents compatible gateways
        // from returning another concurrent Session's cached completion.
        "session": request.transport_session_key,
        "model": request.model,
    });
    let digest = Sha256::digest(stable_prefix.to_string().as_bytes());
    let hash = hex::encode(digest);
    PromptCacheConfig {
        // Put entropy first as a defensive measure for compatible gateways
        // that truncate or namespace routing keys at punctuation.
        key: format!("{}-session", &hash[..32]),
        strategy: PromptCacheStrategy::Auto,
    }
}

fn history_token_budget(
    request: &AgentTurnRequest,
    tools: &[ToolDefinition],
) -> Result<usize, AgentError> {
    let static_tokens = estimate_text_tokens(&request.instructions).saturating_add(
        tools
            .iter()
            .map(|tool| {
                estimate_text_tokens(&tool.name)
                    .saturating_add(estimate_text_tokens(&tool.description))
                    .saturating_add(estimate_text_tokens(&tool.input_schema.to_string()))
                    .saturating_add(16)
            })
            .sum::<usize>(),
    );
    let output_reserve =
        (request.model_context_window / 8).clamp(1_024, DEFAULT_OUTPUT_RESERVE_TOKENS);
    let safety_reserve = (request.model_context_window / 20).clamp(256, 4_096);
    let reserved = static_tokens
        .saturating_add(output_reserve)
        .saturating_add(safety_reserve);
    request
        .model_context_window
        .checked_sub(reserved)
        .ok_or(AgentError::ContextWindowTooSmall {
            window: request.model_context_window,
            reserved,
        })
}

fn estimate_text_tokens(text: &str) -> usize {
    let mut ascii_bytes = 0_usize;
    let mut non_ascii_chars = 0_usize;
    for character in text.chars() {
        if character.is_ascii() {
            ascii_bytes += 1;
        } else {
            non_ascii_chars += 1;
        }
    }
    ascii_bytes.div_ceil(4).saturating_add(non_ascii_chars)
}

fn estimate_input_item_tokens(item: &ModelInputItem) -> usize {
    let content = match item {
        ModelInputItem::Message { content, .. } => estimate_text_tokens(content),
        ModelInputItem::FunctionCall {
            name, arguments, ..
        } => estimate_text_tokens(name).saturating_add(estimate_text_tokens(arguments)),
        ModelInputItem::FunctionCallOutput { output, .. } => {
            estimate_text_tokens(&output.to_string())
        }
        ModelInputItem::ResponseItem { item } => estimate_text_tokens(&item.to_string()),
    };
    content.saturating_add(8)
}

fn trim_history(history: &mut Vec<ModelInputItem>, max_tokens: usize) -> usize {
    let total = history
        .iter()
        .map(estimate_input_item_tokens)
        .sum::<usize>();
    if total <= max_tokens || history.len() <= 2 {
        return 0;
    }

    let first = history[0].clone();
    let mut tail = Vec::new();
    let mut tail_tokens = estimate_input_item_tokens(&first);
    for item in history.iter().skip(1).rev() {
        let size = estimate_input_item_tokens(item);
        if tail_tokens.saturating_add(size) > max_tokens && !tail.is_empty() {
            break;
        }
        tail_tokens = tail_tokens.saturating_add(size);
        tail.push(item.clone());
    }
    tail.reverse();
    while matches!(
        tail.first(),
        Some(ModelInputItem::FunctionCallOutput { .. })
    ) {
        tail.remove(0);
    }
    let removed = history.len().saturating_sub(1 + tail.len());
    history.clear();
    history.push(first);
    history.extend(tail);
    removed
}

fn build_compacted_history(
    history: &[ModelInputItem],
    summary: &str,
    user_message_budget: usize,
) -> Vec<ModelInputItem> {
    let mut selected = Vec::new();
    let mut used_tokens = 0_usize;
    for item in history.iter().rev() {
        let ModelInputItem::Message {
            role: MessageRole::User,
            content,
        } = item
        else {
            continue;
        };
        let tokens = estimate_text_tokens(content).saturating_add(8);
        if used_tokens.saturating_add(tokens) > user_message_budget && !selected.is_empty() {
            break;
        }
        if tokens <= user_message_budget.saturating_sub(used_tokens) {
            selected.push(item.clone());
            used_tokens = used_tokens.saturating_add(tokens);
        }
    }
    selected.reverse();
    selected.push(ModelInputItem::Message {
        role: MessageRole::User,
        content: format!("{SUMMARY_PREFIX}\n\n{summary}"),
    });
    selected
}

fn tool_call_from_response_item(item: &Value) -> Result<Option<ModelToolCall>, ModelError> {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return Ok(None);
    }
    let field = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ModelError::Stream(format!("function call missing {key}")))
    };
    Ok(Some(ModelToolCall {
        call_id: field("call_id")?,
        name: field("name")?,
        arguments: field("arguments")?,
    }))
}

fn tool_call_ids_from_history(history: &[ModelInputItem]) -> HashSet<String> {
    history
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::FunctionCall { call_id, .. }
            | ModelInputItem::FunctionCallOutput { call_id, .. } => Some(call_id.clone()),
            ModelInputItem::ResponseItem { item }
                if item.get("type").and_then(Value::as_str) == Some("function_call") =>
            {
                item.get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }
            _ => None,
        })
        .collect()
}

fn reserve_tool_call_ids(
    calls: &[ModelToolCall],
    used: &mut HashSet<String>,
) -> Result<(), AgentError> {
    let mut current = HashSet::new();
    for call in calls {
        if call.call_id.trim().is_empty() {
            return Err(AgentError::InvalidToolCallIdentity(
                "call ID is empty".to_string(),
            ));
        }
        if call.call_id.len() > MAX_TOOL_CALL_ID_BYTES {
            return Err(AgentError::InvalidToolCallIdentity(format!(
                "call ID exceeds {MAX_TOOL_CALL_ID_BYTES} bytes"
            )));
        }
        if used.contains(&call.call_id) || !current.insert(call.call_id.clone()) {
            return Err(AgentError::InvalidToolCallIdentity(format!(
                "duplicate call ID {:?}",
                call.call_id
            )));
        }
    }
    used.extend(current);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_trim_keeps_objective_and_drops_orphan_tool_output() {
        let mut history = vec![
            ModelInputItem::Message {
                role: MessageRole::User,
                content: "objective".to_string(),
            },
            ModelInputItem::Message {
                role: MessageRole::Assistant,
                content: "a".repeat(100),
            },
            ModelInputItem::FunctionCallOutput {
                call_id: "old".to_string(),
                output: json!({"long": "b".repeat(100)}),
            },
            ModelInputItem::Message {
                role: MessageRole::Assistant,
                content: "final".to_string(),
            },
        ];

        let removed = trim_history(&mut history, 20);
        assert!(removed > 0);
        assert!(matches!(history[0], ModelInputItem::Message { .. }));
        assert!(!matches!(
            history.get(1),
            Some(ModelInputItem::FunctionCallOutput { .. })
        ));
    }

    #[test]
    fn token_estimate_is_conservative_for_ascii_and_cjk() {
        assert_eq!(estimate_text_tokens("abcdefgh"), 2);
        assert_eq!(estimate_text_tokens("研究工作台"), 5);
    }

    #[test]
    fn prompt_cache_key_is_stable_within_session_and_isolated_between_sessions() {
        let build = |session: &str, question: &str, instructions: &str| ModelRequest {
            model: "gpt-test".to_string(),
            reasoning_effort: None,
            instructions: instructions.to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: question.to_string(),
            }],
            prompt_cache: None,
            transport_session_key: Some(session.to_string()),
            tools: vec![ToolDefinition {
                name: "search".to_string(),
                description: "Search evidence".to_string(),
                input_schema: json!({"type": "object"}),
                supports_parallel: true,
            }],
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: true,
            tool_choice: ModelToolChoice::Auto,
            response_format: None,
        };
        let first = prompt_cache_config(&build("session-a", "question A", "shared instructions"));
        let same_session =
            prompt_cache_config(&build("session-a", "question B", "shared instructions"));
        let second = prompt_cache_config(&build("session-b", "question A", "shared instructions"));
        let changed =
            prompt_cache_config(&build("session-a", "question A", "different instructions"));
        let mut changed_shape = build("session-a", "question A", "shared instructions");
        changed_shape.tools.clear();
        changed_shape.response_format = Some(ModelResponseFormat {
            name: "audit_result".to_string(),
            schema: json!({"type": "object"}),
            strict: false,
        });
        let changed_shape = prompt_cache_config(&changed_shape);

        assert_eq!(first, same_session);
        assert_eq!(first, changed);
        assert_eq!(first, changed_shape);
        assert_ne!(first, second);
        assert!(first.key.ends_with("-session"));
        assert!(!first.key.contains("session-a"));
    }

    #[test]
    fn inspectable_model_output_excludes_encrypted_reasoning() {
        let output = inspectable_model_output(
            "answer",
            &[json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "checked evidence"}],
                "encrypted_content": "secret-replay-state"
            })],
            None,
        );
        let serialized = output.to_string();
        assert!(serialized.contains("checked evidence"));
        assert!(!serialized.contains("secret-replay-state"));
    }
}
