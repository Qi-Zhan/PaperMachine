use async_trait::async_trait;
use papermachine_model::ModelClient;
use papermachine_model::ScriptedModelClient;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelContextMutation;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::PromptSnapshot;
use papermachine_protocol::StepStatus;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::TurnOrigin;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_session::SessionRuntime;
use papermachine_session::SessionRuntimeConfig;
use papermachine_session::WorkflowTurnContext;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::NewWorkflow;
use papermachine_store::Store;
use papermachine_store::StoreHandle;
use papermachine_store::TurnContextCheckpoint;
use papermachine_tools::ToolCatalog;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolError;
use papermachine_tools::ToolExecutor;
use papermachine_tools::ToolOutput;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn workflow_restart_aborts_an_unfinished_tool_without_dispatching_it() {
    let fixture = workflow_recovery_fixture(true, None);
    let turn = fixture
        .runtime()
        .resume_workflow_action(fixture.turn_id, fixture.context, CancellationToken::new())
        .await
        .expect("Workflow Turn should resume");

    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let step = fixture
        .store
        .get_step(fixture.step_id.expect("fixture should create a Step"))
        .expect("Step should load");
    assert_eq!(step.status, StepStatus::Aborted);
    assert_eq!(step.output, Some(Value::String("aborted".to_string())));
    let requests = fixture.model.requests().expect("requests should load");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].input.iter().any(|item| matches!(
        item,
        ModelInputItem::FunctionCallOutput { call_id, output }
            if call_id == "call-recovery" && output == "aborted"
    )));
    let rollout = fixture
        .store
        .reconstruct_session_rollout(turn.session_id)
        .expect("canonical rollout should reconstruct");
    assert_eq!(
        rollout
            .committed_context
            .iter()
            .filter(|item| matches!(
                item,
                ModelInputItem::FunctionCallOutput { call_id, output }
                    if call_id == "call-recovery" && output == "aborted"
            ))
            .count(),
        1,
        "recovery should durably append one synthetic aborted output"
    );
}

#[tokio::test]
async fn workflow_restart_creates_an_aborted_projection_for_a_canonical_call() {
    let fixture = workflow_recovery_fixture(false, None);
    fixture
        .runtime()
        .resume_workflow_action(fixture.turn_id, fixture.context, CancellationToken::new())
        .await
        .expect("Workflow Turn should resume");

    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let steps = fixture
        .store
        .list_steps(fixture.turn_id)
        .expect("Steps should load");
    let step = steps
        .iter()
        .find(|step| step.tool_call_id.as_deref() == Some("call-recovery"))
        .expect("recovery should project the canonical call");
    assert_eq!(step.status, StepStatus::Aborted);
}

#[tokio::test]
async fn workflow_restart_repairs_projection_from_a_canonical_tool_output() {
    let output = json!({"ok": true, "result": {"durable": true}});
    let fixture = workflow_recovery_fixture(true, Some(output.clone()));
    fixture
        .runtime()
        .resume_workflow_action(fixture.turn_id, fixture.context, CancellationToken::new())
        .await
        .expect("Workflow Turn should resume");

    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let step = fixture
        .store
        .get_step(fixture.step_id.expect("fixture should create a Step"))
        .expect("Step should load");
    assert_eq!(step.status, StepStatus::Completed);
    assert_eq!(step.output, Some(output.clone()));
    let requests = fixture.model.requests().expect("requests should load");
    assert!(requests[0].input.iter().any(|item| matches!(
        item,
        ModelInputItem::FunctionCallOutput { call_id, output: value }
            if call_id == "call-recovery" && value == &output
    )));
}

#[tokio::test]
async fn workflow_restart_fails_closed_when_the_model_route_changes() {
    let fixture = workflow_recovery_fixture(true, None);
    let error = fixture
        .runtime_with_context_window(64_000)
        .resume_workflow_action(fixture.turn_id, fixture.context, CancellationToken::new())
        .await
        .expect_err("a changed model route must fail closed");
    assert!(
        error
            .to_string()
            .contains("model route configuration changed")
    );
    assert!(
        fixture
            .model
            .requests()
            .expect("requests should load")
            .is_empty()
    );
}

struct WorkflowRecoveryFixture {
    _directory: TempDir,
    store: Arc<Store>,
    store_handle: StoreHandle,
    skills: Arc<ProjectSkillCatalog>,
    catalog: ToolCatalog,
    model: ScriptedModelClient,
    calls: Arc<AtomicUsize>,
    turn_id: papermachine_protocol::TurnId,
    step_id: Option<papermachine_protocol::StepId>,
    context: WorkflowTurnContext,
}

impl WorkflowRecoveryFixture {
    fn runtime(&self) -> SessionRuntime {
        self.runtime_with_context_window(128_000)
    }

