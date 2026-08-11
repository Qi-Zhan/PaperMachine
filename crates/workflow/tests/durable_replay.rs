use papermachine_model::{ModelClient, ScriptedModelClient};
use papermachine_protocol::{
    AccessPreset, HumanRequestStatus, ModelEvent, SessionEffectStatus, SessionStatus, TokenUsage,
    WorkflowProgramSnapshot, WorkflowProgramSource,
};
use papermachine_session::{TurnRuntime, TurnRuntimeConfig};
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::{NewSession, Store, StoreHandle};
use papermachine_tools::ToolCatalog;
use papermachine_workflow::{
    ActionRunner, SessionExecution, SessionExecutor, WorkflowInterpreter, language::compile_source,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

const SOURCE: &str = r#"
version 1;
schema HumanDecision = string(min_len = 1);
agent Observer {
    access = model_only;
    role = "replay observer";
    system = "";
}
workflow durable_replay {
    slug = "durable-replay";
    name = "Durable replay";
    description = "Exercise replay across an abrupt interpreter loss.";
    request = required;
    params {}
    run(ctx) {
        let observer = Observer(key = "main", name = "Observer");
        let decision = await ask_human(
            question = "Continue after the simulated restart?",
            response = HumanDecision,
            agent = observer,
        );
        return {decision};
    }
}
"#;

const WAIT_SOURCE: &str = r#"
version 1;
workflow durable_wait_replay {
    slug = "durable-wait-replay";
    name = "Durable wait replay";
    description = "Suspend the interpreter until its durable deadline is due.";
    request = required;
    params {}
    run(ctx) {
        await wait(seconds = 0.05, name = "test-wake");
        return {completed: true};
    }
}
"#;

const RUN_ACCESS_SOURCE: &str = r#"
version 1;
agent Conservative {
    access = workspace;
    role = "conservative";
    system = "";
    action inspect(question) { prompt = "Inspect the captured evidence conservatively."; }
}
agent Elevated {
    access = model_only;
    role = "elevated";
    system = "";
    action compare(question) { prompt = "Compare evidence using the configured run access."; }
}
agent Clamped {
    access = full_access;
    role = "clamped";
    system = "";
    action verify(question) { prompt = "Verify that the Session ceiling remains authoritative."; }
}
workflow run_access {
    slug = "run-access";
    name = "Run access";
    description = "Exercise per-Agent access.";
    request = required;
    params {}
    run(ctx) {
        let conservative = Conservative(key = "main", name = "Conservative");
        let elevated = Elevated(key = "main", name = "Elevated");
        let clamped = Clamped(key = "main", name = "Clamped");
        let first = await conservative.inspect(question = ctx.request);
        let second = await elevated.compare(question = ctx.request);
        let third = await clamped.verify(question = ctx.request);
        return {answers: [first, second, third]};
    }
}
"#;

const STRUCTURED_REPAIR_SOURCE: &str = r#"
version 1;
schema Decision = object { message: string, status: enum["complete"] };
agent Decider {
    access = model_only;
    role = "decider";
    system = "";
    action decide(task) {
        tools = [];
        finalize = if_needed;
        result = Decision;
        prompt = "Do the work, report it normally, and submit the decision.";
    }
}
workflow structured_repair {
    slug = "structured-repair";
    name = "Structured repair";
    description = "Exercise bounded structured finalization.";
    request = required;
    params {}
    run(ctx) {
        let decider = Decider(key = "main", name = "Decider");
        return await decider.decide(task = ctx.request);
    }
}
"#;

const PARALLEL_WAIT_SOURCE: &str = r#"
version 1;
agent Worker {
    access = model_only;
    role = "worker";
    system = "";
    action work(task) { tools = []; prompt = "Complete the task once."; }
}
workflow parallel_wait {
    slug = "parallel-wait";
    name = "Parallel wait";
    description = "Exercise partial parallel replay.";
    request = required;
    params {}
    run(ctx) {
        let worker = Worker(key = "main", name = "Worker");
        let results = parallel {
            work => await worker.work(task = ctx.request),
            deadline => {
                await wait(seconds = 0.05, name = "parallel-deadline");
                "deadline fired"
            },
        };
        return results;
    }
}
"#;

const PURE_PARALLEL_SOURCE: &str = r#"
version 1;
workflow pure_parallel {
    slug = "pure-parallel";
    name = "Pure parallel";
    description = "Preserve keyed input order.";
    request = required;
    params {}
    run(ctx) {
        return parallel for value in [3, 1, 2] key value { value * 2 };
    }
}
"#;

const DUPLICATE_PARALLEL_SOURCE: &str = r#"
version 1;
workflow duplicate_parallel {
    slug = "duplicate-parallel";
    name = "Duplicate parallel";
    description = "Reject duplicate dynamic keys.";
    request = required;
    params {}
    run(ctx) {
        return parallel for value in ["same", "same"] key value { value };
    }
}
"#;

fn program_with_source(slug: &str, source_code: &str) -> WorkflowProgramSnapshot {
    let compiled = compile_source(source_code, &BTreeSet::new()).expect("source should compile");
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: compiled.manifest,
        source: WorkflowProgramSource::Builtin,
        definition_path: format!("builtin/{slug}/workflow.pm"),
        sha256: hex::encode(Sha256::digest(source_code.as_bytes())),
        ir_sha256: compiled.ir_sha256,
        source_code: source_code.to_string(),
    }
}

