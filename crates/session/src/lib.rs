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
use papermachine_protocol::ContextReplacementReason;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelContextMutation;
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
use papermachine_protocol::ToolEffectDisposition;
use papermachine_protocol::ToolExecutionState;
use papermachine_protocol::Turn;
use papermachine_protocol::TurnId;
use papermachine_protocol::TurnOrigin;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WebSearchContextSize;
use papermachine_protocol::WorkflowId;
use papermachine_protocol::WorkflowStatus;
use papermachine_protocol::WorkflowUsage;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_skills::ResolvedSkills;
use papermachine_skills::SkillError;
use papermachine_store::Store;
use papermachine_store::StoreError;
use papermachine_tools::ToolCatalog;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolError;
use papermachine_tools::ToolReconciliation;
use papermachine_tools::model_visible_tool_result;
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

const RUNTIME_SYSTEM_PROMPT: &str = "You are an agent working in a persistent PaperMachine Session. Complete the current request using the available tools and prior Session context. Preserve exact evidence and provenance, distinguish verified observations from inference, and state material uncertainty or limitations. Runtime permissions are enforced by code; never claim capabilities or completed tool actions that are not present in the Session history. If a recovered tool result is marked execution_unknown, inspect durable Workspace or external state before deciding whether any effect should be repeated.";
const PROCESS_RESTART_INTERRUPTION: &str = "The PaperMachine server stopped during the previous standalone Turn. That Turn was interrupted and was not resumed automatically. Tool calls marked execution_unknown may have partially or fully executed; inspect durable Workspace or external state before repeating them. Continue only in response to explicit user direction.";

#[derive(Clone)]
pub struct SessionRuntime {
    inner: Arc<SessionRuntimeInner>,
}

