use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_model::ModelStream;
use papermachine_model::ScriptedModelClient;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::ControlMessageStatus;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::PromptLayerKind;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::StepStatus;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::TurnOrigin;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_protocol::{ProjectId, Turn, Workflow, WorkflowParticipant};
use papermachine_session::SessionRuntime;
use papermachine_session::SessionRuntimeConfig;
use papermachine_session::SessionRuntimeError;
use papermachine_session::WorkflowTurnContext;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::NewWorkflow;
use papermachine_store::Store;
use papermachine_store::StoreHandle;
use papermachine_tools::ToolCatalog;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

fn workflow_snapshot() -> WorkflowProgramSnapshot {
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: WorkflowProgramManifest {
            id: WorkflowProgramId::new(),
            slug: "usage-test".to_string(),
            name: "Usage test".to_string(),
            description: "Exercise per-step token accounting.".to_string(),
            entrypoint: "main".to_string(),
            request_mode: Default::default(),
            params_schema: serde_json::json!({"type": "object"}),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: "builtin/usage-test/workflow.py".to_string(),
        sha256: "usage-test".to_string(),
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
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
        },
    ]
}

fn workflow_agent(
    store: &Store,
    project_id: ProjectId,
    system_prompt: &str,
) -> (Workflow, WorkflowParticipant) {
    let run = store
        .create_workflow(NewWorkflow {
            project_id,
            started_from_session_id: None,
            program: workflow_snapshot(),
            request: "Exercise one persistent Agent Session".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: serde_json::json!({}),
            default_model: "test-model".to_string(),
            access: papermachine_protocol::AccessPreset::Research,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    store.start_workflow(run.id).expect("Workflow should start");
    let participant = store
        .create_participant(
            run.id,
            "Agent",
            "Agent",
            "test",
            system_prompt,
            "test-model",
            Vec::new(),
            papermachine_protocol::AccessPreset::Research,
        )
        .expect("Agent should be created");
    (run, participant)
}

async fn execute_action(
    runtime: &SessionRuntime,
    store: &Store,
    run: &Workflow,
    participant: &WorkflowParticipant,
    input: &str,
) -> Turn {
    let invocation = store
        .create_action_invocation(
            run.id,
            participant.id,
            "respond",
            "Respond",
            serde_json::json!({"message": input}),
            Vec::new(),
        )
        .expect("Action should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("Action attempt should start");
    runtime
        .execute_workflow_action(
            participant.session_id,
            TurnOrigin::Workflow,
            input,
            None,
            Vec::new(),
            None,
            Vec::new(),
            true,
            None,
            None,
            WorkflowTurnContext {
                workflow_id: run.id,
                action_invocation_id: invocation.id,
                action_attempt_id: attempt.id,
            },
            tokio_util::sync::CancellationToken::new(),
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
async fn cancelling_a_workflow_action_turn_reaches_its_parent_execution() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("store should open"),
    );
    let project = store
        .create_project("Action cancellation", directory.path().join("workspace"))
        .expect("Project should be created");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let run = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: Some(origin.id),
            program: workflow_snapshot(),
            request: "Wait for cancellation".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: serde_json::json!({}),
            default_model: "test-model".to_string(),
            access: papermachine_protocol::AccessPreset::Research,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    store.start_workflow(run.id).expect("Workflow should start");
    let participant = store
        .create_participant(
            run.id,
            "Researcher",
            "Researcher",
            "evidence",
            "",
            "",
            Vec::new(),
            papermachine_protocol::AccessPreset::Research,
        )
        .expect("participant should be created");
    let invocation = store
        .create_action_invocation(
            run.id,
            participant.id,
            "investigate",
            "Wait",
            serde_json::json!({}),
            Vec::new(),
        )
        .expect("invocation should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("attempt should start");
    let store_handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let skills = Arc::new(ProjectSkillCatalog::new(store_handle.clone()));
    let runtime = SessionRuntime::new(
        store_handle,
        Arc::new(BlockingModelClient),
        ToolCatalog::default(),
        skills,
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    );
    let participant_session_id = participant.session_id;
    let workflow_id = run.id;
    let invocation_id = invocation.id;
    let attempt_id = attempt.id;
    let mut session_events = store.subscribe_sessions();
    let execution = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .execute_workflow_action(
                    participant_session_id,
                    TurnOrigin::Workflow,
                    "Wait",
                    None,
                    Vec::new(),
                    None,
                    Vec::new(),
                    true,
                    None,
                    None,
                    WorkflowTurnContext {
                        workflow_id,
                        action_invocation_id: invocation_id,
                        action_attempt_id: attempt_id,
                    },
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
        })
    };

    let model_started = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = session_events
                .recv()
                .await
                .expect("Session event stream should stay open");
            if event.session_id == participant_session_id
                && matches!(event.payload, SessionEventPayload::ModelStepStarted)
            {
                return event;
            }
        }
    })
    .await
    .expect("Workflow Action should reach its model Step before cancellation");
    let turn = store
        .get_turn(
            model_started
                .turn_id
                .expect("model event should identify its Turn"),
        )
        .expect("Workflow Action Turn should load");
    assert_eq!(turn.status, TurnStatus::Running);
    assert!(
        store
            .list_steps(turn.id)
            .expect("Steps should load before cancellation")
            .is_empty(),
        "an in-flight model sample must remain transient"
    );
    runtime.cancel(turn.id).await.expect("Turn should cancel");
    let result = execution.await.expect("execution task should join");
    assert!(matches!(result, Err(SessionRuntimeError::Cancelled)));
    assert_eq!(
        store.get_turn(turn.id).expect("Turn should load").status,
        TurnStatus::Cancelled
    );
    assert_eq!(
        store.list_steps(turn.id).expect("Steps should load")[0].status,
        StepStatus::Cancelled
    );
}

