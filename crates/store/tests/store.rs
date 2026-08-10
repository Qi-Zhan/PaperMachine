use papermachine_protocol::AccessPreset;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::ActionSource;
use papermachine_protocol::ActionStatus;
use papermachine_protocol::AgentInputKind;
use papermachine_protocol::AgentInputSource;
use papermachine_protocol::AgentInputStatus;
use papermachine_protocol::ArtifactId;
use papermachine_protocol::ArtifactKind;
use papermachine_protocol::HumanRequestId;
use papermachine_protocol::HumanRequestStatus;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelContextMutation;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::Project;
use papermachine_protocol::PromptSnapshot;
use papermachine_protocol::SessionEffectStatus;
use papermachine_protocol::SessionEventPayload;
use papermachine_protocol::SessionStatus;
use papermachine_protocol::SessionTrigger;
use papermachine_protocol::SessionTriggerKind;
use papermachine_protocol::SessionUsage;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::ToolSetSnapshot;
use papermachine_store::NewActionInvocation;
use papermachine_store::NewSession;
use papermachine_store::Store;
use papermachine_store::TurnContextCheckpoint;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Barrier;
use tempfile::TempDir;
use tempfile::tempdir;

mod support;
use support::ActionHarness;
use support::create_root_session;
use support::model_route;
use support::workflow_snapshot;

fn project(store: &Store, directory: &TempDir, name: &str) -> Project {
    store
        .create_project(name, directory.path().join(format!("{name}-workspace")))
        .expect("Project should be created")
}

fn empty_tool_set() -> ToolSetSnapshot {
    ToolSetSnapshot::materialize(Vec::new()).expect("empty ToolSet should be valid")
}

#[test]
fn project_creation_separates_managed_state_from_workspace() {
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let store = Store::open_in_memory(&managed).expect("store should open");
    let workspace = directory.path().join("workspace");
    let project = store
        .create_project("Paper", &workspace)
        .expect("Project should be created");

    assert_eq!(
        project.workspace.path,
        workspace
            .canonicalize()
            .expect("Workspace should canonicalize")
            .to_string_lossy()
    );
    assert!(
        std::fs::read_dir(&workspace)
            .expect("Workspace should list")
            .next()
            .is_none(),
        "PaperMachine state must not be written into the Workspace"
    );
    for child in [
        "prompts", "rollouts", "sessions", "skills", "state", "runtime",
    ] {
        assert!(managed.join(child).is_dir(), "missing managed {child}");
    }
    assert!(store.create_project("Duplicate", &workspace).is_err());
    assert!(store.create_project("Relative", "relative/path").is_err());
    assert!(
        store
            .create_project("Internal", managed.join("workspace"))
            .expect_err("Workspace cannot overlap managed state")
            .to_string()
            .contains("must be separate")
    );
}

