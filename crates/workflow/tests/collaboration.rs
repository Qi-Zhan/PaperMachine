#![cfg(target_os = "macos")]

use papermachine_model::ScriptedModelClient;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::ActionStatus;
use papermachine_protocol::AgentId;
use papermachine_protocol::AuthorizationContext;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::SessionTrigger;
use papermachine_protocol::TokenUsage;
use papermachine_protocol::TurnId;
use papermachine_protocol::WorkflowProgramId;
use papermachine_protocol::WorkflowProgramManifest;
use papermachine_protocol::WorkflowProgramSnapshot;
use papermachine_protocol::WorkflowProgramSource;
use papermachine_session::TurnRuntime;
use papermachine_session::TurnRuntimeConfig;
use papermachine_skills::ProjectSkillCatalog;
use papermachine_store::NewSession;
use papermachine_store::Store;
use papermachine_store::StoreHandle;
use papermachine_tools::ToolCatalog;
use papermachine_tools::ToolContext;
use papermachine_workflow::ActionControl;
use papermachine_workflow::ActionRunner;
use papermachine_workflow::CollaborationTools;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn completed(message: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::OutputTextDelta {
            delta: message.to_string(),
        },
        ModelEvent::Completed {
            usage: TokenUsage::default(),
        },
    ]
}

fn program() -> WorkflowProgramSnapshot {
    WorkflowProgramSnapshot {
        project_id: None,
        manifest: WorkflowProgramManifest {
            id: WorkflowProgramId::new(),
            slug: "collaboration-test".to_string(),
            name: "Collaboration test".to_string(),
            description: "Exercise the Agent collaboration kernel.".to_string(),
            entrypoint: "main".to_string(),
            request_mode: Default::default(),
            params_schema: json!({"type": "object"}),
        },
        source: WorkflowProgramSource::Builtin,
        definition_path: "builtin/collaboration-test/workflow.py".to_string(),
        sha256: "0".repeat(64),
        runtime_sha256: "0".repeat(64),
        source_code: String::new(),
    }
}

fn context(
    project_id: papermachine_protocol::ProjectId,
    session_id: papermachine_protocol::SessionId,
    agent_id: AgentId,
    workspace: &Path,
    managed_root: &Path,
    call_id: &str,
) -> ToolContext {
    ToolContext {
        project_id,
        session_id,
        agent_id,
        turn_id: TurnId::new(),
        tool_call_id: call_id.to_string(),
        action_invocation_id: None,
        action_attempt_id: None,
        sandbox_root: managed_root.join("test-sandbox"),
        authorization: AuthorizationContext::materialize(
            AccessPreset::Workspace,
            workspace.to_string_lossy().into_owned(),
            workspace.to_string_lossy().into_owned(),
            vec![managed_root.to_string_lossy().into_owned()],
        )
        .expect("authorization should materialize"),
        cancellation: CancellationToken::new(),
    }
}

