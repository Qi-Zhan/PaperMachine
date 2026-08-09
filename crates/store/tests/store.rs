use papermachine_protocol::AccessPreset;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::ArtifactKind;
use papermachine_protocol::ControlMessageKind;
use papermachine_protocol::ControlMessageStatus;
use papermachine_protocol::HumanRequestId;
use papermachine_protocol::HumanRequestStatus;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelContextMutation;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::Project;
use papermachine_protocol::Session;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::ToolSetSnapshot;
use papermachine_protocol::WorkflowContextMode;
use papermachine_protocol::WorkflowEffectStatus;
use papermachine_protocol::WorkflowLaunchContext;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_protocol::WorkflowUsage;
use papermachine_store::NewWorkflow;
use papermachine_store::ProjectHomePatchOperation;
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
use support::model_route;

fn empty_tool_set() -> ToolSetSnapshot {
    ToolSetSnapshot::materialize(Vec::new()).expect("empty tool set should be valid")
}

#[test]
fn project_creation_separates_managed_state_from_workspace_and_rejects_reuse() {
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let store = Store::open_in_memory(&managed).expect("store should open");
    let root = directory.path().join("paper-workspace");
    let project = store
        .create_project("Paper", &root)
        .expect("project should be created");

    assert_eq!(
        project.workspace.path,
        root.canonicalize()
            .expect("Project Workspace should be canonicalizable")
            .to_string_lossy()
    );
    assert!(root.is_dir());
    assert!(
        std::fs::read_dir(&root)
            .expect("Workspace should list")
            .next()
            .is_none()
    );
    for child in ["prompts", "workflows", "skills", "state", "runtime"] {
        assert!(
            managed.join(child).is_dir(),
            "missing managed {child} directory"
        );
    }
    let prompt = store
        .get_project_system_prompt(project.id)
        .expect("Project system prompt should load");
    assert!(prompt.content.is_empty());
    assert_eq!(prompt.relative_path, "prompts/system.md");
    let prompt = store
        .set_project_system_prompt(project.id, "Prefer primary evidence.")
        .expect("Project system prompt should update");
    assert_eq!(prompt.content, "Prefer primary evidence.");
    assert_eq!(prompt.sha256.len(), 64);
    assert!(
        store.create_project("Duplicate", &root).is_err(),
        "one directory must belong to only one Project"
    );
    assert!(
        store.create_project("Relative", "relative/path").is_err(),
        "Project Workspaces must be absolute"
    );
    let overlap = store
        .create_project("Internal", managed.join("workspace"))
        .expect_err("Workspace cannot overlap PaperMachine managed state");
    assert!(overlap.to_string().contains("must be separate"));
}

#[test]
fn turn_creation_requires_the_attached_workspace_to_be_available() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let workspace = directory.path().join("workspace");
    let project = store
        .create_project("Detached", &workspace)
        .expect("Project should be created");
    let origin = store
        .create_session(project.id, "Session", "", "test-model", Vec::new())
        .expect("Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    std::fs::remove_dir(&workspace).expect("empty Workspace should be removed");

    let error = harness
        .create_turn(
            &store,
            papermachine_protocol::TurnOrigin::Workflow,
            "Do work",
            AccessPreset::Research,
        )
        .expect_err("Turn creation must stop before sampling without a Workspace");
    assert!(error.to_string().contains("Workspace is unavailable"));
    assert!(
        store
            .list_turns(harness.participant.session_id)
            .expect("Turns should list")
            .is_empty()
    );
}

#[test]
fn archived_session_stays_hidden_when_its_active_turn_finishes() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let research = project(&store, &directory, "Archived Session");
    let origin = store
        .create_session(research.id, "Conversation", "", "test-model", Vec::new())
        .expect("session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let session_id = harness.participant.session_id;
    let turn = harness
        .create_turn(
            &store,
            papermachine_protocol::TurnOrigin::Workflow,
            "Long-running task",
            AccessPreset::Research,
        )
        .expect("turn should be created");

    store
        .set_session_status(
            session_id,
            papermachine_protocol::SessionStatus::Archived,
            Some("closed by user".to_string()),
        )
        .expect("session should archive");
    store
        .cancel_turn(turn.id)
        .expect("active turn should cancel");

    assert_eq!(
        store
            .get_session(session_id)
            .expect("archived session should remain")
            .status,
        papermachine_protocol::SessionStatus::Archived
    );
    assert!(
        store
            .list_sessions(research.id)
            .expect("visible sessions should load")
            .into_iter()
            .all(|session| session.id != session_id)
    );
}