#[test]
fn session_is_the_workflow_instance_and_owns_multiple_agents() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "unified");
    let origin = create_root_session(&store, project.id, "Origin", AccessPreset::Workspace);

    let invalid_trigger = store.create_session(NewSession {
        project_id: project.id,
        program: workflow_snapshot(),
        title: "Missing provenance".to_string(),
        request: "test".to_string(),
        instructions: String::new(),
        trigger: SessionTrigger {
            kind: SessionTriggerKind::User,
            source_session_id: None,
        },
        params: json!({}),
        default_model: "test-model".to_string(),
        access: AccessPreset::Workspace,
        enabled_skills: Vec::new(),
        agent_access_overrides: BTreeMap::new(),
    });
    assert!(
        invalid_trigger.is_err(),
        "Session trigger provenance must be complete"
    );

    let denied = store.create_session(NewSession {
        project_id: project.id,
        program: workflow_snapshot(),
        title: "Too broad".to_string(),
        request: "test".to_string(),
        instructions: String::new(),
        trigger: SessionTrigger {
            kind: SessionTriggerKind::User,
            source_session_id: Some(origin.id),
        },
        params: json!({}),
        default_model: "test-model".to_string(),
        access: AccessPreset::FullAccess,
        enabled_skills: Vec::new(),
        agent_access_overrides: BTreeMap::new(),
    });
    assert!(
        denied
            .expect_err("child Session cannot exceed its source")
            .to_string()
            .contains("exceeds starting Session")
    );

    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: workflow_snapshot(),
            title: "Review".to_string(),
            request: "Review evidence".to_string(),
            instructions: "Prefer primary evidence.".to_string(),
            trigger: SessionTrigger {
                kind: SessionTriggerKind::User,
                source_session_id: Some(origin.id),
            },
            params: json!({"rounds": 2}),
            default_model: "test-model".to_string(),
            access: AccessPreset::Workspace,
            enabled_skills: vec!["citations".to_string()],
            agent_access_overrides: BTreeMap::new(),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should start");
    let planner = store
        .create_agent(
            session.id,
            "Planner",
            "Planner",
            "plan",
            "Plan carefully.",
            "",
            Vec::new(),
            AccessPreset::ReadOnly,
        )
        .expect("Planner should be created");
    let researcher = store
        .create_agent(
            session.id,
            "Researcher",
            "Researcher",
            "research",
            "Gather evidence.",
            "research-model",
            Vec::new(),
            AccessPreset::Workspace,
        )
        .expect("Researcher should be created");

    assert_eq!(planner.session_id, session.id);
    assert_eq!(researcher.session_id, session.id);
    assert_ne!(planner.id, researcher.id);
    assert_eq!(planner.model, "test-model");
    assert_eq!(researcher.model, "research-model");
    assert_eq!(researcher.access, AccessPreset::Workspace);
    assert_eq!(
        store
            .list_agents(session.id)
            .expect("Agents should list")
            .len(),
        2
    );
    assert_eq!(
        store
            .get_session(session.id)
            .expect("Session should load")
            .program,
        session.program
    );
}

#[test]
fn session_owned_records_reject_cross_session_agents() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "ownership");
    let first = create_root_session(&store, project.id, "First", AccessPreset::Workspace);
    let second = create_root_session(&store, project.id, "Second", AccessPreset::Workspace);
    let first_agent = store
        .create_agent(
            first.id,
            "FirstAgent",
            "First",
            "first",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::Workspace,
        )
        .expect("first Agent should be created");
    let second_agent = store
        .create_agent(
            second.id,
            "SecondAgent",
            "Second",
            "second",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::Workspace,
        )
        .expect("second Agent should be created");

    assert!(
        store
            .create_human_request_with_id(
                HumanRequestId::new(),
                first.id,
                second_agent.id,
                "Cross-session request",
                json!({"type": "string"}),
            )
            .is_err(),
        "HumanRequest must use an Agent owned by its Session"
    );
    assert!(
        store
            .create_artifact(
                project.id,
                first.id,
                Some(second_agent.id),
                None,
                ArtifactKind::Report,
                "cross-session.md",
                "text/markdown",
                json!({}),
                b"forbidden",
            )
            .is_err(),
        "Artifact must use an Agent owned by its Session"
    );
    store
        .create_artifact(
            project.id,
            first.id,
            Some(first_agent.id),
            None,
            ArtifactKind::Report,
            "owned.md",
            "text/markdown",
            json!({}),
            b"allowed",
        )
        .expect("matching Session ownership should be accepted");
}

#[test]
fn turn_creation_requires_workspace_and_pins_agent_access() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "turns");
    let origin = create_root_session(&store, project.id, "Origin", AccessPreset::Workspace);
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Workspace);
    let first = harness
        .create_turn(&store, "First", AccessPreset::Workspace)
        .expect("Turn should be created");
    assert_eq!(first.agent_id, harness.agent.id);
    assert_eq!(
        first.environment.authorization.preset,
        AccessPreset::Workspace
    );
    assert!(
        store
            .set_agent_access(harness.agent.id, AccessPreset::Workspace)
            .is_err()
    );

    store.cancel_turn(first.id).expect("Turn should cancel");
    store
        .set_agent_access(harness.agent.id, AccessPreset::Workspace)
        .expect("access may change between Turns");
    let second = harness
        .create_turn(&store, "Second", AccessPreset::Workspace)
        .expect("second Turn should be created");
    assert_eq!(
        second.environment.authorization.preset,
        AccessPreset::Workspace
    );
    assert_eq!(
        store
            .get_turn(first.id)
            .expect("first Turn should load")
            .environment
            .authorization
            .preset,
        AccessPreset::Workspace
    );
    store.cancel_turn(second.id).expect("Turn should cancel");

    let workspace = std::path::PathBuf::from(&project.workspace.path);
    std::fs::remove_dir(&workspace).expect("empty Workspace should be removable");
    assert!(
        harness
            .create_turn(&store, "Detached", AccessPreset::Workspace)
            .expect_err("unavailable Workspace must stop Turn creation")
            .to_string()
            .contains("Workspace is unavailable")
    );
}

