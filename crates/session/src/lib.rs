//! Durable multi-turn sessions built around the Codex sample/tool/follow-up loop.

use async_trait::async_trait;
use papermachine_agent::AgentCheckpoint;
use papermachine_agent::AgentCheckpointContext;
use papermachine_agent::AgentControlPlane;
use papermachine_agent::AgentError;
use papermachine_agent::AgentEvent;
use papermachine_agent::AgentEventSink;
use papermachine_agent::AgentRuntime;
use papermachine_agent::AgentTurnRequest;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_protocol::ActionAttemptId;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::Agent;
use papermachine_protocol::AgentId;
use papermachine_protocol::ContextReplacementReason;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::ModelContextMutation;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelResponseFormat;
use papermachine_protocol::PromptLayer;
use papermachine_protocol::PromptLayerKind;
use papermachine_protocol::PromptSnapshot;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::SessionId;
use papermachine_protocol::SessionStatus;
use papermachine_protocol::SessionUsage;
use papermachine_protocol::StepId;
use papermachine_protocol::StepKind;
use papermachine_protocol::StepStatus;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::Turn;
use papermachine_protocol::TurnId;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WebSearchContextSize;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_skills::ResolvedSkills;
use papermachine_skills::SkillError;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_store::StoreHandle;
use papermachine_store::TurnContextCheckpoint;
use papermachine_tools::ToolCatalog;
use papermachine_tools::ToolError;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const RUNTIME_SYSTEM_PROMPT: &str = "You are an agent working in a persistent PaperMachine Session. Complete the current request using the available tools and prior Session context. Preserve exact evidence and provenance, distinguish verified observations from inference, and state material uncertainty or limitations. Runtime permissions are enforced by code; never claim capabilities or completed tool actions that are not present in the Session history. If a recovered tool result is aborted, inspect durable Workspace or external state before deciding whether any effect should be attempted again.";

#[derive(Clone)]
pub struct TurnRuntime {
    inner: Arc<TurnRuntimeInner>,
}

struct TurnRuntimeInner {
    store: StoreHandle,
    model: Arc<dyn ModelClient>,
    tools: ToolCatalog,
    skills: Arc<ProjectSkillCatalog>,
    default_model: String,
    model_context_window: usize,
    permits: Arc<Semaphore>,
    active: Mutex<HashMap<TurnId, CancellationToken>>,
}

#[derive(Clone, Debug)]
pub struct TurnRuntimeConfig {
    pub default_model: String,
    pub model_context_window: usize,
    pub max_concurrent_turns: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ActionTurnContext {
    pub action_invocation_id: ActionInvocationId,
    pub action_attempt_id: ActionAttemptId,
}

#[derive(Clone, Debug)]
pub struct PromptLayerInput {
    pub kind: PromptLayerKind,
    pub name: String,
    pub source: String,
    pub content: String,
}

impl PromptLayerInput {
    pub fn new(
        kind: PromptLayerKind,
        name: impl Into<String>,
        source: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            name: name.into(),
            source: source.into(),
            content: content.into(),
        }
    }
}

