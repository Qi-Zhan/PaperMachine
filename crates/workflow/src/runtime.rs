use async_trait::async_trait;
use chrono::Utc;
use papermachine_protocol::*;
use papermachine_session::PromptLayerInput;
use papermachine_session::SessionRuntime;
use papermachine_session::SessionRuntimeError;
use papermachine_session::WorkflowTurnContext;
use papermachine_store::Store;
use papermachine_tools::HumanRequestBroker;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolError;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::WorkflowRuntime;

const MAX_RUNTIME_STDERR_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct PythonWorkflowRuntime {
    store: Arc<Store>,
    sessions: SessionRuntime,
    python: PathBuf,
    python_runtime_root: PathBuf,
    work_root: PathBuf,
}

impl PythonWorkflowRuntime {
    pub fn new(
        store: Arc<Store>,
        sessions: SessionRuntime,
        python: impl Into<PathBuf>,
        python_runtime_root: impl Into<PathBuf>,
        work_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            store,
            sessions,
            python: python.into(),
            python_runtime_root: python_runtime_root.into(),
            work_root: work_root.into(),
        }
    }

    async fn execute_inner(
        &self,
        workflow_id: WorkflowId,
        cancellation: CancellationToken,
    ) -> Result<Value, WorkflowRuntimeError> {
        let run = self.store.get_workflow(workflow_id)?;
        let workspace = self.work_root.join(run.id.to_string());
        materialize_runtime(
            &workspace,
            &self.python_runtime_root,
            &run.program.source_code,
        )
        .await?;
        let mut command = sandboxed_python_command(&self.python, &workspace)?;
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| WorkflowRuntimeError::Spawn(error.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or(WorkflowRuntimeError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(WorkflowRuntimeError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(WorkflowRuntimeError::MissingPipe("stderr"))?;
        let initialization = json!({
            "workflow_id": run.id,
            "objective": run.objective,
            "input": run.input,
        });
        stdin
            .write_all(serde_json::to_string(&initialization)?.as_bytes())
            .await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        let effect_cancellation = cancellation.child_token();
        let context = Arc::new(RunEffectContext {
            store: Arc::clone(&self.store),
            sessions: self.sessions.clone(),
            workflow_id,
            cancellation: effect_cancellation.clone(),
            action_permits: Arc::new(Semaphore::new(
                run.budget.max_concurrent_actions.max(1) as usize
            )),
            agent_gates: Mutex::new(HashMap::new()),
            effect_gates: Mutex::new(HashMap::new()),
            completion: Mutex::new(None),
        });
        let (responses_tx, mut responses_rx) = mpsc::unbounded_channel::<EffectResponse>();
        let writer = tokio::spawn(async move {
            while let Some(response) = responses_rx.recv().await {
                let line = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
                stdin
                    .write_all(&line)
                    .await
                    .map_err(|error| error.to_string())?;
                stdin
                    .write_all(b"\n")
                    .await
                    .map_err(|error| error.to_string())?;
                stdin.flush().await.map_err(|error| error.to_string())?;
            }
            Ok::<(), String>(())
        });
        let stderr_task = tokio::spawn(drain_limited(stderr, MAX_RUNTIME_STDERR_BYTES));
        let mut lines = BufReader::new(stdout).lines();
        let mut handlers = JoinSet::new();
        let mut protocol_error = None;
        loop {
            let next = tokio::select! {
                line = lines.next_line() => line,
                _ = cancellation.cancelled() => {
                    terminate_child(&mut child).await;
                    protocol_error = Some(WorkflowRuntimeError::Cancelled);
                    break;
                }
            };
            let Some(line) = next? else { break };
            let request: EffectRequest = match serde_json::from_str(&line) {
                Ok(request) => request,
                Err(error) => {
                    protocol_error = Some(WorkflowRuntimeError::Protocol(format!(
                        "invalid effect request: {error}; line={line}"
                    )));
                    terminate_child(&mut child).await;
                    break;
                }
            };
            let effect_context = Arc::clone(&context);
            let sender = responses_tx.clone();
            handlers.spawn(async move {
                let id = request.id.clone();
                let response = match effect_context.handle(request).await {
                    Ok(result) => EffectResponse {
                        id,
                        ok: true,
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => EffectResponse {
                        id,
                        ok: false,
                        result: None,
                        error: Some(error.to_string()),
                    },
                };
                let _ = sender.send(response);
            });
        }
        effect_cancellation.cancel();
        while handlers.join_next().await.is_some() {}
        drop(responses_tx);
        let writer_result = writer
            .await
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?
            .map_err(WorkflowRuntimeError::Protocol);
        let status_result = child.wait().await.map_err(WorkflowRuntimeError::Io);
        let stderr_result = stderr_task
            .await
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?
            .map_err(WorkflowRuntimeError::Io);
        if let Some(error) = protocol_error {
            return Err(error);
        }
        let status = status_result?;
        let stderr = stderr_result?;
        if !status.success() {
            return Err(WorkflowRuntimeError::Python {
                code: status.code(),
                stderr: stderr.text.trim().to_string(),
                truncated: stderr.truncated,
            });
        }
        writer_result?;
        context.completion.lock().await.take().ok_or_else(|| {
            WorkflowRuntimeError::Protocol(
                "workflow.py exited without submitting a completion output".to_string(),
            )
        })
    }
}

#[async_trait]
impl WorkflowRuntime for PythonWorkflowRuntime {
    async fn execute(
        &self,
        workflow_id: WorkflowId,
        cancellation: CancellationToken,
    ) -> Result<Value, String> {
        self.execute_inner(workflow_id, cancellation)
            .await
            .map_err(|error| error.to_string())
    }
}

struct RunEffectContext {
    store: Arc<Store>,
    sessions: SessionRuntime,
    workflow_id: WorkflowId,
    cancellation: CancellationToken,
    action_permits: Arc<Semaphore>,
    agent_gates: Mutex<HashMap<AgentInstanceId, Arc<Mutex<()>>>>,
    effect_gates: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    completion: Mutex<Option<Value>>,
}

impl RunEffectContext {
    async fn handle(&self, request: EffectRequest) -> Result<Value, WorkflowRuntimeError> {
        validate_effect_key(&request.id)?;
        let gate = {
            let mut gates = self.effect_gates.lock().await;
            Arc::clone(
                gates
                    .entry(request.id.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = gate.lock().await;
        let effect = self.store.begin_workflow_effect(
            self.workflow_id,
            &request.id,
            &request.kind,
            request.payload.clone(),
        )?;
        match effect.status {
            WorkflowEffectStatus::Completed => {
                if request.kind == "complete" {
                    self.remember_completion(&request.payload).await?;
                }
                return Ok(effect.result.unwrap_or(Value::Null));
            }
            WorkflowEffectStatus::Failed => {
                return Err(WorkflowRuntimeError::ReplayedEffect {
                    key: request.id,
                    error: effect.error.unwrap_or_else(|| "effect failed".to_string()),
                });
            }
            WorkflowEffectStatus::Started => {}
        }
        let key = request.id;
        let result = async {
            self.checkpoint().await?;
            self.dispatch(&key, &request.kind, request.payload).await
        }
        .await;
        self.store.finish_workflow_effect(
            self.workflow_id,
            &key,
            result
                .as_ref()
                .map(Clone::clone)
                .map_err(ToString::to_string),
        )?;
        result
    }

    async fn dispatch(
        &self,
        effect_key: &str,
        kind: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        match kind {
            "create_agent" => self.create_agent(effect_key, payload).await,
            "set_agent_access" => self.set_agent_access(effect_key, payload).await,
            "retire_agent" => self.retire_agent(payload),
            "invoke_action" => self.invoke_action(effect_key, payload).await,
            "create_team" => self.create_team(effect_key, payload),
            "set_team_members" => self.set_team_members(payload),
            "set_relation" => self.set_relation(effect_key, payload),
            "open_scope" => self.open_scope(effect_key, payload),
            "close_scope" => self.close_scope(payload),
            "register_timer" => self.register_timer(effect_key, payload),
            "wait_timer" => self.wait_timer(effect_key, payload).await,
            "create_channel" => self.create_channel(effect_key, payload),
            "publish_signal" => self.publish_signal(effect_key, payload),
            "wait_signal" => self.wait_signal(payload).await,
            "ask_human" => self.ask_human(effect_key, payload).await,
            "complete" => self.complete(payload).await,
            other => Err(WorkflowRuntimeError::Protocol(format!(
                "unknown effect kind: {other}"
            ))),
        }
    }

    async fn checkpoint(&self) -> Result<(), WorkflowRuntimeError> {
        loop {
            if self.cancellation.is_cancelled() {
                return Err(WorkflowRuntimeError::Cancelled);
            }
            let run = self.store.get_workflow(self.workflow_id)?;
            match run.status {
                WorkflowStatus::Created | WorkflowStatus::Running => return Ok(()),
                WorkflowStatus::Paused
                | WorkflowStatus::WaitingForUser
                | WorkflowStatus::WaitingForTimer
                | WorkflowStatus::WaitingForSignal => {
                    let mut events = self.store.subscribe();
                    tokio::select! {
                        _ = self.cancellation.cancelled() => return Err(WorkflowRuntimeError::Cancelled),
                        _ = events.recv() => {}
                    }
                }
                WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Cancelled => {
                    return Err(WorkflowRuntimeError::WorkflowTerminal(run.status));
                }
            }
        }
    }

    async fn create_agent(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let payload: CreateAgentEffect = serde_json::from_value(payload)?;
        let workflow = self.store.get_workflow(self.workflow_id)?;
        let initial_access = std::cmp::min(payload.access, workflow.access);
        let participant_id = AgentInstanceId::from_uuid(effect_resource_uuid(
            self.workflow_id,
            effect_key,
            "participant",
        ));
        let session_id = SessionId::from_uuid(effect_resource_uuid(
            self.workflow_id,
            effect_key,
            "session",
        ));
        let participant = match self.store.get_participant(participant_id) {
            Ok(participant) => participant,
            Err(papermachine_store::StoreError::NotFound { .. }) => {
                self.store.create_participant_with_ids(
                    self.workflow_id,
                    participant_id,
                    session_id,
                    payload.class_name,
                    payload.name,
                    payload.role,
                    payload.system_prompt,
                    payload.model,
                    payload.skills,
                    initial_access,
                )?
            }
            Err(error) => return Err(error.into()),
        };
        if participant.workflow_id != self.workflow_id {
            return Err(WorkflowRuntimeError::Protocol(
                "replayed Agent belongs to another Workflow".to_string(),
            ));
        }
        let current_access = self.store.get_session(participant.session_id)?.access;
        if current_access < payload.access
            && let Err(error) = self
                .request_access_grant(effect_key, &participant, current_access, payload.access)
                .await
        {
            let _ = self.store.retire_participant(participant.id);
            return Err(error);
        }
        let access = self.store.get_session(participant.session_id)?.access;
        Ok(json!({
            "agent_instance_id": participant.id,
            "session_id": participant.session_id,
            "access": access,
        }))
    }

    async fn set_agent_access(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let payload: SetAgentAccessEffect = serde_json::from_value(payload)?;
        let agent_id = AgentInstanceId::from_str(&payload.agent_instance_id)
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let participant = self.store.get_participant(agent_id)?;
        if participant.workflow_id != self.workflow_id {
            return Err(WorkflowRuntimeError::Protocol(
                "cannot change access for an Agent in another Workflow".to_string(),
            ));
        }
        let current = self.store.get_session(participant.session_id)?.access;
        if payload.access > current {
            self.request_access_grant(effect_key, &participant, current, payload.access)
                .await?;
        } else if payload.access < current {
            self.store
                .set_session_access(participant.session_id, payload.access)?;
        }
        Ok(json!({"access": payload.access}))
    }

    async fn request_access_grant(
        &self,
        effect_key: &str,
        participant: &WorkflowParticipant,
        current: AgentAccessProfile,
        requested: AgentAccessProfile,
    ) -> Result<(), WorkflowRuntimeError> {
        let request_id = HumanRequestId::from_uuid(effect_resource_uuid(
            self.workflow_id,
            effect_key,
            "access-grant",
        ));
        let request = match self.store.get_human_request(request_id) {
            Ok(request) => request,
            Err(papermachine_store::StoreError::NotFound { .. }) => {
                self.store.create_human_request_with_id(
                    request_id,
                    self.workflow_id,
                    None,
                    None,
                    participant.session_id,
                    None,
                    format!(
                        "Workflow Agent {} requests an access change from {current} to {requested}. Grant this access?",
                        participant.name
                    ),
                    json!({
                        "type": "boolean",
                        "title": "Grant Agent access",
                        "requested_access": requested,
                    }),
                )?
            }
            Err(error) => return Err(error.into()),
        };
        let answer = wait_for_human(&self.store, request.id, &self.cancellation).await?;
        if answer.as_bool() != Some(true) {
            return Err(WorkflowRuntimeError::Protocol(format!(
                "human denied {requested} access for Agent {}",
                participant.name
            )));
        }
        self.store
            .set_session_access(participant.session_id, requested)?;
        Ok(())
    }

    fn retire_agent(&self, payload: Value) -> Result<Value, WorkflowRuntimeError> {
        let agent_id = id_field::<AgentInstanceId>(&payload, "agent_instance_id")?;
        self.store.retire_participant(agent_id)?;
        Ok(Value::Null)
    }

    async fn invoke_action(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let payload: InvokeActionEffect = serde_json::from_value(payload)?;
        let agent_id = AgentInstanceId::from_str(&payload.agent_instance_id)
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let scope_id = payload
            .task_scope_id
            .as_deref()
            .map(TaskScopeId::from_str)
            .transpose()
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let participant = self.store.get_participant(agent_id)?;
        let objective = format_action_objective(&payload.prompt, &payload.arguments);
        let invocation_id = ActionInvocationId::from_uuid(effect_resource_uuid(
            self.workflow_id,
            effect_key,
            "action-invocation",
        ));
        let invocation = match self.store.get_action_invocation(invocation_id) {
            Ok(invocation) => invocation,
            Err(papermachine_store::StoreError::NotFound { .. }) => {
                self.store.create_action_invocation_with_id(
                    invocation_id,
                    self.workflow_id,
                    scope_id,
                    agent_id,
                    payload.action_name.clone(),
                    objective.clone(),
                    payload.arguments.clone(),
                )?
            }
            Err(error) => return Err(error.into()),
        };
        if invocation.workflow_id != self.workflow_id || invocation.agent_instance_id != agent_id {
            return Err(WorkflowRuntimeError::Protocol(
                "replayed ActionInvocation belongs to a different Workflow or Agent".to_string(),
            ));
        }
        match invocation.status {
            ActionStatus::Completed => return completed_action_result(&invocation),
            ActionStatus::Failed | ActionStatus::Cancelled => {
                return Err(WorkflowRuntimeError::Action(
                    invocation
                        .error
                        .unwrap_or_else(|| "previous Action attempt failed".to_string()),
                ));
            }
            ActionStatus::Scheduled
            | ActionStatus::Running
            | ActionStatus::WaitingForHuman
            | ActionStatus::Interrupted => {}
        }
        let _permit = tokio::select! {
            permit = Arc::clone(&self.action_permits).acquire_owned() => {
                permit.map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?
            },
            _ = self.cancellation.cancelled() => return Err(WorkflowRuntimeError::Cancelled),
        };
        let gate = {
            let mut gates = self.agent_gates.lock().await;
            Arc::clone(
                gates
                    .entry(agent_id)
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _agent_guard = gate.lock().await;
        let run = self.store.get_workflow(self.workflow_id)?;
        let relationship_context =
            relationship_instructions(&self.store, self.workflow_id, agent_id)?;
        let mut interruption_guidance = None;
        let mut recovered_attempt = self
            .store
            .list_action_attempts(invocation.id)?
            .into_iter()
            .rev()
            .find(|attempt| !attempt.status.is_terminal());
        loop {
            self.checkpoint().await?;
            let attempt = match recovered_attempt.take() {
                Some(attempt) => attempt,
                None => self.store.start_action_attempt(invocation.id)?,
            };
            let guidance = interruption_guidance.take();
            let max_steps = payload
                .max_steps
                .unwrap_or(run.budget.max_action_steps)
                .min(run.budget.max_action_steps);
            if max_steps == 0 {
                return Err(WorkflowRuntimeError::Protocol(
                    "action max_steps must be positive".to_string(),
                ));
            }
            let context = WorkflowTurnContext {
                workflow_id: self.workflow_id,
                action_invocation_id: invocation.id,
                action_attempt_id: attempt.id,
            };
            let result = if let Some(turn_id) = attempt.turn_id {
                let turn = self.store.get_turn(turn_id)?;
                match turn.status {
                    TurnStatus::Completed => Ok(turn),
                    TurnStatus::Queued
                    | TurnStatus::Running
                    | TurnStatus::WaitingForHuman
                    | TurnStatus::Paused => {
                        self.sessions
                            .resume_workflow_action(
                                turn.id,
                                context,
                                self.cancellation.child_token(),
                            )
                            .await
                    }
                    TurnStatus::Interrupted => Err(SessionRuntimeError::Interrupted(
                        turn.error
                            .unwrap_or_else(|| "Action Turn was interrupted".to_string()),
                    )),
                    TurnStatus::Cancelled => Err(SessionRuntimeError::Cancelled),
                    TurnStatus::Failed => {
                        let error = turn
                            .error
                            .unwrap_or_else(|| "Action Turn failed before restart".to_string());
                        self.store.finish_action(
                            invocation.id,
                            attempt.id,
                            ActionStatus::Failed,
                            None,
                            Some(error.clone()),
                        )?;
                        return Err(WorkflowRuntimeError::Action(error));
                    }
                }
            } else {
                let mut prompt_layers = vec![PromptLayerInput::new(
                    PromptLayerKind::Workflow,
                    "Workflow objective",
                    format!("workflow:{}:objective", run.id),
                    &run.objective,
                )];
                if !run.system_prompt.trim().is_empty() {
                    prompt_layers.push(PromptLayerInput::new(
                        PromptLayerKind::Workflow,
                        "Workflow system prompt",
                        format!("workflow:{}:system-prompt", run.id),
                        &run.system_prompt,
                    ));
                }
                if !relationship_context.trim().is_empty() {
                    prompt_layers.push(PromptLayerInput::new(
                        PromptLayerKind::Workflow,
                        "Agent collaboration context",
                        format!("workflow:{}:relations", run.id),
                        relationship_context.clone(),
                    ));
                }
                if let Some(value) = guidance.as_ref() {
                    prompt_layers.push(PromptLayerInput::new(
                        PromptLayerKind::Control,
                        "Human interruption guidance",
                        format!("action-attempt:{}:guidance", attempt.id),
                        value,
                    ));
                }
                self.sessions
                    .execute_workflow_action(
                        participant.session_id,
                        objective.clone(),
                        if participant.model.trim().is_empty() {
                            None
                        } else {
                            Some(participant.model.as_str())
                        },
                        prompt_layers,
                        payload.reasoning_effort,
                        max_steps,
                        payload.max_search_calls,
                        payload.web_search_context_size,
                        payload.max_output_tokens,
                        payload.response_format.clone(),
                        context,
                        self.cancellation.child_token(),
                    )
                    .await
            };
            match result {
                Ok(turn) => {
                    self.store.finish_action(
                        invocation.id,
                        attempt.id,
                        ActionStatus::Completed,
                        Some(json!({"message": turn.output.clone().unwrap_or_default(), "turn_id": turn.id})),
                        None,
                    )?;
                    return Ok(
                        json!({"output": turn.output.unwrap_or_default(), "turn_id": turn.id}),
                    );
                }
                Err(SessionRuntimeError::Interrupted(reason)) => {
                    self.store.finish_action(
                        invocation.id,
                        attempt.id,
                        ActionStatus::Interrupted,
                        None,
                        Some(reason.clone()),
                    )?;
                    interruption_guidance = Some(reason);
                }
                Err(SessionRuntimeError::Cancelled) if self.cancellation.is_cancelled() => {
                    self.store.finish_action(
                        invocation.id,
                        attempt.id,
                        ActionStatus::Cancelled,
                        None,
                        Some("Workflow cancelled".to_string()),
                    )?;
                    return Err(WorkflowRuntimeError::Cancelled);
                }
                Err(error) => {
                    self.store.finish_action(
                        invocation.id,
                        attempt.id,
                        ActionStatus::Failed,
                        None,
                        Some(error.to_string()),
                    )?;
                    return Err(WorkflowRuntimeError::Action(error.to_string()));
                }
            }
        }
    }

    fn create_team(&self, effect_key: &str, payload: Value) -> Result<Value, WorkflowRuntimeError> {
        let payload: TeamEffect = serde_json::from_value(payload)?;
        let members = parse_ids(&payload.member_ids)?;
        let team_id = TeamId::from_uuid(effect_resource_uuid(self.workflow_id, effect_key, "team"));
        let team = match self.store.get_team(team_id) {
            Ok(team) => team,
            Err(papermachine_store::StoreError::NotFound { .. }) => self
                .store
                .create_team_with_id(team_id, self.workflow_id, payload.name, members)?,
            Err(error) => return Err(error.into()),
        };
        Ok(json!({"team_id": team.id}))
    }

    fn set_team_members(&self, payload: Value) -> Result<Value, WorkflowRuntimeError> {
        let payload: SetTeamEffect = serde_json::from_value(payload)?;
        let team_id = TeamId::from_str(&payload.team_id)
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        self.store
            .set_team_members(team_id, parse_ids(&payload.member_ids)?)?;
        Ok(Value::Null)
    }

    fn set_relation(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let payload: RelationEffect = serde_json::from_value(payload)?;
        let source = AgentInstanceId::from_str(&payload.source_agent_id)
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let target = AgentInstanceId::from_str(&payload.target_agent_id)
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let relation_id = RelationId::from_uuid(effect_resource_uuid(
            self.workflow_id,
            effect_key,
            "relation",
        ));
        let relation = match self.store.get_relation(relation_id) {
            Ok(relation) => relation,
            Err(papermachine_store::StoreError::NotFound { .. }) => {
                self.store.set_relation_with_id(
                    relation_id,
                    self.workflow_id,
                    source,
                    target,
                    payload.kind,
                    payload.instructions,
                )?
            }
            Err(error) => return Err(error.into()),
        };
        Ok(json!({"relation_id": relation.id}))
    }

    fn open_scope(&self, effect_key: &str, payload: Value) -> Result<Value, WorkflowRuntimeError> {
        let payload: OpenScopeEffect = serde_json::from_value(payload)?;
        let parent = payload
            .parent_id
            .as_deref()
            .map(TaskScopeId::from_str)
            .transpose()
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let scope_id = TaskScopeId::from_uuid(effect_resource_uuid(
            self.workflow_id,
            effect_key,
            "task-scope",
        ));
        let scope = match self.store.get_task_scope(scope_id) {
            Ok(scope) => scope,
            Err(papermachine_store::StoreError::NotFound { .. }) => {
                self.store.create_task_scope_with_id(
                    scope_id,
                    self.workflow_id,
                    parent,
                    payload.name,
                    payload.objective,
                )?
            }
            Err(error) => return Err(error.into()),
        };
        Ok(json!({"task_scope_id": scope.id}))
    }

    fn close_scope(&self, payload: Value) -> Result<Value, WorkflowRuntimeError> {
        let payload: CloseScopeEffect = serde_json::from_value(payload)?;
        let id = TaskScopeId::from_str(&payload.task_scope_id)
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let status = match payload.status.as_str() {
            "completed" => TaskScopeStatus::Completed,
            "cancelled" => TaskScopeStatus::Cancelled,
            other => {
                return Err(WorkflowRuntimeError::Protocol(format!(
                    "invalid scope status: {other}"
                )));
            }
        };
        self.store.set_task_scope_status(id, status)?;
        Ok(Value::Null)
    }

    fn register_timer(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let payload: TimerEffect = serde_json::from_value(payload)?;
        if let Some(timer) = self
            .store
            .list_timers(self.workflow_id)?
            .into_iter()
            .find(|timer| {
                timer.name == payload.name
                    && matches!(timer.status, TimerStatus::Active | TimerStatus::Paused)
            })
        {
            return Ok(json!({"timer_id": timer.id}));
        }
        let policy = match payload.policy.as_str() {
            "coalesce" => TimerPolicy::Coalesce,
            "skip" => TimerPolicy::Skip,
            "queue" => TimerPolicy::Queue,
            other => {
                return Err(WorkflowRuntimeError::Protocol(format!(
                    "invalid timer policy: {other}"
                )));
            }
        };
        let timer_id =
            TimerId::from_uuid(effect_resource_uuid(self.workflow_id, effect_key, "timer"));
        let timer = match self.store.get_timer(timer_id) {
            Ok(timer) => timer,
            Err(papermachine_store::StoreError::NotFound { .. }) => {
                self.store.create_timer_with_id(
                    timer_id,
                    self.workflow_id,
                    payload.name,
                    payload.interval_ms,
                    policy,
                )?
            }
            Err(error) => return Err(error.into()),
        };
        Ok(json!({"timer_id": timer.id}))
    }

    async fn wait_timer(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let timer_id = id_field::<TimerId>(&payload, "timer_id")?;
        loop {
            self.checkpoint().await?;
            let timer = self.store.get_timer(timer_id)?;
            if timer.status != TimerStatus::Active {
                return Err(WorkflowRuntimeError::Protocol(
                    "timer is no longer active".to_string(),
                ));
            }
            let wait = (timer.next_fire_at - Utc::now())
                .to_std()
                .unwrap_or_default();
            if wait.is_zero() {
                let fired = self.store.fire_timer_for_effect(timer_id, effect_key)?;
                return Ok(
                    json!({"fire_count": fired.fire_count, "fired_at": fired.last_fired_at}),
                );
            }
            let mut events = self.store.subscribe();
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(WorkflowRuntimeError::Cancelled),
                _ = tokio::time::sleep(wait) => {},
                _ = events.recv() => {},
            }
        }
    }

    fn create_channel(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let payload: ChannelEffect = serde_json::from_value(payload)?;
        if let Some(channel) = self
            .store
            .list_channels(self.workflow_id)?
            .into_iter()
            .find(|item| item.name == payload.name)
        {
            return Ok(json!({"channel_id": channel.id}));
        }
        let channel_id = ChannelId::from_uuid(effect_resource_uuid(
            self.workflow_id,
            effect_key,
            "channel",
        ));
        let channel = match self.store.get_channel(channel_id) {
            Ok(channel) => channel,
            Err(papermachine_store::StoreError::NotFound { .. }) => {
                self.store.create_channel_with_id(
                    channel_id,
                    self.workflow_id,
                    payload.name,
                    payload.schema,
                )?
            }
            Err(error) => return Err(error.into()),
        };
        Ok(json!({"channel_id": channel.id}))
    }

    fn publish_signal(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let payload: PublishSignalEffect = serde_json::from_value(payload)?;
        let channel_id = ChannelId::from_str(&payload.channel_id)
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let sender = payload
            .sender_agent_id
            .as_deref()
            .map(AgentInstanceId::from_str)
            .transpose()
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        let signal_id =
            SignalId::from_uuid(effect_resource_uuid(self.workflow_id, effect_key, "signal"));
        let signal = match self.store.get_signal(signal_id) {
            Ok(signal) => signal,
            Err(papermachine_store::StoreError::NotFound { .. }) => self
                .store
                .publish_signal_with_id(signal_id, channel_id, sender, payload.value)?,
            Err(error) => return Err(error.into()),
        };
        Ok(json!({"signal_id": signal.id, "sequence": signal.sequence}))
    }

    async fn wait_signal(&self, payload: Value) -> Result<Value, WorkflowRuntimeError> {
        let payload: WaitSignalEffect = serde_json::from_value(payload)?;
        let channel_id = ChannelId::from_str(&payload.channel_id)
            .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?;
        loop {
            self.checkpoint().await?;
            if let Some(signal) = self
                .store
                .list_signals(channel_id, payload.after_sequence)?
                .into_iter()
                .next()
            {
                return Ok(
                    json!({"signal_id": signal.id, "sequence": signal.sequence, "value": signal.value}),
                );
            }
            let mut events = self.store.subscribe();
            tokio::select! {
                _ = self.cancellation.cancelled() => return Err(WorkflowRuntimeError::Cancelled),
                _ = events.recv() => {},
            }
        }
    }

    async fn ask_human(
        &self,
        effect_key: &str,
        payload: Value,
    ) -> Result<Value, WorkflowRuntimeError> {
        let payload: AskHumanEffect = serde_json::from_value(payload)?;
        let run = self.store.get_workflow(self.workflow_id)?;
        let session_id = if let Some(agent_id) = payload.agent_instance_id {
            self.store
                .get_participant(
                    AgentInstanceId::from_str(&agent_id)
                        .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))?,
                )?
                .session_id
        } else if let Some(session_id) = run.started_from_session_id {
            session_id
        } else {
            self.store
                .list_participants(self.workflow_id)?
                .first()
                .map(|participant| participant.session_id)
                .ok_or_else(|| {
                    WorkflowRuntimeError::Protocol(
                        "workflow-level ask_human requires a participant Session".to_string(),
                    )
                })?
        };
        let request_id = HumanRequestId::from_uuid(effect_resource_uuid(
            self.workflow_id,
            effect_key,
            "human-request",
        ));
        let request = match self.store.get_human_request(request_id) {
            Ok(request) => request,
            Err(papermachine_store::StoreError::NotFound { .. }) => {
                self.store.create_human_request_with_id(
                    request_id,
                    self.workflow_id,
                    None,
                    None,
                    session_id,
                    None,
                    payload.question,
                    payload.response_schema,
                )?
            }
            Err(error) => return Err(error.into()),
        };
        let answer = wait_for_human(&self.store, request.id, &self.cancellation).await?;
        Ok(json!({"answer": answer}))
    }

    async fn complete(&self, payload: Value) -> Result<Value, WorkflowRuntimeError> {
        self.remember_completion(&payload).await?;
        Ok(Value::Null)
    }

    async fn remember_completion(&self, payload: &Value) -> Result<(), WorkflowRuntimeError> {
        let output = payload.get("output").cloned().unwrap_or(Value::Null);
        let mut completion = self.completion.lock().await;
        if completion
            .as_ref()
            .is_some_and(|existing| existing != &output)
        {
            return Err(WorkflowRuntimeError::Protocol(
                "workflow submitted conflicting completion outputs".to_string(),
            ));
        }
        *completion = Some(output);
        Ok(())
    }
}

#[derive(Clone)]
pub struct StoreHumanRequestBroker {
    store: Arc<Store>,
}

impl StoreHumanRequestBroker {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl HumanRequestBroker for StoreHumanRequestBroker {
    async fn ask(
        &self,
        context: ToolContext,
        question: String,
        response_schema: Value,
    ) -> Result<Value, ToolError> {
        let workflow_id = context.workflow_id.ok_or_else(|| {
            ToolError::Execution("ask_human is available only inside a Workflow action".to_string())
        })?;
        let request = self
            .store
            .create_human_request(
                workflow_id,
                context.action_invocation_id,
                context.action_attempt_id,
                context.session_id,
                Some(context.turn_id),
                question,
                response_schema,
            )
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        wait_for_human(&self.store, request.id, &context.cancellation)
            .await
            .map_err(|error| match error {
                WorkflowRuntimeError::Cancelled => ToolError::Cancelled,
                other => ToolError::Execution(other.to_string()),
            })
    }
}

async fn wait_for_human(
    store: &Store,
    request_id: HumanRequestId,
    cancellation: &CancellationToken,
) -> Result<Value, WorkflowRuntimeError> {
    loop {
        let request = store.get_human_request(request_id)?;
        match request.status {
            HumanRequestStatus::Answered => return Ok(request.answer.unwrap_or(Value::Null)),
            HumanRequestStatus::Cancelled => return Err(WorkflowRuntimeError::Cancelled),
            HumanRequestStatus::Open => {}
        }
        let mut events = store.subscribe();
        tokio::select! {
            _ = cancellation.cancelled() => return Err(WorkflowRuntimeError::Cancelled),
            _ = events.recv() => {},
        }
    }
}

async fn materialize_runtime(
    workspace: &Path,
    python_runtime_root: &Path,
    source: &str,
) -> Result<(), WorkflowRuntimeError> {
    tokio::fs::create_dir_all(workspace).await?;
    tokio::fs::create_dir_all(workspace.join(".home")).await?;
    tokio::fs::create_dir_all(workspace.join(".tmp")).await?;
    tokio::fs::write(workspace.join("workflow.py"), source).await?;
    copy_directory(
        &python_runtime_root.join("papermachine"),
        &workspace.join("papermachine"),
    )
    .await
}

async fn copy_directory(source: &Path, destination: &Path) -> Result<(), WorkflowRuntimeError> {
    let mut stack = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((source, destination)) = stack.pop() {
        tokio::fs::create_dir_all(&destination).await?;
        let mut entries = tokio::fs::read_dir(&source).await?;
        while let Some(entry) = entries.next_entry().await? {
            let from = entry.path();
            let to = destination.join(entry.file_name());
            if entry.file_type().await?.is_dir() {
                stack.push((from, to));
            } else {
                tokio::fs::copy(from, to).await?;
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn sandboxed_python_command(
    python: &Path,
    workspace: &Path,
) -> Result<Command, WorkflowRuntimeError> {
    let sandbox = Path::new("/usr/bin/sandbox-exec");
    if !sandbox.is_file() {
        return Err(WorkflowRuntimeError::Sandbox(
            "macOS sandbox-exec is unavailable".to_string(),
        ));
    }
    let workspace = workspace.canonicalize()?;
    let workspace_literal = seatbelt_literal(&workspace);
    let mut rules = vec![
        "(deny file-write*)".to_string(),
        "(deny network*)".to_string(),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        rules.push(format!(
            "(deny file-read* (subpath \"{}\"))",
            seatbelt_literal(Path::new(&home))
        ));
    }
    for root in [
        "/Volumes",
        "/private/tmp",
        "/tmp",
        "/var/tmp",
        "/private/var/folders",
    ] {
        rules.push(format!("(deny file-read* (subpath \"{root}\"))"));
    }
    rules.push(format!(
        "(allow file-read* (subpath \"{workspace_literal}\"))"
    ));
    rules.push(format!(
        "(allow file-write* (subpath \"{workspace_literal}\"))"
    ));
    rules.push("(allow file-write* (literal \"/dev/null\"))".to_string());
    let profile = format!("(version 1)\n(allow default)\n{}", rules.join("\n"));
    let mut command = Command::new(sandbox);
    command
        .arg("-p")
        .arg(profile)
        .arg(python)
        .args(["-B", "-m", "papermachine._runner", "workflow.py"])
        .arg("main")
        .current_dir(&workspace)
        .env_clear()
        .env(
            "PATH",
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        .env("HOME", workspace.join(".home"))
        .env("TMPDIR", workspace.join(".tmp"))
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("PYTHONDONTWRITEBYTECODE", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    Ok(command)
}

#[cfg(not(target_os = "macos"))]
fn sandboxed_python_command(
    _python: &Path,
    _workspace: &Path,
) -> Result<Command, WorkflowRuntimeError> {
    Err(WorkflowRuntimeError::Sandbox(
        "no fail-closed interactive Python sandbox is implemented for this platform".to_string(),
    ))
}

#[cfg(target_os = "macos")]
fn seatbelt_literal(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    if let Some(process_id) = child.id() {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &format!("-{process_id}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn drain_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> Result<LimitedText, std::io::Error> {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let take = read.min(limit.saturating_sub(kept.len()));
        kept.extend_from_slice(&buffer[..take]);
        truncated |= take < read;
    }
    Ok(LimitedText {
        text: String::from_utf8_lossy(&kept).into_owned(),
        truncated,
    })
}

fn relationship_instructions(
    store: &Store,
    workflow_id: WorkflowId,
    agent_id: AgentInstanceId,
) -> Result<String, WorkflowRuntimeError> {
    let participants = store
        .list_participants(workflow_id)?
        .into_iter()
        .map(|participant| (participant.id, participant.name))
        .collect::<HashMap<_, _>>();
    let mut lines = Vec::new();
    for relation in store.list_relations(workflow_id)? {
        if relation.source_agent_id == agent_id || relation.target_agent_id == agent_id {
            let source = participants
                .get(&relation.source_agent_id)
                .cloned()
                .unwrap_or_else(|| relation.source_agent_id.to_string());
            let target = participants
                .get(&relation.target_agent_id)
                .cloned()
                .unwrap_or_else(|| relation.target_agent_id.to_string());
            lines.push(format!(
                "- {source} --{}--> {target}: {}",
                relation.kind, relation.instructions
            ));
        }
    }
    if lines.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!(
            "Collaboration relations relevant to this Agent:\n{}",
            lines.join("\n")
        ))
    }
}

fn format_action_objective(prompt: &str, arguments: &Value) -> String {
    if arguments.as_object().is_some_and(serde_json::Map::is_empty) {
        prompt.to_string()
    } else {
        format!(
            "{prompt}\n\nAction arguments:\n{}",
            serde_json::to_string_pretty(arguments).unwrap_or_else(|_| arguments.to_string())
        )
    }
}

fn completed_action_result(invocation: &ActionInvocation) -> Result<Value, WorkflowRuntimeError> {
    let output = invocation.output.as_ref().ok_or_else(|| {
        WorkflowRuntimeError::Protocol(format!(
            "completed ActionInvocation {} has no output",
            invocation.id
        ))
    })?;
    Ok(json!({
        "output": output.get("message").and_then(Value::as_str).unwrap_or_default(),
        "turn_id": output.get("turn_id").cloned().unwrap_or(Value::Null),
    }))
}

fn effect_resource_uuid(workflow_id: WorkflowId, effect_key: &str, resource: &str) -> uuid::Uuid {
    uuid::Uuid::new_v5(
        workflow_id.as_uuid(),
        format!("{effect_key}:{resource}").as_bytes(),
    )
}

fn id_field<T: FromStr>(value: &Value, field: &str) -> Result<T, WorkflowRuntimeError>
where
    T::Err: std::fmt::Display,
{
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| WorkflowRuntimeError::Protocol(format!("missing string field {field}")))?
        .parse()
        .map_err(|error: T::Err| WorkflowRuntimeError::Protocol(error.to_string()))
}

fn validate_effect_key(key: &str) -> Result<(), WorkflowRuntimeError> {
    if key.is_empty()
        || key.len() > 512
        || !key.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
    {
        return Err(WorkflowRuntimeError::Protocol(
            "effect id must be a non-empty logical path of at most 512 ASCII characters"
                .to_string(),
        ));
    }
    Ok(())
}

fn parse_ids(values: &[String]) -> Result<Vec<AgentInstanceId>, WorkflowRuntimeError> {
    values
        .iter()
        .map(|value| {
            AgentInstanceId::from_str(value)
                .map_err(|error| WorkflowRuntimeError::Protocol(error.to_string()))
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct EffectRequest {
    id: String,
    kind: String,
    payload: Value,
}

#[derive(Debug, Serialize)]
struct EffectResponse {
    id: String,
    ok: bool,
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateAgentEffect {
    class_name: String,
    name: String,
    role: String,
    system_prompt: String,
    model: String,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    access: AgentAccessProfile,
}

#[derive(Debug, Deserialize)]
struct SetAgentAccessEffect {
    agent_instance_id: String,
    access: AgentAccessProfile,
}

#[derive(Debug, Deserialize)]
struct InvokeActionEffect {
    agent_instance_id: String,
    action_name: String,
    prompt: String,
    arguments: Value,
    response_format: Option<ModelResponseFormat>,
    #[serde(default)]
    max_steps: Option<u32>,
    #[serde(default)]
    max_search_calls: Option<u32>,
    #[serde(default)]
    web_search_context_size: Option<WebSearchContextSize>,
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    task_scope_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TeamEffect {
    name: String,
    member_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SetTeamEffect {
    team_id: String,
    member_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RelationEffect {
    source_agent_id: String,
    target_agent_id: String,
    kind: String,
    #[serde(default)]
    instructions: String,
}

#[derive(Debug, Deserialize)]
struct OpenScopeEffect {
    name: String,
    #[serde(default)]
    objective: String,
    parent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CloseScopeEffect {
    task_scope_id: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct TimerEffect {
    name: String,
    interval_ms: u64,
    policy: String,
}

#[derive(Debug, Deserialize)]
struct ChannelEffect {
    name: String,
    #[serde(default)]
    schema: Value,
}

#[derive(Debug, Deserialize)]
struct PublishSignalEffect {
    channel_id: String,
    sender_agent_id: Option<String>,
    value: Value,
}

#[derive(Debug, Deserialize)]
struct WaitSignalEffect {
    channel_id: String,
    #[serde(default)]
    after_sequence: u64,
}

#[derive(Debug, Deserialize)]
struct AskHumanEffect {
    question: String,
    #[serde(default)]
    response_schema: Value,
    agent_instance_id: Option<String>,
}

struct LimitedText {
    text: String,
    truncated: bool,
}

#[derive(Debug, Error)]
pub enum WorkflowRuntimeError {
    #[error(transparent)]
    Store(#[from] papermachine_store::StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("workflow runtime failed to spawn: {0}")]
    Spawn(String),
    #[error("workflow runtime is missing its {0} pipe")]
    MissingPipe(&'static str),
    #[error("workflow effect protocol failed: {0}")]
    Protocol(String),
    #[error("replayed Workflow effect {key} failed previously: {error}")]
    ReplayedEffect { key: String, error: String },
    #[error("workflow Python process exited with {code:?}: {stderr}{suffix}", suffix = if *.truncated { " [truncated]" } else { "" })]
    Python {
        code: Option<i32>,
        stderr: String,
        truncated: bool,
    },
    #[error("workflow sandbox unavailable: {0}")]
    Sandbox(String),
    #[error("Workflow was cancelled")]
    Cancelled,
    #[error("Workflow is terminal: {0:?}")]
    WorkflowTerminal(WorkflowStatus),
    #[error("Agent action failed: {0}")]
    Action(String),
}
