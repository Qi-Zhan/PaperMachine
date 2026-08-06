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
use papermachine_protocol::Budget;
use papermachine_protocol::BudgetUsage;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelResponseFormat;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::SessionId;
use papermachine_protocol::StepId;
use papermachine_protocol::StepKind;
use papermachine_protocol::StepStatus;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::Turn;
use papermachine_protocol::TurnId;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WebSearchContextSize;
use papermachine_protocol::WorkflowRunId;
use papermachine_protocol::WorkflowRunStatus;
use papermachine_skills::ResearchSkillCatalog;
use papermachine_skills::SkillError;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_tools::ToolRegistry;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct SessionRuntime {
    inner: Arc<SessionRuntimeInner>,
}

struct SessionRuntimeInner {
    store: Arc<Store>,
    model: Arc<dyn ModelClient>,
    tools: ToolRegistry,
    skills: Arc<ResearchSkillCatalog>,
    workspace_root: PathBuf,
    default_model: String,
    model_context_window: usize,
    permits: Arc<Semaphore>,
    active: Mutex<HashMap<TurnId, CancellationToken>>,
}

#[derive(Clone, Debug)]
pub struct SessionRuntimeConfig {
    pub workspace_root: PathBuf,
    pub default_model: String,
    pub model_context_window: usize,
    pub max_concurrent_turns: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct WorkflowTurnContext {
    pub workflow_run_id: WorkflowRunId,
    pub action_invocation_id: ActionInvocationId,
    pub action_attempt_id: ActionAttemptId,
}

impl SessionRuntime {
    pub fn new(
        store: Arc<Store>,
        model: Arc<dyn ModelClient>,
        tools: ToolRegistry,
        skills: Arc<ResearchSkillCatalog>,
        config: SessionRuntimeConfig,
    ) -> Self {
        Self {
            inner: Arc::new(SessionRuntimeInner {
                store,
                model,
                tools,
                skills,
                workspace_root: config.workspace_root,
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
                session_id, input, None, "", None, 32, None, None, None, None,
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
                input,
                model_override,
                additional_instructions,
                None,
                max_steps,
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
        input: impl Into<String>,
        model_override: Option<&str>,
        additional_instructions: &str,
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
                input,
                model_override,
                additional_instructions,
                reasoning_effort,
                max_steps,
                max_search_calls,
                web_search_context_size,
                max_output_tokens,
                response_format,
            )
            .await?;
        self.inner
            .store
            .attach_turn_to_attempt(context.action_attempt_id, turn.id)?;
        run_scheduled_turn(
            Arc::clone(&self.inner),
            turn.id,
            Some(context),
            cancellation,
        )
        .await?;
        Ok(self.inner.store.get_turn(turn.id)?)
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_turn(
        &self,
        session_id: SessionId,
        input: impl Into<String>,
        model_override: Option<&str>,
        additional_instructions: &str,
        reasoning_effort: Option<ReasoningEffort>,
        max_steps: u32,
        max_search_calls: Option<u32>,
        web_search_context_size: Option<WebSearchContextSize>,
        max_output_tokens: Option<u32>,
        response_format: Option<ModelResponseFormat>,
    ) -> Result<Turn, SessionRuntimeError> {
        let session = self.inner.store.get_session(session_id)?;
        let model = if model_override.is_some_and(|model| !model.trim().is_empty()) {
            model_override.unwrap_or_default().trim().to_string()
        } else if session.model.trim().is_empty() {
            self.inner.default_model.clone()
        } else {
            session.model.clone()
        };
        let workspace = session_workspace(&self.inner, &session);
        let resolved =
            self.inner
                .skills
                .resolve(session.research_id, &session.enabled_skills, &workspace)?;
        let session_instructions = [session.instructions.trim(), additional_instructions.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let instructions = build_instructions(&session_instructions, &resolved.instructions);
        let turn = self.inner.store.create_turn(
            session_id,
            input,
            model,
            instructions,
            reasoning_effort,
            max_steps,
            max_search_calls,
            web_search_context_size,
            max_output_tokens,
            response_format,
            resolved.snapshots,
        )?;
        Ok(turn)
    }

    pub async fn recover(&self) -> Result<Vec<TurnId>, SessionRuntimeError> {
        let turns = self.inner.store.list_resumable_turns()?;
        let mut recovered = Vec::with_capacity(turns.len());
        for turn in turns {
            let session = self.inner.store.get_session(turn.session_id)?;
            let workspace = session_workspace(&self.inner, &session);
            self.inner
                .skills
                .resolve_snapshots(&workspace, &turn.skill_snapshots)?;
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
    let session = inner.store.get_session(turn.session_id)?;
    let history = previous_history(&inner.store, &turn)?;
    let workspace = session_workspace(&inner, &session);
    let workflow_run_id = workflow_context
        .as_ref()
        .map(|context| context.workflow_run_id);
    let event_sink = Arc::new(SessionAgentEventSink::new(
        Arc::clone(&inner.store),
        session.id,
        turn.id,
        workflow_run_id,
    ));
    let events: Arc<dyn AgentEventSink> = event_sink.clone();
    let control: Arc<dyn AgentControlPlane> = Arc::new(StoreAgentControlPlane {
        store: Arc::clone(&inner.store),
    });
    let runtime = AgentRuntime::new(Arc::clone(&inner.model), inner.tools.clone(), events)
        .with_control(control);
    let mut request = AgentTurnRequest::new(
        session.research_id,
        session.id,
        turn.id,
        workspace,
        turn.model.clone(),
        turn.instructions.clone(),
        turn.input.clone(),
    );
    if let Some(context) = workflow_context {
        request.workflow_run_id = Some(context.workflow_run_id);
        request.action_invocation_id = Some(context.action_invocation_id);
        request.action_attempt_id = Some(context.action_attempt_id);
        let run = inner.store.get_workflow_run(context.workflow_run_id)?;
        if let Some(limit) = run.budget.max_hosted_search_calls {
            let remaining = limit.saturating_sub(run.usage.hosted_search_calls);
            request.max_search_calls = Some(
                turn.max_search_calls
                    .map_or(remaining, |action_limit| action_limit.min(remaining)),
            );
        }
    }
    request.initial_history = history;
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
        let Some(run_id) = context.workflow_run_id else {
            return Ok(AgentCheckpoint::default());
        };
        loop {
            let run = self
                .store
                .get_workflow_run(run_id)
                .map_err(|error| error.to_string())?;
            match run.status {
                WorkflowRunStatus::Paused => {
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
                WorkflowRunStatus::Created | WorkflowRunStatus::Running => {
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
                                    "The WorkflowRun hosted web-search budget is exhausted: used {} of {limit} calls. Finish from evidence already gathered and state any remaining limitations.",
                                    run.usage.hosted_search_calls
                                )),
                            });
                        }
                    }
                    break;
                }
                WorkflowRunStatus::Completed
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Cancelled => {
                    return Ok(AgentCheckpoint {
                        guidance: Vec::new(),
                        interrupt: Some(format!("WorkflowRun entered {:?}", run.status)),
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
            .take_control_messages(run_id, context.session_id, context.action_invocation_id)
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
) -> PathBuf {
    inner
        .workspace_root
        .join(session.research_id.to_string())
        .join(session.id.to_string())
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

fn build_instructions(session_instructions: &str, skill_instructions: &str) -> String {
    let base = "You are a research agent working in a persistent PaperMachine session. Use tools to gather and verify evidence. Distinguish observations, inferences, limitations, and open questions.";
    [base, session_instructions.trim(), skill_instructions.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

struct SessionAgentEventSink {
    store: Arc<Store>,
    session_id: SessionId,
    turn_id: TurnId,
    workflow_run_id: Option<WorkflowRunId>,
    model_steps: Mutex<HashMap<u32, StepId>>,
    tool_steps: Mutex<HashMap<String, StepId>>,
    compaction_steps: Mutex<Vec<StepId>>,
}

impl SessionAgentEventSink {
    fn new(
        store: Arc<Store>,
        session_id: SessionId,
        turn_id: TurnId,
        workflow_run_id: Option<WorkflowRunId>,
    ) -> Self {
        Self {
            store,
            session_id,
            turn_id,
            workflow_run_id,
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
                if let Some(run_id) = self.workflow_run_id {
                    let run = self
                        .store
                        .add_budget_usage(
                            run_id,
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
                if let Some(run_id) = self.workflow_run_id {
                    let run = self
                        .store
                        .add_budget_usage(
                            run_id,
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
                    .create_step(self.turn_id, StepKind::Tool, call.name.clone(), input)
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
                    && let Some(run_id) = self.workflow_run_id
                {
                    self.store
                        .add_budget_usage(
                            run_id,
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
                if let Some(run_id) = self.workflow_run_id {
                    let run = self
                        .store
                        .add_budget_usage(
                            run_id,
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
        }
    }
}

impl SessionAgentEventSink {
    async fn charge_action_step(&self) -> Result<(), String> {
        let Some(run_id) = self.workflow_run_id else {
            return Ok(());
        };
        let run = self
            .store
            .add_budget_usage(
                run_id,
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
    #[error("turn was cancelled")]
    Cancelled,
    #[error("turn was interrupted: {0}")]
    Interrupted(String),
}

#[cfg(test)]
mod budget_tests {
    use super::*;

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
}
