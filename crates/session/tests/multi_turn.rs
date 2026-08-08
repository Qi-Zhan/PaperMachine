use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_model::ModelStream;
use papermachine_model::ScriptedModelClient;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::PromptLayerKind;
use papermachine_protocol::SessionStatus;
use papermachine_protocol::StepStatus;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::TurnOrigin;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_protocol::WorkflowStatus;
use papermachine_session::SessionRuntime;
use papermachine_session::SessionRuntimeConfig;
use papermachine_session::SessionRuntimeError;
use papermachine_session::WorkflowTurnContext;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::NewWorkflow;
use papermachine_store::Store;
use papermachine_tools::ToolRegistry;
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
            output_schema: serde_json::json!({"type": "object"}),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: "builtin/usage-test/workflow.py".to_string(),
        sha256: "usage-test".to_string(),
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
async fn cancelling_a_turn_closes_its_running_model_step() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open"),
    );
    let research = store
        .create_project("Cancellation", "", directory.path().join("project"))
        .expect("research should be created");
    let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
    skills
        .ensure_project(research.id)
        .expect("research directories should exist");
    let session = store
        .create_session(research.id, "Session", "", "test-model", Vec::new())
        .expect("session should be created");
    let runtime = SessionRuntime::new(
        Arc::clone(&store),
        Arc::new(BlockingModelClient),
        ToolRegistry::default(),
        skills,
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    );

    let turn = runtime
        .submit(session.id, "Wait indefinitely")
        .await
        .expect("turn should submit");
    for _ in 0..100 {
        if store
            .list_steps(turn.id)
            .expect("steps should load")
            .iter()
            .any(|step| step.status == StepStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    runtime.cancel(turn.id).await.expect("turn should cancel");
    for _ in 0..100 {
        if store.get_turn(turn.id).expect("turn should load").status == TurnStatus::Cancelled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        store.get_turn(turn.id).expect("turn should load").status,
        TurnStatus::Cancelled
    );
    assert_eq!(
        store
            .get_session(session.id)
            .expect("session should load")
            .status,
        SessionStatus::Ready
    );
    let steps = store.list_steps(turn.id).expect("steps should load");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, StepStatus::Cancelled);
}

#[tokio::test]
async fn cancelling_a_workflow_action_turn_reaches_its_parent_execution() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("store should open"),
    );
    let project = store
        .create_project(
            "Action cancellation",
            "",
            directory.path().join("workspace"),
        )
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
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    store
        .set_workflow_status(run.id, WorkflowStatus::Running, None)
        .expect("Workflow should start");
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
            None,
            participant.id,
            "investigate",
            "Wait",
            serde_json::json!({}),
        )
        .expect("invocation should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("attempt should start");
    let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
    let runtime = SessionRuntime::new(
        Arc::clone(&store),
        Arc::new(BlockingModelClient),
        ToolRegistry::default(),
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

    let mut active_turn = None;
    for _ in 0..100 {
        active_turn = store
            .list_turns(participant_session_id)
            .expect("Turns should load")
            .into_iter()
            .find(|turn| turn.status == TurnStatus::Running);
        if let Some(turn) = active_turn.as_ref()
            && !store
                .list_steps(turn.id)
                .expect("Steps should load")
                .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let turn = active_turn.expect("Workflow Action Turn should be running");
    assert!(
        !store
            .list_steps(turn.id)
            .expect("Step should be running before cancellation")
            .is_empty(),
        "Workflow Action should reach its model Step before cancellation"
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
        .create_project(
            "Persistent conversation",
            "",
            directory.path().join("project"),
        )
        .expect("research should be created");
    let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
    skills
        .ensure_project(research.id)
        .expect("research directories should exist");
    let session = store
        .create_session(research.id, "Session", "", "test-model", Vec::new())
        .expect("session should be created");
    let model = ScriptedModelClient::new([
        response("First answer."),
        response("Second answer using context."),
    ]);
    let runtime = SessionRuntime::new(
        Arc::clone(&store),
        Arc::new(model.clone()),
        ToolRegistry::default(),
        skills,
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    );

    let first = runtime
        .submit(session.id, "First question")
        .await
        .expect("first turn should submit");
    wait_for_completion(&store, first.id).await;
    let second = runtime
        .submit(session.id, "Follow-up question")
        .await
        .expect("second turn should submit");
    wait_for_completion(&store, second.id).await;

    let requests = model.requests().expect("requests should be captured");
    assert_eq!(requests.len(), 2);
    let expected_transport_key = session.id.to_string();
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
async fn turn_prompt_snapshots_preserve_layer_provenance_across_prompt_edits() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open"),
    );
    let project = store
        .create_project("Prompt snapshots", "", directory.path().join("project"))
        .expect("Project should be created");
    store
        .set_project_system_prompt(project.id, "Project prompt one.")
        .expect("Project prompt should update");
    let session = store
        .create_session(
            project.id,
            "Session",
            "Session prompt one.",
            "test-model",
            Vec::new(),
        )
        .expect("Session should be created");
    let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
    let model = ScriptedModelClient::new([response("First."), response("Second.")]);
    let runtime = SessionRuntime::new(
        Arc::clone(&store),
        Arc::new(model.clone()),
        ToolRegistry::default(),
        skills,
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    );

    let first = runtime
        .submit(session.id, "First question")
        .await
        .expect("first Turn should submit");
    wait_for_completion(&store, first.id).await;
    let first = store.get_turn(first.id).expect("first Turn should load");
    assert_eq!(first.origin, TurnOrigin::User);
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
            PromptLayerKind::Session,
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
        .set_session_system_prompt(session.id, "Session prompt two.")
        .expect("Session prompt should update");
    let second = runtime
        .submit(session.id, "Second question")
        .await
        .expect("second Turn should submit");
    wait_for_completion(&store, second.id).await;
    let second = store.get_turn(second.id).expect("second Turn should load");

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
            .any(|layer| layer.content == "Session prompt two.")
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
        .create_project("Usage", "", directory.path().join("project"))
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
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("run should be created");
    store
        .set_workflow_status(run.id, WorkflowStatus::Running, None)
        .expect("run should start");
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
            None,
            participant.id,
            "investigate",
            "Research",
            serde_json::json!({}),
        )
        .expect("invocation should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("attempt should start");

    let model = ScriptedModelClient::new([response("Usage was recorded.")]);
    let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
    skills
        .ensure_project(research.id)
        .expect("research directories should exist");
    let runtime = SessionRuntime::new(
        Arc::clone(&store),
        Arc::new(model),
        ToolRegistry::default(),
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

async fn wait_for_completion(store: &Store, turn_id: papermachine_protocol::TurnId) {
    for _ in 0..100 {
        let turn = store.get_turn(turn_id).expect("turn should load");
        if turn.status == TurnStatus::Completed {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("turn did not complete");
}
