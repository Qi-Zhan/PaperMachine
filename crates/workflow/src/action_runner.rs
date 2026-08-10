use papermachine_protocol::ActionInvocation;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::ActionStatus;
use papermachine_protocol::AgentId;
use papermachine_protocol::PromptLayerKind;
use papermachine_protocol::SessionId;
use papermachine_protocol::SessionStatus;
use papermachine_protocol::TurnStatus;
use papermachine_session::ActionTurnContext;
use papermachine_session::PromptLayerInput;
use papermachine_session::TurnRuntime;
use papermachine_session::TurnRuntimeError;
use papermachine_store::StoreError;
use papermachine_store::StoreHandle;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use thiserror::Error;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// The only runtime path from a durable ActionInvocation to a Turn.
///
/// The runner derives work from Store state. Callers only admit Actions and
/// wait for their durable result; they never execute a Turn themselves.
#[derive(Clone)]
pub struct ActionRunner {
    store: StoreHandle,
    turns: TurnRuntime,
}

impl ActionRunner {
    pub fn new(store: StoreHandle, turns: TurnRuntime) -> Self {
        Self { store, turns }
    }

    pub async fn run_session(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), ActionRunnerError> {
        let mut events = self
            .store
            .call::<_, StoreError, _>(|store| Ok(store.subscribe()))
            .await?;
        let mut active_agents = HashSet::new();
        let mut tasks = JoinSet::new();
        let mut closing = false;

        loop {
            let (session, actions) = self
                .store
                .call(move |store| {
                    Ok::<_, StoreError>((
                        store.get_session(session_id)?,
                        store.list_action_invocations(session_id)?,
                    ))
                })
                .await?;
            if session.status.is_terminal() || session.status == SessionStatus::Closing {
                closing = session.status == SessionStatus::Closing;
                break;
            }

            if session.status == SessionStatus::Running {
                for action in first_pending_action_per_agent(actions) {
                    if active_agents.insert(action.agent_id) {
                        let runner = self.clone();
                        let child = cancellation.child_token();
                        tasks.spawn(async move {
                            let result = runner.run_action(action.id, child).await;
                            (action.agent_id, result)
                        });
                    }
                }
            }

            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = events.recv() => {},
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    match joined {
                        Some(Ok((agent_id, result))) => {
                            active_agents.remove(&agent_id);
                            if let Err(error) = result
                                && !matches!(error, ActionRunnerError::Cancelled)
                            {
                                return Err(error);
                            }
                        }
                        Some(Err(error)) => {
                            return Err(ActionRunnerError::Join(error.to_string()));
                        }
                        None => {}
                    }
                }
            }
        }

        cancellation.cancel();
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((_agent_id, Ok(()))) => {}
                Ok((_agent_id, Err(ActionRunnerError::Cancelled))) => {}
                Ok((_agent_id, Err(error))) => return Err(error),
                Err(error) => return Err(ActionRunnerError::Join(error.to_string())),
            }
        }
        if !closing {
            closing = self
                .store
                .call(move |store| Ok::<_, StoreError>(store.get_session(session_id)?.status))
                .await?
                == SessionStatus::Closing;
        }
        if closing {
            self.cancel_remaining_actions(session_id).await?;
        }
        Ok(())
    }

    async fn cancel_remaining_actions(
        &self,
        session_id: SessionId,
    ) -> Result<(), ActionRunnerError> {
        let actions = self
            .store
            .call(move |store| store.list_action_invocations(session_id))
            .await?;
        for action in actions {
            match action.status {
                ActionStatus::Scheduled | ActionStatus::Interrupted => {
                    self.store
                        .call(move |store| {
                            store.cancel_pending_action(
                                action.id,
                                "Session completed before this Action ran",
                            )
                        })
                        .await?;
                }
                ActionStatus::Running => {
                    self.finish_running_action_during_close(action).await?;
                }
                ActionStatus::Completed | ActionStatus::Failed | ActionStatus::Cancelled => {}
            }
        }
        Ok(())
    }

    async fn finish_running_action_during_close(
        &self,
        action: ActionInvocation,
    ) -> Result<(), ActionRunnerError> {
        let attempts = self
            .store
            .call(move |store| store.list_action_attempts(action.id))
            .await?;
        let attempt = attempts
            .into_iter()
            .rev()
            .find(|attempt| attempt.status == ActionStatus::Running)
            .ok_or_else(|| {
                StoreError::Invariant(format!(
                    "running Action {} has no running attempt",
                    action.id
                ))
            })?;
        let (status, output, error) = if let Some(turn_id) = attempt.turn_id {
            let turn = self
                .store
                .call(move |store| store.get_turn(turn_id))
                .await?;
            match turn.status {
                TurnStatus::Completed => (
                    ActionStatus::Completed,
                    Some(json!({
                        "message": turn.output.unwrap_or_default(),
                        "turn_id": turn.id,
                        "hosted_search_calls_used": turn.hosted_search_calls_used,
                    })),
                    None,
                ),
                TurnStatus::Failed => (
                    ActionStatus::Failed,
                    None,
                    Some(
                        turn.error
                            .unwrap_or_else(|| "Action Turn failed".to_string()),
                    ),
                ),
                TurnStatus::Queued | TurnStatus::Running | TurnStatus::Paused => {
                    self.turns.cancel(turn.id).await?;
                    (
                        ActionStatus::Cancelled,
                        None,
                        Some("Session completed while Action was running".to_string()),
                    )
                }
                TurnStatus::Interrupted | TurnStatus::Cancelled => (
                    ActionStatus::Cancelled,
                    None,
                    Some(turn.error.unwrap_or_else(|| {
                        "Session completed while Action was running".to_string()
                    })),
                ),
            }
        } else {
            (
                ActionStatus::Cancelled,
                None,
                Some("Session completed before the Action Turn started".to_string()),
            )
        };
        self.store
            .call(move |store| store.finish_action(action.id, attempt.id, status, output, error))
            .await?;
        Ok(())
    }

    async fn run_action(
        &self,
        invocation_id: ActionInvocationId,
        cancellation: CancellationToken,
    ) -> Result<(), ActionRunnerError> {
        let (invocation, session, mut recovered_attempt) = self
            .store
            .call(move |store| {
                let invocation = store.get_action_invocation(invocation_id)?;
                let session = store.get_session(invocation.session_id)?;
                let recovered_attempt = store
                    .list_action_attempts(invocation_id)?
                    .into_iter()
                    .rev()
                    .find(|attempt| !attempt.status.is_terminal());
                Ok::<_, StoreError>((invocation, session, recovered_attempt))
            })
            .await?;
        if matches!(
            invocation.status,
            ActionStatus::Completed | ActionStatus::Failed | ActionStatus::Cancelled
        ) {
            return Ok(());
        }

        let mut interruption_guidance = (invocation.status == ActionStatus::Interrupted)
            .then(|| invocation.error.clone())
            .flatten();
        loop {
            if cancellation.is_cancelled() {
                return Err(ActionRunnerError::Cancelled);
            }
            let current_session = self
                .store
                .call(move |store| store.get_session(session.id))
                .await?;
            if current_session.status != SessionStatus::Running {
                return Err(ActionRunnerError::Cancelled);
            }
            let attempt = match recovered_attempt.take() {
                Some(attempt) => attempt,
                None => {
                    self.store
                        .call(move |store| store.start_action_attempt(invocation_id))
                        .await?
                }
            };
            let context = ActionTurnContext {
                action_invocation_id: invocation.id,
                action_attempt_id: attempt.id,
            };
            let result = if let Some(turn_id) = attempt.turn_id {
                let turn = self
                    .store
                    .call(move |store| store.get_turn(turn_id))
                    .await?;
                match turn.status {
                    TurnStatus::Completed => Ok(turn),
                    TurnStatus::Queued | TurnStatus::Running | TurnStatus::Paused => {
                        self.turns
                            .resume_action_attempt(turn.id, context, cancellation.child_token())
                            .await
                    }
                    TurnStatus::Interrupted => Err(TurnRuntimeError::Interrupted(
                        turn.error
                            .unwrap_or_else(|| "Action Turn was interrupted".to_string()),
                    )),
                    TurnStatus::Cancelled => Err(TurnRuntimeError::Cancelled),
                    TurnStatus::Failed => Err(TurnRuntimeError::Scheduling(
                        turn.error
                            .unwrap_or_else(|| "Action Turn failed before restart".to_string()),
                    )),
                }
            } else {
                let mut prompt_layers = Vec::new();
                if !session.instructions.trim().is_empty() {
                    prompt_layers.push(PromptLayerInput::new(
                        PromptLayerKind::Session,
                        "Session instructions",
                        format!("session:{}:instructions", session.id),
                        &session.instructions,
                    ));
                }
                if !invocation.contract.trim().is_empty() {
                    prompt_layers.push(PromptLayerInput::new(
                        PromptLayerKind::Session,
                        "Action contract",
                        format!(
                            "session:{}:action-contract:{}",
                            session.id, invocation.action_name
                        ),
                        &invocation.contract,
                    ));
                }
                if let Some(guidance) = interruption_guidance.take() {
                    prompt_layers.push(PromptLayerInput::new(
                        PromptLayerKind::Control,
                        "Human interruption guidance",
                        format!("action-attempt:{}:guidance", attempt.id),
                        guidance,
                    ));
                }
                self.turns
                    .execute_action_attempt(
                        invocation.agent_id,
                        invocation.input.clone(),
                        None,
                        prompt_layers,
                        invocation.reasoning_effort,
                        invocation.tool_policy.clone(),
                        invocation.web_search_context_size,
                        invocation.response_format.clone(),
                        context,
                        cancellation.child_token(),
                    )
                    .await
            };

            match result {
                Ok(turn) => {
                    let output = json!({
                        "message": turn.output.clone().unwrap_or_default(),
                        "turn_id": turn.id,
                        "hosted_search_calls_used": turn.hosted_search_calls_used,
                    });
                    self.store
                        .call(move |store| {
                            store.finish_action(
                                invocation_id,
                                attempt.id,
                                ActionStatus::Completed,
                                Some(output),
                                None,
                            )
                        })
                        .await?;
                    return Ok(());
                }
                Err(TurnRuntimeError::Interrupted(reason)) => {
                    let stored_reason = reason.clone();
                    self.store
                        .call(move |store| {
                            store.finish_action(
                                invocation_id,
                                attempt.id,
                                ActionStatus::Interrupted,
                                None,
                                Some(stored_reason),
                            )
                        })
                        .await?;
                    interruption_guidance = Some(reason);
                }
                Err(TurnRuntimeError::Cancelled) if cancellation.is_cancelled() => {
                    self.store
                        .call(move |store| {
                            store.finish_action(
                                invocation_id,
                                attempt.id,
                                ActionStatus::Cancelled,
                                None,
                                Some("Session cancelled".to_string()),
                            )
                        })
                        .await?;
                    return Err(ActionRunnerError::Cancelled);
                }
                Err(error) => {
                    let message = error.to_string();
                    let stored_message = message.clone();
                    self.store
                        .call(move |store| {
                            store.finish_action(
                                invocation_id,
                                attempt.id,
                                ActionStatus::Failed,
                                None,
                                Some(stored_message),
                            )
                        })
                        .await?;
                    return Ok(());
                }
            }
        }
    }
}

