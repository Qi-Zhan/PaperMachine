#![cfg(target_os = "macos")]

use papermachine_model::{ModelClient, ScriptedModelClient};
use papermachine_protocol::{
    AccessPreset, HumanRequestStatus, ModelEvent, TokenUsage, WorkflowContextMode,
    WorkflowEffectStatus, WorkflowLaunchContext, WorkflowProgramId, WorkflowProgramManifest,
    WorkflowProgramSnapshot, WorkflowProgramSource, WorkflowStatus,
};
use papermachine_session::{SessionRuntime, SessionRuntimeConfig};
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::{NewWorkflow, Store, StoreHandle};
use papermachine_tools::ToolCatalog;
use papermachine_workflow::{
    PythonWorkflowRuntime, WorkflowExecution, WorkflowRuntime, WorkflowScheduler,
    python_runtime_sha256, resolve_python_executable,
};
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

const SOURCE: &str = r#"from papermachine import Agent, Team, ask_human, workflow


class Observer(Agent):
    access = "model_only"
    role = "replay observer"


@workflow(
    slug="durable-replay",
    name="Durable replay",
    description="Exercise replay across an abrupt Python process loss.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    observer = Observer(name="Observer")
    team = Team("Review team", observer)
    await team.activate()
    decision = await ask_human(
        "Continue after the simulated restart?",
        response_schema={"type": "string"},
    )
    return {"decision": decision}
"#;

const TIMER_SOURCE: &str = r#"from papermachine import wait, workflow


@workflow(
    slug="durable-timer-replay",
    name="Durable timer replay",
    description="Suspend a Python process until its durable timer is due.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    fired = await wait(seconds=0.05, name="test-wake")
    return {"fire_count": fired["fire_count"]}
"#;

const SIGNAL_SOURCE: &str = r#"from papermachine import Channel, together, workflow


async def receive(channel):
    return await channel.receive()


async def publish(channel):
    await channel.publish({"message": "ready"})
    return "published"


@workflow(
    slug="durable-signal-replay",
    name="Durable signal replay",
    description="Synchronize concurrent branches through a durable Channel.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    channel = Channel("handoff", schema={"type": "object"})
    received, _ = await together(receive(channel), publish(channel))
    return received
"#;

const BACKGROUND_TIMER_SOURCE: &str = r#"from papermachine import Agent, ask_human, every, workflow


class Coordinator(Agent):
    access = "model_only"


@every(seconds=0.05, name="background-summary")
async def summarize_on_timer():
    return None


@workflow(
    slug="background-timer-human",
    name="Background timer and human",
    description="Keep a durable timer active while the main flow waits for a human.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    coordinator = Coordinator(name="Coordinator")
    answer = await ask_human("How should this continue?", agent=coordinator)
    return {"answer": answer}
"#;

const LAUNCH_CONTEXT_SOURCE: &str = r#"from papermachine import Agent, action, workflow


class Conservative(Agent):
    access = "research"

    @action
    async def inspect(self, question: str) -> str:
        """Inspect the captured evidence conservatively."""


class Elevated(Agent):
    access = "model_only"

    @action
    async def compare(self, question: str) -> str:
        """Compare evidence using the configured run access."""


class Clamped(Agent):
    access = "full_access"

    @action
    async def verify(self, question: str) -> str:
        """Verify that the Workflow ceiling remains authoritative."""


@workflow(
    slug="launch-context-access",
    name="Launch context and access",
    description="Exercise immutable launch context and per-Agent access.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    conservative = Conservative(name="Conservative")
    elevated = Elevated(name="Elevated")
    clamped = Clamped(name="Clamped")
    first = await conservative.inspect(ctx.context["project"]["name"])
    second = await elevated.compare(ctx.context["project"]["name"])
    third = await clamped.verify(ctx.context["project"]["name"])
    return {
        "context": ctx.context,
        "answers": [first, second, third],
    }
"#;

fn program_with_source(slug: &str, source_code: &str) -> WorkflowProgramSnapshot {
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: WorkflowProgramManifest {
            id: WorkflowProgramId::new(),
            slug: slug.to_string(),
            name: "Durable replay".to_string(),
            description: "Runtime recovery test".to_string(),
            entrypoint: "main".to_string(),
            request_mode: Default::default(),
            params_schema: json!({"type": "object"}),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: format!("builtin/{slug}/workflow.py"),
        sha256: hex::encode(Sha256::digest(source_code.as_bytes())),
        runtime_sha256: python_runtime_sha256(&python_runtime_root())
            .expect("Python runtime should hash"),
        source_code: source_code.to_string(),
    }
}

fn python_runtime_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python")
}

fn runtime_with(
    store: Arc<Store>,
    work_root: &Path,
    model: Arc<dyn ModelClient>,
    tools: ToolCatalog,
) -> PythonWorkflowRuntime {
    let store = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    runtime_on_handle(store, work_root, model, tools)
}

fn runtime_on_handle(
    store: StoreHandle,
    work_root: &Path,
    model: Arc<dyn ModelClient>,
    tools: ToolCatalog,
) -> PythonWorkflowRuntime {
    let skills = Arc::new(ProjectSkillCatalog::new(store.clone()));
    let sessions = SessionRuntime::new(
        store.clone(),
        model,
        tools,
        skills,
        SessionRuntimeConfig {
            default_model: "scripted".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 2,
        },
    );
    PythonWorkflowRuntime::new(
        store,
        sessions,
        resolve_python_executable().expect("Python 3.11 or newer should be available"),
        python_runtime_root(),
        work_root,
    )
}

fn runtime(store: Arc<Store>, work_root: &Path) -> PythonWorkflowRuntime {
    runtime_with(
        store,
        work_root,
        Arc::new(ScriptedModelClient::default()),
        ToolCatalog::default(),
    )
}

fn completed_response(text: &str, input_tokens: u64, output_tokens: u64) -> Vec<ModelEvent> {
    vec![
        ModelEvent::OutputTextDelta {
            delta: text.to_string(),
        },
        ModelEvent::Completed {
            usage: TokenUsage {
                input_tokens,
                output_tokens,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
            },
        },
    ]
}

#[tokio::test]
async fn workflow_runtime_fails_closed_when_the_python_abi_snapshot_differs() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("ABI mismatch", directory.path().join("project"))
        .expect("Project should be created");
    let mut program = program_with_source("abi-mismatch", TIMER_SOURCE);
    program.runtime_sha256 = "0".repeat(64);
    let workflow = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: None,
            program,
            request: "Do not run with a different ABI.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::ModelOnly,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    store
        .start_workflow(workflow.id)
        .expect("Workflow should be runnable");

    let error = runtime(Arc::clone(&store), &directory.path().join("runtime"))
        .execute(workflow.id, CancellationToken::new())
        .await
        .expect_err("ABI mismatch must fail before Python starts");
    assert!(error.contains("Python Workflow ABI differs"));
    assert!(
        store
            .list_workflow_effects(workflow.id)
            .expect("effects should load")
            .is_empty()
    );
}

#[tokio::test]
async fn launch_context_is_stable_and_agent_access_respects_run_configuration() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Configured run", directory.path().join("project"))
        .expect("Project should be created");
    let launch_context = WorkflowLaunchContext {
        mode: WorkflowContextMode::ProjectSnapshot,
        snapshot: Some(json!({
            "cursor": 7,
            "project": {
                "id": project.id,
                "name": project.name,
            },
        })),
    };
    let workflow = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: None,
            program: program_with_source("launch-context-access", LAUNCH_CONTEXT_SOURCE),
            request: "Continue from captured Project evidence.".to_string(),
            instructions: "Keep provenance visible.".to_string(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::Workspace,
            enabled_skills: Vec::new(),
            launch_context: launch_context.clone(),
            agent_access_overrides: BTreeMap::from([
                ("Conservative".to_string(), AccessPreset::ReadOnly),
                ("Elevated".to_string(), AccessPreset::Workspace),
            ]),
        })
        .expect("Workflow should be created");
    store
        .start_workflow(workflow.id)
        .expect("Workflow should be runnable");
    let model = ScriptedModelClient::new([
        completed_response("conservative answer", 20, 4),
        completed_response("elevated answer", 20, 4),
        completed_response("clamped answer", 20, 4),
    ]);

    let work_root = directory.path().join("runtime");
    let execution = runtime_with(
        Arc::clone(&store),
        &work_root,
        Arc::new(model),
        ToolCatalog::default(),
    )
    .execute(workflow.id, CancellationToken::new())
    .await
    .expect("Workflow should execute");
    let WorkflowExecution::Completed(output) = execution else {
        panic!("Workflow should complete without suspension")
    };
    let Some(expected_context) = launch_context.snapshot.as_ref() else {
        panic!("test launch context should contain a snapshot")
    };
    assert_eq!(&output["context"], expected_context);
    assert_eq!(
        output["answers"],
        json!(["conservative answer", "elevated answer", "clamped answer"])
    );
    assert!(
        store
            .list_human_requests(workflow.id)
            .expect("human requests should load")
            .is_empty(),
        "launch-time access choices at or below the ceiling are already authorized"
    );
    assert!(!work_root.join(workflow.id.to_string()).exists());
    assert!(
        std::fs::read_dir(store.managed_root().join("runtime/sandboxes"))
            .expect("sandbox root should list")
            .next()
            .is_none()
    );

    let participants = store
        .list_participants(workflow.id)
        .expect("participants should load");
    for participant in participants {
        let session = store
            .get_session(participant.session_id)
            .expect("participant Session should load");
        let expected = match participant.class_name.as_str() {
            "Conservative" => AccessPreset::ReadOnly,
            "Elevated" | "Clamped" => AccessPreset::Workspace,
            class_name => panic!("unexpected Agent class {class_name}"),
        };
        assert_eq!(session.access, expected);
        let turn = store
            .list_turns(session.id)
            .expect("Agent Turns should load")
            .into_iter()
            .next()
            .expect("each Agent should have one Turn");
        assert!(
            turn.prompt
                .layers
                .iter()
                .all(|layer| layer.name != "Workflow launch context"),
            "launch context must not be injected as instructions"
        );
        assert!(
            turn.input.contains("Configured run"),
            "the Workflow explicitly passed ctx.context as Action Turn data"
        );
    }
}

#[tokio::test]
async fn abrupt_runtime_loss_replays_effects_without_duplicate_resources() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Durable replay", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: None,
            program: program_with_source("durable-replay", SOURCE),
            request: "Prove replay semantics.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::Research,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    store
        .start_workflow(workflow.id)
        .expect("Workflow should be running");

    let first_runtime = runtime(Arc::clone(&store), &directory.path().join("runtime"));
    let workflow_id = workflow.id;
    let first_execution = tokio::spawn(async move {
        first_runtime
            .execute(workflow_id, CancellationToken::new())
            .await
    });

    let request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(request) = store
                .list_human_requests(workflow.id)
                .expect("human requests should load")
                .into_iter()
                .find(|request| request.status == HumanRequestStatus::Open)
                && store
                    .list_workflow_effects(workflow.id)
                    .expect("effects should load")
                    .iter()
                    .any(|effect| {
                        effect.kind == "ask_human" && effect.status == WorkflowEffectStatus::Started
                    })
            {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Workflow should reach its human wait");

    let first_output = first_execution
        .await
        .expect("runtime task should join")
        .expect("durable human wait should suspend cleanly");
    assert!(matches!(
        first_output,
        WorkflowExecution::Suspended(ref suspension)
            if suspension.status == WorkflowStatus::WaitingForUser
    ));
    store
        .answer_human_request(request.id, json!("yes"))
        .expect("the durable request should remain answerable");

    let output = runtime(Arc::clone(&store), &directory.path().join("runtime"))
        .execute(workflow.id, CancellationToken::new())
        .await
        .expect("replayed Workflow should complete");
    assert_eq!(
        output,
        WorkflowExecution::Completed(json!({"decision": "yes"}))
    );
    assert_eq!(
        store
            .list_participants(workflow.id)
            .expect("participants should load")
            .len(),
        1
    );
    assert_eq!(
        store
            .list_teams(workflow.id)
            .expect("teams should load")
            .len(),
        1
    );
    assert_eq!(
        store
            .list_human_requests(workflow.id)
            .expect("human requests should load")
            .len(),
        1
    );
    let effects = store
        .list_workflow_effects(workflow.id)
        .expect("effects should load");
    assert_eq!(effects.len(), 4);
    assert_eq!(
        effects
            .iter()
            .map(|effect| effect.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["create_agent", "create_team", "ask_human", "complete"]
    );
    assert!(
        effects
            .iter()
            .all(|effect| effect.status == WorkflowEffectStatus::Completed)
    );
}

#[tokio::test]
async fn durable_timer_suspends_the_python_process_and_replays_when_due() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Durable timer", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: None,
            program: program_with_source("durable-timer-replay", TIMER_SOURCE),
            request: "Wait without retaining a Python process.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::ModelOnly,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    store
        .start_workflow(workflow.id)
        .expect("Workflow should be running");

    let first = runtime(Arc::clone(&store), &directory.path().join("runtime"))
        .execute(workflow.id, CancellationToken::new())
        .await
        .expect("timer wait should suspend cleanly");
    let wake_at = match first {
        WorkflowExecution::Suspended(suspension) => {
            assert_eq!(suspension.status, WorkflowStatus::WaitingForTimer);
            suspension
                .wake_at
                .expect("timer suspension should have a wake time")
        }
        WorkflowExecution::Completed(output) => {
            panic!("timer completed before suspension: {output}")
        }
    };
    assert!(
        store
            .list_workflow_effects(workflow.id)
            .expect("effects should load")
            .iter()
            .any(|effect| effect.kind == "wait_timer"
                && effect.status == WorkflowEffectStatus::Started)
    );

    let delay = (wake_at - chrono::Utc::now()).to_std().unwrap_or_default();
    tokio::time::sleep(delay + Duration::from_millis(10)).await;
    store
        .start_workflow(workflow.id)
        .expect("timer Workflow should be runnable");
    let output = runtime(Arc::clone(&store), &directory.path().join("runtime"))
        .execute(workflow.id, CancellationToken::new())
        .await
        .expect("due timer should replay");
    assert_eq!(
        output,
        WorkflowExecution::Completed(json!({"fire_count": 1}))
    );
}

#[tokio::test]
async fn concurrent_channel_branches_replay_a_signal_published_before_suspension() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Durable signal", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: None,
            program: program_with_source("durable-signal-replay", SIGNAL_SOURCE),
            request: "Coordinate concurrent work.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::ModelOnly,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    let store_handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let executor = runtime_on_handle(
        store_handle.clone(),
        &directory.path().join("runtime"),
        Arc::new(ScriptedModelClient::default()),
        ToolCatalog::default(),
    );
    let scheduler = WorkflowScheduler::new(store_handle, Arc::new(executor), 1);

    scheduler
        .start(workflow.id)
        .await
        .expect("signal Workflow should start");
    let output = tokio::time::timeout(Duration::from_secs(10), scheduler.wait(workflow.id))
        .await
        .expect("signal Workflow should not deadlock")
        .expect("signal Workflow should remain scheduled")
        .expect("signal Workflow should complete");
    assert_eq!(output, json!({"message": "ready"}));
    assert_eq!(
        store
            .list_signals(
                store
                    .list_channels(workflow.id)
                    .expect("channels should load")[0]
                    .id,
                0,
            )
            .expect("signals should load")
            .len(),
        1,
        "replay must not publish the signal twice"
    );
}