#[test]
fn archived_session_is_history_not_an_execution_status() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "archive");
    let session = create_root_session(&store, project.id, "Done", AccessPreset::ReadOnly);
    store
        .complete_session(session.id, json!({"answer": 42}))
        .expect("Session should complete");
    let archived = store
        .archive_session(session.id)
        .expect("Session should archive");

    assert_eq!(archived.status, SessionStatus::Completed);
    assert!(archived.archived_at.is_some());
    assert!(
        store
            .list_sessions(project.id)
            .expect("active Sessions should list")
            .is_empty()
    );
    assert_eq!(
        store
            .list_project_sessions(project.id)
            .expect("all Sessions should list")
            .len(),
        1
    );
    let events = store
        .list_session_events(session.id, 0)
        .expect("Session events should list");
    assert!(matches!(
        events.last().map(|event| &event.payload),
        Some(SessionEventPayload::SessionChanged {
            status: SessionStatus::Completed,
            reason: Some(reason),
        }) if reason == "archived"
    ));
}

#[test]
fn human_answer_is_the_only_valid_input_for_its_action() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "human");
    let session = create_root_session(&store, project.id, "Interactive", AccessPreset::Workspace);
    let agent = store
        .create_agent(
            session.id,
            "InteractiveAgent",
            "Assistant",
            "interactive",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::Workspace,
        )
        .expect("Agent should be created");
    let request = store
        .create_human_request_with_id(
            HumanRequestId::new(),
            session.id,
            agent.id,
            "Next message",
            json!({"type": "string"}),
        )
        .expect("HumanRequest should open");
    assert_eq!(
        store
            .get_session(session.id)
            .expect("Session should load")
            .status,
        SessionStatus::WaitingForInput
    );
    store
        .answer_human_request(request.id, json!("Inspect the cache."))
        .expect("HumanRequest should be answered");
    assert_eq!(
        store
            .get_session(session.id)
            .expect("Session should load")
            .status,
        SessionStatus::Running
    );

    let invocation = store
        .create_action_invocation_with_id(
            ActionInvocationId::new(),
            NewActionInvocation {
                session_id: session.id,
                agent_id: agent.id,
                action_name: "respond".to_string(),
                contract: "Respond to the human".to_string(),
                arguments: json!({"message": "Inspect the cache."}),
                input: "Inspect the cache.".to_string(),
                source: ActionSource::HumanRequest {
                    request_id: request.id,
                },
                tool_policy: Some(Vec::new()),
                web_search_context_size: None,
                reasoning_effort: None,
                response_format: None,
            },
        )
        .expect("Action should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("ActionAttempt should start");
    let turn = store
        .create_turn_for_attempt(
            attempt.id,
            agent.id,
            "Inspect the cache.",
            model_route("test-model"),
            PromptSnapshot::default(),
            AccessPreset::Workspace,
            empty_tool_set(),
            None,
            None,
            Vec::new(),
        )
        .expect("exact answer should become the Turn input");
    store.cancel_turn(turn.id).expect("first Turn should end");

    let forged = store
        .create_action_invocation_with_id(
            ActionInvocationId::new(),
            NewActionInvocation {
                session_id: session.id,
                agent_id: agent.id,
                action_name: "respond".to_string(),
                contract: "Respond to the human".to_string(),
                arguments: json!({"message": "Different"}),
                input: "Different".to_string(),
                source: ActionSource::HumanRequest {
                    request_id: request.id,
                },
                tool_policy: Some(Vec::new()),
                web_search_context_size: None,
                reasoning_effort: None,
                response_format: None,
            },
        )
        .expect("Action should be recorded before provenance validation");
    let forged_attempt = store
        .start_action_attempt(forged.id)
        .expect("forged ActionAttempt should start before provenance validation");
    assert!(
        store
            .create_turn_for_attempt(
                forged_attempt.id,
                agent.id,
                "Different",
                model_route("test-model"),
                PromptSnapshot::default(),
                AccessPreset::Workspace,
                empty_tool_set(),
                None,
                None,
                Vec::new(),
            )
            .is_err()
    );
}