#[test]
fn project_level_workflow_keeps_program_snapshot_after_program_update() {
    let directory = tempdir().expect("temporary directory should be created");
    let store =
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open");
    let project = project(&store, &directory, "Snapshot");
    let mut original = workflow();
    original.project_id = Some(project.id);
    original.source = WorkflowProgramSource::User;
    original.definition_path = "workflows/parallel-review/workflow.py".to_string();
    original.sha256 = "original-sha".to_string();
    original.source_code = "async def main(ctx): return {'revision': 1}\n".to_string();
    let workflow = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: None,
            program: original,
            request: "Summarize the Project".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "test-model".to_string(),
            access: AccessPreset::Research,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("Project-level Workflow should be created without a Session");

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
}

#[test]
fn workflow_access_is_bounded_by_its_origin_and_agent_overrides() {
    let directory = tempdir().expect("temporary directory should be created");
    let store =
        Store::open_in_memory(directory.path().join("artifacts")).expect("store should open");
    let project = project(&store, &directory, "Permission ceiling");
    let origin = store
        .create_session(project.id, "Workspace origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let origin = store
        .set_session_access(origin.id, AccessPreset::Workspace)
        .expect("origin access should update");

    let above_origin = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: Some(origin.id),
            program: workflow(),
            request: "Attempt to exceed the origin".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "test-model".to_string(),
            access: AccessPreset::Research,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect_err("a Workflow must not exceed its starting Session");
    assert!(
        above_origin
            .to_string()
            .contains("exceeds starting Session")
    );

    let above_workflow = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: Some(origin.id),
            program: workflow(),
            request: "Attempt an Agent override above the ceiling".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "test-model".to_string(),
            access: AccessPreset::Workspace,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: BTreeMap::from([(
                "Researcher".to_string(),
                AccessPreset::Research,
            )]),
        })
        .expect_err("an Agent override must not exceed its Workflow");
    assert!(
        above_workflow
            .to_string()
            .contains("exceeds Workflow access")
    );

    let launch_context = WorkflowLaunchContext {
        mode: WorkflowContextMode::ProjectSnapshot,
        snapshot: Some(json!({"evidence": "captured once"})),
    };
    let created = store
        .create_workflow(NewWorkflow {
            project_id: project.id,
            started_from_session_id: Some(origin.id),
            program: workflow(),
            request: "Use an admissible override".to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "test-model".to_string(),
            access: AccessPreset::Workspace,
            enabled_skills: Vec::new(),
            launch_context: launch_context.clone(),
            agent_access_overrides: BTreeMap::from([(
                "Researcher".to_string(),
                AccessPreset::ReadOnly,
            )]),
        })
        .expect("an override below the Workflow ceiling should be accepted");
    assert_eq!(created.launch_context, launch_context);
    assert_eq!(
        created.agent_access_overrides["Researcher"],
        AccessPreset::ReadOnly
    );
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
            request_mode: Default::default(),
            params_schema: json!({"type": "object"}),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: "builtin/parallel-review/workflow.py".to_string(),
        sha256: "test-source".to_string(),
        runtime_sha256: "test-runtime".to_string(),
        source_code: "async def main(ctx): return {}\n".to_string(),
    }
}

fn project(store: &Store, directory: &TempDir, name: &str) -> Project {
    store
        .create_project(name, directory.path().join("project"))
        .expect("project should be created")
}