impl TurnRuntime {
    pub fn new(
        store: StoreHandle,
        model: Arc<dyn ModelClient>,
        tools: ToolCatalog,
        skills: Arc<ProjectSkillCatalog>,
        config: TurnRuntimeConfig,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(config.max_concurrent_turns.max(1)));
        Self::new_with_permits(store, model, tools, skills, config, permits)
    }

    pub fn new_with_permits(
        store: StoreHandle,
        model: Arc<dyn ModelClient>,
        tools: ToolCatalog,
        skills: Arc<ProjectSkillCatalog>,
        config: TurnRuntimeConfig,
        permits: Arc<Semaphore>,
    ) -> Self {
        Self {
            inner: Arc::new(TurnRuntimeInner {
                store,
                model,
                tools,
                skills,
                default_model: config.default_model,
                model_context_window: config.model_context_window,
                permits,
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute_action_attempt(
        &self,
        agent_id: AgentId,
        input: impl Into<String>,
        model_override: Option<&str>,
        prompt_layers: Vec<PromptLayerInput>,
        reasoning_effort: Option<ReasoningEffort>,
        requested_tools: Vec<String>,
        tools_enabled: bool,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        context: ActionTurnContext,
        cancellation: CancellationToken,
    ) -> Result<Turn, TurnRuntimeError> {
        let turn = self
            .prepare_turn(
                agent_id,
                input,
                model_override,
                prompt_layers,
                reasoning_effort,
                requested_tools,
                tools_enabled,
                web_search_context_size,
                response_format,
                context.action_attempt_id,
            )
            .await?;
        self.run_tracked_turn(turn.id, context, cancellation)
            .await?;
        Ok(self
            .inner
            .store
            .call(move |store| store.get_turn(turn.id))
            .await?)
    }

    pub async fn resume_action_attempt(
        &self,
        turn_id: TurnId,
        context: ActionTurnContext,
        cancellation: CancellationToken,
    ) -> Result<Turn, TurnRuntimeError> {
        self.run_tracked_turn(turn_id, context, cancellation)
            .await?;
        Ok(self
            .inner
            .store
            .call(move |store| store.get_turn(turn_id))
            .await?)
    }

    async fn run_tracked_turn(
        &self,
        turn_id: TurnId,
        action_context: ActionTurnContext,
        parent_cancellation: CancellationToken,
    ) -> Result<(), TurnRuntimeError> {
        let cancellation = parent_cancellation.child_token();
        {
            let mut active = self.inner.active.lock().await;
            if active.contains_key(&turn_id) {
                return Err(TurnRuntimeError::Scheduling(format!(
                    "Turn {turn_id} is already running"
                )));
            }
            active.insert(turn_id, cancellation.clone());
        }
        let result = run_scheduled_turn(
            Arc::clone(&self.inner),
            turn_id,
            action_context,
            cancellation,
        )
        .await;
        self.inner.active.lock().await.remove(&turn_id);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_turn(
        &self,
        agent_id: AgentId,
        input: impl Into<String>,
        model_override: Option<&str>,
        prompt_layers: Vec<PromptLayerInput>,
        reasoning_effort: Option<ReasoningEffort>,
        requested_tools: Vec<String>,
        tools_enabled: bool,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        action_attempt_id: ActionAttemptId,
    ) -> Result<Turn, TurnRuntimeError> {
        let agent = self
            .inner
            .store
            .call(move |store| store.get_agent(agent_id))
            .await?;
        let session_id = agent.session_id;
        let session = self
            .inner
            .store
            .call(move |store| store.get_session(session_id))
            .await?;
        let model = if model_override.is_some_and(|model| !model.trim().is_empty()) {
            model_override.unwrap_or_default().trim().to_string()
        } else if agent.model.trim().is_empty() {
            self.inner.default_model.clone()
        } else {
            agent.model.clone()
        };
        let resolved = self
            .inner
            .skills
            .resolve(session.project_id, &agent.skills)
            .await?;
        let project_id = session.project_id;
        let project_prompt = self
            .inner
            .store
            .call(move |store| store.get_project_system_prompt(project_id))
            .await?;
        let prompt = build_prompt_snapshot(&agent, project_prompt, prompt_layers, &resolved);
        let tool_set = self.inner.tools.materialize_action_tools(
            &requested_tools,
            agent.access,
            tools_enabled,
        )?;
        let model_route = self.inner.model.resolve_route_snapshot(
            &model,
            reasoning_effort,
            self.inner.model_context_window,
        )?;
        let access = agent.access;
        let snapshots = resolved.snapshots;
        let input = input.into();
        Ok(self
            .inner
            .store
            .call(move |store| {
                store.create_turn_for_attempt(
                    action_attempt_id,
                    agent_id,
                    input,
                    model_route,
                    prompt,
                    tools_enabled,
                    access,
                    tool_set,
                    web_search_context_size,
                    response_format,
                    snapshots,
                )
            })
            .await?)
    }

    pub async fn cancel(&self, turn_id: TurnId) -> Result<(), TurnRuntimeError> {
        if let Some(cancellation) = self.inner.active.lock().await.get(&turn_id) {
            cancellation.cancel();
            return Ok(());
        }
        let turn = self
            .inner
            .store
            .call(move |store| store.get_turn(turn_id))
            .await?;
        if matches!(
            turn.status,
            TurnStatus::Queued | TurnStatus::Running | TurnStatus::Paused
        ) {
            self.inner
                .store
                .call(move |store| store.cancel_turn(turn_id))
                .await?;
            return Ok(());
        }
        Err(TurnRuntimeError::TerminalTurn(turn_id))
    }
}

async fn run_scheduled_turn(
    inner: Arc<TurnRuntimeInner>,
    turn_id: TurnId,
    action_context: ActionTurnContext,
    cancellation: CancellationToken,
) -> Result<(), TurnRuntimeError> {
    let sandbox = inner
        .store
        .call(move |store| store.get_turn(turn_id))
        .await
        .ok()
        .map(|turn| {
            inner
                .store
                .managed_root()
                .join("runtime/sandboxes")
                .join(turn.agent_id.to_string())
                .join(turn.id.to_string())
        });
    let result =
        run_scheduled_turn_inner(Arc::clone(&inner), turn_id, action_context, cancellation).await;
    if let Some(sandbox) = sandbox
        && tokio::fs::try_exists(&sandbox).await.unwrap_or(false)
    {
        let _ = tokio::fs::remove_dir_all(&sandbox).await;
        if let Some(session_root) = sandbox.parent()
            && let Ok(mut entries) = tokio::fs::read_dir(session_root).await
            && matches!(entries.next_entry().await, Ok(None))
        {
            let _ = tokio::fs::remove_dir(session_root).await;
        }
    }
    result
}

async fn run_scheduled_turn_inner(
    inner: Arc<TurnRuntimeInner>,
    turn_id: TurnId,
    action_context: ActionTurnContext,
    cancellation: CancellationToken,
) -> Result<(), TurnRuntimeError> {
    let (turn, agent, session, rollout) = inner
        .store
        .call(move |store| {
            let turn = store.start_turn(turn_id)?;
            let agent = store.get_agent(turn.agent_id)?;
            let session = store.get_session(agent.session_id)?;
            let rollout = store.reconstruct_agent_rollout(agent.id)?;
            Ok::<_, StoreError>((turn, agent, session, rollout))
        })
        .await?;
    verify_prompt_snapshot(&turn.prompt)?;
    inner
        .model
        .validate_route_snapshot(&turn.model_route, inner.model_context_window)?;
    let tools = inner.tools.registry_for_snapshot(&turn.tool_set)?;
    let active_rollout = rollout.active_turn.ok_or_else(|| {
        StoreError::Invariant(format!(
            "Agent rollout has no active state for running Turn {}",
            turn.id
        ))
    })?;
    if active_rollout.turn_id != turn.id {
        return Err(StoreError::Invariant(format!(
            "Agent rollout active Turn {} does not match running Turn {}",
            active_rollout.turn_id, turn.id
        ))
        .into());
    }
    let resume_current_turn = active_rollout.has_checkpoint;
    let mut rollout_context = active_rollout.context;
    if resume_current_turn {
        let projected_turn = turn.clone();
        let projected_context = rollout_context.clone();
        inner
            .store
            .call(move |store| {
                let recovered =
                    reconcile_step_projections(store, &projected_turn, &projected_context)?;
                if recovered > 0 {
                    store.publish_transient_session_event(
                        session.id,
                        Some(projected_turn.agent_id),
                        Some(projected_turn.id),
                        None,
                        SessionEventPayload::AssistantMessageReset,
                    )?;
                }
                Ok::<_, StoreError>(())
            })
            .await?;
        let repaired = repair_interrupted_tool_calls(rollout_context.clone());
        if repaired.len() != rollout_context.len() {
            let checkpoint = TurnContextCheckpoint {
                mutation: ModelContextMutation::Append {
                    items: repaired[rollout_context.len()..].to_vec(),
                },
                usage: active_rollout.usage,
                completed_model_steps: active_rollout.completed_model_steps,
                hosted_search_calls_used: active_rollout.hosted_search_calls_used,
                checkpoint_message: active_rollout.checkpoint_message.clone(),
                acknowledged_control_ids: Vec::new(),
            };
            let checkpoint_turn_id = turn.id;
            inner
                .store
                .call(move |store| store.checkpoint_turn_context(checkpoint_turn_id, checkpoint))
                .await?;
            rollout_context = repaired;
        }
    }
    let history = rollout_context.clone();
    let event_sink = Arc::new(TurnEventSink::new(
        inner.store.clone(),
        session.id,
        agent.id,
        turn.id,
        rollout_context,
        active_rollout.completed_model_steps,
    ));
    let events: Arc<dyn AgentEventSink> = event_sink.clone();
    let control: Arc<dyn AgentControlPlane> = Arc::new(StoreAgentControlPlane {
        store: inner.store.clone(),
    });
    let runtime = AgentRuntime::new(Arc::clone(&inner.model), tools, events)
        .with_control(control)
        .with_sampling_permits(Arc::clone(&inner.permits));
    let mut request = AgentTurnRequest::new(
        session.project_id,
        session.id,
        agent.id,
        turn.id,
        turn.environment.clone(),
        inner
            .store
            .managed_root()
            .join("runtime/sandboxes")
            .join(agent.id.to_string())
            .join(turn.id.to_string()),
        turn.model_route.profile.clone(),
        turn.prompt.rendered.clone(),
        turn.input.clone(),
    );
    request.action_invocation_id = Some(action_context.action_invocation_id);
    request.action_attempt_id = Some(action_context.action_attempt_id);
    request.initial_history = history;
    request.initial_usage = active_rollout.usage;
    request.completed_model_steps = active_rollout.completed_model_steps;
    request.hosted_search_calls_used = active_rollout.hosted_search_calls_used;
    request.resume_current_turn = resume_current_turn;
    request.checkpoint_message = active_rollout.checkpoint_message;
    request.reasoning_effort = turn.model_route.reasoning_effort;
    request.tools_enabled = turn.tools_enabled;
    request.web_search_context_size = turn.web_search_context_size;
    request.response_format = turn.response_format;
    request.model_context_window = turn.model_route.context_window;
    request.hosted_web_search_supported = turn.model_route.capabilities.hosted_web_search;
    match runtime.run(request, cancellation).await {
        Ok(result) => {
            papermachine_store::process_fault::reach_process_fault_boundary(
                papermachine_store::process_fault::TURN_TERMINAL_CHECKPOINTED_BEFORE_COMMIT,
            );
            inner
                .store
                .call(move |store| store.complete_turn(turn.id, result.final_message, result.usage))
                .await?;
            Ok(())
        }
        Err(AgentError::Cancelled) => {
            event_sink
                .finish_pending(StepStatus::Cancelled, "cancelled by user")
                .await?;
            let turn_id = turn.id;
            inner
                .store
                .call(move |store| store.cancel_turn(turn_id))
                .await?;
            Err(TurnRuntimeError::Cancelled)
        }
        Err(AgentError::Interrupted {
            reason,
            control_message_ids,
        }) => {
            event_sink
                .finish_pending(StepStatus::Cancelled, &reason)
                .await?;
            let turn_id = turn.id;
            let stored_reason = reason.clone();
            inner
                .store
                .call(move |store| {
                    store.interrupt_turn_with_controls(turn_id, stored_reason, &control_message_ids)
                })
                .await?;
            Err(TurnRuntimeError::Interrupted(reason))
        }
        Err(error) => {
            event_sink
                .finish_pending(StepStatus::Failed, &error.to_string())
                .await?;
            let turn_id = turn.id;
            let message = error.to_string();
            inner
                .store
                .call(move |store| store.fail_turn(turn_id, message))
                .await?;
            Err(TurnRuntimeError::Agent(error))
        }
    }
}

struct StoreAgentControlPlane {
    store: StoreHandle,
}

#[async_trait]
impl AgentControlPlane for StoreAgentControlPlane {
    async fn checkpoint(
        &self,
        context: AgentCheckpointContext,
        cancellation: CancellationToken,
    ) -> Result<AgentCheckpoint, String> {
        let session_id = context.session_id;
        loop {
            let run = self
                .store
                .call::<_, StoreError, _>(move |store| store.get_session(session_id))
                .await
                .map_err(|error| error.to_string())?;
            match run.status {
                SessionStatus::Paused
                | SessionStatus::WaitingForInput
                | SessionStatus::WaitingForDeadline => {
                    let turn_id = context.turn_id;
                    let mut events = self
                        .store
                        .call(move |store| {
                            let turn = store.get_turn(turn_id)?;
                            if turn.status != TurnStatus::Paused {
                                store.pause_turn(turn_id)?;
                            }
                            Ok::<_, StoreError>(store.subscribe())
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err("cancelled".to_string()),
                        event = events.recv() => {
                            if event.is_err() {
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
                SessionStatus::Created | SessionStatus::Running => break,
                SessionStatus::Closing
                | SessionStatus::Completed
                | SessionStatus::Failed
                | SessionStatus::Cancelled => {
                    return Ok(AgentCheckpoint {
                        guidance: Vec::new(),
                        interrupt: Some(format!("Session entered {:?}", run.status)),
                        finish: None,
                        control_message_ids: Vec::new(),
                    });
                }
            }
        }
        let turn_id = context.turn_id;
        let agent_id = context.agent_id;
        let action_invocation_id = context.action_invocation_id;
        let messages = self
            .store
            .call(move |store| {
                let turn = store.get_turn(turn_id)?;
                if turn.status == TurnStatus::Paused {
                    store.resume_turn(turn_id)?;
                }
                store.claim_control_messages(session_id, agent_id, action_invocation_id, turn_id)
            })
            .await
            .map_err(|error| error.to_string())?;
        let mut checkpoint = AgentCheckpoint::default();
        for message in messages {
            checkpoint.control_message_ids.push(message.id);
            match message.kind {
                ControlMessageKind::Guide => checkpoint.guidance.push(message.content),
                ControlMessageKind::Interrupt => checkpoint.interrupt = Some(message.content),
                ControlMessageKind::Finish => checkpoint.finish = Some(message.content),
            }
        }
        Ok(checkpoint)
    }
}

#[derive(Clone)]
struct CanonicalToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

fn canonical_tool_state(
    history: &[ModelInputItem],
) -> (Vec<CanonicalToolCall>, HashMap<String, Value>) {
    let mut calls = Vec::new();
    let mut outputs = HashMap::new();
    for item in history {
        match item {
            ModelInputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => calls.push(CanonicalToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            ModelInputItem::FunctionCallOutput { call_id, output } => {
                outputs.insert(call_id.clone(), output.clone());
            }
            ModelInputItem::ResponseItem { item }
                if item.get("type").and_then(Value::as_str) == Some("function_call") =>
            {
                if let (Some(call_id), Some(name)) = (
                    item.get("call_id").and_then(Value::as_str),
                    item.get("name").and_then(Value::as_str),
                ) {
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            item.get("arguments")
                                .cloned()
                                .unwrap_or_else(|| json!({}))
                                .to_string()
                        });
                    calls.push(CanonicalToolCall {
                        call_id: call_id.to_string(),
                        name: name.to_string(),
                        arguments,
                    });
                }
            }
            _ => {}
        }
    }
    (calls, outputs)
}

fn repair_interrupted_tool_calls(mut history: Vec<ModelInputItem>) -> Vec<ModelInputItem> {
    let (calls, outputs) = canonical_tool_state(&history);
    for call in calls {
        if !outputs.contains_key(&call.call_id) {
            history.push(ModelInputItem::FunctionCallOutput {
                call_id: call.call_id,
                output: Value::String("aborted".to_string()),
            });
        }
    }
    history
}

fn reconcile_step_projections(
    store: &Store,
    turn: &Turn,
    history: &[ModelInputItem],
) -> Result<usize, StoreError> {
    let agent = store.get_agent(turn.agent_id)?;
    let (calls, outputs) = canonical_tool_state(history);
    let steps = store.list_steps(turn.id)?;
    let mut by_call = steps
        .iter()
        .filter_map(|step| {
            step.tool_call_id
                .as_ref()
                .map(|call_id| (call_id.clone(), step.id))
        })
        .collect::<HashMap<_, _>>();
    let mut recovered = 0_usize;

    for step in steps
        .iter()
        .filter(|step| step.status == StepStatus::Running)
    {
        let (status, output) = match step
            .tool_call_id
            .as_ref()
            .and_then(|call_id| outputs.get(call_id))
        {
            Some(output) => (tool_step_status(output), output.clone()),
            None => (StepStatus::Aborted, Value::String("aborted".to_string())),
        };
        store.finish_step(step.id, status, Some(output), TokenUsage::default(), None)?;
        if step.kind == StepKind::Tool {
            store.append_session_event(
                agent.session_id,
                Some(agent.id),
                Some(turn.id),
                Some(step.id),
                SessionEventPayload::ToolCallCompleted,
            )?;
        }
        recovered = recovered.saturating_add(1);
    }

    for call in calls {
        if by_call.contains_key(&call.call_id) {
            continue;
        }
        let input = serde_json::from_str(&call.arguments)
            .unwrap_or_else(|_| Value::String(call.arguments.clone()));
        let step = store.create_tool_step(turn.id, &call.call_id, call.name, input)?;
        by_call.insert(call.call_id.clone(), step.id);
        let output = outputs
            .get(&call.call_id)
            .cloned()
            .unwrap_or_else(|| Value::String("aborted".to_string()));
        let status = if outputs.contains_key(&call.call_id) {
            tool_step_status(&output)
        } else {
            StepStatus::Aborted
        };
        store.finish_step(step.id, status, Some(output), TokenUsage::default(), None)?;
        store.append_session_event(
            agent.session_id,
            Some(agent.id),
            Some(turn.id),
            Some(step.id),
            SessionEventPayload::ToolCallCompleted,
        )?;
        recovered = recovered.saturating_add(1);
    }
    Ok(recovered)
}

fn tool_step_status(output: &Value) -> StepStatus {
    if output == "aborted" {
        StepStatus::Aborted
    } else if output.get("ok").and_then(Value::as_bool) == Some(false) {
        StepStatus::Failed
    } else {
        StepStatus::Completed
    }
}

fn build_prompt_snapshot(
    agent: &Agent,
    project_prompt: papermachine_protocol::ProjectSystemPrompt,
    additional: Vec<PromptLayerInput>,
    skills: &ResolvedSkills,
) -> PromptSnapshot {
    let mut layers = vec![make_prompt_layer(
        PromptLayerKind::Runtime,
        "PaperMachine runtime",
        "builtin:runtime",
        RUNTIME_SYSTEM_PROMPT,
    )];
    if !project_prompt.content.trim().is_empty() {
        layers.push(PromptLayer {
            kind: PromptLayerKind::Project,
            name: "Project system prompt".to_string(),
            source: project_prompt.relative_path,
            content: project_prompt.content,
            sha256: project_prompt.sha256,
        });
    }
    layers.extend(additional.iter().filter_map(prompt_layer_from_input));
    if !agent.system_prompt.trim().is_empty() {
        layers.push(make_prompt_layer(
            PromptLayerKind::Agent,
            "Agent system prompt",
            format!("agent:{}", agent.id),
            &agent.system_prompt,
        ));
    }
    if !skills.instructions.trim().is_empty() {
        let source = skills
            .snapshots
            .iter()
            .map(|snapshot| format!("{}@{}", snapshot.slug, snapshot.sha256))
            .collect::<Vec<_>>()
            .join(",");
        layers.push(make_prompt_layer(
            PromptLayerKind::Skills,
            "Enabled Project skills",
            format!("skills:{source}"),
            &skills.instructions,
        ));
    }
    // Stable sorting makes the layer contract hold even when future runtimes
    // contribute Agent or Control layers through `additional`.
    layers.sort_by_key(|layer| prompt_layer_rank(layer.kind));
    let rendered = render_prompt_layers(&layers);
    PromptSnapshot {
        sha256: hash_text(&rendered),
        layers,
        rendered,
    }
}

const fn prompt_layer_rank(kind: PromptLayerKind) -> u8 {
    match kind {
        PromptLayerKind::Runtime => 0,
        PromptLayerKind::Project => 1,
        PromptLayerKind::Session => 2,
        PromptLayerKind::Agent => 3,
        PromptLayerKind::Skills => 4,
        PromptLayerKind::Control => 5,
    }
}

fn prompt_layer_from_input(input: &PromptLayerInput) -> Option<PromptLayer> {
    if input.content.trim().is_empty() {
        None
    } else {
        Some(make_prompt_layer(
            input.kind,
            &input.name,
            &input.source,
            &input.content,
        ))
    }
}

fn make_prompt_layer(
    kind: PromptLayerKind,
    name: impl Into<String>,
    source: impl Into<String>,
    content: impl Into<String>,
) -> PromptLayer {
    let content = content.into();
    PromptLayer {
        kind,
        name: name.into(),
        source: source.into(),
        sha256: hash_text(&content),
        content,
    }
}

fn render_prompt_layers(layers: &[PromptLayer]) -> String {
    layers
        .iter()
        .map(|layer| {
            format!(
                "## {} [{}]\n{}",
                layer.name,
                layer.kind.as_str(),
                layer.content.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn verify_prompt_snapshot(snapshot: &PromptSnapshot) -> Result<(), TurnRuntimeError> {
    for layer in &snapshot.layers {
        if hash_text(&layer.content) != layer.sha256 {
            return Err(TurnRuntimeError::InvalidPromptSnapshot(format!(
                "layer {:?} from {} has changed",
                layer.kind, layer.source
            )));
        }
    }
    let rendered = render_prompt_layers(&snapshot.layers);
    if rendered != snapshot.rendered || hash_text(&rendered) != snapshot.sha256 {
        return Err(TurnRuntimeError::InvalidPromptSnapshot(
            "rendered prompt does not match its layers".to_string(),
        ));
    }
    Ok(())
}

fn hash_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

struct TurnEventSink {
    store: StoreHandle,
    session_id: SessionId,
    agent_id: AgentId,
    turn_id: TurnId,
    model_steps: Mutex<HashMap<u32, Value>>,
    tool_steps: Mutex<HashMap<String, StepId>>,
    compaction_steps: Mutex<Vec<StepId>>,
    checkpoint: Mutex<RolloutCheckpointState>,
}

struct RolloutCheckpointState {
    context: Vec<ModelInputItem>,
    replacement_reason: Option<ContextReplacementReason>,
    completed_model_steps: u32,
}

impl TurnEventSink {
    fn new(
        store: StoreHandle,
        session_id: SessionId,
        agent_id: AgentId,
        turn_id: TurnId,
        context: Vec<ModelInputItem>,
        completed_model_steps: u32,
    ) -> Self {
        Self {
            store,
            session_id,
            agent_id,
            turn_id,
            model_steps: Mutex::new(HashMap::new()),
            tool_steps: Mutex::new(HashMap::new()),
            compaction_steps: Mutex::new(Vec::new()),
            checkpoint: Mutex::new(RolloutCheckpointState {
                context,
                replacement_reason: None,
                completed_model_steps,
            }),
        }
    }
}

#[async_trait]
impl AgentEventSink for TurnEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), String> {
        match event {
            AgentEvent::Started { .. } => Ok(()),
            AgentEvent::MessageDelta { delta } => {
                self.transient(None, SessionEventPayload::AssistantMessageDelta { delta })
                    .await
            }
            AgentEvent::MessageReset => {
                self.transient(None, SessionEventPayload::AssistantMessageReset)
                    .await
            }
            AgentEvent::MessageCompleted { .. } => {
                self.append(None, SessionEventPayload::AssistantMessageCompleted)
                    .await
            }
            AgentEvent::ModelStepStarted { step, input } => {
                self.model_steps.lock().await.insert(step, input);
                self.transient(None, SessionEventPayload::ModelStepStarted)
                    .await
            }
            AgentEvent::ModelStepCompleted {
                step,
                output,
                usage,
                duration_ms,
            } => {
                let input = self.model_steps.lock().await.remove(&step).ok_or_else(|| {
                    format!("model Step {step} completed without a matching start")
                })?;
                let turn_id = self.turn_id;
                let stored = self
                    .store
                    .call(move |store| {
                        store.create_terminal_step(
                            turn_id,
                            StepKind::Model,
                            format!("model sample {step}"),
                            input,
                            StepStatus::Completed,
                            Some(output),
                            usage,
                            Some(duration_ms),
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                self.charge_action_step().await?;
                self.append(Some(stored.id), SessionEventPayload::ModelStepCompleted)
                    .await
            }
            AgentEvent::ModelStepFailed {
                step,
                error,
                usage,
                duration_ms,
            } => {
                let input = self
                    .model_steps
                    .lock()
                    .await
                    .remove(&step)
                    .unwrap_or_else(|| json!({"model_step": step}));
                let turn_id = self.turn_id;
                let stored = self
                    .store
                    .call(move |store| {
                        store.create_terminal_step(
                            turn_id,
                            StepKind::Model,
                            format!("model sample {step}"),
                            input,
                            StepStatus::Failed,
                            Some(json!({"error": &error})),
                            usage,
                            Some(duration_ms),
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                self.charge_action_step().await?;
                let session_id = self.session_id;
                self.store
                    .call(move |store| {
                        store.add_session_usage(
                            session_id,
                            SessionUsage {
                                tokens: usage,
                                ..SessionUsage::default()
                            },
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                self.append(Some(stored.id), SessionEventPayload::ModelStepFailed)
                    .await
            }
            AgentEvent::ToolCallStarted { call } => {
                let input = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone()));
                let turn_id = self.turn_id;
                let step = self
                    .store
                    .call({
                        let call_id = call.call_id.clone();
                        let name = call.name.clone();
                        move |store| store.create_tool_step(turn_id, call_id, name, input)
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                self.tool_steps
                    .lock()
                    .await
                    .insert(call.call_id.clone(), step.id);
                self.charge_action_step().await?;
                self.append(Some(step.id), SessionEventPayload::ToolCallStarted)
                    .await?;
                papermachine_store::process_fault::reach_process_fault_boundary(
                    papermachine_store::process_fault::FUNCTION_CALL_COMMITTED_BEFORE_DISPATCH,
                );
                Ok(())
            }
            AgentEvent::ToolCallCompleted {
                call_id,
                output,
                duration_ms,
                success,
                ..
            } => {
                let step_id = self.tool_steps.lock().await.remove(&call_id);
                if let Some(step_id) = step_id {
                    self.store
                        .call({
                            let output = output.clone();
                            move |store| {
                                store.finish_step(
                                    step_id,
                                    if success {
                                        StepStatus::Completed
                                    } else {
                                        StepStatus::Failed
                                    },
                                    Some(output),
                                    TokenUsage::default(),
                                    Some(duration_ms),
                                )
                            }
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                }
                self.append(step_id, SessionEventPayload::ToolCallCompleted)
                    .await
            }
            AgentEvent::HostedToolCompleted {
                tool_name,
                input,
                output,
            } => {
                let turn_id = self.turn_id;
                let step = self
                    .store
                    .call(move |store| {
                        store.create_terminal_step(
                            turn_id,
                            StepKind::Tool,
                            tool_name,
                            input,
                            StepStatus::Completed,
                            Some(output),
                            TokenUsage::default(),
                            None,
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                self.charge_action_step().await?;
                self.append(Some(step.id), SessionEventPayload::HostedToolCompleted)
                    .await
            }
            AgentEvent::ContextTrimmed { removed_items } => {
                let mut checkpoint = self.checkpoint.lock().await;
                if checkpoint.replacement_reason.is_none() {
                    checkpoint.replacement_reason = Some(ContextReplacementReason::Trim);
                }
                drop(checkpoint);
                self.append(None, SessionEventPayload::ContextTrimmed { removed_items })
                    .await
            }
            AgentEvent::ContextCompactionStarted { before_tokens } => {
                let turn_id = self.turn_id;
                let step = self
                    .store
                    .call(move |store| {
                        store.create_step(
                            turn_id,
                            StepKind::Model,
                            "context compaction",
                            json!({"before_tokens": before_tokens}),
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                self.compaction_steps.lock().await.push(step.id);
                self.charge_action_step().await?;
                Ok(())
            }
            AgentEvent::ContextCompactionCompleted {
                before_tokens,
                after_tokens,
                removed_items,
                summary,
                usage,
                duration_ms,
            } => {
                let step_id = self.compaction_steps.lock().await.pop();
                if let Some(step_id) = step_id {
                    self.store
                        .call({
                            let summary = summary.clone();
                            move |store| {
                                store.finish_step(
                                    step_id,
                                    StepStatus::Completed,
                                    Some(json!({
                                        "summary": summary,
                                        "before_tokens": before_tokens,
                                        "after_tokens": after_tokens,
                                        "removed_items": removed_items,
                                    })),
                                    usage,
                                    Some(duration_ms),
                                )
                            }
                        })
                        .await
                        .map_err(|error| error.to_string())?;
                }
                self.checkpoint.lock().await.replacement_reason =
                    Some(ContextReplacementReason::Compaction);
                self.append(
                    step_id,
                    SessionEventPayload::ContextCompacted {
                        before_tokens,
                        after_tokens,
                        removed_items,
                    },
                )
                .await
            }
            AgentEvent::SamplingRetry { attempt, error } => {
                self.append(None, SessionEventPayload::SamplingRetry { attempt, error })
                    .await
            }
            AgentEvent::HistoryCheckpoint {
                history,
                usage,
                completed_model_steps,
                hosted_search_calls_used,
                message,
                acknowledged_control_ids,
            } => {
                let mut checkpoint = self.checkpoint.lock().await;
                let mutation = if history == checkpoint.context {
                    ModelContextMutation::Unchanged
                } else if history.starts_with(&checkpoint.context) {
                    ModelContextMutation::Append {
                        items: history[checkpoint.context.len()..].to_vec(),
                    }
                } else {
                    let reason = checkpoint.replacement_reason.ok_or_else(|| {
                        "Agent replaced Session context without a compaction or trim event"
                            .to_string()
                    })?;
                    ModelContextMutation::Replace {
                        items: history.clone(),
                        reason,
                    }
                };
                let model_advanced = completed_model_steps > checkpoint.completed_model_steps;
                let (contains_function_call, contains_function_call_output) = match &mutation {
                    ModelContextMutation::Append { items }
                    | ModelContextMutation::Replace { items, .. } => (
                        items.iter().any(|item| {
                            matches!(item, ModelInputItem::FunctionCall { .. })
                                || matches!(item, ModelInputItem::ResponseItem { item }
                                    if item.get("type").and_then(Value::as_str)
                                        == Some("function_call"))
                        }),
                        items
                            .iter()
                            .any(|item| matches!(item, ModelInputItem::FunctionCallOutput { .. })),
                    ),
                    ModelContextMutation::Unchanged => (false, false),
                };
                if contains_function_call {
                    papermachine_store::process_fault::reach_process_fault_boundary(
                        papermachine_store::process_fault::FUNCTION_CALL_RECEIVED_BEFORE_CHECKPOINT,
                    );
                }
                if contains_function_call_output {
                    papermachine_store::process_fault::reach_process_fault_boundary(
                        papermachine_store::process_fault::TOOL_EFFECT_COMPLETED_BEFORE_OUTPUT_CHECKPOINT,
                    );
                }
                let turn_id = self.turn_id;
                self.store
                    .call(move |store| {
                        store.checkpoint_turn_context(
                            turn_id,
                            TurnContextCheckpoint {
                                mutation,
                                usage,
                                completed_model_steps,
                                hosted_search_calls_used,
                                checkpoint_message: message,
                                acknowledged_control_ids,
                            },
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())?;
                checkpoint.context = history;
                checkpoint.replacement_reason = None;
                checkpoint.completed_model_steps = completed_model_steps;
                drop(checkpoint);
                if model_advanced {
                    papermachine_store::process_fault::reach_process_fault_boundary(
                        papermachine_store::process_fault::MODEL_OUTPUT_COMMITTED_BEFORE_STEP_PROJECTION,
                    );
                }
                if contains_function_call_output {
                    papermachine_store::process_fault::reach_process_fault_boundary(
                        papermachine_store::process_fault::FUNCTION_CALL_OUTPUT_COMMITTED_BEFORE_STEP_PROJECTION,
                    );
                }
                Ok(())
            }
        }
    }
}

impl TurnEventSink {
    async fn charge_action_step(&self) -> Result<(), String> {
        let session_id = self.session_id;
        self.store
            .call(move |store| {
                store.add_session_usage(
                    session_id,
                    SessionUsage {
                        action_steps: 1,
                        ..SessionUsage::default()
                    },
                )
            })
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn finish_pending(&self, status: StepStatus, error: &str) -> Result<(), StoreError> {
        let model_steps = self.model_steps.lock().await.drain().collect::<Vec<_>>();
        let tool_steps = self
            .tool_steps
            .lock()
            .await
            .drain()
            .map(|(_, step_id)| step_id)
            .collect::<Vec<_>>();
        let compaction_steps = self
            .compaction_steps
            .lock()
            .await
            .drain(..)
            .collect::<Vec<_>>();
        let turn_id = self.turn_id;
        let error = error.to_string();
        self.store
            .call(move |store| {
                for (step, input) in model_steps {
                    store.create_terminal_step(
                        turn_id,
                        StepKind::Model,
                        format!("model sample {step}"),
                        input,
                        status,
                        Some(json!({"error": &error})),
                        TokenUsage::default(),
                        None,
                    )?;
                }
                for step_id in tool_steps.into_iter().chain(compaction_steps) {
                    store.finish_step(
                        step_id,
                        status,
                        Some(json!({"error": &error})),
                        TokenUsage::default(),
                        None,
                    )?;
                }
                Ok::<_, StoreError>(())
            })
            .await
    }

    async fn append(
        &self,
        step_id: Option<StepId>,
        payload: SessionEventPayload,
    ) -> Result<(), String> {
        let session_id = self.session_id;
        let agent_id = self.agent_id;
        let turn_id = self.turn_id;
        self.store
            .call(move |store| {
                store.append_session_event(
                    session_id,
                    Some(agent_id),
                    Some(turn_id),
                    step_id,
                    payload,
                )
            })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn transient(
        &self,
        step_id: Option<StepId>,
        payload: SessionEventPayload,
    ) -> Result<(), String> {
        let session_id = self.session_id;
        let agent_id = self.agent_id;
        let turn_id = self.turn_id;
        self.store
            .call(move |store| {
                store.publish_transient_session_event(
                    session_id,
                    Some(agent_id),
                    Some(turn_id),
                    step_id,
                    payload,
                )
            })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Error)]
pub enum TurnRuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Skill(#[from] SkillError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("turn {0} is already terminal")]
    TerminalTurn(TurnId),
    #[error("turn scheduling failed: {0}")]
    Scheduling(String),
    #[error("invalid Turn prompt snapshot: {0}")]
    InvalidPromptSnapshot(String),
    #[error("turn was cancelled")]
    Cancelled,
    #[error("turn was interrupted: {0}")]
    Interrupted(String),
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn recovery_synthesizes_one_stable_aborted_output() {
        let repaired = repair_interrupted_tool_calls(vec![ModelInputItem::FunctionCall {
            call_id: "call-read".to_string(),
            name: "read_file".to_string(),
            arguments: "{\"path\":\"evidence.md\"}".to_string(),
        }]);

        assert!(repaired.iter().any(|item| {
            matches!(item, ModelInputItem::FunctionCallOutput { call_id, output: value }
                if call_id == "call-read" && value == "aborted")
        }));
    }
}
