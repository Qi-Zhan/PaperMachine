#![cfg(target_os = "macos")]

use papermachine_model::{ModelClient, ScriptedModelClient};
use papermachine_protocol::{
    AgentAccessProfile, Budget, HumanRequestStatus, ModelEvent, ModelInputItem, TokenUsage,
    WorkflowEffectStatus, WorkflowProgramId, WorkflowProgramManifest, WorkflowProgramSnapshot,
    WorkflowProgramSource, WorkflowStatus,
};
use papermachine_session::{SessionRuntime, SessionRuntimeConfig};
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::Store;
use papermachine_tools::{AskHumanTool, ToolRegistry};
use papermachine_workflow::{
    PythonWorkflowRuntime, StoreHumanRequestBroker, WorkflowExecution, WorkflowRuntime,
    WorkflowScheduler,
};
use serde_json::json;
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
    input_schema={"type": "object", "additionalProperties": False},
    output_schema={"type": "object", "properties": {"decision": {"type": "string"}}},
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

const ACTION_SOURCE: &str = r#"from papermachine import Agent, action, workflow


class Researcher(Agent):
    access = "research"
    role = "recovery researcher"

    @action(max_steps=2)
    async def investigate(self, question: str) -> str:
        """Investigate the question, requesting human guidance when needed."""


@workflow(
    slug="durable-action-replay",
    name="Durable action replay",
    description="Resume one Agent Turn after an abrupt runtime loss.",
    input_schema={"type": "object", "additionalProperties": False},
    output_schema={"type": "object", "properties": {"answer": {"type": "string"}}},
)
async def main(ctx):
    researcher = Researcher(name="Researcher")
    answer = await researcher.investigate(ctx.objective)
    return {"answer": answer}
"#;

const TIMER_SOURCE: &str = r#"from papermachine import wait, workflow


@workflow(
    slug="durable-timer-replay",
    name="Durable timer replay",
    description="Suspend a Python process until its durable timer is due.",
    input_schema={"type": "object", "additionalProperties": False},
    output_schema={"type": "object", "properties": {"fire_count": {"type": "integer"}}},
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
    input_schema={"type": "object", "additionalProperties": False},
    output_schema={"type": "object"},
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
    input_schema={"type": "object", "additionalProperties": False},
    output_schema={"type": "object"},
)
async def main(ctx):
    coordinator = Coordinator(name="Coordinator")
    answer = await ask_human("How should this continue?", agent=coordinator)
    return {"answer": answer}
"#;

fn python() -> PathBuf {
    if let Some(value) = std::env::var_os("PAPERMACHINE_PYTHON") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return path;
        }
    }
    [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/usr/bin/python3",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
    .expect("a Python executable should be available on macOS")
}