fn workflow_for_session(
    store: &Store,
    session: &Session,
    request: &str,
) -> papermachine_protocol::Workflow {
    store
        .create_workflow(NewWorkflow {
            project_id: session.project_id,
            started_from_session_id: Some(session.id),
            program: workflow(),
            request: request.to_string(),
            instructions: String::new(),
            trigger: Default::default(),
            params: json!({}),
            default_model: "test-model".to_string(),
            access: AccessPreset::Research,
            enabled_skills: Vec::new(),
            launch_context: Default::default(),
            agent_access_overrides: Default::default(),
        })
        .expect("workflow should be created")
}

#[test]
fn access_changes_only_between_turns_and_each_turn_keeps_its_snapshot() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let research = project(&store, &directory, "Access snapshots");
    let origin = store
        .create_session(research.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let session_id = harness.participant.session_id;
    store
        .set_session_access(session_id, AccessPreset::Workspace)
        .expect("participant should start with Workspace access");
    let first = harness
        .create_turn(
            &store,
            papermachine_protocol::TurnOrigin::Workflow,
            "First",
            AccessPreset::Workspace,
        )
        .expect("first turn should be created");
    assert_eq!(first.environment.workspace.id, research.workspace.id);
    assert_eq!(first.environment.workspace.revision, 1);
    assert_eq!(
        first.environment.authorization.preset,
        AccessPreset::Workspace
    );
    assert!(
        store
            .set_session_access(session_id, AccessPreset::Research)
            .is_err(),
        "an active Turn must block access changes"
    );

    store.cancel_turn(first.id).expect("first turn should end");
    let updated = store
        .set_session_access(session_id, AccessPreset::Research)
        .expect("access should change between turns");
    assert_eq!(updated.access, AccessPreset::Research);
    let relocated_root = directory.path().join("relocated-workspace");
    std::fs::create_dir_all(&relocated_root).expect("relocated Workspace should be created");
    let relocated = store
        .relocate_project(research.id, &relocated_root)
        .expect("Project Workspace should relocate between Turns");
    assert_eq!(relocated.workspace.id, research.workspace.id);
    assert_eq!(relocated.workspace.revision, 2);
    assert_eq!(
        store
            .get_turn(first.id)
            .expect("first turn should remain")
            .environment
            .authorization
            .preset,
        AccessPreset::Workspace
    );
    let second = harness
        .create_turn(
            &store,
            papermachine_protocol::TurnOrigin::Workflow,
            "Second",
            AccessPreset::Research,
        )
        .expect("second turn should be created");
    assert_eq!(
        second.environment.authorization.preset,
        AccessPreset::Research
    );
    assert_eq!(second.environment.workspace.id, research.workspace.id);
    assert_eq!(second.environment.workspace.revision, 2);
    assert_eq!(
        second.environment.workspace.path,
        relocated_root
            .canonicalize()
            .expect("relocated Workspace should canonicalize")
            .to_string_lossy()
    );
    let persisted_first = store.get_turn(first.id).expect("first Turn should remain");
    assert_eq!(persisted_first.environment.workspace.revision, 1);
    assert_eq!(
        persisted_first.environment.authorization_sha256,
        first.environment.authorization_sha256
    );
}

#[test]
fn session_system_prompt_cannot_change_while_a_turn_is_queued() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "Prompt locking");
    let origin = store
        .create_session(
            project.id,
            "Origin",
            "Original prompt",
            "test-model",
            Vec::new(),
        )
        .expect("session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let session_id = harness.participant.session_id;
    store
        .set_session_system_prompt(session_id, "Original prompt")
        .expect("participant prompt should initialize");
    let turn = harness
        .create_turn(
            &store,
            papermachine_protocol::TurnOrigin::Workflow,
            "Question",
            AccessPreset::Research,
        )
        .expect("Turn should be queued");

    assert!(
        store
            .set_session_system_prompt(session_id, "Changed too early")
            .is_err(),
        "a queued Turn must lock the Session prompt"
    );
    assert_eq!(
        store
            .get_session(session_id)
            .expect("Session should load")
            .system_prompt,
        "Original prompt"
    );

    store.cancel_turn(turn.id).expect("Turn should end");
    let updated = store
        .set_session_system_prompt(session_id, "Changed between Turns")
        .expect("prompt should change after the Turn ends");
    assert_eq!(updated.system_prompt, "Changed between Turns");
}