#[tokio::test]
async fn background_timer_keeps_firing_while_main_flow_waits_for_human() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Timer plus human", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: None,
            program: program_with_source("background-timer-human", BACKGROUND_TIMER_SOURCE),
            request: "Wait and summarize periodically.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::ModelOnly,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("Workflow should be created");
    let store_handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let executor = runtime_on_handle(
        store_handle.clone(),
        &directory.path().join("runtime"),
        Arc::new(ScriptedModelClient::default()),
        ToolCatalog::default(),
    );
    let scheduler = WorkflowScheduler::new(store_handle, Arc::new(executor), 1);
    scheduler
        .start(workflow.id)
        .await
        .expect("Workflow should start");

    let request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let request = store
                .list_human_requests(workflow.id)
                .expect("human requests should load")
                .into_iter()
                .find(|request| request.status == HumanRequestStatus::Open);
            let fired = store
                .list_timers(workflow.id)
                .expect("timers should load")
                .first()
                .is_some_and(|timer| timer.fire_count >= 1);
            if let Some(request) = request
                && fired
            {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "background timer should fire before the human answers: workflow={:?}, effects={:?}, timers={:?}",
            store.get_workflow(workflow.id),
            store.list_workflow_effects(workflow.id),
            store.list_timers(workflow.id),
        )
    });
    store
        .answer_human_request(request.id, json!("Proceed."))
        .expect("human answer should be accepted");
    let output = tokio::time::timeout(Duration::from_secs(10), scheduler.wait(workflow.id))
        .await
        .expect("Workflow should finish after the answer")
        .expect("Workflow should remain scheduled")
        .expect("Workflow should complete");
    assert_eq!(output, json!({"answer": "Proceed."}));
}
