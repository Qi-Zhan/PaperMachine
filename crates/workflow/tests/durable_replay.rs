#![cfg(target_os = "macos")]

use papermachine_model::{ModelClient, ScriptedModelClient};
use papermachine_protocol::{
    AccessPreset, HumanRequestStatus, ModelEvent, SessionEffectStatus, SessionStatus, TokenUsage,
    WorkflowProgramId, WorkflowProgramManifest, WorkflowProgramSnapshot, WorkflowProgramSource,
};
use papermachine_session::{TurnRuntime, TurnRuntimeConfig};
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::{NewSession, Store, StoreHandle};
use papermachine_tools::ToolCatalog;
use papermachine_workflow::{
    PythonSessionExecutor, SessionExecution, SessionExecutor, python_runtime_sha256,
    resolve_python_executable,
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

const SOURCE: &str = r#"from papermachine import Agent, ask_human, workflow


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
    decision = await ask_human(
        "Continue after the simulated restart?",
        response_schema={"type": "string"},
        agent=observer,
    )
    return {"decision": decision}
"#;

const WAIT_SOURCE: &str = r#"from papermachine import wait, workflow


@workflow(
    slug="durable-wait-replay",
    name="Durable wait replay",
    description="Suspend a Python process until its durable deadline is due.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    await wait(seconds=0.05, name="test-wake")
    return {"completed": True}
"#;

const RUN_ACCESS_SOURCE: &str = r#"from papermachine import Agent, action, workflow


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
        """Verify that the Session ceiling remains authoritative."""


@workflow(
    slug="run-access",
    name="Run access",
    description="Exercise per-Agent access.",
    params_schema={"type": "object", "additionalProperties": False},
)
async def main(ctx):
    conservative = Conservative(name="Conservative")
    elevated = Elevated(name="Elevated")
    clamped = Clamped(name="Clamped")
    first = await conservative.inspect(ctx.request)
    second = await elevated.compare(ctx.request)
    third = await clamped.verify(ctx.request)
    return {"answers": [first, second, third]}
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
) -> PythonSessionExecutor {
    let store = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    runtime_on_handle(store, work_root, model, tools)
}

fn runtime_on_handle(
    store: StoreHandle,
    work_root: &Path,
    model: Arc<dyn ModelClient>,
    tools: ToolCatalog,
) -> PythonSessionExecutor {
    let skills = Arc::new(ProjectSkillCatalog::new(store.clone()));
    let turns = TurnRuntime::new(
        store.clone(),
        model,
        tools,
        skills,
        TurnRuntimeConfig {
            default_model: "scripted".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 2,
        },
    );
    PythonSessionExecutor::new(
        store,
        turns,
        resolve_python_executable().expect("Python 3.11 or newer should be available"),
        python_runtime_root(),
        work_root,
    )
}

fn runtime(store: Arc<Store>, work_root: &Path) -> PythonSessionExecutor {
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
    let mut program = program_with_source("abi-mismatch", WAIT_SOURCE);
    program.runtime_sha256 = "0".repeat(64);
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program,
            title: "ABI mismatch".to_string(),
            request: "Do not run with a different ABI.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::ModelOnly,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should be runnable");

    let error = runtime(Arc::clone(&store), &directory.path().join("runtime"))
        .execute(session.id, CancellationToken::new())
        .await
        .expect_err("ABI mismatch must fail before Python starts");
    assert!(error.contains("Python Workflow ABI differs"));
    assert!(
        store
            .list_session_effects(session.id)
            .expect("effects should load")
            .is_empty()
    );
}