#[test]
fn database_reopens_and_artifacts_are_content_addressed() {
    let directory = tempdir().expect("temporary directory should be created");
    let managed = directory.path().join("managed");
    let artifacts = managed.join("artifacts");
    let (project_id, workflow_id, artifact_path) = {
        let store = Store::create(&managed).expect("store should be created");
        let research = project(&store, &directory, "Persistent");
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
        assert!(artifact.relative_path.starts_with(&run.id.to_string()));
        assert!(artifacts.join(&artifact.relative_path).is_file());
        std::fs::write(artifacts.join("orphan.tmp"), "uncommitted")
            .expect("orphan fixture should be written");
        (research.id, run.id, artifact.relative_path)
    };

    let reopened = Store::open(&managed).expect("store should reopen");
    reopened
        .reconcile_artifacts()
        .expect("Artifact storage should reconcile");
    assert!(!artifacts.join("orphan.tmp").exists());
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
    std::fs::write(artifacts.join(&artifact_path), b"tampered")
        .expect("artifact corruption fixture should be written");
    assert!(
        reopened
            .read_artifact(&stored[0])
            .expect_err("corrupted Artifact must fail closed")
            .to_string()
            .contains("hash")
    );
    std::fs::write(artifacts.join(&artifact_path), b"evidence")
        .expect("artifact fixture should be restored");
    drop(reopened);
    std::fs::remove_file(artifacts.join(&artifact_path))
        .expect("artifact fixture should be removed");
    assert!(
        Store::open(&managed)
            .expect("database should still open")
            .reconcile_artifacts()
            .expect_err("a durable Artifact without its file must fail closed")
            .to_string()
            .contains("unavailable")
    );
}

#[test]
fn terminal_runs_close_pending_human_and_control_state() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let research = project(&store, &directory, "Terminal cleanup");
    let origin = store
        .create_session(research.id, "Origin", "", "test-model", Vec::new())
        .expect("Session should exist");
    let run = workflow_for_session(&store, &origin, "Finish cleanly");
    store.start_workflow(run.id).expect("run should start");
    let request = store
        .create_human_request_with_id(
            HumanRequestId::new(),
            run.id,
            origin.id,
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
    store
        .cancel_workflow(run.id, "test cleanup")
        .expect("run should cancel");

    assert_eq!(
        store
            .get_human_request(request.id)
            .expect("request should load")
            .status,
        HumanRequestStatus::Cancelled
    );
    let terminal_control = store
        .list_control_messages(run.id)
        .expect("controls should load")
        .into_iter()
        .find(|item| item.id == control.id)
        .expect("control should exist");
    assert_eq!(terminal_control.status, ControlMessageStatus::Applied);
    assert!(terminal_control.applied_at.is_some());
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
fn workflow_turn_and_action_attempt_are_attached_atomically() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "Atomic Turn");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let workflow = workflow_for_session(&store, &origin, "Attach one Turn");
    store
        .start_workflow(workflow.id)
        .expect("Workflow should be running");
    let participant = store
        .create_participant(
            workflow.id,
            "Researcher",
            "Researcher",
            "research",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::Research,
        )
        .expect("participant should be created");
    let invocation = store
        .create_action_invocation(
            workflow.id,
            participant.id,
            "research",
            "Research",
            json!({}),
            Vec::new(),
        )
        .expect("Action should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("Attempt should start");
    let turn = store
        .create_turn_for_attempt(
            attempt.id,
            participant.session_id,
            papermachine_protocol::TurnOrigin::Workflow,
            "Research",
            model_route("test-model"),
            papermachine_protocol::PromptSnapshot::default(),
            true,
            AccessPreset::Research,
            empty_tool_set(),
            None,
            None,
            Vec::new(),
        )
        .expect("Turn and Attempt should attach");

    assert_eq!(
        store
            .get_action_attempt(attempt.id)
            .expect("Attempt should load")
            .turn_id,
        Some(turn.id)
    );
}