#[tokio::test]
async fn later_turns_reuse_the_completed_session_history() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open"),
    );
    let research = store
        .create_project("Persistent conversation", directory.path().join("project"))
        .expect("research should be created");
    let store_handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let skills = Arc::new(ProjectSkillCatalog::new(store_handle.clone()));
    skills
        .ensure_project(research.id)
        .await
        .expect("research directories should exist");
    let (run, participant) = workflow_agent(&store, research.id, "");
    let model = ScriptedModelClient::new([
        response("First answer."),
        response("Second answer using context."),
    ]);
    let runtime = SessionRuntime::new(
        store_handle,
        Arc::new(model.clone()),
        ToolCatalog::default(),
        skills,
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    );

    execute_action(&runtime, &store, &run, &participant, "First question").await;
    execute_action(&runtime, &store, &run, &participant, "Follow-up question").await;

    let requests = model.requests().expect("requests should be captured");
    assert_eq!(requests.len(), 2);
    let expected_transport_key = participant.session_id.to_string();
    let first_cache = requests[0]
        .prompt_cache
        .as_ref()
        .expect("first request should have a prompt-cache configuration");
    let second_cache = requests[1]
        .prompt_cache
        .as_ref()
        .expect("second request should have a prompt-cache configuration");
    assert!(first_cache.key.ends_with("-session"));
    assert_ne!(first_cache.key, expected_transport_key);
    assert_eq!(first_cache, second_cache);
    assert_eq!(
        requests[0].transport_session_key.as_deref(),
        Some(expected_transport_key.as_str())
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
async fn claimed_guidance_is_checkpointed_before_sampling_and_not_lost() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("store should open"),
    );
    let project = store
        .create_project("Guidance", directory.path().join("workspace"))
        .expect("Project should be created");
    let (run, participant) = workflow_agent(&store, project.id, "");
    let invocation = store
        .create_action_invocation(
            run.id,
            participant.id,
            "respond",
            "Use guidance",
            serde_json::json!({}),
            Vec::new(),
        )
        .expect("Action should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("Attempt should start");
    let control = store
        .create_control_message(
            run.id,
            participant.session_id,
            Some(invocation.id),
            ControlMessageKind::Guide,
            "Verify the final claim",
        )
        .expect("guidance should queue");
    let model = ScriptedModelClient::new([response("Verified answer.")]);
    let store_handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let runtime = SessionRuntime::new(
        store_handle.clone(),
        Arc::new(model.clone()),
        ToolCatalog::default(),
        Arc::new(ProjectSkillCatalog::new(store_handle)),
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    );

    let turn = runtime
        .execute_workflow_action(
            participant.session_id,
            TurnOrigin::Workflow,
            "Answer",
            None,
            Vec::new(),
            None,
            Vec::new(),
            true,
            None,
            None,
            WorkflowTurnContext {
                workflow_id: run.id,
                action_invocation_id: invocation.id,
                action_attempt_id: attempt.id,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("Action should complete");

    assert_eq!(turn.output.as_deref(), Some("Verified answer."));
    let requests = model.requests().expect("model requests should load");
    assert!(requests[0].input.iter().any(|item| matches!(
        item,
        ModelInputItem::Message { role: MessageRole::User, content }
            if content.contains("Verify the final claim")
    )));
    let applied = store
        .list_control_messages(run.id)
        .expect("controls should load")
        .into_iter()
        .find(|message| message.id == control.id)
        .expect("guidance should remain queryable");
    assert_eq!(applied.status, ControlMessageStatus::Applied);
    assert_eq!(applied.claimed_turn_id, Some(turn.id));
}

#[tokio::test]
async fn turn_prompt_snapshots_preserve_layer_provenance_across_prompt_edits() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open"),
    );
    let project = store
        .create_project("Prompt snapshots", directory.path().join("project"))
        .expect("Project should be created");
    store
        .set_project_system_prompt(project.id, "Project prompt one.")
        .expect("Project prompt should update");
    let (run, participant) = workflow_agent(&store, project.id, "Agent prompt one.");
    let store_handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let skills = Arc::new(ProjectSkillCatalog::new(store_handle.clone()));
    let model = ScriptedModelClient::new([response("First."), response("Second.")]);
    let runtime = SessionRuntime::new(
        store_handle,
        Arc::new(model.clone()),
        ToolCatalog::default(),
        skills,
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    );

    let first = execute_action(&runtime, &store, &run, &participant, "First question").await;
    assert_eq!(first.origin, TurnOrigin::Workflow);
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
    assert!(
        first
            .prompt
            .layers
            .iter()
            .any(|layer| layer.content == "Project prompt one.")
    );

    store
        .set_project_system_prompt(project.id, "Project prompt two.")
        .expect("Project prompt should update");
    store
        .set_session_system_prompt(participant.session_id, "Agent prompt two.")
        .expect("Agent prompt should update");
    let second = execute_action(&runtime, &store, &run, &participant, "Second question").await;

    assert_ne!(first.prompt.sha256, second.prompt.sha256);
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
            .any(|layer| layer.content == "Agent prompt two.")
    );
    assert!(
        first
            .prompt
            .layers
            .iter()
            .any(|layer| layer.content == "Project prompt one.")
    );
    let requests = model.requests().expect("model requests should be captured");
    assert_eq!(requests[0].instructions, first.prompt.rendered);
    assert_eq!(requests[1].instructions, second.prompt.rendered);
}