    fn runtime_with_context_window(&self, model_context_window: usize) -> SessionRuntime {
        SessionRuntime::new(
            self.store_handle.clone(),
            Arc::new(self.model.clone()),
            self.catalog.clone(),
            Arc::clone(&self.skills),
            SessionRuntimeConfig {
                default_model: "test-model".to_string(),
                model_context_window,
                max_concurrent_turns: 1,
            },
        )
    }
}

fn workflow_recovery_fixture(
    create_step: bool,
    canonical_output: Option<Value>,
) -> WorkflowRecoveryFixture {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("Store should open"),
    );
    let project = store
        .create_project("Workflow", directory.path().join("workspace"))
        .expect("Project should be created");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let run = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: Some(origin.id),
            program: workflow_snapshot(),
            request: "recover".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "test-model".to_string(),
            access: AccessPreset::Research,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    store.start_workflow(run.id).expect("Workflow should run");
    let participant = store
        .create_participant(
            run.id,
            "Worker",
            "Worker",
            "recover",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::Research,
        )
        .expect("participant should be created");
    let calls = Arc::new(AtomicUsize::new(0));
    let catalog = ToolCatalog::builder()
        .register_workspace(CountingTool {
            calls: Arc::clone(&calls),
        })
        .expect("tool should register")
        .build();
    let requested_tools = vec!["write_file".to_string()];
    let invocation = store
        .create_action_invocation(
            run.id,
            None,
            participant.id,
            "recover",
            "recover",
            json!({}),
            requested_tools.clone(),
        )
        .expect("invocation should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("attempt should start");
    let model = ScriptedModelClient::new([response("recovered")]);
    let model_route = model
        .resolve_route_snapshot("test-model", None, 128_000)
        .expect("test model route should resolve");
    let tool_set = catalog
        .materialize_action_tools(&requested_tools, AccessPreset::Research, true)
        .expect("Action tool set should materialize");
    let turn = store
        .create_turn_for_attempt(
            attempt.id,
            participant.session_id,
            TurnOrigin::Workflow,
            "recover",
            model_route,
            empty_prompt_snapshot(),
            true,
            AccessPreset::Research,
            tool_set,
            None,
            None,
            Vec::new(),
        )
        .expect("Turn should be created");
    store.start_turn(turn.id).expect("Turn should start");
    let mut items = vec![
        message(MessageRole::User, "recover"),
        ModelInputItem::FunctionCall {
            call_id: "call-recovery".to_string(),
            name: "write_file".to_string(),
            arguments: "{}".to_string(),
        },
    ];
    if let Some(output) = canonical_output {
        items.push(ModelInputItem::FunctionCallOutput {
            call_id: "call-recovery".to_string(),
            output,
        });
    }
    store
        .checkpoint_turn_context(
            turn.id,
            TurnContextCheckpoint {
                mutation: ModelContextMutation::Append { items },
                usage: TokenUsage::default(),
                completed_model_steps: 1,
                hosted_search_calls_used: 0,
                checkpoint_message: None,
                acknowledged_control_ids: Vec::new(),
            },
        )
        .expect("context should checkpoint");
    let step_id = create_step.then(|| {
        store
            .create_tool_step(turn.id, "call-recovery", "write_file", json!({}))
            .expect("Tool Step should be created")
            .id
    });

    store
        .ensure_managed_directory("skills")
        .expect("skill directory should exist");
    store
        .ensure_managed_directory("sources")
        .expect("source directory should exist");
    let store_handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let skills = Arc::new(ProjectSkillCatalog::new(store_handle.clone()));
    WorkflowRecoveryFixture {
        _directory: directory,
        store,
        store_handle,
        skills,
        catalog,
        model,
        calls,
        turn_id: turn.id,
        step_id,
        context: WorkflowTurnContext {
            workflow_id: run.id,
            action_invocation_id: invocation.id,
            action_attempt_id: attempt.id,
        },
    }
}

#[derive(Clone)]
struct CountingTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolExecutor for CountingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_string(),
            description: "recovery probe".to_string(),
            input_schema: json!({"type": "object"}),
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        _context: ToolContext,
        _arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            value: json!({"unexpected": true}),
            summary: "unexpected dispatch".to_string(),
        })
    }
}

fn workflow_snapshot() -> WorkflowProgramSnapshot {
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: WorkflowProgramManifest {
            id: WorkflowProgramId::new(),
            slug: "recovery-test".to_string(),
            name: "Recovery test".to_string(),
            description: "Test recovery".to_string(),
            entrypoint: "main".to_string(),
            request_mode: Default::default(),
            params_schema: json!({"type": "object"}),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: "builtin/recovery-test/workflow.py".to_string(),
        sha256: "recovery-test".to_string(),
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
            usage: TokenUsage::default(),
        },
    ]
}

fn message(role: MessageRole, content: &str) -> ModelInputItem {
    ModelInputItem::Message {
        role,
        content: content.to_string(),
    }
}

fn empty_prompt_snapshot() -> PromptSnapshot {
    PromptSnapshot {
        layers: Vec::new(),
        rendered: String::new(),
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
    }
}