#[test]
fn project_home_draft_supports_idempotent_semantic_patches_and_preview_source() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "Project Home");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let workflow = workflow_for_session(&store, &origin, "Maintain the Project home");
    store
        .start_workflow(workflow.id)
        .expect("Workflow should be running");
    let participant = store
        .create_participant(
            workflow.id,
            "SummaryAgent",
            "Summary",
            "curator",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::ModelOnly,
        )
        .expect("participant should be created");
    let invocation = store
        .create_action_invocation(
            workflow.id,
            participant.id,
            "maintain_project_home",
            "Maintain the page",
            json!({}),
            vec![
                "read_project_home".to_string(),
                "patch_project_home".to_string(),
                "preview_project_home".to_string(),
            ],
        )
        .expect("Action should be created");

    let empty = store
        .read_project_home_draft(workflow.id, invocation.id)
        .expect("empty draft should load");
    assert!(empty.blocks.is_empty());
    let patched = store
        .patch_project_home_draft(
            workflow.id,
            invocation.id,
            &empty.revision,
            vec![ProjectHomePatchOperation::Upsert {
                id: "overview".to_string(),
                html: "<header><h1>Current research state</h1></header>".to_string(),
            }],
        )
        .expect("semantic patch should apply");
    assert_ne!(patched.revision, empty.revision);
    assert_eq!(patched.blocks[0].id, "overview");

    let conflict = store
        .patch_project_home_draft(
            workflow.id,
            invocation.id,
            &empty.revision,
            vec![ProjectHomePatchOperation::Remove {
                id: "overview".to_string(),
            }],
        )
        .expect_err("stale revisions must not overwrite a newer draft");
    assert!(conflict.to_string().contains("revision conflict"));

    let unsafe_patch = store
        .patch_project_home_draft(
            workflow.id,
            invocation.id,
            &patched.revision,
            vec![ProjectHomePatchOperation::Upsert {
                id: "unsafe".to_string(),
                html: "<script>alert(1)</script>".to_string(),
            }],
        )
        .expect_err("active content must be rejected before publication");
    assert!(unsafe_patch.to_string().contains("forbidden <script>"));

    let source = patched.source();
    assert!(source.html().starts_with("<article>"));
    assert!(source.html().contains("Current research state"));

    let published = store
        .publish_project_home_draft(
            workflow.id,
            invocation.id,
            participant.session_id,
            papermachine_protocol::ArtifactId::new(),
            papermachine_protocol::ArtifactId::new(),
            json!({"test": "initial"}),
        )
        .expect("first Project home should publish");
    assert!(published.changed);
    assert_eq!(published.home.revision, patched.revision);
    assert_eq!(
        store
            .get_project_home(project.id)
            .expect("canonical home should load"),
        Some(published.home.clone())
    );

    let action = |name: &str| {
        store
            .create_action_invocation(
                workflow.id,
                participant.id,
                name,
                "Maintain the page",
                json!({}),
                vec![
                    "read_project_home".to_string(),
                    "patch_project_home".to_string(),
                    "preview_project_home".to_string(),
                ],
            )
            .expect("Project-home Action should be created")
    };
    let no_op_action = action("no_op");
    let no_op_draft = store
        .read_project_home_draft(workflow.id, no_op_action.id)
        .expect("no-op draft should load current canonical home");
    assert_eq!(no_op_draft.base_artifact_id, Some(published.artifact.id));
    let no_op = store
        .publish_project_home_draft(
            workflow.id,
            no_op_action.id,
            participant.session_id,
            papermachine_protocol::ArtifactId::new(),
            papermachine_protocol::ArtifactId::new(),
            json!({}),
        )
        .expect("unchanged Project home should reuse the canonical revision");
    assert!(!no_op.changed);
    assert_eq!(no_op.artifact.id, published.artifact.id);
    assert_eq!(
        store
            .list_project_artifacts(project.id)
            .expect("Artifacts should list")
            .len(),
        2
    );

    let winning_action = action("winning_update");
    let stale_action = action("stale_update");
    let winning_base = store
        .read_project_home_draft(workflow.id, winning_action.id)
        .expect("winning draft should load");
    let stale_base = store
        .read_project_home_draft(workflow.id, stale_action.id)
        .expect("stale draft should load");
    store
        .patch_project_home_draft(
            workflow.id,
            winning_action.id,
            &winning_base.revision,
            vec![ProjectHomePatchOperation::Upsert {
                id: "status".to_string(),
                html: "<section><h2>Verified status</h2></section>".to_string(),
            }],
        )
        .expect("winning patch should apply");
    store
        .patch_project_home_draft(
            workflow.id,
            stale_action.id,
            &stale_base.revision,
            vec![ProjectHomePatchOperation::Upsert {
                id: "status".to_string(),
                html: "<section><h2>Stale status</h2></section>".to_string(),
            }],
        )
        .expect("concurrent stale draft should patch locally");
    store
        .publish_project_home_draft(
            workflow.id,
            winning_action.id,
            participant.session_id,
            papermachine_protocol::ArtifactId::new(),
            papermachine_protocol::ArtifactId::new(),
            json!({}),
        )
        .expect("winning revision should publish");
    let conflict = store
        .publish_project_home_draft(
            workflow.id,
            stale_action.id,
            participant.session_id,
            papermachine_protocol::ArtifactId::new(),
            papermachine_protocol::ArtifactId::new(),
            json!({}),
        )
        .expect_err("stale Project-home base must fail closed");
    assert!(conflict.to_string().contains("base revision changed"));
}