#[test]
fn session_effect_replay_requires_the_identical_request() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "effects");
    let session = create_root_session(&store, project.id, "Effects", AccessPreset::Workspace);
    let payload = json!({"name": "Researcher"});
    let started = store
        .begin_session_effect(
            session.id,
            "root/create-agent",
            "create_agent",
            payload.clone(),
        )
        .expect("effect should begin");
    assert_eq!(started.status, SessionEffectStatus::Started);
    let completed = store
        .finish_session_effect(session.id, &started.key, Ok(json!({"agent": "stable"})))
        .expect("effect should finish");
    assert_eq!(completed.status, SessionEffectStatus::Completed);
    assert_eq!(
        store
            .begin_session_effect(session.id, &started.key, "create_agent", payload)
            .expect("identical effect should replay"),
        completed
    );
    assert!(
        store
            .begin_session_effect(
                session.id,
                &started.key,
                "create_agent",
                json!({"name": "Different"}),
            )
            .is_err()
    );
}

#[test]
fn concurrent_action_start_admits_one_attempt_and_counts_completion_once() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("store should open"),
    );
    let project = project(&store, &directory, "action-cas");
    let origin = create_root_session(&store, project.id, "Origin", AccessPreset::Workspace);
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Workspace);
    let invocation = store
        .create_action_invocation(NewActionInvocation {
            session_id: harness.session.id,
            agent_id: harness.agent.id,
            action_name: "once".to_string(),
            contract: "Start once".to_string(),
            arguments: json!({}),
            input: "{}".to_string(),
            source: ActionSource::Workflow,
            tool_policy: Some(Vec::new()),
            web_search_context_size: None,
            reasoning_effort: None,
            response_format: None,
        })
        .expect("Action should be created");
    let barrier = Arc::new(Barrier::new(3));
    let workers = (0..2)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store.start_action_attempt(invocation.id)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker should join"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let attempt = results
        .iter()
        .find_map(|result| result.as_ref().ok())
        .expect("one Attempt should start");
    store
        .finish_action(
            invocation.id,
            attempt.id,
            ActionStatus::Completed,
            Some(json!({"answer": 1})),
            None,
        )
        .expect("Action should finish");
    store
        .finish_action(
            invocation.id,
            attempt.id,
            ActionStatus::Completed,
            Some(json!({"answer": 1})),
            None,
        )
        .expect("identical completion should be idempotent");
    assert_eq!(
        store
            .get_session(harness.session.id)
            .expect("Session should load")
            .usage
            .actions_completed,
        1
    );
}

#[test]
fn concurrent_human_answers_use_one_open_request_cas() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("store should open"),
    );
    let project = project(&store, &directory, "human-cas");
    let origin = create_root_session(&store, project.id, "Origin", AccessPreset::Workspace);
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Workspace);
    let request = store
        .create_human_request_with_id(
            HumanRequestId::new(),
            harness.session.id,
            harness.agent.id,
            "Choose once",
            json!({"type": "string"}),
        )
        .expect("HumanRequest should open");
    let barrier = Arc::new(Barrier::new(3));
    let workers = ["first", "second"]
        .into_iter()
        .map(|answer| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store.answer_human_request(request.id, json!(answer))
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker should join"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(
        store
            .get_human_request(request.id)
            .expect("HumanRequest should load")
            .status,
        HumanRequestStatus::Answered
    );
}

