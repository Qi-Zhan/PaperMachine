use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_model::ModelStream;
use papermachine_model::ScriptedModelClient;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::ActionSource;
use papermachine_protocol::Agent;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::ControlMessageStatus;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ProjectId;
use papermachine_protocol::PromptLayerKind;
use papermachine_protocol::Session;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::StepStatus;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::Turn;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_session::ActionTurnContext;
use papermachine_session::TurnRuntime;
use papermachine_session::TurnRuntimeConfig;
use papermachine_session::TurnRuntimeError;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::NewActionInvocation;
use papermachine_store::NewSession;
use papermachine_store::Store;
use papermachine_store::StoreHandle;
use papermachine_tools::ToolCatalog;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn workflow_snapshot() -> WorkflowProgramSnapshot {
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: WorkflowProgramManifest {
            id: WorkflowProgramId::new(),
            slug: "turn-runtime-test".to_string(),
            name: "Turn runtime test".to_string(),
            description: "Exercise one persistent Agent rollout.".to_string(),
            entrypoint: "main".to_string(),
            request_mode: Default::default(),
            params_schema: json!({"type": "object"}),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: "builtin/turn-runtime-test/workflow.py".to_string(),
        sha256: "test-source".to_string(),
        runtime_sha256: "test-runtime".to_string(),
        source_code: "async def main(ctx): return {}\n".to_string(),
    }
}

fn response(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::OutputTextDelta {
            delta: text.to_string(),
        },
        ModelEvent::Completed {
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 3,
                ..TokenUsage::default()
            },
        },
    ]
}

fn session_agent(store: &Store, project_id: ProjectId, system_prompt: &str) -> (Session, Agent) {
    let session = store
        .create_session(NewSession {
            project_id,
            program: workflow_snapshot(),
            title: "Persistent Agent".to_string(),
            request: "Exercise one persistent Agent".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "test-model".to_string(),
            access: AccessPreset::Research,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Session should be created");
    let session = store
        .start_session(session.id)
        .expect("Session should start");
    let agent = store
        .create_agent(
            session.id,
            "Agent",
            "Agent",
            "test",
            system_prompt,
            "test-model",
            Vec::new(),
            AccessPreset::Research,
        )
        .expect("Agent should be created");
    (session, agent)
}

fn runtime(store: &Store, model: Arc<dyn ModelClient>) -> (TurnRuntime, StoreHandle) {
    let handle = StoreHandle::spawn(store.clone()).expect("Store thread should start");
    let skills = Arc::new(ProjectSkillCatalog::new(handle.clone()));
    (
        TurnRuntime::new(
            handle.clone(),
            model,
            ToolCatalog::default(),
            skills,
            TurnRuntimeConfig {
                default_model: "test-model".to_string(),
                model_context_window: 128_000,
                max_concurrent_turns: 1,
            },
        ),
        handle,
    )
}

async fn execute_action(
    runtime: &TurnRuntime,
    store: &Store,
    session: &Session,
    agent: &Agent,
    input: &str,
) -> Turn {
    let invocation = store
        .create_action_invocation(NewActionInvocation {
            session_id: session.id,
            agent_id: agent.id,
            action_name: "respond".to_string(),
            contract: "Respond".to_string(),
            arguments: json!({"message": input}),
            input: input.to_string(),
            source: ActionSource::Workflow,
            requested_tools: Vec::new(),
            tools_enabled: true,
            web_search_context_size: None,
            reasoning_effort: None,
            response_format: None,
        })
        .expect("Action should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("ActionAttempt should start");
    runtime
        .execute_action_attempt(
            agent.id,
            input,
            None,
            Vec::new(),
            None,
            Vec::new(),
            true,
            None,
            None,
            ActionTurnContext {
                action_invocation_id: invocation.id,
                action_attempt_id: attempt.id,
            },
            CancellationToken::new(),
        )
        .await
        .expect("Action Turn should complete")
}

#[derive(Clone, Copy)]
struct BlockingModelClient;

#[async_trait]
impl ModelClient for BlockingModelClient {
    async fn stream(
        &self,
        _request: papermachine_protocol::ModelRequest,
    ) -> Result<ModelStream, ModelError> {
        Ok(stream::pending().boxed())
    }
}

#[tokio::test]
async fn cancelling_an_action_turn_reaches_its_execution() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("Store should open"),
    );
    let project = store
        .create_project("Cancellation", directory.path().join("workspace"))
        .expect("Project should be created");
    let (session, agent) = session_agent(&store, project.id, "");
    let invocation = store
        .create_action_invocation(NewActionInvocation {
            session_id: session.id,
            agent_id: agent.id,
            action_name: "investigate".to_string(),
            contract: "Wait".to_string(),
            arguments: json!({}),
            input: "Wait".to_string(),
            source: ActionSource::Workflow,
            requested_tools: Vec::new(),
            tools_enabled: true,
            web_search_context_size: None,
            reasoning_effort: None,
            response_format: None,
        })
        .expect("Action should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("Attempt should start");
    let (runtime, _) = runtime(&store, Arc::new(BlockingModelClient));
    let mut events = store.subscribe();
    let execution = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .execute_action_attempt(
                    agent.id,
                    "Wait",
                    None,
                    Vec::new(),
                    None,
                    Vec::new(),
                    true,
                    None,
                    None,
                    ActionTurnContext {
                        action_invocation_id: invocation.id,
                        action_attempt_id: attempt.id,
                    },
                    CancellationToken::new(),
                )
                .await
        })
    };

    let model_started = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.expect("event stream should stay open");
            if event.session_id == session.id
                && matches!(event.payload, SessionEventPayload::ModelStepStarted)
            {
                return event;
            }
        }
    })
    .await
    .expect("Turn should reach model sampling");
    let turn_id = model_started
        .turn_id
        .expect("model event should identify its Turn");
    assert_eq!(
        store
            .get_turn(turn_id)
            .expect("running Turn should load")
            .status,
        TurnStatus::Running
    );
    runtime.cancel(turn_id).await.expect("Turn should cancel");
    let result = execution.await.expect("execution task should join");
    assert!(matches!(result, Err(TurnRuntimeError::Cancelled)));
    assert_eq!(
        store
            .get_turn(turn_id)
            .expect("cancelled Turn should load")
            .status,
        TurnStatus::Cancelled
    );
    let steps = store.list_steps(turn_id).expect("Turn Steps should list");
    assert_eq!(
        steps
            .first()
            .expect("cancelled Turn should retain its model Step")
            .status,
        StepStatus::Cancelled
    );
}