#[test]
fn workflow_action_accepts_only_the_exact_answer_as_a_user_turn() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "Human Turn");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let workflow = workflow_for_session(&store, &origin, "Interactive conversation");
    store
        .start_workflow(workflow.id)
        .expect("Workflow should be running");
    let participant = store
        .create_participant(
            workflow.id,
            "InteractiveAgent",
            "Assistant",
            "interactive",
            "",
            "test-model",
            Vec::new(),
            AccessPreset::Research,
        )
        .expect("participant should be created");
    let request = store
        .create_human_request_with_id(
            HumanRequestId::new(),
            workflow.id,
            participant.session_id,
            "Next message",
            json!({"type": "string"}),
        )
        .expect("human request should open");
    store
        .answer_human_request(request.id, json!("Inspect the cache."))
        .expect("human request should be answered");

    let invocation = store
        .create_action_invocation_with_id(
            ActionInvocationId::new(),
            workflow.id,
            participant.id,
            "respond",
            "Respond to the human",
            json!({"message": "Inspect the cache."}),
            Vec::new(),
            Some(request.id),
        )
        .expect("human action should be created");
    let attempt = store
        .start_action_attempt(invocation.id)
        .expect("Attempt should start");
    let turn = store
        .create_turn_for_attempt(
            attempt.id,
            participant.session_id,
            papermachine_protocol::TurnOrigin::User,
            "Inspect the cache.",
            model_route("test-model"),
            papermachine_protocol::PromptSnapshot::default(),
            true,
            AccessPreset::Research,
            empty_tool_set(),
            None,
            None,
            Vec::new(),
        )
        .expect("exact human answer should become a user Turn");
    assert_eq!(turn.origin, papermachine_protocol::TurnOrigin::User);
    assert_eq!(invocation.source_human_request_id, Some(request.id));

    let forged = store
        .create_action_invocation_with_id(
            ActionInvocationId::new(),
            workflow.id,
            participant.id,
            "respond",
            "Respond to the human",
            json!({"message": "A different message"}),
            Vec::new(),
            Some(request.id),
        )
        .expect("second invocation should be recorded before Turn validation");
    let forged_attempt = store
        .start_action_attempt(forged.id)
        .expect("forged Attempt should start");
    assert!(
        store
            .create_turn_for_attempt(
                forged_attempt.id,
                participant.session_id,
                papermachine_protocol::TurnOrigin::User,
                "A different message",
                model_route("test-model"),
                papermachine_protocol::PromptSnapshot::default(),
                true,
                AccessPreset::Research,
                empty_tool_set(),
                None,
                None,
                Vec::new(),
            )
            .is_err(),
        "a Workflow must not forge human provenance with different text"
    );
}