#[tokio::test]
async fn collaboration_uses_durable_actions_and_one_agent_fifo() {
    let directory = tempdir().expect("temporary directory should exist");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace should exist");
    let store = Arc::new(
        Store::open_in_memory(directory.path().join("managed")).expect("Project store should open"),
    );
    let project = store
        .create_project("Collaboration", &workspace)
        .expect("Project should be created");
    let session = store
        .create_session(NewSession {
            project_id: project.id,
            program: program(),
            title: "Collaboration".to_string(),
            request: "Delegate work.".to_string(),
            instructions: String::new(),
            trigger: SessionTrigger::default(),
            params: json!({}),
            default_model: "scripted".to_string(),
            access: AccessPreset::Workspace,
            enabled_skills: Vec::new(),
            agent_access_overrides: BTreeMap::new(),
        })
        .expect("Session should be created");
    store
        .start_session(session.id)
        .expect("Session should start");
    let parent = store
        .create_agent(
            session.id,
            "Coordinator",
            "Coordinator",
            "coordinate work",
            "Delegate when useful.",
            "scripted",
            Vec::new(),
            AccessPreset::Workspace,
        )
        .expect("parent Agent should be created");

    let handle = StoreHandle::spawn((*store).clone()).expect("Store thread should start");
    let control = ActionControl::default();
    let collaboration = CollaborationTools::new(handle.clone(), control.clone(), 2);
    let tools = collaboration
        .register(ToolCatalog::builder())
        .expect("collaboration tools should register")
        .build();
    let root_snapshot = tools
        .materialize_action_tools(None, AccessPreset::Workspace, true)
        .expect("root ToolSet should materialize");
    let root_registry = tools
        .registry_for_snapshot(&root_snapshot)
        .expect("root Registry should rebuild");
    assert_eq!(root_snapshot.definitions.len(), 5);
    assert_eq!(
        tools
            .materialize_action_tools(None, AccessPreset::ModelOnly, true)
            .expect("model-only Agents still collaborate")
            .definitions
            .len(),
        5
    );
    assert!(
        tools
            .materialize_action_tools(Some(&[]), AccessPreset::Workspace, true)
            .expect("explicit empty ToolSet should materialize")
            .definitions
            .is_empty()
    );
    let child_snapshot = tools
        .materialize_action_tools(None, AccessPreset::Workspace, false)
        .expect("child ToolSet should materialize");
    assert_eq!(child_snapshot.definitions.len(), 4);
    assert!(
        child_snapshot
            .definitions
            .iter()
            .all(|definition| definition.name != "spawn_agent")
    );
    assert!(
        tools
            .materialize_action_tools(
                Some(&["spawn_agent".to_string()]),
                AccessPreset::Workspace,
                false,
            )
            .expect("child spawn request should fail closed")
            .definitions
            .is_empty()
    );

    let spawn = root_registry
        .execute(
            "spawn_agent",
            context(
                project.id,
                session.id,
                parent.id,
                &workspace,
                store.managed_root(),
                "spawn-first",
            ),
            json!({"task": "Inspect the evidence.", "access": "read_only"}),
        )
        .await
        .expect("spawn should succeed");
    let child_id = AgentId::from_str(
        spawn.value["agent_id"]
            .as_str()
            .expect("spawn should return an Agent id"),
    )
    .expect("Agent id should parse");
    let child_action_id = ActionInvocationId::from_str(
        spawn.value["action_invocation_id"]
            .as_str()
            .expect("spawn should return an Action id"),
    )
    .expect("Action id should parse");
    let child = store
        .get_agent(child_id)
        .expect("child Agent should be durable");
    assert_eq!(child.parent_agent_id, Some(parent.id));
    assert_eq!(child.access, AccessPreset::ReadOnly);

    let model =
        ScriptedModelClient::new([completed("Child evidence."), completed("Follow-up result.")]);
    let turns = TurnRuntime::new(
        handle.clone(),
        Arc::new(model.clone()),
        tools.clone(),
        Arc::new(ProjectSkillCatalog::new(handle.clone())),
        TurnRuntimeConfig {
            default_model: "scripted".to_string(),
            model_context_window: 128_000,
            max_concurrent_turns: 2,
        },
    );
    let runner = ActionRunner::new(handle.clone(), turns, control.clone());
    let runner_cancellation = CancellationToken::new();
    let runner_task = tokio::spawn({
        let cancellation = runner_cancellation.clone();
        async move { runner.run_session(session.id, cancellation).await }
    });

    let waited = root_registry
        .execute(
            "wait_agent",
            context(
                project.id,
                session.id,
                parent.id,
                &workspace,
                store.managed_root(),
                "wait-first",
            ),
            json!({
                "action_invocation_ids": [child_action_id],
                "timeout_ms": 5000,
            }),
        )
        .await
        .expect("wait should observe child completion");
    assert_eq!(waited.value["timed_out"], false);
    assert_eq!(
        waited.value["actions"][0]["status"],
        json!(ActionStatus::Completed)
    );

    root_registry
        .execute(
            "send_message",
            context(
                project.id,
                session.id,
                parent.id,
                &workspace,
                store.managed_root(),
                "queue-message",
            ),
            json!({
                "agent_id": child.id,
                "message": "Also check the caveat.",
            }),
        )
        .await
        .expect("queue-only message should persist");
    assert_eq!(
        store
            .list_agent_inputs(session.id)
            .expect("Agent inputs should load")
            .len(),
        1
    );

    let follow_up = root_registry
        .execute(
            "send_message",
            context(
                project.id,
                session.id,
                parent.id,
                &workspace,
                store.managed_root(),
                "start-follow-up",
            ),
            json!({
                "agent_id": child.id,
                "message": "Return the final caveat.",
                "start_turn": true,
            }),
        )
        .await
        .expect("start_turn message should schedule an Action");
    let follow_up_id = ActionInvocationId::from_str(
        follow_up.value["action_invocation_id"]
            .as_str()
            .expect("follow-up Action id should exist"),
    )
    .expect("follow-up Action id should parse");
    let waited = root_registry
        .execute(
            "wait_agent",
            context(
                project.id,
                session.id,
                parent.id,
                &workspace,
                store.managed_root(),
                "wait-follow-up",
            ),
            json!({
                "action_invocation_ids": [follow_up_id],
                "timeout_ms": 5000,
            }),
        )
        .await
        .expect("follow-up should complete");
    assert_eq!(waited.value["timed_out"], false);
    assert!(
        model
            .requests()
            .expect("model requests should be recorded")
            .iter()
            .any(|request| request.input.iter().any(|item| matches!(
                item,
                ModelInputItem::Message { role: MessageRole::User, content }
                    if content.contains("Also check the caveat")
            )))
    );
    let delivered = store
        .list_agent_inputs(session.id)
        .expect("Agent inputs should load")
        .into_iter()
        .next()
        .expect("queued input should remain durable");
    assert_eq!(
        delivered.status,
        papermachine_protocol::AgentInputStatus::Applied
    );
    assert_eq!(
        delivered.source,
        papermachine_protocol::AgentInputSource::Agent {
            sender_agent_id: parent.id,
        }
    );

    runner_cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(5), runner_task)
        .await
        .expect("ActionRunner should stop")
        .expect("ActionRunner task should join")
        .expect("ActionRunner should exit cleanly");

    let second = root_registry
        .execute(
            "spawn_agent",
            context(
                project.id,
                session.id,
                parent.id,
                &workspace,
                store.managed_root(),
                "spawn-second",
            ),
            json!({"task": "Wait for more work."}),
        )
        .await
        .expect("second child should spawn");
    let second_id = AgentId::from_str(
        second.value["agent_id"]
            .as_str()
            .expect("second child id should exist"),
    )
    .expect("second child id should parse");
    root_registry
        .execute(
            "interrupt_agent",
            context(
                project.id,
                session.id,
                parent.id,
                &workspace,
                store.managed_root(),
                "interrupt-second",
            ),
            json!({"agent_id": second_id}),
        )
        .await
        .expect("parent should interrupt its child");
    let second_action = ActionInvocationId::from_str(
        second.value["action_invocation_id"]
            .as_str()
            .expect("second Action id should exist"),
    )
    .expect("second Action id should parse");
    assert_eq!(
        store
            .get_action_invocation(second_action)
            .expect("second Action should load")
            .status,
        ActionStatus::Cancelled
    );

    let forest = root_registry
        .execute(
            "list_agents",
            context(
                project.id,
                session.id,
                parent.id,
                &workspace,
                store.managed_root(),
                "list-final",
            ),
            json!({}),
        )
        .await
        .expect("forest should list");
    assert_eq!(
        forest.value["agents"]
            .as_array()
            .expect("Agent list should be an array")
            .len(),
        3
    );
}
