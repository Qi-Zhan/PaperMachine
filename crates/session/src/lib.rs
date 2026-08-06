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
use papermachine_protocol::ActionAttemptId;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::AgentStep;
use papermachine_protocol::Budget;
use papermachine_protocol::BudgetUsage;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelResponseFormat;
use papermachine_protocol::PromptLayer;
use papermachine_protocol::PromptLayerKind;
use papermachine_protocol::PromptSnapshot;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::Session;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::SessionId;
use papermachine_protocol::StepId;
use papermachine_protocol::StepKind;
use papermachine_protocol::StepStatus;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::Turn;
use papermachine_protocol::TurnId;
use papermachine_protocol::TurnOrigin;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WebSearchContextSize;
use papermachine_protocol::WorkflowId;
use papermachine_protocol::WorkflowStatus;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_skills::ResolvedSkills;
use papermachine_skills::SkillError;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_tools::ToolRegistry;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const RUNTIME_SYSTEM_PROMPT: &str = "You are an agent working in a persistent PaperMachine Session. Complete the current request using the available tools and prior Session context. Preserve exact evidence and provenance, distinguish verified observations from inference, and state material uncertainty or limitations. Runtime permissions and budgets are enforced by code; never claim capabilities or completed tool actions that are not present in the Session history.";

#[derive(Clone)]
pub struct SessionRuntime {
    inner: Arc<SessionRuntimeInner>,
}

struct SessionRuntimeInner {
    store: Arc<Store>,
    model: Arc<dyn ModelClient>,
    tools: ToolRegistry,
    skills: Arc<ProjectSkillCatalog>,
    default_model: String,
    model_context_window: usize,
    permits: Arc<Semaphore>,
    active: Mutex<HashMap<TurnId, CancellationToken>>,
}