#[test]
fn workflow_effect_journal_replays_only_an_identical_request() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "Effect journal");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("Session should be created");
    let workflow = workflow_for_session(&store, &origin, "Replay safely");
    let payload = json!({"name": "Researcher", "access": "research"});

    let started = store
        .begin_workflow_effect(
            workflow.id,
            "root/agent:0/create_agent",
            "create_agent",
            payload.clone(),
        )
        .expect("effect should begin");
    assert_eq!(started.status, WorkflowEffectStatus::Started);
    assert_eq!(started.request_sha256.len(), 64);

    let completed = store
        .finish_workflow_effect(
            workflow.id,
            &started.key,
            Ok(json!({"agent_instance_id": "stable-agent"})),
        )
        .expect("effect should complete");
    assert_eq!(completed.status, WorkflowEffectStatus::Completed);

    let replay = store
        .begin_workflow_effect(workflow.id, &started.key, "create_agent", payload)
        .expect("identical request should replay");
    assert_eq!(replay, completed);
    assert_eq!(
        store
            .list_workflow_effects(workflow.id)
            .expect("journal should load")
            .len(),
        1
    );
    assert!(
        store
            .begin_workflow_effect(
                workflow.id,
                &started.key,
                "create_agent",
                json!({"name": "Different"}),
            )
            .is_err(),
        "one logical effect path must never accept a changed request"
    );
}