#[tokio::test]
async fn later_turns_reuse_the_same_agent_rollout() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("Store should open");
    let project = store
        .create_project("Conversation", directory.path().join("workspace"))
        .expect("Project should be created");
    let (session, agent) = session_agent(&store, project.id, "");
    let model = ScriptedModelClient::new([
        response("First answer."),
        response("Second answer using context."),
    ]);
    let (runtime, _) = runtime(&store, Arc::new(model.clone()));

    execute_action(&runtime, &store, &session, &agent, "First question").await;
    execute_action(&runtime, &store, &session, &agent, "Follow-up question").await;

    let requests = model.requests().expect("requests should be captured");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].transport_session_key.as_deref(),
        Some(agent.id.to_string().as_str())
    );
    assert_eq!(
        requests[0].transport_session_key,
        requests[1].transport_session_key
    );
    assert!(requests[1].input.iter().any(|item| {
        matches!(item, ModelInputItem::Message { role: MessageRole::User, content } if content == "First question")
    }));
    assert!(requests[1].input.iter().any(|item| {
        matches!(item, ModelInputItem::Message { role: MessageRole::Assistant, content } if content == "First answer.")
    }));
    assert!(requests[1].input.iter().any(|item| {
        matches!(item, ModelInputItem::Message { role: MessageRole::User, content } if content == "Follow-up question")
    }));
}