#[derive(Clone, Debug)]
pub struct SessionRuntimeConfig {
    pub default_model: String,
    pub model_context_window: usize,
    pub max_concurrent_turns: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct WorkflowTurnContext {
    pub workflow_id: WorkflowId,
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

impl SessionRuntime {
    pub fn new(
        store: Arc<Store>,
        model: Arc<dyn ModelClient>,
        tools: ToolRegistry,
        skills: Arc<ProjectSkillCatalog>,
        config: SessionRuntimeConfig,
    ) -> Self {
        Self {
            inner: Arc::new(SessionRuntimeInner {
                store,
                model,
                tools,
                skills,
                default_model: config.default_model,
                model_context_window: config.model_context_window,
                permits: Arc::new(Semaphore::new(config.max_concurrent_turns.max(1))),
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn submit(
        &self,
        session_id: SessionId,
        input: impl Into<String>,
    ) -> Result<Turn, SessionRuntimeError> {
        let turn = self
            .prepare_turn(
                session_id,
                TurnOrigin::User,
                input,
                None,
                Vec::new(),
                None,
                32,
                None,
                None,
                None,
                None,
                None,
            )
            .await?;
        self.schedule(turn.id).await?;
        Ok(turn)
    }

    pub async fn execute(
        &self,
        session_id: SessionId,
        input: impl Into<String>,
        model_override: Option<&str>,
        additional_instructions: &str,
        max_steps: u32,
        cancellation: CancellationToken,
    ) -> Result<Turn, SessionRuntimeError> {
        let turn = self
            .prepare_turn(
                session_id,
                TurnOrigin::Workflow,
                input,
                model_override,
                prompt_layer_from_text(
                    PromptLayerKind::Control,
                    "Runtime instructions",
                    "runtime:execute",
                    additional_instructions,
                ),
                None,
                max_steps,
                None,
                None,
                None,
                None,
                None,
            )
            .await?;
        run_scheduled_turn(Arc::clone(&self.inner), turn.id, None, cancellation).await?;
        Ok(self.inner.store.get_turn(turn.id)?)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn execute_workflow_action(
        &self,
        session_id: SessionId,
        origin: TurnOrigin,
        input: impl Into<String>,
        model_override: Option<&str>,
        prompt_layers: Vec<PromptLayerInput>,
        reasoning_effort: Option<ReasoningEffort>,
        max_steps: u32,
        max_search_calls: Option<u32>,
        web_search_context_size: Option<WebSearchContextSize>,
        max_output_tokens: Option<u32>,
        response_format: Option<ModelResponseFormat>,
        context: WorkflowTurnContext,
        cancellation: CancellationToken,
    ) -> Result<Turn, SessionRuntimeError> {
        let turn = self
            .prepare_turn(
                session_id,
                origin,
                input,
                model_override,
                prompt_layers,
                reasoning_effort,
                max_steps,
                max_search_calls,
                web_search_context_size,
                max_output_tokens,
                response_format,
                Some(context.action_attempt_id),
            )
            .await?;
        run_scheduled_turn(
            Arc::clone(&self.inner),
            turn.id,
            Some(context),
            cancellation,
        )
        .await?;
        Ok(self.inner.store.get_turn(turn.id)?)
    }

    pub async fn resume_workflow_action(
        &self,
        turn_id: TurnId,
        context: WorkflowTurnContext,
        cancellation: CancellationToken,
    ) -> Result<Turn, SessionRuntimeError> {
        self.inner
            .store
            .cancel_open_human_requests_for_recovery(turn_id)?;
        let cancelled_steps = self.inner.store.cancel_running_steps_for_recovery(
            turn_id,
            "server process restarted while this Step was running",
        )?;
        if cancelled_steps > 0 {
            let turn = self.inner.store.get_turn(turn_id)?;
            self.inner.store.append_session_event(
                turn.session_id,
                Some(turn_id),
                None,
                SessionEventPayload::AssistantMessageReset,
            )?;
        }
        run_scheduled_turn(
            Arc::clone(&self.inner),
            turn_id,
            Some(context),
            cancellation,
        )
        .await?;
        Ok(self.inner.store.get_turn(turn_id)?)
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_turn(
        &self,
        session_id: SessionId,
        origin: TurnOrigin,
        input: impl Into<String>,
        model_override: Option<&str>,
        prompt_layers: Vec<PromptLayerInput>,
        reasoning_effort: Option<ReasoningEffort>,
        max_steps: u32,
        max_search_calls: Option<u32>,
        web_search_context_size: Option<WebSearchContextSize>,
        max_output_tokens: Option<u32>,
        response_format: Option<ModelResponseFormat>,
        action_attempt_id: Option<ActionAttemptId>,
    ) -> Result<Turn, SessionRuntimeError> {
        let session = self.inner.store.get_session(session_id)?;
        let model = if model_override.is_some_and(|model| !model.trim().is_empty()) {
            model_override.unwrap_or_default().trim().to_string()
        } else if session.model.trim().is_empty() {
            self.inner.default_model.clone()
        } else {
            session.model.clone()
        };
        let workspace = session_workspace(&self.inner, &session)?;
        let resolved =
            self.inner
                .skills
                .resolve(session.project_id, &session.enabled_skills, &workspace)?;
        let project_prompt = self
            .inner
            .store
            .get_project_system_prompt(session.project_id)?;
        let prompt = build_prompt_snapshot(&session, project_prompt, prompt_layers, &resolved);
        let turn = if let Some(attempt_id) = action_attempt_id {
            self.inner.store.create_turn_for_attempt(
                attempt_id,
                session_id,
                origin,
                input,
                model,
                prompt,
                reasoning_effort,
                max_steps,
                max_search_calls,
                web_search_context_size,
                max_output_tokens,
                response_format,
                resolved.snapshots,
            )?
        } else {
            self.inner.store.create_turn(
                session_id,
                origin,
                input,
                model,
                prompt,
                reasoning_effort,
                max_steps,
                max_search_calls,
                web_search_context_size,
                max_output_tokens,
                response_format,
                resolved.snapshots,
            )?
        };
        Ok(turn)
    }

    pub async fn recover(&self) -> Result<Vec<TurnId>, SessionRuntimeError> {
        let turns = self.inner.store.list_resumable_standalone_turns()?;
        let mut recovered = Vec::with_capacity(turns.len());
        for turn in turns {
            let session = self.inner.store.get_session(turn.session_id)?;
            let workspace = session_workspace(&self.inner, &session)?;
            self.inner
                .skills
                .resolve_snapshots(&workspace, &turn.skill_snapshots)?;
            let cancelled_steps = self.inner.store.cancel_running_steps_for_recovery(
                turn.id,
                "server process restarted while this Step was running",
            )?;
            if cancelled_steps > 0 {
                self.inner.store.append_session_event(
                    turn.session_id,
                    Some(turn.id),
                    None,
                    SessionEventPayload::AssistantMessageReset,
                )?;
            }
            if self.schedule(turn.id).await? {
                recovered.push(turn.id);
            }
        }
        Ok(recovered)
    }

    async fn schedule(&self, turn_id: TurnId) -> Result<bool, SessionRuntimeError> {
        let mut active = self.inner.active.lock().await;
        if active.contains_key(&turn_id) {
            return Ok(false);
        }
        let cancellation = CancellationToken::new();
        active.insert(turn_id, cancellation.clone());
        drop(active);

        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let result =
                run_scheduled_turn(Arc::clone(&inner), turn_id, None, cancellation.clone()).await;
            if let Err(error) = result {
                let current = inner.store.get_turn(turn_id);
                if matches!(current, Ok(turn) if matches!(turn.status, TurnStatus::Queued | TurnStatus::Running))
                {
                    let _ = if cancellation.is_cancelled() {
                        inner.store.cancel_turn(turn_id)
                    } else {
                        inner.store.fail_turn(turn_id, error.to_string())
                    };
                }
            }
            inner.active.lock().await.remove(&turn_id);
        });
        Ok(true)
    }

    pub async fn cancel(&self, turn_id: TurnId) -> Result<(), SessionRuntimeError> {
        if let Some(cancellation) = self.inner.active.lock().await.get(&turn_id) {
            cancellation.cancel();
            return Ok(());
        }
        let turn = self.inner.store.get_turn(turn_id)?;
        if matches!(
            turn.status,
            TurnStatus::Queued
                | TurnStatus::Running
                | TurnStatus::WaitingForHuman
                | TurnStatus::Paused
        ) {
            self.inner.store.cancel_turn(turn_id)?;
            return Ok(());
        }
        Err(SessionRuntimeError::TerminalTurn(turn_id))
    }
}

async fn run_scheduled_turn(
    inner: Arc<SessionRuntimeInner>,
    turn_id: TurnId,
    workflow_context: Option<WorkflowTurnContext>,
    cancellation: CancellationToken,
) -> Result<(), SessionRuntimeError> {
    let _permit = tokio::select! {
        permit = Arc::clone(&inner.permits).acquire_owned() => {
            permit.map_err(|error| SessionRuntimeError::Scheduling(error.to_string()))?
        }
        _ = cancellation.cancelled() => return Err(SessionRuntimeError::Cancelled),
    };
    let turn = inner.store.start_turn(turn_id)?;
    verify_prompt_snapshot(&turn.prompt)?;
    let session = inner.store.get_session(turn.session_id)?;
    let resume_current_turn = !turn.history.is_empty()
        || turn.completed_model_steps > 0
        || turn.checkpoint_message.is_some();
    let history = if resume_current_turn {
        repair_interrupted_tool_calls(turn.history.clone(), &inner.store.list_steps(turn.id)?)
    } else {
        previous_history(&inner.store, &turn)?
    };
    let workspace = session_workspace(&inner, &session)?;
    let workflow_id = workflow_context.as_ref().map(|context| context.workflow_id);
    let event_sink = Arc::new(SessionAgentEventSink::new(
        Arc::clone(&inner.store),
        session.id,
        turn.id,
        workflow_id,
    ));
    let events: Arc<dyn AgentEventSink> = event_sink.clone();
    let control: Arc<dyn AgentControlPlane> = Arc::new(StoreAgentControlPlane {
        store: Arc::clone(&inner.store),
    });
    let runtime = AgentRuntime::new(Arc::clone(&inner.model), inner.tools.clone(), events)
        .with_control(control);
    let mut request = AgentTurnRequest::new(
        session.project_id,
        session.id,
        turn.id,
        workspace,
        turn.model.clone(),
        turn.prompt.rendered.clone(),
        turn.input.clone(),
    );
    if let Some(context) = workflow_context {
        request.workflow_id = Some(context.workflow_id);
        request.action_invocation_id = Some(context.action_invocation_id);
        request.action_attempt_id = Some(context.action_attempt_id);
        let run = inner.store.get_workflow(context.workflow_id)?;
        if let Some(limit) = run.budget.max_hosted_search_calls {
            let remaining = limit.saturating_sub(run.usage.hosted_search_calls);
            request.max_search_calls = Some(
                turn.max_search_calls
                    .map_or(remaining, |action_limit| action_limit.min(remaining)),
            );
        }
    }
    request.initial_history = history;
    request.initial_usage = turn.usage;
    request.completed_model_steps = turn.completed_model_steps;
    request.hosted_search_calls_used = turn.hosted_search_calls_used;
    request.resume_current_turn = resume_current_turn;
    request.checkpoint_message = turn.checkpoint_message.clone();
    request.access = turn.access;
    request.reasoning_effort = turn.reasoning_effort;
    request.max_steps = turn.max_steps;
    if request.max_search_calls.is_none() {
        request.max_search_calls = turn.max_search_calls;
    }
    request.web_search_context_size = turn.web_search_context_size;
    request.response_format = turn.response_format;
    request.max_output_tokens = turn.max_output_tokens;
    request.model_context_window = inner
        .model
        .model_context_window(&turn.model)
        .unwrap_or(inner.model_context_window);
    match runtime.run(request, cancellation).await {
        Ok(result) => {
            inner.store.complete_turn(
                turn.id,
                result.final_message,
                result.history,
                result.usage,
            )?;
            Ok(())
        }
        Err(AgentError::Cancelled) => {
            event_sink
                .finish_pending(StepStatus::Cancelled, "cancelled by user")
                .await?;
            inner.store.cancel_turn(turn.id)?;
            Err(SessionRuntimeError::Cancelled)
        }
        Err(AgentError::Interrupted(reason)) => {
            event_sink
                .finish_pending(StepStatus::Cancelled, &reason)
                .await?;
            inner.store.interrupt_turn(turn.id, reason.clone())?;
            Err(SessionRuntimeError::Interrupted(reason))
        }
        Err(error) => {
            event_sink
                .finish_pending(StepStatus::Failed, &error.to_string())
                .await?;
            inner.store.fail_turn(turn.id, error.to_string())?;
            Err(SessionRuntimeError::Agent(error))
        }
    }
}

struct StoreAgentControlPlane {
    store: Arc<Store>,
}

#[async_trait]
impl AgentControlPlane for StoreAgentControlPlane {
    async fn checkpoint(
        &self,
        context: AgentCheckpointContext,
        cancellation: CancellationToken,
    ) -> Result<AgentCheckpoint, String> {
        let Some(workflow_id) = context.workflow_id else {
            return Ok(AgentCheckpoint::default());
        };
        loop {
            let run = self
                .store
                .get_workflow(workflow_id)
                .map_err(|error| error.to_string())?;
            match run.status {
                WorkflowStatus::Paused
                | WorkflowStatus::WaitingForUser
                | WorkflowStatus::WaitingForTimer
                | WorkflowStatus::WaitingForSignal => {
                    let turn = self
                        .store
                        .get_turn(context.turn_id)
                        .map_err(|error| error.to_string())?;
                    if turn.status != TurnStatus::Paused {
                        self.store
                            .set_turn_status(context.turn_id, TurnStatus::Paused, None)
                            .map_err(|error| error.to_string())?;
                    }
                    let mut events = self.store.subscribe();
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err("cancelled".to_string()),
                        event = events.recv() => {
                            if event.is_err() {
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
                WorkflowStatus::Created | WorkflowStatus::Running => {
                    if let Some(error) = workflow_action_step_budget_error(
                        &run.budget,
                        run.usage.action_steps,
                        BudgetBoundary::BeforeModelStep,
                    ) {
                        return Err(error);
                    }
                    if let Some(error) = workflow_token_budget_error(
                        &run.budget,
                        run.usage.tokens,
                        BudgetBoundary::BeforeModelStep,
                    ) {
                        return Err(error);
                    }
                    if let Some(limit) = run.budget.max_hosted_search_calls
                        && run.usage.hosted_search_calls >= limit
                    {
                        let turn = self
                            .store
                            .get_turn(context.turn_id)
                            .map_err(|error| error.to_string())?;
                        if turn.access.allows_research_network() {
                            return Ok(AgentCheckpoint {
                                guidance: Vec::new(),
                                interrupt: None,
                                finish: Some(format!(
                                    "The Workflow hosted web-search budget is exhausted: used {} of {limit} calls. Finish from evidence already gathered and state any remaining limitations.",
                                    run.usage.hosted_search_calls
                                )),
                            });
                        }
                    }
                    break;
                }
                WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Cancelled => {
                    return Ok(AgentCheckpoint {
                        guidance: Vec::new(),
                        interrupt: Some(format!("Workflow entered {:?}", run.status)),
                        finish: None,
                    });
                }
            }
        }
        let turn = self
            .store
            .get_turn(context.turn_id)
            .map_err(|error| error.to_string())?;
        if turn.status == TurnStatus::Paused {
            self.store
                .set_turn_status(context.turn_id, TurnStatus::Running, None)
                .map_err(|error| error.to_string())?;
        }
        let messages = self
            .store
            .take_control_messages(
                workflow_id,
                context.session_id,
                context.action_invocation_id,
            )
            .map_err(|error| error.to_string())?;
        let mut checkpoint = AgentCheckpoint::default();
        for message in messages {
            match message.kind {
                ControlMessageKind::Guide => checkpoint.guidance.push(message.content),
                ControlMessageKind::Interrupt => checkpoint.interrupt = Some(message.content),
                ControlMessageKind::Finish => checkpoint.finish = Some(message.content),
            }
        }
        Ok(checkpoint)
    }
}

fn session_workspace(
    inner: &SessionRuntimeInner,
    session: &papermachine_protocol::Session,
) -> Result<PathBuf, StoreError> {
    Ok(PathBuf::from(
        inner.store.get_project(session.project_id)?.root_path,
    ))
}

fn previous_history(store: &Store, current: &Turn) -> Result<Vec<ModelInputItem>, StoreError> {
    Ok(store
        .list_turns(current.session_id)?
        .into_iter()
        .rev()
        .find(|turn| turn.id != current.id && turn.status == TurnStatus::Completed)
        .map(|turn| turn.history)
        .unwrap_or_default())
}

fn repair_interrupted_tool_calls(
    mut history: Vec<ModelInputItem>,
    steps: &[AgentStep],
) -> Vec<ModelInputItem> {
    let mut calls = Vec::new();
    let mut outputs = std::collections::HashSet::new();
    let recovered_outputs = steps
        .iter()
        .filter_map(|step| {
            step.tool_call_id
                .as_ref()
                .zip(step.output.as_ref())
                .map(|(call_id, output)| (call_id.clone(), output.clone()))
        })
        .collect::<std::collections::HashMap<_, _>>();
    for item in &history {
        match item {
            ModelInputItem::FunctionCall { call_id, .. } => calls.push(call_id.clone()),
            ModelInputItem::FunctionCallOutput { call_id, .. } => {
                outputs.insert(call_id.clone());
            }
            ModelInputItem::ResponseItem { item }
                if item.get("type").and_then(Value::as_str) == Some("function_call") =>
            {
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    calls.push(call_id.to_string());
                }
            }
            _ => {}
        }
    }
    for call_id in calls {
        if outputs.insert(call_id.clone()) {
            history.push(ModelInputItem::FunctionCallOutput {
                output: recovered_outputs.get(&call_id).cloned().unwrap_or_else(|| {
                    json!({
                        "error": "tool execution was interrupted by a server restart; inspect durable state before retrying",
                        "recovered": true,
                    })
                }),
                call_id,
            });
        }
    }
    history
}

fn prompt_layer_from_text(
    kind: PromptLayerKind,
    name: &str,
    source: &str,
    content: &str,
) -> Vec<PromptLayerInput> {
    if content.trim().is_empty() {
        Vec::new()
    } else {
        vec![PromptLayerInput::new(kind, name, source, content)]
    }
}

fn build_prompt_snapshot(
    session: &Session,
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
    if !session.system_prompt.trim().is_empty() {
        let (kind, name) = match session.origin {
            papermachine_protocol::SessionOrigin::User => {
                (PromptLayerKind::Session, "Session system prompt")
            }
            papermachine_protocol::SessionOrigin::WorkflowAgent => {
                (PromptLayerKind::Agent, "Agent system prompt")
            }
        };
        layers.push(make_prompt_layer(
            kind,
            name,
            format!("session:{}", session.id),
            &session.system_prompt,
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
    // contribute Agent/Session or Control layers through `additional`.
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
        PromptLayerKind::Workflow => 2,
        PromptLayerKind::Agent | PromptLayerKind::Session => 3,
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

fn verify_prompt_snapshot(snapshot: &PromptSnapshot) -> Result<(), SessionRuntimeError> {
    for layer in &snapshot.layers {
        if hash_text(&layer.content) != layer.sha256 {
            return Err(SessionRuntimeError::InvalidPromptSnapshot(format!(
                "layer {:?} from {} has changed",
                layer.kind, layer.source
            )));
        }
    }
    let rendered = render_prompt_layers(&snapshot.layers);
    if rendered != snapshot.rendered || hash_text(&rendered) != snapshot.sha256 {
        return Err(SessionRuntimeError::InvalidPromptSnapshot(
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

struct SessionAgentEventSink {
    store: Arc<Store>,
    session_id: SessionId,
    turn_id: TurnId,
    workflow_id: Option<WorkflowId>,
    model_steps: Mutex<HashMap<u32, StepId>>,
    tool_steps: Mutex<HashMap<String, StepId>>,
    compaction_steps: Mutex<Vec<StepId>>,
}

impl SessionAgentEventSink {
    fn new(
        store: Arc<Store>,
        session_id: SessionId,
        turn_id: TurnId,
        workflow_id: Option<WorkflowId>,
    ) -> Self {
        Self {
            store,
            session_id,
            turn_id,
            workflow_id,
            model_steps: Mutex::new(HashMap::new()),
            tool_steps: Mutex::new(HashMap::new()),
            compaction_steps: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl AgentEventSink for SessionAgentEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), String> {
        match event {
            AgentEvent::Started { objective, model } => {
                self.append(None, SessionEventPayload::AgentStarted { objective, model })
            }
            AgentEvent::MessageDelta { delta } => {
                self.append(None, SessionEventPayload::AssistantMessageDelta { delta })
            }
            AgentEvent::MessageReset => {
                self.append(None, SessionEventPayload::AssistantMessageReset)
            }
            AgentEvent::MessageCompleted { message } => self.append(
                None,
                SessionEventPayload::AssistantMessageCompleted { message },
            ),
            AgentEvent::ModelStepStarted { step, input } => {
                let stored = self
                    .store
                    .create_step(
                        self.turn_id,
                        StepKind::Model,
                        format!("model sample {step}"),
                        input,
                    )
                    .map_err(|error| error.to_string())?;
                self.model_steps.lock().await.insert(step, stored.id);
                self.charge_action_step().await?;
                self.append(
                    Some(stored.id),
                    SessionEventPayload::ModelStepStarted { step },
                )
            }
            AgentEvent::ModelStepCompleted {
                step,
                output,
                usage,
                duration_ms,
            } => {
                let step_id = self.model_steps.lock().await.remove(&step);
                if let Some(step_id) = step_id {
                    self.store
                        .finish_step(
                            step_id,
                            StepStatus::Completed,
                            Some(output),
                            usage,
                            Some(duration_ms),
                        )
                        .map_err(|error| error.to_string())?;
                }
                if let Some(workflow_id) = self.workflow_id {
                    let run = self
                        .store
                        .add_budget_usage(
                            workflow_id,
                            BudgetUsage {
                                tokens: usage,
                                ..BudgetUsage::default()
                            },
                        )
                        .map_err(|error| error.to_string())?;
                    if let Some(error) = workflow_token_budget_error(
                        &run.budget,
                        run.usage.tokens,
                        BudgetBoundary::AfterModelStep,
                    ) {
                        return Err(error);
                    }
                }
                self.append(
                    step_id,
                    SessionEventPayload::ModelStepCompleted { step, usage },
                )
            }
            AgentEvent::ModelStepFailed {
                step,
                error,
                usage,
                duration_ms,
            } => {
                let step_id = self.model_steps.lock().await.remove(&step);
                if let Some(step_id) = step_id {
                    self.store
                        .finish_step(
                            step_id,
                            StepStatus::Failed,
                            Some(json!({"error": &error})),
                            usage,
                            Some(duration_ms),
                        )
                        .map_err(|error| error.to_string())?;
                }
                if let Some(workflow_id) = self.workflow_id {
                    let run = self
                        .store
                        .add_budget_usage(
                            workflow_id,
                            BudgetUsage {
                                tokens: usage,
                                ..BudgetUsage::default()
                            },
                        )
                        .map_err(|error| error.to_string())?;
                    if let Some(error) = workflow_token_budget_error(
                        &run.budget,
                        run.usage.tokens,
                        BudgetBoundary::AfterModelStep,
                    ) {
                        return Err(error);
                    }
                }
                self.append(
                    step_id,
                    SessionEventPayload::ModelStepFailed { step, error, usage },
                )
            }
            AgentEvent::ToolCallStarted { call } => {
                let input = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone()));
                let step = self
                    .store
                    .create_tool_step(self.turn_id, call.call_id.clone(), call.name.clone(), input)
                    .map_err(|error| error.to_string())?;
                self.tool_steps
                    .lock()
                    .await
                    .insert(call.call_id.clone(), step.id);
                self.charge_action_step().await?;
                self.append(Some(step.id), SessionEventPayload::ToolCallStarted { call })
            }
            AgentEvent::ToolCallCompleted {
                call_id,
                tool_name,
                output,
                duration_ms,
                success,
            } => {
                let step_id = self.tool_steps.lock().await.remove(&call_id);
                if let Some(step_id) = step_id {
                    self.store
                        .finish_step(
                            step_id,
                            if success {
                                StepStatus::Completed
                            } else {
                                StepStatus::Failed
                            },
                            Some(output.clone()),
                            TokenUsage::default(),
                            Some(duration_ms),
                        )
                        .map_err(|error| error.to_string())?;
                }
                self.append(
                    step_id,
                    SessionEventPayload::ToolCallCompleted {
                        call_id,
                        tool_name,
                        output,
                        duration_ms,
                        success,
                    },
                )
            }
            AgentEvent::HostedToolCompleted {
                tool_name,
                input,
                output,
            } => {
                let step = self
                    .store
                    .create_step(
                        self.turn_id,
                        StepKind::Tool,
                        tool_name.clone(),
                        input.clone(),
                    )
                    .map_err(|error| error.to_string())?;
                self.store
                    .finish_step(
                        step.id,
                        StepStatus::Completed,
                        Some(output),
                        TokenUsage::default(),
                        None,
                    )
                    .map_err(|error| error.to_string())?;
                if tool_name == "web_search"
                    && let Some(workflow_id) = self.workflow_id
                {
                    self.store
                        .add_budget_usage(
                            workflow_id,
                            BudgetUsage {
                                hosted_search_calls: 1,
                                ..BudgetUsage::default()
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
                self.charge_action_step().await?;
                self.append(
                    Some(step.id),
                    SessionEventPayload::HostedToolCompleted { tool_name, input },
                )
            }
            AgentEvent::ContextTrimmed { removed_items } => {
                self.append(None, SessionEventPayload::ContextTrimmed { removed_items })
            }
            AgentEvent::ContextCompactionStarted { before_tokens } => {
                let step = self
                    .store
                    .create_step(
                        self.turn_id,
                        StepKind::Model,
                        "context compaction",
                        json!({"before_tokens": before_tokens}),
                    )
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
                        .finish_step(
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
                        .map_err(|error| error.to_string())?;
                }
                if let Some(workflow_id) = self.workflow_id {
                    let run = self
                        .store
                        .add_budget_usage(
                            workflow_id,
                            BudgetUsage {
                                tokens: usage,
                                ..BudgetUsage::default()
                            },
                        )
                        .map_err(|error| error.to_string())?;
                    if let Some(error) = workflow_token_budget_error(
                        &run.budget,
                        run.usage.tokens,
                        BudgetBoundary::AfterModelStep,
                    ) {
                        return Err(error);
                    }
                }
                self.append(
                    step_id,
                    SessionEventPayload::ContextCompacted {
                        before_tokens,
                        after_tokens,
                        removed_items,
                    },
                )
            }
            AgentEvent::SamplingRetry { attempt, error } => {
                self.append(None, SessionEventPayload::SamplingRetry { attempt, error })
            }
            AgentEvent::HistoryCheckpoint {
                history,
                usage,
                completed_model_steps,
                hosted_search_calls_used,
                message,
            } => self
                .store
                .checkpoint_turn_history(
                    self.turn_id,
                    history,
                    usage,
                    completed_model_steps,
                    hosted_search_calls_used,
                    message,
                )
                .map(|_| ())
                .map_err(|error| error.to_string()),
        }
    }
}

impl SessionAgentEventSink {
    async fn charge_action_step(&self) -> Result<(), String> {
        let Some(workflow_id) = self.workflow_id else {
            return Ok(());
        };
        let run = self
            .store
            .add_budget_usage(
                workflow_id,
                BudgetUsage {
                    action_steps: 1,
                    ..BudgetUsage::default()
                },
            )
            .map_err(|error| error.to_string())?;
        if let Some(error) = workflow_action_step_budget_error(
            &run.budget,
            run.usage.action_steps,
            BudgetBoundary::AfterModelStep,
        ) {
            return Err(error);
        }
        Ok(())
    }

    async fn finish_pending(&self, status: StepStatus, error: &str) -> Result<(), StoreError> {
        let model_steps = self
            .model_steps
            .lock()
            .await
            .drain()
            .map(|(_, step_id)| step_id)
            .collect::<Vec<_>>();
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
        for step_id in model_steps
            .into_iter()
            .chain(tool_steps)
            .chain(compaction_steps)
        {
            self.store.finish_step(
                step_id,
                status,
                Some(json!({"error": error})),
                TokenUsage::default(),
                None,
            )?;
        }
        Ok(())
    }

    fn append(&self, step_id: Option<StepId>, payload: SessionEventPayload) -> Result<(), String> {
        self.store
            .append_session_event(self.session_id, Some(self.turn_id), step_id, payload)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy)]
enum BudgetBoundary {
    BeforeModelStep,
    AfterModelStep,
}

fn workflow_token_budget_error(
    budget: &Budget,
    usage: TokenUsage,
    boundary: BudgetBoundary,
) -> Option<String> {
    let crossed = |used: u64, limit: u64| match boundary {
        BudgetBoundary::BeforeModelStep => used >= limit,
        BudgetBoundary::AfterModelStep => used > limit,
    };
    let state = match boundary {
        BudgetBoundary::BeforeModelStep => "exhausted",
        BudgetBoundary::AfterModelStep => "exceeded",
    };

    if let Some(limit) = budget.max_total_tokens
        && crossed(usage.total_tokens(), limit)
    {
        return Some(format!(
            "Workflow raw token budget {state}: used {} of {limit} total input-plus-output tokens",
            usage.total_tokens()
        ));
    }
    if let Some(limit) = budget.max_uncached_tokens
        && crossed(usage.uncached_tokens(), limit)
    {
        return Some(format!(
            "Workflow uncached token budget {state}: used {} of {limit} uncached-input-plus-output tokens (input {}, cached input {}, output {})",
            usage.uncached_tokens(),
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens
        ));
    }
    None
}

fn workflow_action_step_budget_error(
    budget: &Budget,
    used: u32,
    boundary: BudgetBoundary,
) -> Option<String> {
    let crossed = match boundary {
        BudgetBoundary::BeforeModelStep => used >= budget.max_action_steps,
        BudgetBoundary::AfterModelStep => used > budget.max_action_steps,
    };
    if !crossed {
        return None;
    }
    let state = match boundary {
        BudgetBoundary::BeforeModelStep => "exhausted",
        BudgetBoundary::AfterModelStep => "exceeded",
    };
    Some(format!(
        "Workflow action-step budget {state}: used {used} of {} persisted model, tool, and compaction steps",
        budget.max_action_steps
    ))
}

#[derive(Debug, Error)]
pub enum SessionRuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Skill(#[from] SkillError),
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
mod budget_tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn prompt_cache_reads_do_not_consume_uncached_budget() {
        let budget = Budget {
            max_total_tokens: Some(1_000),
            max_uncached_tokens: Some(100),
            ..Budget::default()
        };
        let usage = TokenUsage {
            input_tokens: 900,
            cached_input_tokens: 850,
            output_tokens: 40,
            cache_write_input_tokens: 0,
        };

        assert_eq!(usage.total_tokens(), 940);
        assert_eq!(usage.uncached_tokens(), 90);
        assert!(
            workflow_token_budget_error(&budget, usage, BudgetBoundary::AfterModelStep).is_none()
        );
    }

    #[test]
    fn raw_and_uncached_limits_are_enforced_independently() {
        let raw_limited = Budget {
            max_total_tokens: Some(900),
            max_uncached_tokens: Some(500),
            ..Budget::default()
        };
        let cached_usage = TokenUsage {
            input_tokens: 900,
            cached_input_tokens: 850,
            output_tokens: 40,
            cache_write_input_tokens: 0,
        };
        assert!(
            workflow_token_budget_error(
                &raw_limited,
                cached_usage,
                BudgetBoundary::AfterModelStep,
            )
            .is_some_and(|error| error.contains("raw token budget exceeded"))
        );

        let uncached_limited = Budget {
            max_total_tokens: Some(2_000),
            max_uncached_tokens: Some(80),
            ..Budget::default()
        };
        assert!(
            workflow_token_budget_error(
                &uncached_limited,
                cached_usage,
                BudgetBoundary::AfterModelStep,
            )
            .is_some_and(|error| error.contains("uncached token budget exceeded"))
        );
    }

    #[test]
    fn action_step_budget_checks_before_and_after_step_boundaries() {
        let budget = Budget {
            max_action_steps: 3,
            ..Budget::default()
        };

        assert_eq!(
            workflow_action_step_budget_error(&budget, 3, BudgetBoundary::BeforeModelStep,)
                .as_deref(),
            Some(
                "Workflow action-step budget exhausted: used 3 of 3 persisted model, tool, and compaction steps"
            )
        );
        assert!(
            workflow_action_step_budget_error(&budget, 3, BudgetBoundary::AfterModelStep,)
                .is_none()
        );
        assert_eq!(
            workflow_action_step_budget_error(&budget, 4, BudgetBoundary::AfterModelStep,)
                .as_deref(),
            Some(
                "Workflow action-step budget exceeded: used 4 of 3 persisted model, tool, and compaction steps"
            )
        );
    }

    #[test]
    fn recovery_reuses_a_completed_tool_step_output() {
        let directory = tempdir().expect("temporary directory should be created");
        let store = Store::open_in_memory(directory.path()).expect("Store should open");
        let project = store
            .create_project("Tool recovery", "", directory.path().join("project"))
            .expect("Project should be created");
        let session = store
            .create_session(project.id, "Session", "", "test-model", Vec::new())
            .expect("Session should be created");
        let turn = store
            .create_turn(
                session.id,
                TurnOrigin::User,
                "Read evidence",
                "test-model",
                PromptSnapshot::default(),
                None,
                2,
                None,
                None,
                None,
                None,
                Vec::new(),
            )
            .expect("Turn should be created");
        let step = store
            .create_tool_step(
                turn.id,
                "call-read",
                "read_file",
                json!({"path": "evidence.md"}),
            )
            .expect("Tool Step should be created");
        let output = json!({"content": "durable evidence"});
        let step = store
            .finish_step(
                step.id,
                StepStatus::Completed,
                Some(output.clone()),
                TokenUsage::default(),
                Some(3),
            )
            .expect("Tool Step should complete");
        let repaired = repair_interrupted_tool_calls(
            vec![ModelInputItem::FunctionCall {
                call_id: "call-read".to_string(),
                name: "read_file".to_string(),
                arguments: "{\"path\":\"evidence.md\"}".to_string(),
            }],
            &[step],
        );

        assert!(repaired.iter().any(|item| {
            matches!(item, ModelInputItem::FunctionCallOutput { call_id, output: value }
                if call_id == "call-read" && value == &output)
        }));
    }
}