#[test]
fn project_changes_page_current_entities_and_chunk_text_artifacts() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "changes");
    let source = create_root_session(&store, project.id, "Evidence", AccessPreset::Workspace);
    let source_agent = store
        .create_agent(
            source.id,
            "Researcher",
            "Researcher",
            "collect evidence",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::Workspace,
        )
        .expect("source Agent should be created");
    let summary = create_root_session(&store, project.id, "Summary", AccessPreset::ModelOnly);
    let action_harness = ActionHarness::create(&store, &source, AccessPreset::Workspace);
    let action_turn = action_harness
        .create_action_turn(&store, "Trace this result", AccessPreset::Workspace)
        .expect("provenance Turn should be created");
    let text = "evidence\n".repeat(160_000);
    let text_artifact = store
        .create_artifact(
            project.id,
            source.id,
            Some(source_agent.id),
            None,
            ArtifactKind::Report,
            "evidence.txt",
            "text/plain",
            json!({"source": "experiment"}),
            text.as_bytes(),
        )
        .expect("text Artifact should be created");
    let binary_artifact = store
        .create_artifact(
            project.id,
            source.id,
            Some(source_agent.id),
            None,
            ArtifactKind::Dataset,
            "samples.bin",
            "application/octet-stream",
            json!({}),
            &[0, 1, 2, 3],
        )
        .expect("binary Artifact should be created");

    let mut cursor = None;
    let mut seen = Vec::new();
    loop {
        let page = store
            .project_snapshot_changes(project.id, summary.id, cursor.as_deref())
            .expect("Project changes should page");
        assert!(serde_json::to_vec(&page).expect("page should encode").len() <= 1024 * 1024);
        cursor = Some(page.cursor);
        seen.extend(page.resources);
        if !page.has_more {
            break;
        }
    }

    assert!(seen.iter().any(|resource| resource.kind == "project"));
    assert!(
        seen.iter()
            .any(|resource| { resource.kind == "session" && resource.id == source.id.to_string() })
    );
    assert!(
        !seen.iter().any(|resource| {
            resource.kind == "session" && resource.id == summary.id.to_string()
        })
    );
    assert_eq!(
        seen.iter()
            .filter(|resource| {
                resource.kind == "session" && resource.id == source.id.to_string()
            })
            .count(),
        1
    );
    let turn = seen
        .iter()
        .find(|resource| resource.id == action_turn.turn.id.to_string())
        .expect("Turn snapshot should be present");
    assert_eq!(
        turn.data["action"]["id"],
        action_turn.invocation.id.to_string()
    );
    let reconstructed = seen
        .iter()
        .filter(|resource| resource.id == text_artifact.id.to_string())
        .filter_map(|resource| resource.data["content"].as_str())
        .collect::<String>();
    assert_eq!(reconstructed, text);
    let binary = seen
        .iter()
        .find(|resource| resource.id == binary_artifact.id.to_string())
        .expect("binary Artifact metadata should be present");
    assert!(binary.data["content"].is_null());

    let incremental = store
        .project_snapshot_changes(project.id, summary.id, cursor.as_deref())
        .expect("stable cursor should resume");
    assert!(!incremental.changed);
    assert!(!incremental.has_more);
    assert!(incremental.resources.is_empty());
    assert!(
        store
            .project_snapshot_changes(project.id, summary.id, Some("not-a-cursor"))
            .is_err()
    );
}

#[test]
fn agent_input_claim_is_agent_scoped_and_applied_by_checkpoint() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "agent-inputs");
    let origin = create_root_session(&store, project.id, "Origin", AccessPreset::Workspace);
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Workspace);
    let created = harness
        .create_action_turn(&store, "Work", AccessPreset::Workspace)
        .expect("Action Turn should be created");
    store
        .start_turn(created.turn.id)
        .expect("Turn should start");
    let input = store
        .create_agent_input(
            harness.session.id,
            harness.agent.id,
            Some(created.invocation.id),
            AgentInputSource::Human,
            AgentInputKind::Guide,
            "Check the evidence",
        )
        .expect("Agent input should queue");
    let first = store
        .claim_agent_inputs(
            harness.session.id,
            harness.agent.id,
            Some(created.invocation.id),
            created.turn.id,
        )
        .expect("Agent input should claim");
    let recovered = store
        .claim_agent_inputs(
            harness.session.id,
            harness.agent.id,
            Some(created.invocation.id),
            created.turn.id,
        )
        .expect("same Turn should recover its input claim");
    assert_eq!(first, recovered);
    assert_eq!(first[0].status, AgentInputStatus::Claimed);

    store
        .checkpoint_turn_context(
            created.turn.id,
            TurnContextCheckpoint {
                mutation: ModelContextMutation::Append {
                    items: vec![ModelInputItem::Message {
                        role: MessageRole::User,
                        content: "Check the evidence".to_string(),
                    }],
                },
                usage: TokenUsage::default(),
                completed_model_steps: 0,
                hosted_search_calls_used: 0,
                checkpoint_message: None,
                acknowledged_agent_input_ids: vec![input.id],
            },
        )
        .expect("checkpoint should acknowledge Agent input");
    let inputs = store
        .list_agent_inputs(harness.session.id)
        .expect("Agent inputs should list");
    assert_eq!(
        inputs
            .first()
            .expect("the acknowledged Agent input should remain visible")
            .status,
        AgentInputStatus::Applied
    );
}