#[tokio::test]
async fn workflow_token_usage_is_recorded_at_each_model_step() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open"),
    );
    let research = store
        .create_project("Usage", directory.path().join("project"))
        .expect("research should be created");
    let origin = store
        .create_session(research.id, "Origin", "", "test-model", Vec::new())
        .expect("origin session should be created");
    let run = store
        .create_workflow(NewWorkflow {
            project_id: research.id,
            started_from_session_id: Some(origin.id),
            program: workflow_snapshot(),
            request: "Record usage".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: serde_json::json!({}),
            default_model: "test-model".to_string(),
            access: papermachine_protocol::AccessPreset::Research,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("run should be created");
    store.start_workflow(run.id).expect("run should start");
    let participant = store
        .create_participant(
            run.id,
            "Researcher",
            "Researcher",
            "evidence",
            "",
            "",
            Vec::new(),
            papermachine_protocol::AccessPreset::Research,
        )
        .expect("participant should be created");
    let invocation = store
        .create_action_invocation(
            run.id,
            participant.id,
            "investigate",
            "Research",
            serde_json::json!({}),
            Vec::new(),
        )
        .expect("invocation should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("attempt should start");

    let model = ScriptedModelClient::new([response("Usage was recorded.")]);
    let store_handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let skills = Arc::new(ProjectSkillCatalog::new(store_handle.clone()));
    skills
        .ensure_project(research.id)
        .await
        .expect("research directories should exist");
    let runtime = SessionRuntime::new(
        store_handle,
        Arc::new(model),
        ToolCatalog::default(),
        skills,
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    );

    let turn = runtime
        .execute_workflow_action(
            participant.session_id,
            papermachine_protocol::TurnOrigin::Workflow,
            "Research",
            None,
            Vec::new(),
            None,
            Vec::new(),
            true,
            None,
            None,
            WorkflowTurnContext {
                workflow_id: run.id,
                action_invocation_id: invocation.id,
                action_attempt_id: attempt.id,
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("turn should complete");
    assert_eq!(turn.status, TurnStatus::Completed);
    let updated = store
        .get_workflow(run.id)
        .expect("run should load after the model step");
    assert_eq!(updated.usage.tokens.input_tokens, 10);
    assert_eq!(updated.usage.tokens.output_tokens, 3);
}
