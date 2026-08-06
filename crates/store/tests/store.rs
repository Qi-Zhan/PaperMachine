use papermachine_protocol::AgentAccessProfile;
use papermachine_protocol::ArtifactKind;
use papermachine_protocol::Budget;
use papermachine_protocol::BudgetUsage;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::ControlMessageStatus;
use papermachine_protocol::HumanRequestStatus;
use papermachine_protocol::TaskScopeStatus;
use papermachine_protocol::TimerPolicy;
use papermachine_protocol::TimerStatus;
use papermachine_protocol::WorkflowId;
use papermachine_protocol::WorkflowManifest;
use papermachine_protocol::WorkflowRunEventPayload;
use papermachine_protocol::WorkflowRunStatus;
use papermachine_protocol::WorkflowSnapshot;
use papermachine_protocol::WorkflowSource;
use papermachine_store::Store;
use serde_json::json;
use tempfile::tempdir;

fn workflow() -> WorkflowSnapshot {
    WorkflowSnapshot {
        manifest: WorkflowManifest {
            id: WorkflowId::new(),
            slug: "parallel-review".to_string(),
            name: "Parallel review".to_string(),
            version: "0.1.0".to_string(),
            description: "Run independent Sessions and synthesize them.".to_string(),
            entrypoint: "main".to_string(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            default_budget: Budget::default(),
        },
        source: WorkflowSource::Builtin,
        definition_path: "builtin/parallel-review/workflow.py".to_string(),
        sha256: "test-source".to_string(),
        source_code: "async def main(ctx): return {}\n".to_string(),
    }
}

#[test]
fn access_changes_only_between_turns_and_each_turn_keeps_its_snapshot() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path()).expect("store should open");
    let research = store
        .create_research("Access snapshots", "")
        .expect("research should be created");
    let session = store
        .create_session_with_access(
            research.id,
            "Workspace session",
            "",
            "test-model",
            Vec::new(),
            AgentAccessProfile::Workspace,
        )
        .expect("session should be created");
    let first = store
        .create_turn(
            session.id,
            "First",
            "test-model",
            "",
            None,
            4,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .expect("first turn should be created");
    assert_eq!(first.access, AgentAccessProfile::Workspace);
    assert!(
        store
            .set_session_access(session.id, AgentAccessProfile::Research)
            .is_err(),
        "an active Turn must block access changes"
    );

    store.cancel_turn(first.id).expect("first turn should end");
    let updated = store
        .set_session_access(session.id, AgentAccessProfile::Research)
        .expect("access should change between turns");
    assert_eq!(updated.access, AgentAccessProfile::Research);
    assert_eq!(
        store
            .get_turn(first.id)
            .expect("first turn should remain")
            .access,
        AgentAccessProfile::Workspace
    );
    let second = store
        .create_turn(
            session.id,
            "Second",
            "test-model",
            "",
            None,
            4,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .expect("second turn should be created");
    assert_eq!(second.access, AgentAccessProfile::Research);
}

#[test]
fn collaboration_state_is_research_owned_and_events_are_ordered() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path()).expect("store should open");
    let research = store
        .create_research("Paper", "Test research")
        .expect("research should be created");
    let origin = store
        .create_session(research.id, "Claim audit", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let run = store
        .create_workflow_run(origin.id, workflow(), "Research a claim", json!({}), None)
        .expect("run should be created");
    store
        .set_workflow_run_status(run.id, WorkflowRunStatus::Running, None)
        .expect("run should start");

    let researcher = store
        .create_participant(
            run.id,
            "Researcher",
            "Evidence",
            "primary evidence",
            "Preserve provenance.",
            "",
            Vec::new(),
            AgentAccessProfile::Research,
        )
        .expect("researcher should be created");
    let reviewer = store
        .create_participant(
            run.id,
            "Reviewer",
            "Review",
            "critical synthesis",
            "Compare uncertainty.",
            "",
            Vec::new(),
            AgentAccessProfile::ModelOnly,
        )
        .expect("reviewer should be created");
    let team = store
        .create_team(run.id, "Review team", vec![researcher.id, reviewer.id])
        .expect("team should be created");
    store
        .set_relation(
            run.id,
            researcher.id,
            reviewer.id,
            "reports_to",
            "Report evidence and uncertainty.",
        )
        .expect("relation should be created");
    let scope = store
        .create_task_scope(run.id, None, "Evidence", "Gather primary evidence")
        .expect("scope should be created");
    store
        .set_task_scope_status(scope.id, TaskScopeStatus::Completed)
        .expect("scope should complete");
    let timer = store
        .create_timer(run.id, "periodic summary", 10, TimerPolicy::Coalesce)
        .expect("timer should be created");
    store.fire_timer(timer.id).expect("timer should fire");
    let channel = store
        .create_channel(run.id, "findings", json!({"type": "string"}))
        .expect("channel should be created");
    let signal = store
        .publish_signal(channel.id, Some(researcher.id), json!("evidence"))
        .expect("signal should publish without locking the Store recursively");

    assert_eq!(
        store
            .list_sessions(research.id)
            .expect("Sessions should load")
            .len(),
        3
    );
    assert_eq!(
        store
            .list_participants(run.id)
            .expect("participants should load")
            .len(),
        2
    );
    assert_eq!(
        store
            .get_team(team.id)
            .expect("team should load")
            .member_ids
            .len(),
        2
    );
    assert_eq!(
        store
            .list_signals(channel.id, 0)
            .expect("signals should load"),
        vec![signal]
    );

    let events = store
        .list_workflow_run_events(run.id, 0)
        .expect("events should load");
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        (1..=events.len() as u64).collect::<Vec<_>>()
    );
    assert!(events.iter().any(|event| matches!(
        event.payload,
        WorkflowRunEventPayload::ParticipantCreated { .. }
    )));
    assert!(events.iter().any(|event| matches!(
        event.payload,
        WorkflowRunEventPayload::SignalPublished { .. }
    )));
}