#[test]
fn project_home_is_owned_by_exact_session_action_and_agent() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "home");
    let session = create_root_session(&store, project.id, "Summary", AccessPreset::ModelOnly);
    let agent = store
        .create_agent(
            session.id,
            "SummaryAgent",
            "Summary",
            "summary",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::ModelOnly,
        )
        .expect("Summary Agent should be created");
    let other = store
        .create_agent(
            session.id,
            "OtherAgent",
            "Other",
            "other",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::ModelOnly,
        )
        .expect("second Agent should be created");
    let action = store
        .create_action_invocation(NewActionInvocation {
            session_id: session.id,
            agent_id: agent.id,
            action_name: "maintain_project_home".to_string(),
            contract: "Maintain the Project home".to_string(),
            arguments: json!({}),
            input: "{}".to_string(),
            source: ActionSource::Workflow,
            tool_policy: Some(Vec::new()),
            web_search_context_size: None,
            reasoning_effort: None,
            response_format: None,
        })
        .expect("Action should be created");
    let html = "<section><h2>Verified result</h2></section>".to_string();
    assert!(
        store
            .publish_project_home(
                session.id,
                action.id,
                other.id,
                ArtifactId::new(),
                ArtifactId::new(),
                html.clone(),
                json!({}),
            )
            .is_err(),
        "another Agent must not publish the Action"
    );
    let published = store
        .publish_project_home(
            session.id,
            action.id,
            agent.id,
            ArtifactId::new(),
            ArtifactId::new(),
            html.clone(),
            json!({}),
        )
        .expect("owning Agent should publish");
    assert!(published.changed);
    assert_eq!(published.artifact.session_id, session.id);
    assert_eq!(published.artifact.agent_id, Some(agent.id));
    let unchanged = store
        .publish_project_home(
            session.id,
            action.id,
            agent.id,
            ArtifactId::new(),
            ArtifactId::new(),
            html,
            json!({}),
        )
        .expect("unchanged page should be accepted");
    assert!(!unchanged.changed);
    assert_eq!(unchanged.artifact.id, published.artifact.id);
}

#[test]
fn database_reopens_artifacts_and_accumulates_usage() {
    const WORKERS: u32 = 4;
    const UPDATES: u32 = 10;
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let (project_id, session_id, artifact_id) = {
        let store = Store::create(&managed).expect("store should be created");
        let project = project(&store, &directory, "persist");
        let session =
            create_root_session(&store, project.id, "Persistent", AccessPreset::Workspace);
        let agent = store
            .create_agent(
                session.id,
                "Writer",
                "Writer",
                "write",
                "",
                "test-model",
                Vec::new(),
                AccessPreset::Workspace,
            )
            .expect("Agent should be created");
        let artifact = store
            .create_artifact(
                project.id,
                session.id,
                Some(agent.id),
                None,
                ArtifactKind::Report,
                "result.md",
                "text/markdown",
                json!({}),
                b"evidence",
            )
            .expect("Artifact should be created");
        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                let store = store.clone();
                scope.spawn(move || {
                    for _ in 0..UPDATES {
                        store
                            .add_session_usage(
                                session.id,
                                SessionUsage {
                                    actions_started: 1,
                                    ..SessionUsage::default()
                                },
                            )
                            .expect("usage should update");
                    }
                });
            }
        });
        assert_eq!(
            store
                .get_session(session.id)
                .expect("Session should load")
                .usage
                .actions_started,
            WORKERS * UPDATES
        );
        (project.id, session.id, artifact.id)
    };

    let reopened = Store::open(&managed).expect("store should reopen");
    assert_eq!(
        reopened
            .get_session(session_id)
            .expect("Session should reopen")
            .project_id,
        project_id
    );
    let artifact = reopened
        .get_artifact(artifact_id)
        .expect("Artifact should load");
    assert_eq!(
        reopened
            .read_artifact(&artifact)
            .expect("Artifact bytes should reopen"),
        b"evidence"
    );
}