struct SessionRuntimeInner {
    store: Arc<Store>,
    model: Arc<dyn ModelClient>,
    tools: ToolCatalog,
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
        tools: ToolCatalog,
        skills: Arc<ProjectSkillCatalog>,
        config: SessionRuntimeConfig,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(config.max_concurrent_turns.max(1)));
        Self::new_with_permits(store, model, tools, skills, config, permits)
    }

    pub fn new_with_permits(
        store: Arc<Store>,
        model: Arc<dyn ModelClient>,
        tools: ToolCatalog,
        skills: Arc<ProjectSkillCatalog>,
        config: SessionRuntimeConfig,
        permits: Arc<Semaphore>,
    ) -> Self {
        Self {
            inner: Arc::new(SessionRuntimeInner {
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
                None,
                true,
                None,
                None,
                None,
                None,
            )
            .await?;
        self.schedule(turn.id).await?;
        Ok(turn)
    }

    pub async fn resume_interrupted(
        &self,
        interrupted_turn_id: TurnId,
    ) -> Result<Turn, SessionRuntimeError> {
        let interrupted = self.inner.store.get_turn(interrupted_turn_id)?;
        let turn = self
            .prepare_turn(
                interrupted.session_id,
                TurnOrigin::User,
                format!("Resume interrupted Turn {interrupted_turn_id}."),
                None,
                prompt_layer_from_text(
                    PromptLayerKind::Control,
                    "Process-recovery instruction",
                    "runtime:resume-interrupted-turn",
                    "Continue the original task from durable Session context. Inspect any execution_unknown tool outcomes before repeating side effects.",
                ),
                None,
                None,
                true,
                None,
                None,
                None,
                Some(interrupted_turn_id),
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
                None,
                true,
                None,
                None,
                None,
                None,
            )
            .await?;
        self.run_tracked_turn(turn.id, None, cancellation).await?;
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
        requested_tools: Vec<String>,
        tools_enabled: bool,
        web_search_context_size: Option<WebSearchContextSize>,
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
                Some(requested_tools),
                tools_enabled,
                web_search_context_size,
                response_format,
                Some(context.action_attempt_id),
                None,
            )
            .await?;
        self.run_tracked_turn(turn.id, Some(context), cancellation)
            .await?;
        Ok(self.inner.store.get_turn(turn.id)?)
    }

    pub async fn resume_workflow_action(
        &self,
        turn_id: TurnId,
        context: WorkflowTurnContext,
        cancellation: CancellationToken,
    ) -> Result<Turn, SessionRuntimeError> {
        let turn = self.inner.store.get_turn(turn_id)?;
        let recovered_steps = self
            .recover_orphaned_steps(&turn, Some(context), cancellation.clone())
            .await?;
        if recovered_steps > 0 {
            self.inner.store.append_session_event(
                turn.session_id,
                Some(turn_id),
                None,
                SessionEventPayload::AssistantMessageReset,
            )?;
        }
        self.run_tracked_turn(turn_id, Some(context), cancellation)
            .await?;
        Ok(self.inner.store.get_turn(turn_id)?)
    }

    async fn run_tracked_turn(
        &self,
        turn_id: TurnId,
        workflow_context: Option<WorkflowTurnContext>,
        parent_cancellation: CancellationToken,
    ) -> Result<(), SessionRuntimeError> {
        let cancellation = parent_cancellation.child_token();
        {
            let mut active = self.inner.active.lock().await;
            if active.contains_key(&turn_id) {
                return Err(SessionRuntimeError::Scheduling(format!(
                    "Turn {turn_id} is already running"
                )));
            }
            active.insert(turn_id, cancellation.clone());
        }
        let result = run_scheduled_turn(
            Arc::clone(&self.inner),
            turn_id,
            workflow_context,
            cancellation,
        )
        .await;
        self.inner.active.lock().await.remove(&turn_id);
        result
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
        requested_tools: Option<Vec<String>>,
        tools_enabled: bool,
        web_search_context_size: Option<WebSearchContextSize>,
        response_format: Option<ModelResponseFormat>,
        action_attempt_id: Option<ActionAttemptId>,
        resumed_from_turn_id: Option<TurnId>,
    ) -> Result<Turn, SessionRuntimeError> {
        let session = self.inner.store.get_session(session_id)?;
        let model = if model_override.is_some_and(|model| !model.trim().is_empty()) {
            model_override.unwrap_or_default().trim().to_string()
        } else if session.model.trim().is_empty() {
            self.inner.default_model.clone()
        } else {
            session.model.clone()
        };
        let resolved = self
            .inner
            .skills
            .resolve(session.project_id, &session.enabled_skills)?;
        let project_prompt = self
            .inner
            .store
            .get_project_system_prompt(session.project_id)?;
        let prompt = build_prompt_snapshot(&session, project_prompt, prompt_layers, &resolved);
        let tool_set = match requested_tools {
            Some(requested) => self.inner.tools.materialize_action_tools(
                &requested,
                session.access,
                tools_enabled,
            )?,
            None => self
                .inner
                .tools
                .materialize_session_tools(session.access, tools_enabled)?,
        };
        let turn = if let Some(attempt_id) = action_attempt_id {
            self.inner.store.create_turn_for_attempt(
                attempt_id,
                session_id,
                origin,
                input,
                model,
                prompt,
                reasoning_effort,
                tools_enabled,
                session.access,
                tool_set,
                web_search_context_size,
                response_format,
                resolved.snapshots,
            )?
        } else if let Some(interrupted_turn_id) = resumed_from_turn_id {
            self.inner.store.create_resumed_turn(
                interrupted_turn_id,
                session_id,
                input,
                model,
                prompt,
                reasoning_effort,
                tools_enabled,
                session.access,
                tool_set,
                web_search_context_size,
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
                tools_enabled,
                session.access,
                tool_set,
                web_search_context_size,
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
            self.inner
                .skills
                .resolve_snapshots(session.project_id, &turn.skill_snapshots)?;
            let rollout = self.inner.store.reconstruct_session_rollout(session.id)?;
            let active = rollout.active_turn.ok_or_else(|| {
                StoreError::Invariant(format!(
                    "Session rollout has no active state for resumable Turn {}",
                    turn.id
                ))
            })?;
            if active.turn_id != turn.id {
                return Err(StoreError::Invariant(format!(
                    "Session rollout active Turn {} does not match resumable Turn {}",
                    active.turn_id, turn.id
                ))
                .into());
            }
            if let Some(message) = active.checkpoint_message.clone() {
                self.inner
                    .store
                    .complete_turn(turn.id, message, active.usage)?;
                recovered.push(turn.id);
                continue;
            }

            let settled_steps = self
                .recover_orphaned_steps(&turn, None, CancellationToken::new())
                .await?;
            if settled_steps > 0 {
                self.inner.store.append_session_event(
                    turn.session_id,
                    Some(turn.id),
                    None,
                    SessionEventPayload::AssistantMessageReset,
                )?;
            }
            self.interrupt_standalone_after_restart(&turn)?;
            recovered.push(turn.id);
        }
        Ok(recovered)
    }

    async fn recover_orphaned_steps(
        &self,
        turn: &Turn,
        workflow_context: Option<WorkflowTurnContext>,
        cancellation: CancellationToken,
    ) -> Result<usize, SessionRuntimeError> {
        let steps = self
            .inner
            .store
            .list_steps(turn.id)?
            .into_iter()
            .filter(|step| step.status == StepStatus::Running)
            .collect::<Vec<_>>();
        if steps.is_empty() {
            return Ok(0);
        }
        let session = self.inner.store.get_session(turn.session_id)?;
        let registry = self
            .inner
            .tools
            .registry_for_snapshot(&turn.tool_set, workflow_context.is_some())?;
        for step in &steps {
            if step.kind != StepKind::Tool {
                self.inner.store.finish_step(
                    step.id,
                    StepStatus::Cancelled,
                    Some(json!({
                        "error": "server process stopped while this internal Step was running",
                        "recovered": true,
                    })),
                    TokenUsage::default(),
                    None,
                )?;
                continue;
            }

            let Some(call_id) = step.tool_call_id.as_ref() else {
                self.finish_recovered_tool_step(
                    turn,
                    step,
                    StepStatus::ExecutionUnknown,
                    recovery_unknown_output(
                        step,
                        "Tool Step has no durable provider call identity",
                    ),
                    false,
                    0,
                )?;
                continue;
            };
            let disposition = step
                .effect_disposition
                .unwrap_or(ToolEffectDisposition::Unknown);
            let execution_state = step
                .execution_state
                .unwrap_or(ToolExecutionState::ExecutionUnknown);

            let Some(context) = workflow_context else {
                let (status, output) = if execution_state == ToolExecutionState::Prepared {
                    (
                        StepStatus::Cancelled,
                        json!({
                            "ok": false,
                            "error": "server stopped before the prepared tool crossed its execution boundary",
                            "recovery": {
                                "effect_id": call_id,
                                "effect_disposition": disposition,
                                "execution_state": execution_state,
                                "automatic_replay": false,
                            }
                        }),
                    )
                } else {
                    (
                        StepStatus::ExecutionUnknown,
                        recovery_unknown_output(
                            step,
                            "standalone Turns are never auto-resumed after process loss",
                        ),
                    )
                };
                self.finish_recovered_tool_step(turn, step, status, output, false, 0)?;
                continue;
            };

            let registered_disposition = registry.effect_disposition(&step.name);
            if registered_disposition != Some(disposition) {
                let output = recovery_unknown_output(
                    step,
                    "the registered tool is missing or its effect disposition changed",
                );
                let status = if execution_state == ToolExecutionState::Prepared {
                    StepStatus::Failed
                } else {
                    StepStatus::ExecutionUnknown
                };
                self.finish_recovered_tool_step(turn, step, status, output, false, 0)?;
                continue;
            }

            let tool_context = ToolContext {
                project_id: session.project_id,
                session_id: session.id,
                turn_id: turn.id,
                workflow_id: Some(context.workflow_id),
                action_invocation_id: Some(context.action_invocation_id),
                action_attempt_id: Some(context.action_attempt_id),
                effect_id: call_id.clone(),
                sandbox_root: self
                    .inner
                    .store
                    .managed_root()
                    .join("runtime/sandboxes")
                    .join(session.id.to_string())
                    .join(turn.id.to_string()),
                authorization: turn.environment.authorization.clone(),
                cancellation: cancellation.clone(),
            };

            let started = std::time::Instant::now();
            let result = match execution_state {
                ToolExecutionState::Prepared => {
                    // This is the recovery equivalent of
                    // `AgentEvent::ToolExecutionStarted`: persist that the
                    // external-effect boundary may be crossed before invoking
                    // the tool. A second crash must never see this call as
                    // still safely prepared.
                    self.inner.store.start_tool_execution(step.id)?;
                    registry
                        .execute(&step.name, tool_context, step.input.clone())
                        .await
                }
                ToolExecutionState::Executing => match disposition {
                    ToolEffectDisposition::Pure | ToolEffectDisposition::Idempotent => {
                        registry
                            .execute(&step.name, tool_context, step.input.clone())
                            .await
                    }
                    ToolEffectDisposition::Reconcilable => match registry
                        .reconcile(&step.name, tool_context.clone(), step.input.clone())
                        .await
                    {
                        Ok(ToolReconciliation::Completed(output)) => Ok(output),
                        Ok(ToolReconciliation::Retry) => {
                            registry
                                .execute(&step.name, tool_context, step.input.clone())
                                .await
                        }
                        Ok(ToolReconciliation::Unknown { message }) => {
                            let output = recovery_unknown_output(step, &message);
                            self.finish_recovered_tool_step(
                                turn,
                                step,
                                StepStatus::ExecutionUnknown,
                                output,
                                false,
                                elapsed_millis(started),
                            )?;
                            continue;
                        }
                        Err(error) => {
                            let output = recovery_unknown_output(
                                step,
                                &format!("tool reconciliation failed: {error}"),
                            );
                            self.finish_recovered_tool_step(
                                turn,
                                step,
                                StepStatus::ExecutionUnknown,
                                output,
                                false,
                                elapsed_millis(started),
                            )?;
                            continue;
                        }
                    },
                    ToolEffectDisposition::Unknown => {
                        let output = recovery_unknown_output(
                            step,
                            "unknown external effects are never replayed automatically",
                        );
                        self.finish_recovered_tool_step(
                            turn,
                            step,
                            StepStatus::ExecutionUnknown,
                            output,
                            false,
                            elapsed_millis(started),
                        )?;
                        continue;
                    }
                },
                ToolExecutionState::Completed | ToolExecutionState::ExecutionUnknown => {
                    let output = recovery_unknown_output(
                        step,
                        "running Step has an inconsistent terminal execution state",
                    );
                    self.finish_recovered_tool_step(
                        turn,
                        step,
                        StepStatus::ExecutionUnknown,
                        output,
                        false,
                        elapsed_millis(started),
                    )?;
                    continue;
                }
            };
            let (output, success) = model_visible_tool_result(result);
            self.finish_recovered_tool_step(
                turn,
                step,
                if success {
                    StepStatus::Completed
                } else {
                    StepStatus::Failed
                },
                output,
                success,
                elapsed_millis(started),
            )?;
        }
        Ok(steps.len())
    }

    fn finish_recovered_tool_step(
        &self,
        turn: &Turn,
        step: &AgentStep,
        status: StepStatus,
        output: Value,
        success: bool,
        duration_ms: u64,
    ) -> Result<(), SessionRuntimeError> {
        self.inner.store.finish_step(
            step.id,
            status,
            Some(output.clone()),
            TokenUsage::default(),
            Some(duration_ms),
        )?;
        self.inner.store.append_session_event(
            turn.session_id,
            Some(turn.id),
            Some(step.id),
            SessionEventPayload::ToolCallCompleted {
                call_id: step.tool_call_id.clone().unwrap_or_default(),
                tool_name: step.name.clone(),
                output,
                duration_ms,
                success,
            },
        )?;
        Ok(())
    }

    fn interrupt_standalone_after_restart(&self, turn: &Turn) -> Result<(), SessionRuntimeError> {
        let rollout = self
            .inner
            .store
            .reconstruct_session_rollout(turn.session_id)?;
        let active = rollout.active_turn.ok_or_else(|| {
            StoreError::Invariant(format!(
                "Session rollout has no active state for standalone Turn {}",
                turn.id
            ))
        })?;
        if active.turn_id != turn.id {
            return Err(StoreError::Invariant(format!(
                "Session rollout active Turn {} does not match standalone Turn {}",
                active.turn_id, turn.id
            ))
            .into());
        }
        let previous = active.context;
        let mut interrupted = previous.clone();
        if !active.has_checkpoint {
            interrupted.push(ModelInputItem::Message {
                role: MessageRole::User,
                content: turn.input.clone(),
            });
        }
        interrupted =
            repair_interrupted_tool_calls(interrupted, &self.inner.store.list_steps(turn.id)?);
        interrupted.push(ModelInputItem::Message {
            role: MessageRole::Developer,
            content: PROCESS_RESTART_INTERRUPTION.to_string(),
        });
        if !interrupted.starts_with(&previous) {
            return Err(StoreError::Invariant(
                "standalone interruption did not preserve the rollout prefix".to_string(),
            )
            .into());
        }
        self.inner.store.checkpoint_turn_context(
            turn.id,
            ModelContextMutation::Append {
                items: interrupted[previous.len()..].to_vec(),
            },
            active.usage,
            active.completed_model_steps,
            active.hosted_search_calls_used,
            None,
        )?;
        self.inner
            .store
            .interrupt_turn(turn.id, "server process restarted during a standalone Turn")?;
        Ok(())
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
            TurnStatus::Queued | TurnStatus::Running | TurnStatus::Paused
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
    let tools = inner
        .tools
        .registry_for_snapshot(&turn.tool_set, workflow_context.is_some())?;
    let session = inner.store.get_session(turn.session_id)?;
    let rollout = inner.store.reconstruct_session_rollout(session.id)?;
    let active_rollout = rollout.active_turn.ok_or_else(|| {
        StoreError::Invariant(format!(
            "Session rollout has no active state for running Turn {}",
            turn.id
        ))
    })?;
    if active_rollout.turn_id != turn.id {
        return Err(StoreError::Invariant(format!(
            "Session rollout active Turn {} does not match running Turn {}",
            active_rollout.turn_id, turn.id
        ))
        .into());
    }
    let resume_current_turn = active_rollout.has_checkpoint;
    let rollout_context = active_rollout.context;
    let history = if resume_current_turn {
        repair_interrupted_tool_calls(rollout_context.clone(), &inner.store.list_steps(turn.id)?)
    } else {
        rollout_context.clone()
    };
    let workflow_id = workflow_context.as_ref().map(|context| context.workflow_id);
    let event_sink = Arc::new(SessionAgentEventSink::new(
        Arc::clone(&inner.store),
        session.id,
        turn.id,
        workflow_id,
        rollout_context,
    ));
    let events: Arc<dyn AgentEventSink> = event_sink.clone();
    let control: Arc<dyn AgentControlPlane> = Arc::new(StoreAgentControlPlane {
        store: Arc::clone(&inner.store),
    });
    let runtime = AgentRuntime::new(Arc::clone(&inner.model), tools, events).with_control(control);
    let mut request = AgentTurnRequest::new(
        session.project_id,
        session.id,
        turn.id,
        turn.environment.clone(),
        inner
            .store
            .managed_root()
            .join("runtime/sandboxes")
            .join(session.id.to_string())
            .join(turn.id.to_string()),
        turn.model.clone(),
        turn.prompt.rendered.clone(),
        turn.input.clone(),
    );
    if let Some(context) = workflow_context {
        request.workflow_id = Some(context.workflow_id);
        request.action_invocation_id = Some(context.action_invocation_id);
        request.action_attempt_id = Some(context.action_attempt_id);
    }
    request.initial_history = history;
    request.initial_usage = active_rollout.usage;
    request.completed_model_steps = active_rollout.completed_model_steps;
    request.hosted_search_calls_used = active_rollout.hosted_search_calls_used;
    request.resume_current_turn = resume_current_turn;
    request.checkpoint_message = active_rollout.checkpoint_message;
    request.reasoning_effort = turn.reasoning_effort;
    request.tools_enabled = turn.tools_enabled;
    request.web_search_context_size = turn.web_search_context_size;
    request.response_format = turn.response_format;
    request.model_context_window = inner
        .model
        .model_context_window(&turn.model)
        .unwrap_or(inner.model_context_window);
    match runtime.run(request, cancellation).await {
        Ok(result) => {
            papermachine_store::process_fault::reach_process_fault_boundary(
                papermachine_store::process_fault::TURN_TERMINAL_CHECKPOINTED_BEFORE_COMMIT,
            );
            inner
                .store
                .complete_turn(turn.id, result.final_message, result.usage)?;
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
                WorkflowStatus::Created | WorkflowStatus::Running => break,
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

fn recovery_unknown_output(step: &AgentStep, message: &str) -> Value {
    json!({
        "ok": false,
        "error": "tool execution state is unknown after process restart",
        "recovery": {
            "message": message,
            "effect_id": step.tool_call_id,
            "effect_disposition": step.effect_disposition,
            "execution_state": step.execution_state,
            "automatic_replay": false,
        }
    })
}

fn elapsed_millis(started: std::time::Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
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
    checkpoint: Mutex<RolloutCheckpointState>,
}

struct RolloutCheckpointState {
    context: Vec<ModelInputItem>,
    replacement_reason: Option<ContextReplacementReason>,
}

impl SessionAgentEventSink {
    fn new(
        store: Arc<Store>,
        session_id: SessionId,
        turn_id: TurnId,
        workflow_id: Option<WorkflowId>,
        context: Vec<ModelInputItem>,
    ) -> Self {
        Self {
            store,
            session_id,
            turn_id,
            workflow_id,
            model_steps: Mutex::new(HashMap::new()),
            tool_steps: Mutex::new(HashMap::new()),
            compaction_steps: Mutex::new(Vec::new()),
            checkpoint: Mutex::new(RolloutCheckpointState {
                context,
                replacement_reason: None,
            }),
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
                    self.store
                        .add_workflow_usage(
                            workflow_id,
                            WorkflowUsage {
                                tokens: usage,
                                ..WorkflowUsage::default()
                            },
                        )
                        .map_err(|error| error.to_string())?;
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
                    self.store
                        .add_workflow_usage(
                            workflow_id,
                            WorkflowUsage {
                                tokens: usage,
                                ..WorkflowUsage::default()
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
                self.append(
                    step_id,
                    SessionEventPayload::ModelStepFailed { step, error, usage },
                )
            }
            AgentEvent::ToolCallStarted {
                call,
                effect_disposition,
            } => {
                let input = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone()));
                let step = self
                    .store
                    .create_tool_step(
                        self.turn_id,
                        call.call_id.clone(),
                        call.name.clone(),
                        input,
                        effect_disposition,
                    )
                    .map_err(|error| error.to_string())?;
                self.tool_steps
                    .lock()
                    .await
                    .insert(call.call_id.clone(), step.id);
                self.charge_action_step().await?;
                self.append(Some(step.id), SessionEventPayload::ToolCallStarted { call })?;
                papermachine_store::process_fault::reach_process_fault_boundary(
                    papermachine_store::process_fault::TOOL_PREPARED_BEFORE_EXECUTION,
                );
                Ok(())
            }
            AgentEvent::ToolExecutionStarted { call_id } => {
                let step_id = self.tool_steps.lock().await.get(&call_id).copied();
                let step_id = step_id.ok_or_else(|| {
                    format!("tool execution started without a prepared Step for {call_id}")
                })?;
                self.store
                    .start_tool_execution(step_id)
                    .map(|_| ())
                    .map_err(|error| error.to_string())?;
                papermachine_store::process_fault::reach_process_fault_boundary(
                    papermachine_store::process_fault::TOOL_EXECUTION_STARTED,
                );
                Ok(())
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
                        .add_workflow_usage(
                            workflow_id,
                            WorkflowUsage {
                                hosted_search_calls: 1,
                                ..WorkflowUsage::default()
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
                let mut checkpoint = self.checkpoint.lock().await;
                if checkpoint.replacement_reason.is_none() {
                    checkpoint.replacement_reason = Some(ContextReplacementReason::Trim);
                }
                drop(checkpoint);
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
                    self.store
                        .add_workflow_usage(
                            workflow_id,
                            WorkflowUsage {
                                tokens: usage,
                                ..WorkflowUsage::default()
                            },
                        )
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
                self.store
                    .checkpoint_turn_context(
                        self.turn_id,
                        mutation,
                        usage,
                        completed_model_steps,
                        hosted_search_calls_used,
                        message,
                    )
                    .map_err(|error| error.to_string())?;
                checkpoint.context = history;
                checkpoint.replacement_reason = None;
                Ok(())
            }
        }
    }
}

impl SessionAgentEventSink {
    async fn charge_action_step(&self) -> Result<(), String> {
        let Some(workflow_id) = self.workflow_id else {
            return Ok(());
        };
        self.store
            .add_workflow_usage(
                workflow_id,
                WorkflowUsage {
                    action_steps: 1,
                    ..WorkflowUsage::default()
                },
            )
            .map_err(|error| error.to_string())?;
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

#[derive(Debug, Error)]
pub enum SessionRuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
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
    use tempfile::tempdir;

    #[test]
    fn recovery_reuses_a_completed_tool_step_output() {
        let directory = tempdir().expect("temporary directory should be created");
        let store =
            Store::open_in_memory(directory.path().join("managed")).expect("Store should open");
        let project = store
            .create_project("Tool recovery", directory.path().join("project"))
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
                true,
                papermachine_protocol::AccessPreset::Research,
                papermachine_protocol::ToolSetSnapshot::materialize(Vec::new())
                    .expect("empty tool set should be valid"),
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
                papermachine_protocol::ToolEffectDisposition::Pure,
            )
            .expect("Tool Step should be created");
        store
            .start_tool_execution(step.id)
            .expect("Tool Step should cross its execution boundary");
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
