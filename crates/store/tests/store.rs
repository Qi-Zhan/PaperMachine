use papermachine_protocol::AgentAccessProfile;
use papermachine_protocol::ArtifactKind;
use papermachine_protocol::Budget;
use papermachine_protocol::BudgetUsage;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::ControlMessageStatus;
use papermachine_protocol::HumanRequestStatus;
use papermachine_protocol::Project;
use papermachine_protocol::Session;
use papermachine_protocol::TaskScopeStatus;
use papermachine_protocol::TimerPolicy;
use papermachine_protocol::TimerStatus;
use papermachine_protocol::WorkflowEventPayload;
use papermachine_protocol::WorkflowProgram;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_protocol::WorkflowStatus;
use papermachine_store::Store;
use serde_json::json;
use tempfile::TempDir;
use tempfile::tempdir;

#[test]
fn project_creation_initializes_owned_directory_and_rejects_reuse() {
    let directory = tempdir().expect("temporary directory should be created");
    let store =
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open");
    let root = directory.path().join("paper-project");
    let project = store
        .create_project("Paper", "Directory ownership", &root)
        .expect("project should be created");

    assert_eq!(
        project.root_path,
        root.canonicalize()
            .expect("Project root should be canonicalizable")
            .to_string_lossy()
    );
    let metadata = root.join(".papermachine");
    for child in ["prompts", "workflows", "skills", "state"] {
        assert!(metadata.join(child).is_dir(), "missing {child} directory");
    }
    let prompt = store
        .get_project_system_prompt(project.id)
        .expect("Project system prompt should load");
    assert!(prompt.content.is_empty());
    assert_eq!(prompt.relative_path, ".papermachine/prompts/system.md");
    let prompt = store
        .set_project_system_prompt(project.id, "Prefer primary evidence.")
        .expect("Project system prompt should update");
    assert_eq!(prompt.content, "Prefer primary evidence.");
    assert_eq!(prompt.sha256.len(), 64);
    let config = std::fs::read_to_string(metadata.join("project.toml"))
        .expect("project config should be readable");
    assert!(config.contains(&project.id.to_string()));
    assert!(config.contains("name = \"Paper\""));
    assert!(
        store.create_project("Duplicate", "", &root).is_err(),
        "one directory must belong to only one Project"
    );
    assert!(
        store
            .create_project("Relative", "", "relative/path")
            .is_err(),
        "Project roots must be absolute"
    );
}

#[test]
fn project_level_workflow_keeps_program_snapshot_after_program_update() {
    let directory = tempdir().expect("temporary directory should be created");
    let store =
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open");
    let project = project(&store, &directory, "Snapshot", "");
    let mut original = workflow();
    original.project_id = Some(project.id);
    original.source = WorkflowProgramSource::User;
    original.definition_path = ".papermachine/workflows/parallel-review/workflow.py".to_string();
    original.sha256 = "original-sha".to_string();
    original.source_code = "async def main(ctx): return {'revision': 1}\n".to_string();
    store
        .register_workflow_program(&WorkflowProgram {
            project_id: original.project_id,
            manifest: original.manifest.clone(),
            source: original.source,
            definition_path: original.definition_path.clone(),
            sha256: original.sha256.clone(),
            updated_at: chrono::Utc::now(),
        })
        .expect("original program should register");

    let workflow = store
        .create_workflow(
            project.id,
            None,
            original,
            "Summarize the Project",
            "",
            json!({}),
            None,
            "test-model",
            AgentAccessProfile::Research,
            Vec::new(),
        )
        .expect("Project-level Workflow should be created without a Session");

    let mut replacement = workflow.program.clone();
    replacement.manifest.id = WorkflowProgramId::new();
    replacement.sha256 = "replacement-sha".to_string();
    replacement.source_code = "async def main(ctx): return {'revision': 2}\n".to_string();
    store
        .register_workflow_program(&WorkflowProgram {
            project_id: replacement.project_id,
            manifest: replacement.manifest.clone(),
            source: replacement.source,
            definition_path: replacement.definition_path.clone(),
            sha256: replacement.sha256.clone(),
            updated_at: chrono::Utc::now(),
        })
        .expect("replacement program should register");

    let persisted = store
        .get_workflow(workflow.id)
        .expect("Workflow should remain readable");
    assert_eq!(persisted.started_from_session_id, None);
    assert_eq!(persisted.program.sha256, "original-sha");
    assert!(persisted.program.source_code.contains("revision': 1"));
    assert_eq!(
        store
            .list_project_workflows(project.id)
            .expect("Project Workflows should load"),
        vec![persisted]
    );
    let registrations = store
        .list_workflow_programs()
        .expect("programs should load");
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].sha256, "replacement-sha");
}