fn first_pending_action_per_agent(actions: Vec<ActionInvocation>) -> Vec<ActionInvocation> {
    let mut first = HashMap::<AgentId, ActionInvocation>::new();
    for action in actions {
        if matches!(
            action.status,
            ActionStatus::Scheduled | ActionStatus::Running | ActionStatus::Interrupted
        ) {
            first.entry(action.agent_id).or_insert(action);
        }
    }
    first.into_values().collect()
}

pub async fn wait_for_action(
    store: &StoreHandle,
    invocation_id: ActionInvocationId,
    cancellation: &CancellationToken,
) -> Result<ActionInvocation, ActionRunnerError> {
    let mut events = store
        .call::<_, StoreError, _>(|store| Ok(store.subscribe()))
        .await?;
    loop {
        let invocation = store
            .call(move |store| store.get_action_invocation(invocation_id))
            .await?;
        if matches!(
            invocation.status,
            ActionStatus::Completed | ActionStatus::Failed | ActionStatus::Cancelled
        ) {
            return Ok(invocation);
        }
        tokio::select! {
            _ = cancellation.cancelled() => return Err(ActionRunnerError::Cancelled),
            _ = events.recv() => {}
        }
    }
}

#[derive(Debug, Error)]
pub enum ActionRunnerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Runtime(#[from] TurnRuntimeError),
    #[error("Action runner task failed: {0}")]
    Join(String),
    #[error("Action runner was cancelled")]
    Cancelled,
}