fn runtime_with(
    store: Arc<Store>,
    model: Arc<dyn ModelClient>,
    tools: ToolCatalog,
) -> TestSessionRuntime {
    let store = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    runtime_on_handle(store, model, tools)
}

fn runtime_on_handle(
    store: StoreHandle,
    model: Arc<dyn ModelClient>,
    tools: ToolCatalog,
) -> TestSessionRuntime {
    let known_tools = tools.names().map(str::to_string).collect::<Vec<_>>();
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
    let executor = WorkflowInterpreter::new(store.clone(), known_tools);
    TestSessionRuntime {
        executor,
        actions: ActionRunner::new(store, turns, Default::default()),
    }
}

fn runtime(store: Arc<Store>) -> TestSessionRuntime {
    runtime_with(
        store,
        Arc::new(ScriptedModelClient::default()),
        ToolCatalog::default(),
    )
}

struct TestSessionRuntime {
    executor: WorkflowInterpreter,
    actions: ActionRunner,
}

#[async_trait::async_trait]
impl SessionExecutor for TestSessionRuntime {
    async fn execute(
        &self,
        session_id: papermachine_protocol::SessionId,
        cancellation: CancellationToken,
    ) -> Result<SessionExecution, String> {
        let action_cancellation = cancellation.child_token();
        let action_task = tokio::spawn({
            let runner = self.actions.clone();
            let action_cancellation = action_cancellation.clone();
            async move { runner.run_session(session_id, action_cancellation).await }
        });
        let result = self.executor.execute(session_id, cancellation).await;
        action_cancellation.cancel();
        action_task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        result
    }
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
async fn workflow_runtime_fails_closed_when_the_ir_snapshot_differs() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("IR mismatch", directory.path().join("project"))
        .expect("Project should be created");
    let mut program = program_with_source("ir-mismatch", WAIT_SOURCE);
    program.ir_sha256 = "0".repeat(64);
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program,
            title: "IR mismatch".to_string(),
            request: "Do not run with a different IR.".to_string(),
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

    let error = runtime(Arc::clone(&store))
        .execute(session.id, CancellationToken::new())
        .await
        .expect_err("IR mismatch must fail before effects start");
    assert!(error.contains("canonical Workflow IR differs"));
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

    let execution = runtime_with(Arc::clone(&store), Arc::new(model), ToolCatalog::default())
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
            .is_empty()
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
            access: AccessPreset::Workspace,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should be running");