fn workflow() -> WorkflowProgramSnapshot {
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: WorkflowProgramManifest {
            id: WorkflowProgramId::new(),
            slug: "parallel-review".to_string(),
            name: "Parallel review".to_string(),
            description: "Run independent Sessions and synthesize them.".to_string(),
            entrypoint: "main".to_string(),
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            default_budget: Budget::default(),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: "builtin/parallel-review/workflow.py".to_string(),
        sha256: "test-source".to_string(),
        source_code: "async def main(ctx): return {}\n".to_string(),
    }
}

fn project(store: &Store, directory: &TempDir, name: &str, description: &str) -> Project {
    store
        .create_project(name, description, directory.path().join("project"))
        .expect("project should be created")
}

fn workflow_for_session(
    store: &Store,
    session: &Session,
    objective: &str,
) -> papermachine_protocol::Workflow {
    store
        .create_workflow(
            session.project_id,
            Some(session.id),
            workflow(),
            objective,
            "",
            json!({}),
            None,
            "test-model",
            AgentAccessProfile::Research,
            Vec::new(),
        )
        .expect("workflow should be created")
}

#[test]
fn access_changes_only_between_turns_and_each_turn_keeps_its_snapshot() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path()).expect("store should open");
    let research = project(&store, &directory, "Access snapshots", "");
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
            papermachine_protocol::TurnOrigin::User,
            "First",
            "test-model",
            papermachine_protocol::PromptSnapshot::default(),
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
            papermachine_protocol::TurnOrigin::User,
            "Second",
            "test-model",
            papermachine_protocol::PromptSnapshot::default(),
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
fn session_system_prompt_cannot_change_while_a_turn_is_queued() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path()).expect("store should open");
    let project = project(&store, &directory, "Prompt locking", "");
    let session = store
        .create_session(
            project.id,
            "Prompted session",
            "Original prompt",
            "test-model",
            Vec::new(),
        )
        .expect("session should be created");
    let turn = store
        .create_turn(
            session.id,
            papermachine_protocol::TurnOrigin::User,
            "Question",
            "test-model",
            papermachine_protocol::PromptSnapshot::default(),
            None,
            4,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .expect("Turn should be queued");

    assert!(
        store
            .set_session_system_prompt(session.id, "Changed too early")
            .is_err(),
        "a queued Turn must lock the Session prompt"
    );
    assert_eq!(
        store
            .get_session(session.id)
            .expect("Session should load")
            .system_prompt,
        "Original prompt"
    );

    store.cancel_turn(turn.id).expect("Turn should end");
    let updated = store
        .set_session_system_prompt(session.id, "Changed between Turns")
        .expect("prompt should change after the Turn ends");
    assert_eq!(updated.system_prompt, "Changed between Turns");
}

#[test]
fn collaboration_state_is_research_owned_and_events_are_ordered() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path()).expect("store should open");
    let research = project(&store, &directory, "Paper", "Test research");
    let origin = store
        .create_session(research.id, "Claim audit", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let run = workflow_for_session(&store, &origin, "Research a claim");
    store
        .set_workflow_status(run.id, WorkflowStatus::Running, None)
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
        .list_workflow_events(run.id, 0)
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
        WorkflowEventPayload::ParticipantCreated { .. }
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, WorkflowEventPayload::SignalPublished { .. }))
    );
}

#[test]
fn database_reopens_and_artifacts_are_content_addressed() {
    let directory = tempdir().expect("temporary directory should be created");
    let database = directory.path().join("papermachine.db");
    let artifacts = directory.path().join("artifacts");
    let (project_id, workflow_id) = {
        let store = Store::open(&database, &artifacts).expect("store should open");
        let research = project(&store, &directory, "Persistent", "Reopen test");
        let session = store
            .create_session(research.id, "Persistence", "", "test-model", Vec::new())
            .expect("Session should be created");
        let run = workflow_for_session(&store, &session, "Persist");
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
        reopened.list_projects().expect("projects should load")[0].id,
        project_id
    );
    assert_eq!(
        reopened
            .get_workflow(workflow_id)
            .expect("run should load")
            .id,
        workflow_id
    );
    let stored = reopened
        .list_artifacts(workflow_id)
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
    let research = project(&store, &directory, "Terminal cleanup", "");
    let origin = store
        .create_session(research.id, "Origin", "", "test-model", Vec::new())
        .expect("Session should exist");
    let run = workflow_for_session(&store, &origin, "Finish cleanly");
    store
        .set_workflow_status(run.id, WorkflowStatus::Running, None)
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
        .complete_workflow(run.id, json!({"ok": true}))
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
    let research = project(&store, &directory, "Concurrent usage", "");
    let origin = store
        .create_session(research.id, "Origin", "", "test-model", Vec::new())
        .expect("session should be created");
    let run = workflow_for_session(&store, &origin, "Count all actions");

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
            .get_workflow(run.id)
            .expect("run should load")
            .usage
            .actions_started,
        WORKERS * UPDATES_PER_WORKER
    );
    assert_eq!(
        store
            .get_workflow(run.id)
            .expect("run should load")
            .usage
            .hosted_search_calls,
        WORKERS * UPDATES_PER_WORKER
    );
}