#[tokio::test]
async fn claimed_guidance_is_checkpointed_before_sampling() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("Store should open");
    let project = store
        .create_project("Guidance", directory.path().join("workspace"))
        .expect("Project should be created");
    let (session, agent) = session_agent(&store, project.id, "");
    let invocation = store
        .create_action_invocation(NewActionInvocation {
            session_id: session.id,
            agent_id: agent.id,
            action_name: "respond".to_string(),
            contract: "Use guidance".to_string(),
            arguments: json!({}),
            input: "Use guidance".to_string(),
            source: ActionSource::Workflow,
            requested_tools: Vec::new(),
            tools_enabled: true,
            web_search_context_size: None,
            reasoning_effort: None,
            response_format: None,
        })
        .expect("Action should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("Attempt should start");
    let control = store
        .create_control_message(
            session.id,
            agent.id,
            Some(invocation.id),
            ControlMessageKind::Guide,
            "Verify the final claim",
        )
        .expect("guidance should queue");
    let model = ScriptedModelClient::new([response("Verified answer.")]);
    let (runtime, _) = runtime(&store, Arc::new(model.clone()));
    let turn = runtime
        .execute_action_attempt(
            agent.id,
            "Answer",
            None,
            Vec::new(),
            None,
            Vec::new(),
            true,
            None,
            None,
            ActionTurnContext {
                action_invocation_id: invocation.id,
                action_attempt_id: attempt.id,
            },
            CancellationToken::new(),
        )
        .await
        .expect("Action should complete");

    let requests = model.requests().expect("model requests should be recorded");
    assert!(
        requests
            .first()
            .expect("Action should make one model request")
            .input
            .iter()
            .any(|item| matches!(
                item,
                ModelInputItem::Message { role: MessageRole::User, content }
                    if content.contains("Verify the final claim")
            ))
    );
    let applied = store
        .list_control_messages(session.id)
        .expect("control messages should list")
        .into_iter()
        .find(|message| message.id == control.id)
        .expect("guidance should remain queryable");
    assert_eq!(applied.status, ControlMessageStatus::Applied);
    assert_eq!(applied.claimed_turn_id, Some(turn.id));
}

#[tokio::test]
async fn turn_prompt_snapshots_preserve_layer_provenance() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("Store should open");
    let project = store
        .create_project("Prompts", directory.path().join("workspace"))
        .expect("Project should be created");
    store
        .set_project_system_prompt(project.id, "Project prompt one.")
        .expect("Project prompt should update");
    let (session, agent) = session_agent(&store, project.id, "Agent prompt.");
    let model = ScriptedModelClient::new([response("First."), response("Second.")]);
    let (runtime, _) = runtime(&store, Arc::new(model.clone()));

    let first = execute_action(&runtime, &store, &session, &agent, "First question").await;
    assert_eq!(
        first
            .prompt
            .layers
            .iter()
            .map(|layer| layer.kind)
            .collect::<Vec<_>>(),
        vec![
            PromptLayerKind::Runtime,
            PromptLayerKind::Project,
            PromptLayerKind::Agent,
        ]
    );
    store
        .set_project_system_prompt(project.id, "Project prompt two.")
        .expect("Project prompt should update");
    let second = execute_action(&runtime, &store, &session, &agent, "Second question").await;

    assert_ne!(first.prompt.sha256, second.prompt.sha256);
    assert!(
        first
            .prompt
            .layers
            .iter()
            .any(|layer| layer.content == "Project prompt one.")
    );
    assert!(
        second
            .prompt
            .layers
            .iter()
            .any(|layer| layer.content == "Project prompt two.")
    );
    assert!(
        second
            .prompt
            .layers
            .iter()
            .any(|layer| layer.content == "Agent prompt.")
    );
    let requests = model.requests().expect("model requests should be captured");
    assert_eq!(requests[0].instructions, first.prompt.rendered);
    assert_eq!(requests[1].instructions, second.prompt.rendered);
}

#[tokio::test]
async fn model_usage_updates_the_parent_session() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("Store should open");
    let project = store
        .create_project("Usage", directory.path().join("workspace"))
        .expect("Project should be created");
    let (session, agent) = session_agent(&store, project.id, "");
    let model = ScriptedModelClient::new([response("Usage recorded.")]);
    let (runtime, _) = runtime(&store, Arc::new(model));

    let turn = execute_action(&runtime, &store, &session, &agent, "Research").await;
    assert_eq!(turn.status, TurnStatus::Completed);
    let updated = store.get_session(session.id).expect("Session should load");
    assert_eq!(updated.usage.tokens.input_tokens, 10);
    assert_eq!(updated.usage.tokens.output_tokens, 3);
}