#[test]
fn database_reopens_and_artifacts_are_content_addressed() {
    let directory = tempdir().expect("temporary directory should be created");
    let database = directory.path().join("papermachine-v3.db");
    let artifacts = directory.path().join("artifacts-v3");
    let (research_id, workflow_run_id) = {
        let store = Store::open(&database, &artifacts).expect("store should open");
        let research = store
            .create_research("Persistent", "Reopen test")
            .expect("research should be created");
        let session = store
            .create_session(research.id, "Persistence", "", "test-model", Vec::new())
            .expect("Session should be created");
        let run = store
            .create_workflow_run(session.id, workflow(), "Persist", json!({}), None)
            .expect("run should be created");
        let artifact = store
            .create_artifact(
                research.id,
                run.id,
                Some(session.id),
                None,
                ArtifactKind::Report,
                "result.md",
                "text/markdown",
                json!({}),
                b"evidence",
            )
            .expect("artifact should be created");
        assert_eq!(artifact.size_bytes, 8);
        assert_eq!(artifact.sha256.len(), 64);
        assert!(artifacts.join(&artifact.relative_path).is_file());
        (research.id, run.id)
    };

    let reopened = Store::open(&database, &artifacts).expect("store should reopen");
    assert_eq!(
        reopened.list_researches().expect("researches should load")[0].id,
        research_id
    );
    assert_eq!(
        reopened
            .get_workflow_run(workflow_run_id)
            .expect("run should load")
            .id,
        workflow_run_id
    );
    let stored = reopened
        .list_artifacts(workflow_run_id)
        .expect("artifacts should load");
    assert_eq!(stored.len(), 1);
    assert_eq!(
        reopened
            .read_artifact(&stored[0])
            .expect("artifact should read"),
        b"evidence"
    );
}

#[test]
fn terminal_runs_close_pending_human_control_and_timer_state() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path()).expect("store should open");
    let research = store
        .create_research("Terminal cleanup", "")
        .expect("Research should exist");
    let origin = store
        .create_session(research.id, "Origin", "", "test-model", Vec::new())
        .expect("Session should exist");
    let run = store
        .create_workflow_run(origin.id, workflow(), "Finish cleanly", json!({}), None)
        .expect("run should exist");
    store
        .set_workflow_run_status(run.id, WorkflowRunStatus::Running, None)
        .expect("run should start");
    let request = store
        .create_human_request(
            run.id,
            None,
            None,
            origin.id,
            None,
            "Continue?",
            json!({"type": "string"}),
        )
        .expect("human request should open");
    let control = store
        .create_control_message(
            run.id,
            origin.id,
            None,
            ControlMessageKind::Guide,
            "Finish now",
        )
        .expect("control should queue");
    let timer = store
        .create_timer(run.id, "summary", 1000, TimerPolicy::Coalesce)
        .expect("timer should start");

    store
        .complete_workflow_run(run.id, json!({"ok": true}))
        .expect("run should complete");

    assert_eq!(
        store
            .get_human_request(request.id)
            .expect("request should load")
            .status,
        HumanRequestStatus::Cancelled
    );
    assert_eq!(
        store
            .list_control_messages(run.id)
            .expect("controls should load")
            .into_iter()
            .find(|item| item.id == control.id)
            .expect("control should exist")
            .status,
        ControlMessageStatus::Cancelled
    );
    assert_eq!(
        store.get_timer(timer.id).expect("timer should load").status,
        TimerStatus::Completed
    );
    assert!(
        store
            .create_control_message(
                run.id,
                origin.id,
                None,
                ControlMessageKind::Guide,
                "Too late",
            )
            .is_err()
    );
}

#[test]
fn concurrent_budget_updates_do_not_lose_deltas() {
    const WORKERS: u32 = 8;
    const UPDATES_PER_WORKER: u32 = 25;

    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path()).expect("store should open");
    let research = store
        .create_research("Concurrent usage", "")
        .expect("research should be created");
    let origin = store
        .create_session(research.id, "Origin", "", "test-model", Vec::new())
        .expect("session should be created");
    let run = store
        .create_workflow_run(origin.id, workflow(), "Count all actions", json!({}), None)
        .expect("run should be created");

    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            let store = store.clone();
            scope.spawn(move || {
                for _ in 0..UPDATES_PER_WORKER {
                    store
                        .add_budget_usage(
                            run.id,
                            BudgetUsage {
                                actions_started: 1,
                                hosted_search_calls: 1,
                                ..BudgetUsage::default()
                            },
                        )
                        .expect("usage update should succeed");
                }
            });
        }
    });

    assert_eq!(
        store
            .get_workflow_run(run.id)
            .expect("run should load")
            .usage
            .actions_started,
        WORKERS * UPDATES_PER_WORKER
    );
    assert_eq!(
        store
            .get_workflow_run(run.id)
            .expect("run should load")
            .usage
            .hosted_search_calls,
        WORKERS * UPDATES_PER_WORKER
    );
}