    let first_runtime = runtime(Arc::clone(&store));
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
    store
        .resume_session(session.id)
        .expect("waiting Session should resume");

    let output = runtime(Arc::clone(&store))
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
    assert_eq!(effects.len(), 2);
    assert_eq!(
        effects
            .iter()
            .map(|effect| effect.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["create_agent", "ask_human"]
    );
    assert!(
        effects
            .iter()
            .all(|effect| effect.status == SessionEffectStatus::Completed)
    );
}

#[tokio::test]
async fn durable_wait_suspends_and_replays_when_due() {
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
            request: "Wait durably.".to_string(),
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

    let first = runtime(Arc::clone(&store))
        .execute(session.id, CancellationToken::new())
        .await
        .expect("durable wait should suspend cleanly");
    let wake_at = match first {
        SessionExecution::Suspended(suspension) => {
            assert_eq!(suspension.status, SessionStatus::WaitingForDeadline);
            suspension.wake_at.expect("deadline should have wake time")
        }
        SessionExecution::Completed(output) => panic!("wait completed before suspension: {output}"),
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
    let output = runtime(Arc::clone(&store))
        .execute(session.id, CancellationToken::new())
        .await
        .expect("due wait should replay");
    assert_eq!(
        output,
        SessionExecution::Completed(json!({"completed": true}))
    );
}

#[tokio::test]
async fn builtin_goal_reuses_one_agent_until_active_becomes_complete() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Goal", directory.path().join("project"))
        .expect("project should be created");
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../workflows/builtin/goal/workflow.pm"),
    )
    .expect("Goal source should load");
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: program_with_source("goal", &source),
            title: "Goal".to_string(),
            request: "Create and verify the requested result.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({
                "session_title": "Goal worker",
                "agent_model": "",
                "agent_access": "workspace"
            }),
            default_model: "scripted".to_string(),
            access: AccessPreset::Workspace,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should start");
    let model = ScriptedModelClient::new([
        completed_response(
            "Initialized the result and inspected the files.\n\n{\"message\":\"Initialization complete; verification remains.\",\"status\":\"active\"}",
            30,
            12,
        ),
        completed_response(
            "Reopened the result and verified every requested outcome.\n\n{\"message\":\"The full objective is verified.\",\"status\":\"complete\"}",
            30,
            12,
        ),
    ]);

    let execution = runtime_with(Arc::clone(&store), Arc::new(model), ToolCatalog::default())
        .execute(session.id, CancellationToken::new())
        .await
        .expect("Goal should execute");
    assert_eq!(
        execution,
        SessionExecution::Completed(json!({
            "result": "The full objective is verified.",
            "status": "complete",
            "iterations": 2
        }))
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
            .list_action_invocations(session.id)
            .expect("Actions should load")
            .len(),
        2
    );
}

#[tokio::test]
async fn structured_action_uses_one_finalizer_and_at_most_two_repairs() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Repair", directory.path().join("project"))
        .expect("project should be created");
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: program_with_source("structured-repair", STRUCTURED_REPAIR_SOURCE),
            title: "Repair".to_string(),
            request: "Reach a typed result.".to_string(),
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
        .expect("Session should start");
    let model = ScriptedModelClient::new([
        completed_response("Work is done, but the trailer is malformed.", 10, 5),
        completed_response("still not json", 8, 3),
        completed_response("{\"message\":1}", 8, 3),
        completed_response(
            "```json\n{\"message\":\"Verified result\",\"status\":\"complete\"}\n```",
            8,
            5,
        ),
    ]);

    let execution = runtime_with(Arc::clone(&store), Arc::new(model), ToolCatalog::default())
        .execute(session.id, CancellationToken::new())
        .await
        .expect("structured Action should recover");
    assert_eq!(
        execution,
        SessionExecution::Completed(json!({
            "message": "Verified result",
            "status": "complete"
        }))
    );
    let actions = store
        .list_action_invocations(session.id)
        .expect("Actions should load");
    assert_eq!(
        actions
            .iter()
            .map(|action| action.action_name.as_str())
            .collect::<Vec<_>>(),
        vec![
            "decide",
            "decide_finalize",
            "decide_json_repair",
            "decide_json_repair"
        ]
    );
    assert!(
        actions[1..]
            .iter()
            .all(|action| action.tool_policy.as_deref() == Some(&[]))
    );
    assert!(
        actions[2..]
            .iter()
            .all(|action| action.reasoning_effort
                == Some(papermachine_protocol::ReasoningEffort::Low))
    );
}