#[tokio::test]
async fn agent_access_respects_run_configuration() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Configured run", directory.path().join("project"))
        .expect("Project should be created");
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: program_with_source("run-access", RUN_ACCESS_SOURCE),
            title: "Configured run".to_string(),
            request: "Inspect the configured run.".to_string(),
            instructions: "Keep provenance visible.".to_string(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::Workspace,
            enabled_skills: Vec::new(),
            agent_access_overrides: BTreeMap::from([
                ("Conservative".to_string(), AccessPreset::ReadOnly),
                ("Elevated".to_string(), AccessPreset::Workspace),
            ]),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should be runnable");
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
    .execute(session.id, CancellationToken::new())
    .await
    .expect("Session should execute");
    let SessionExecution::Completed(output) = execution else {
        panic!("Session should complete without suspension")
    };
    assert_eq!(
        output["answers"],
        json!(["conservative answer", "elevated answer", "clamped answer"])
    );
    assert!(
        store
            .list_human_requests(session.id)
            .expect("human requests should load")
            .is_empty(),
        "launch-time access choices at or below the ceiling are already authorized"
    );
    assert!(!work_root.join(session.id.to_string()).exists());
    assert!(
        std::fs::read_dir(store.managed_root().join("runtime/sandboxes"))
            .expect("sandbox root should list")
            .next()
            .is_none()
    );

    let agents = store.list_agents(session.id).expect("Agents should load");
    for agent in agents {
        let expected = match agent.class_name.as_str() {
            "Conservative" => AccessPreset::ReadOnly,
            "Elevated" | "Clamped" => AccessPreset::Workspace,
            class_name => panic!("unexpected Agent class {class_name}"),
        };
        assert_eq!(agent.access, expected);
        let turn = store
            .list_turns(agent.id)
            .expect("Agent Turns should load")
            .into_iter()
            .next()
            .expect("each Agent should have one Turn");
        assert!(turn.input.contains("Inspect the configured run"));
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
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: program_with_source("durable-replay", SOURCE),
            title: "Durable replay".to_string(),
            request: "Prove replay semantics.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::Research,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should be running");

    let first_runtime = runtime(Arc::clone(&store), &directory.path().join("runtime"));
    let session_id = session.id;
    let first_execution = tokio::spawn(async move {
        first_runtime
            .execute(session_id, CancellationToken::new())
            .await
    });

    let request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(request) = store
                .list_human_requests(session.id)
                .expect("human requests should load")
                .into_iter()
                .find(|request| request.status == HumanRequestStatus::Open)
                && store
                    .list_session_effects(session.id)
                    .expect("effects should load")
                    .iter()
                    .any(|effect| {
                        effect.kind == "ask_human" && effect.status == SessionEffectStatus::Started
                    })
            {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Session should reach its human wait");

    let first_output = first_execution
        .await
        .expect("runtime task should join")
        .expect("durable human wait should suspend cleanly");
    assert!(matches!(
        first_output,
        SessionExecution::Suspended(ref suspension)
            if suspension.status == SessionStatus::WaitingForInput
    ));
    store
        .answer_human_request(request.id, json!("yes"))
        .expect("the durable request should remain answerable");

    let output = runtime(Arc::clone(&store), &directory.path().join("runtime"))
        .execute(session.id, CancellationToken::new())
        .await
        .expect("replayed Session should complete");
    assert_eq!(
        output,
        SessionExecution::Completed(json!({"decision": "yes"}))
    );
    assert_eq!(
        store
            .list_agents(session.id)
            .expect("Agents should load")
            .len(),
        1
    );
    assert_eq!(
        store
            .list_human_requests(session.id)
            .expect("human requests should load")
            .len(),
        1
    );
    let effects = store
        .list_session_effects(session.id)
        .expect("effects should load");
    assert_eq!(effects.len(), 3);
    assert_eq!(
        effects
            .iter()
            .map(|effect| effect.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["create_agent", "ask_human", "complete"]
    );
    assert!(
        effects
            .iter()
            .all(|effect| effect.status == SessionEffectStatus::Completed)
    );
}

#[tokio::test]
async fn durable_wait_suspends_the_python_process_and_replays_when_due() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Durable wait", directory.path().join("project"))
        .expect("project should be created");
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: program_with_source("durable-wait-replay", WAIT_SOURCE),
            title: "Durable wait".to_string(),
            request: "Wait without retaining a Python process.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::ModelOnly,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should be running");

    let first = runtime(Arc::clone(&store), &directory.path().join("runtime"))
        .execute(session.id, CancellationToken::new())
        .await
        .expect("durable wait should suspend cleanly");
    let wake_at = match first {
        SessionExecution::Suspended(suspension) => {
            assert_eq!(suspension.status, SessionStatus::WaitingForDeadline);
            suspension
                .wake_at
                .expect("deadline suspension should have a wake time")
        }
        SessionExecution::Completed(output) => {
            panic!("wait completed before suspension: {output}")
        }
    };
    assert!(
        store
            .list_session_effects(session.id)
            .expect("effects should load")
            .iter()
            .any(|effect| effect.kind == "wait" && effect.status == SessionEffectStatus::Started)
    );

    let delay = (wake_at - chrono::Utc::now()).to_std().unwrap_or_default();
    tokio::time::sleep(delay + Duration::from_millis(10)).await;
    store
        .resume_session(session.id)
        .expect("waiting Session should be runnable");
    let output = runtime(Arc::clone(&store), &directory.path().join("runtime"))
        .execute(session.id, CancellationToken::new())
        .await
        .expect("due wait should replay");
    assert_eq!(
        output,
        SessionExecution::Completed(json!({"completed": true}))
    );
}