#[test]
fn concurrent_usage_updates_do_not_lose_deltas() {
    const WORKERS: u32 = 8;
    const UPDATES_PER_WORKER: u32 = 25;

    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let research = project(&store, &directory, "Concurrent usage");
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
                        .add_workflow_usage(
                            run.id,
                            WorkflowUsage {
                                actions_started: 1,
                                hosted_search_calls: 1,
                                ..WorkflowUsage::default()
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

#[test]
fn concurrent_action_start_admits_exactly_one_attempt() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("store should open"),
    );
    let project = project(&store, &directory, "Concurrent Action");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let invocation = store
        .create_action_invocation(
            harness.workflow.id,
            harness.participant.id,
            "one_attempt",
            "Start once",
            json!({}),
            Vec::new(),
        )
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
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert_eq!(
        store
            .list_action_attempts(invocation.id)
            .expect("attempts should load")
            .len(),
        1
    );
    let attempt_id = results
        .iter()
        .find_map(|result| result.as_ref().ok().map(|attempt| attempt.id))
        .expect("one attempt should have started");
    let barrier = Arc::new(Barrier::new(3));
    let finishers = (0..2)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store.finish_action(
                    invocation.id,
                    attempt_id,
                    papermachine_protocol::ActionStatus::Completed,
                    Some(json!({"answer": 1})),
                    None,
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    assert!(
        finishers
            .into_iter()
            .all(|worker| worker.join().expect("finisher should join").is_ok())
    );
    assert_eq!(
        store
            .get_workflow(harness.workflow.id)
            .expect("Workflow should load")
            .usage
            .actions_completed,
        1
    );
    assert!(
        store
            .finish_action(
                invocation.id,
                attempt_id,
                papermachine_protocol::ActionStatus::Completed,
                Some(json!({"answer": 2})),
                None,
            )
            .is_err(),
        "a terminal Action must reject a conflicting replay"
    );
}

#[test]
fn concurrent_human_answers_use_one_open_request_cas() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("store should open"),
    );
    let project = project(&store, &directory, "Human CAS");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let request = store
        .create_human_request_with_id(
            HumanRequestId::new(),
            harness.workflow.id,
            harness.participant.session_id,
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
    let resolved = store
        .get_human_request(request.id)
        .expect("HumanRequest should load");
    assert_eq!(resolved.status, HumanRequestStatus::Answered);
    assert!(matches!(resolved.answer, Some(value) if value == "first" || value == "second"));
}

#[test]
fn claimed_control_is_recovered_by_the_same_turn_and_applied_by_its_checkpoint() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "Control checkpoint");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let created = harness
        .create_action_turn(
            &store,
            papermachine_protocol::TurnOrigin::Workflow,
            "Work",
            AccessPreset::Research,
        )
        .expect("Action Turn should be created");
    let invocation = created.invocation;
    let turn = created.turn;
    store.start_turn(turn.id).expect("Turn should start");
    let control = store
        .create_control_message(
            harness.workflow.id,
            harness.participant.session_id,
            Some(invocation.id),
            ControlMessageKind::Guide,
            "Check the durable evidence",
        )
        .expect("control should queue");

    let first_claim = store
        .claim_control_messages(
            harness.workflow.id,
            harness.participant.session_id,
            Some(invocation.id),
            turn.id,
        )
        .expect("control should claim");
    let recovered_claim = store
        .claim_control_messages(
            harness.workflow.id,
            harness.participant.session_id,
            Some(invocation.id),
            turn.id,
        )
        .expect("the same Turn should recover its claim");
    assert_eq!(first_claim, recovered_claim);
    assert_eq!(first_claim[0].status, ControlMessageStatus::Claimed);
    assert_eq!(first_claim[0].claimed_turn_id, Some(turn.id));

    store
        .checkpoint_turn_context(
            turn.id,
            TurnContextCheckpoint {
                mutation: ModelContextMutation::Append {
                    items: vec![ModelInputItem::Message {
                        role: MessageRole::User,
                        content:
                            "Human guidance for this running action:\nCheck the durable evidence"
                                .to_string(),
                    }],
                },
                usage: TokenUsage::default(),
                completed_model_steps: 0,
                hosted_search_calls_used: 0,
                checkpoint_message: None,
                acknowledged_control_ids: vec![control.id],
            },
        )
        .expect("canonical context should acknowledge the control");
    let applied = store
        .list_control_messages(harness.workflow.id)
        .expect("controls should load")
        .into_iter()
        .find(|message| message.id == control.id)
        .expect("control should remain queryable");
    assert_eq!(applied.status, ControlMessageStatus::Applied);
    assert!(applied.applied_at.is_some());
}

#[test]
fn interrupt_applies_its_claim_in_the_turn_terminal_transaction() {
    let directory = tempdir().expect("temporary directory should be created");
    let store = Store::open_in_memory(directory.path().join("managed")).expect("store should open");
    let project = project(&store, &directory, "Interrupt control");
    let origin = store
        .create_session(project.id, "Origin", "", "test-model", Vec::new())
        .expect("origin Session should be created");
    let harness = ActionHarness::create(&store, &origin, AccessPreset::Research);
    let created = harness
        .create_action_turn(
            &store,
            papermachine_protocol::TurnOrigin::Workflow,
            "Work",
            AccessPreset::Research,
        )
        .expect("Action Turn should be created");
    let invocation = created.invocation;
    let turn = created.turn;
    store.start_turn(turn.id).expect("Turn should start");
    let control = store
        .create_control_message(
            harness.workflow.id,
            harness.participant.session_id,
            Some(invocation.id),
            ControlMessageKind::Interrupt,
            "Stop and preserve the partial result",
        )
        .expect("interrupt should queue");
    store
        .claim_control_messages(
            harness.workflow.id,
            harness.participant.session_id,
            Some(invocation.id),
            turn.id,
        )
        .expect("interrupt should claim");
    store
        .interrupt_turn_with_controls(
            turn.id,
            "Stop and preserve the partial result",
            &[control.id],
        )
        .expect("Turn should interrupt atomically");
    let applied = store
        .list_control_messages(harness.workflow.id)
        .expect("controls should load")
        .into_iter()
        .find(|message| message.id == control.id)
        .expect("control should remain queryable");
    assert_eq!(applied.status, ControlMessageStatus::Applied);
}