#[tokio::test]
async fn builtin_evidence_loop_runs_effectful_helpers_and_keyed_parallel_routes() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Evidence", directory.path().join("project"))
        .expect("project should be created");
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../workflows/builtin/evidence-loop/workflow.pm"),
    )
    .expect("Evidence source should load");
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: program_with_source("evidence-loop", &source),
            title: "Evidence".to_string(),
            request: "Compare the claim from two independent routes.".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({
                "route_count": 2,
                "max_rounds": 2,
                "max_followups_per_round": 2,
                "max_draft_revisions": 0,
                "extra_requirements": []
            }),
            default_model: "scripted".to_string(),
            access: AccessPreset::ModelOnly,
            enabled_skills: Vec::new(),
            agent_access_overrides: Default::default(),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should start");
    let model = ScriptedModelClient::new([
        completed_response(
            r#"{"deliverable":"comparison","acceptance_criteria":["two routes"],"routes":[{"key":"primary","name":"Primary","objective":"Find primary support"},{"key":"challenge","name":"Challenge","objective":"Find counterevidence"}],"verification_notes":["cross-check"]}"#,
            20,
            20,
        ),
        completed_response("Primary evidence report", 20, 8),
        completed_response("Counterevidence report", 20, 8),
        completed_response(
            r#"{"complete":false,"rationale":"A new route is requested.","supported_conclusions":[],"unresolved_gaps":["new route"],"contradictions":[],"follow_ups":[{"route_key":"invented","objective":"Start another route"}]}"#,
            20,
            15,
        ),
        completed_response(
            r#"{"complete":false,"rationale":"Primary evidence needs one check.","supported_conclusions":[],"unresolved_gaps":["primary check"],"contradictions":[],"follow_ups":[{"route_key":"primary","objective":"Verify the primary boundary"}]}"#,
            20,
            15,
        ),
        completed_response("Primary follow-up evidence report", 20, 8),
        completed_response(
            r#"{"complete":true,"rationale":"Both routes and the follow-up are covered.","supported_conclusions":["bounded conclusion"],"unresolved_gaps":[],"contradictions":[],"follow_ups":[]}"#,
            20,
            15,
        ),
        completed_response("Final evidence-grounded report", 20, 8),
        completed_response(
            r#"{"complete":true,"feedback":"The draft is supported."}"#,
            10,
            6,
        ),
    ]);

    let execution_result =
        runtime_with(Arc::clone(&store), Arc::new(model), ToolCatalog::default())
            .execute(session.id, CancellationToken::new())
            .await;
    let execution = execution_result.unwrap_or_else(|error| {
        let actions = store
            .list_action_invocations(session.id)
            .expect("failed Actions should load");
        panic!("evidence loop should execute: {error}; actions={actions:#?}")
    });
    let SessionExecution::Completed(output) = execution else {
        panic!("evidence loop should complete")
    };
    assert_eq!(output["report"], "Final evidence-grounded report");
    assert_eq!(output["completion"]["status"], "passed");
    assert_eq!(output["evidence_ledger"].as_array().map(Vec::len), Some(3));
    assert_eq!(output["rounds"], 2);
    assert_eq!(output["route_sessions_reused"], true);
    assert_eq!(
        store
            .list_agents(session.id)
            .expect("Agents should load")
            .len(),
        5
    );
    let route_agents = store
        .list_agents(session.id)
        .expect("Agents should load")
        .into_iter()
        .filter(|agent| agent.class_name == "Researcher")
        .collect::<Vec<_>>();
    assert_eq!(route_agents.len(), 2);
    assert_ne!(route_agents[0].id, route_agents[1].id);
    assert_eq!(
        store
            .list_action_invocations(session.id)
            .expect("Actions should load")
            .iter()
            .filter(|action| action.action_name == "research")
            .count(),
        3
    );
}

