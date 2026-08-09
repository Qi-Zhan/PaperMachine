use async_trait::async_trait;
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
use papermachine_protocol::ToolEffectDisposition;
use papermachine_protocol::ToolExecutionState;
use papermachine_protocol::TurnOrigin;
use papermachine_protocol::TurnStatus;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_protocol::WorkflowStatus;
use papermachine_session::SessionRuntime;
use papermachine_session::SessionRuntimeConfig;
use papermachine_session::WorkflowTurnContext;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::NewWorkflow;
use papermachine_store::Store;
use papermachine_tools::ToolCatalog;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolError;
use papermachine_tools::ToolExecutor;
use papermachine_tools::ToolOutput;
use papermachine_tools::ToolReconciliation;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn workflow_restart_replays_an_executing_idempotent_tool_once() {
    let fixture = workflow_recovery_fixture(ToolEffectDisposition::Idempotent, "write_file");
    let runtime = runtime(
        Arc::clone(&fixture.store),
        Arc::new(fixture.model.clone()),
        fixture.catalog.clone(),
        Arc::clone(&fixture.skills),
    );
    let turn = runtime
        .resume_workflow_action(fixture.turn_id, fixture.context, CancellationToken::new())
        .await
        .expect("Workflow Turn should resume");

    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let step = fixture
        .store
        .get_step(fixture.step_id)
        .expect("Step should load");
    assert_eq!(step.status, StepStatus::Completed);
    assert_eq!(step.execution_state, Some(ToolExecutionState::Completed));
    let requests = fixture.model.requests().expect("requests should load");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].input.iter().any(|item| matches!(
        item,
        ModelInputItem::FunctionCallOutput { call_id, output }
            if call_id == "call-recovery" && output.get("ok") == Some(&Value::Bool(true))
    )));
}

#[tokio::test]
async fn workflow_restart_surfaces_unknown_effect_without_executing_it() {
    let fixture = workflow_recovery_fixture(ToolEffectDisposition::Unknown, "exec_command");
    let runtime = runtime(
        Arc::clone(&fixture.store),
        Arc::new(fixture.model.clone()),
        fixture.catalog.clone(),
        Arc::clone(&fixture.skills),
    );
    let turn = runtime
        .resume_workflow_action(fixture.turn_id, fixture.context, CancellationToken::new())
        .await
        .expect("Workflow Turn should resume around unknown execution");

    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let step = fixture
        .store
        .get_step(fixture.step_id)
        .expect("Step should load");
    assert_eq!(step.status, StepStatus::ExecutionUnknown);
    assert_eq!(
        step.execution_state,
        Some(ToolExecutionState::ExecutionUnknown)
    );
    let requests = fixture.model.requests().expect("requests should load");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].input.iter().any(|item| matches!(
        item,
        ModelInputItem::FunctionCallOutput { call_id, output }
            if call_id == "call-recovery"
                && output.pointer("/recovery/automatic_replay") == Some(&Value::Bool(false))
    )));
}

#[tokio::test]
async fn workflow_restart_executes_a_prepared_unknown_tool_once() {
    let fixture =
        workflow_recovery_fixture_with(ToolEffectDisposition::Unknown, "exec_command", false, None);
    let runtime = runtime(
        Arc::clone(&fixture.store),
        Arc::new(fixture.model.clone()),
        fixture.catalog.clone(),
        Arc::clone(&fixture.skills),
    );
    let turn = runtime
        .resume_workflow_action(fixture.turn_id, fixture.context, CancellationToken::new())
        .await
        .expect("prepared Workflow tool should execute after restart");

    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let step = fixture
        .store
        .get_step(fixture.step_id)
        .expect("Step should load");
    assert_eq!(step.status, StepStatus::Completed);
    assert_eq!(step.execution_state, Some(ToolExecutionState::Completed));
}

#[tokio::test]
async fn workflow_restart_reconciles_before_resolving_an_external_effect() {
    let fixture = workflow_recovery_fixture_with(
        ToolEffectDisposition::Reconcilable,
        "write_file",
        true,
        Some(ToolReconciliation::Completed(ToolOutput {
            value: json!({"reconciled": true}),
            summary: "external effect already completed".to_string(),
        })),
    );
    let runtime = runtime(
        Arc::clone(&fixture.store),
        Arc::new(fixture.model.clone()),
        fixture.catalog.clone(),
        Arc::clone(&fixture.skills),
    );
    runtime
        .resume_workflow_action(fixture.turn_id, fixture.context, CancellationToken::new())
        .await
        .expect("reconcilable Workflow tool should recover");

    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.reconciliations.load(Ordering::SeqCst), 1);
    let requests = fixture.model.requests().expect("requests should load");
    assert!(requests[0].input.iter().any(|item| matches!(
        item,
        ModelInputItem::FunctionCallOutput { call_id, output }
            if call_id == "call-recovery"
                && output.pointer("/result/reconciled") == Some(&Value::Bool(true))
    )));
}