fn program_with_source(slug: &str, source_code: &str) -> WorkflowProgramSnapshot {
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: WorkflowProgramManifest {
            id: WorkflowProgramId::new(),
            slug: slug.to_string(),
            name: "Durable replay".to_string(),
            description: "Runtime recovery test".to_string(),
            entrypoint: "main".to_string(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            default_budget: Budget::default(),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: format!("builtin/{slug}/workflow.py"),
        sha256: "durable-replay-test".to_string(),
        source_code: source_code.to_string(),
    }
}

fn runtime_with(
    store: Arc<Store>,
    work_root: &Path,
    model: Arc<dyn ModelClient>,
    tools: ToolRegistry,
) -> PythonWorkflowRuntime {
    let skills = Arc::new(ProjectSkillCatalog::new(Arc::clone(&store)));
    let sessions = SessionRuntime::new(
        Arc::clone(&store),
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
        python(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../python"),
        work_root,
    )
}

fn runtime(store: Arc<Store>, work_root: &Path) -> PythonWorkflowRuntime {
    runtime_with(
        store,
        work_root,
        Arc::new(ScriptedModelClient::default()),
        ToolRegistry::builder().build(),
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
async fn abrupt_runtime_loss_replays_effects_without_duplicate_resources() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Durable replay", "", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(
            project.id,
            None,
            program_with_source("durable-replay", SOURCE),
            "Prove replay semantics.",
            "",
            json!({}),
            None,
            "scripted",
            AgentAccessProfile::Research,
            Vec::new(),
        )
        .expect("Workflow should be created");
    store
        .set_workflow_status(workflow.id, WorkflowStatus::Running, None)
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
        .create_project("Durable timer", "", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(
            project.id,
            None,
            program_with_source("durable-timer-replay", TIMER_SOURCE),
            "Wait without retaining a Python process.",
            "",
            json!({}),
            None,
            "scripted",
            AgentAccessProfile::ModelOnly,
            Vec::new(),
        )
        .expect("Workflow should be created");
    store
        .set_workflow_status(workflow.id, WorkflowStatus::Running, None)
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
        .set_workflow_status(workflow.id, WorkflowStatus::Running, None)
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
        .create_project("Durable signal", "", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(
            project.id,
            None,
            program_with_source("durable-signal-replay", SIGNAL_SOURCE),
            "Coordinate concurrent work.",
            "",
            json!({}),
            None,
            "scripted",
            AgentAccessProfile::ModelOnly,
            Vec::new(),
        )
        .expect("Workflow should be created");
    let executor = runtime(Arc::clone(&store), &directory.path().join("runtime"));
    let scheduler = WorkflowScheduler::new(Arc::clone(&store), Arc::new(executor), 1);

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
        .create_project("Timer plus human", "", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(
            project.id,
            None,
            program_with_source("background-timer-human", BACKGROUND_TIMER_SOURCE),
            "Wait and summarize periodically.",
            "",
            json!({}),
            None,
            "scripted",
            AgentAccessProfile::ModelOnly,
            Vec::new(),
        )
        .expect("Workflow should be created");
    let executor = runtime(Arc::clone(&store), &directory.path().join("runtime"));
    let scheduler = WorkflowScheduler::new(Arc::clone(&store), Arc::new(executor), 1);
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

#[tokio::test]
async fn unfinished_agent_action_resumes_its_checkpointed_turn_once() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Durable Agent action", "", directory.path().join("project"))
        .expect("project should be created");
    let workflow = store
        .create_workflow(
            project.id,
            None,
            program_with_source("durable-action-replay", ACTION_SOURCE),
            "Investigate safely across a restart.",
            "",
            json!({}),
            None,
            "scripted",
            AgentAccessProfile::Research,
            Vec::new(),
        )
        .expect("Workflow should be created");
    store
        .set_workflow_status(workflow.id, WorkflowStatus::Running, None)
        .expect("Workflow should be running");

    let model = ScriptedModelClient::new([
        vec![
            ModelEvent::ResponseItemCompleted {
                item: json!({
                    "type": "function_call",
                    "call_id": "call-human-before-restart",
                    "name": "ask_human",
                    "arguments": "{\"question\":\"Which source should I prioritize?\",\"response_schema\":{\"type\":\"string\"}}"
                }),
            },
            ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 17,
                    output_tokens: 4,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            },
        ],
        completed_response("Recovered without replaying the first sample.", 23, 7),
    ]);
    let model_handle = model.clone();
    let tools = ToolRegistry::builder()
        .register(AskHumanTool::new(Arc::new(StoreHumanRequestBroker::new(
            Arc::clone(&store),
        ))))
        .expect("ask_human should register")
        .build();
    let first_runtime = runtime_with(
        Arc::clone(&store),
        &directory.path().join("runtime"),
        Arc::new(model.clone()),
        tools.clone(),
    );
    let workflow_id = workflow.id;
    let first_execution = tokio::spawn(async move {
        first_runtime
            .execute(workflow_id, CancellationToken::new())
            .await
    });

    let first_request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(request) = store
                .list_human_requests(workflow.id)
                .expect("human requests should load")
                .into_iter()
                .find(|request| request.status == HumanRequestStatus::Open)
            {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Agent should reach its human tool call");
    let turn_id = first_request
        .turn_id
        .expect("tool request should belong to a Turn");
    let checkpoint = store
        .get_turn(turn_id)
        .expect("checkpointed Turn should load");
    assert_eq!(checkpoint.completed_model_steps, 1);
    assert!(checkpoint.history.iter().any(|item| {
        matches!(item, ModelInputItem::ResponseItem { item }
            if item.get("type").and_then(serde_json::Value::as_str) == Some("function_call"))
    }));

    first_execution.abort();
    assert!(
        first_execution
            .await
            .expect_err("aborted runtime task should not complete")
            .is_cancelled()
    );
    store
        .set_workflow_status(workflow.id, WorkflowStatus::Running, None)
        .expect("recovery should make the Workflow runnable before replay");
    let output = runtime_with(
        Arc::clone(&store),
        &directory.path().join("runtime"),
        Arc::new(model),
        tools,
    )
    .execute(workflow.id, CancellationToken::new())
    .await
    .expect("unfinished Agent action should resume");

    assert_eq!(
        output,
        WorkflowExecution::Completed(
            json!({"answer": "Recovered without replaying the first sample."})
        )
    );
    let requests = model_handle.requests().expect("model requests should load");
    assert_eq!(
        requests.len(),
        2,
        "the first model sample must not be replayed"
    );
    assert!(requests[1].input.iter().any(|item| {
        matches!(item, ModelInputItem::FunctionCallOutput { call_id, output }
            if call_id == "call-human-before-restart"
                && output.get("recovered").and_then(serde_json::Value::as_bool) == Some(true))
    }));
    assert_eq!(
        store
            .get_human_request(first_request.id)
            .expect("orphaned human request should load")
            .status,
        HumanRequestStatus::Cancelled
    );
    assert_eq!(
        store
            .list_action_invocations(workflow.id)
            .expect("Actions should load")
            .len(),
        1
    );
    assert_eq!(
        store
            .get_workflow(workflow.id)
            .expect("Workflow usage should load")
            .usage
            .actions_started,
        1
    );
    let turn = store.get_turn(turn_id).expect("resumed Turn should load");
    assert_eq!(turn.completed_model_steps, 2);
    assert_eq!(turn.usage.input_tokens, 40);
    assert_eq!(turn.usage.output_tokens, 11);
}