#[tokio::test]
async fn parallel_partial_completion_replays_without_duplicate_action() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Parallel replay", directory.path().join("project"))
        .expect("project should be created");
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: program_with_source("parallel-wait", PARALLEL_WAIT_SOURCE),
            title: "Parallel replay".to_string(),
            request: "Run once.".to_string(),
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
        .expect("Session should start");
    let model = ScriptedModelClient::new([completed_response("one action result", 10, 4)]);
    let runtime = runtime_with(Arc::clone(&store), Arc::new(model), ToolCatalog::default());

    let first = runtime
        .execute(session.id, CancellationToken::new())
        .await
        .expect("parallel wait should suspend");
    let SessionExecution::Suspended(suspension) = first else {
        panic!("parallel wait should suspend")
    };
    let wake_at = suspension.wake_at.expect("wait should have deadline");
    assert_eq!(
        store
            .list_action_invocations(session.id)
            .expect("Actions should load")
            .len(),
        1
    );

    let delay = (wake_at - chrono::Utc::now()).to_std().unwrap_or_default();
    tokio::time::sleep(delay + Duration::from_millis(10)).await;
    store
        .resume_session(session.id)
        .expect("Session should resume");
    let second = runtime
        .execute(session.id, CancellationToken::new())
        .await
        .expect("parallel replay should complete");
    assert_eq!(
        second,
        SessionExecution::Completed(json!({
            "deadline": "deadline fired",
            "work": "one action result"
        }))
    );
    assert_eq!(
        store
            .list_action_invocations(session.id)
            .expect("Actions should load")
            .len(),
        1
    );
}

#[tokio::test]
async fn parallel_for_preserves_order_and_rejects_duplicate_keys() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("artifacts"))
            .expect("store should open in memory"),
    );
    let project = store
        .create_project("Pure parallel", directory.path().join("project"))
        .expect("project should be created");
    let make_session = |source: &str, slug: &str| {
        store
            .create_session(NewSession {
                project_id: project.id,
                program: program_with_source(slug, source),
                title: slug.to_string(),
                request: "Run.".to_string(),
                instructions: String::new(),
                trigger: Default::default(),
                params: json!({}),
                default_model: "scripted".to_string(),
                access: AccessPreset::ModelOnly,
                enabled_skills: Vec::new(),
                agent_access_overrides: Default::default(),
            })
            .expect("Session should be created")
    };
    let ordered = make_session(PURE_PARALLEL_SOURCE, "pure-parallel");
    store
        .start_session(ordered.id)
        .expect("ordered Session should start");
    assert_eq!(
        runtime(Arc::clone(&store))
            .execute(ordered.id, CancellationToken::new())
            .await
            .expect("ordered parallel should run"),
        SessionExecution::Completed(json!([6, 2, 4]))
    );

    let duplicate = make_session(DUPLICATE_PARALLEL_SOURCE, "duplicate-parallel");
    store
        .start_session(duplicate.id)
        .expect("duplicate Session should start");
    let error = runtime(Arc::clone(&store))
        .execute(duplicate.id, CancellationToken::new())
        .await
        .expect_err("duplicate key must fail");
    assert!(error.contains("parallel for key is not unique"));
    assert!(
        store
            .list_session_effects(duplicate.id)
            .expect("effects should load")
            .is_empty()
    );
}