struct WorkflowRecoveryFixture {
    _directory: TempDir,
    store: Arc<Store>,
    skills: Arc<ProjectSkillCatalog>,
    catalog: ToolCatalog,
    model: ScriptedModelClient,
    calls: Arc<AtomicUsize>,
    reconciliations: Arc<AtomicUsize>,
    turn_id: papermachine_protocol::TurnId,
    step_id: papermachine_protocol::StepId,
    context: WorkflowTurnContext,
}

fn workflow_recovery_fixture(
    disposition: ToolEffectDisposition,
    tool_name: &str,
) -> WorkflowRecoveryFixture {
    workflow_recovery_fixture_with(disposition, tool_name, true, None)
}

fn workflow_recovery_fixture_with(
    disposition: ToolEffectDisposition,
    tool_name: &str,
    execution_started: bool,
    reconciliation: Option<ToolReconciliation>,
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
    store
        .set_workflow_status(run.id, WorkflowStatus::Running, None)
        .expect("Workflow should run");
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
    let reconciliations = Arc::new(AtomicUsize::new(0));
    let catalog = ToolCatalog::builder()
        .register_workspace(CountingTool {
            name: tool_name.to_string(),
            disposition,
            calls: Arc::clone(&calls),
            reconciliations: Arc::clone(&reconciliations),
            reconciliation,
        })
        .expect("tool should register")
        .build();
    let requested_tools = vec![tool_name.to_string()];
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
    let tool_set = catalog
        .materialize_action_tools(&requested_tools, AccessPreset::Research, true)
        .expect("Action tool set should materialize");
    let turn = store
        .create_turn_for_attempt(
            attempt.id,
            participant.session_id,
            TurnOrigin::Workflow,
            "recover",
            "test-model",
            empty_prompt_snapshot(),
            None,
            true,
            AccessPreset::Research,
            tool_set,
            None,
            None,
            Vec::new(),
        )
        .expect("Turn should be created");
    store.start_turn(turn.id).expect("Turn should start");
    store
        .checkpoint_turn_context(
            turn.id,
            ModelContextMutation::Append {
                items: vec![
                    message(MessageRole::User, "recover"),
                    ModelInputItem::FunctionCall {
                        call_id: "call-recovery".to_string(),
                        name: tool_name.to_string(),
                        arguments: "{}".to_string(),
                    },
                ],
            },
            TokenUsage::default(),
            1,
            0,
            None,
        )
        .expect("context should checkpoint");
    let step = store
        .create_tool_step(turn.id, "call-recovery", tool_name, json!({}), disposition)
        .expect("Tool Step should be created");
    if execution_started {
        store
            .start_tool_execution(step.id)
            .expect("Tool Step should execute");
    }

    let model = ScriptedModelClient::new([response("recovered")]);
    let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
    skills
        .ensure_project(project.id)
        .expect("skill directories should exist");
    WorkflowRecoveryFixture {
        _directory: directory,
        store,
        skills,
        catalog,
        model,
        calls,
        reconciliations,
        turn_id: turn.id,
        step_id: step.id,
        context: WorkflowTurnContext {
            workflow_id: run.id,
            action_invocation_id: invocation.id,
            action_attempt_id: attempt.id,
        },
    }
}

#[derive(Clone)]
struct CountingTool {
    name: String,
    disposition: ToolEffectDisposition,
    calls: Arc<AtomicUsize>,
    reconciliations: Arc<AtomicUsize>,
    reconciliation: Option<ToolReconciliation>,
}

#[async_trait]
impl ToolExecutor for CountingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "recovery probe".to_string(),
            input_schema: json!({"type": "object"}),
            supports_parallel: false,
        }
    }

    fn effect_disposition(&self) -> ToolEffectDisposition {
        self.disposition
    }

    async fn execute(
        &self,
        context: ToolContext,
        _arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        assert_eq!(context.effect_id, "call-recovery");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            value: json!({"effect_id": context.effect_id}),
            summary: "recovered once".to_string(),
        })
    }

    async fn reconcile(
        &self,
        context: ToolContext,
        _arguments: Value,
    ) -> Result<ToolReconciliation, ToolError> {
        assert_eq!(context.effect_id, "call-recovery");
        self.reconciliations.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .reconciliation
            .clone()
            .unwrap_or(ToolReconciliation::Unknown {
                message: "no reconciliation result configured".to_string(),
            }))
    }
}

fn runtime(
    store: Arc<Store>,
    model: Arc<ScriptedModelClient>,
    tools: ToolCatalog,
    skills: Arc<ProjectSkillCatalog>,
) -> SessionRuntime {
    SessionRuntime::new(
        store,
        model,
        tools,
        skills,
        SessionRuntimeConfig {
            default_model: "test-model".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 1,
        },
    )
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
            output_schema: json!({"type": "object"}),
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
