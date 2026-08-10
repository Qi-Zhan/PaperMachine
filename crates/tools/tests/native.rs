use async_trait::async_trait;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::AgentId;
use papermachine_protocol::AuthorizationContext;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionId;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::TurnId;
use papermachine_tools::ApplyPatchTool;
use papermachine_tools::ExecCommandTool;
use papermachine_tools::ProcessTable;
use papermachine_tools::ToolCatalog;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolError;
use papermachine_tools::ToolExecutor;
use papermachine_tools::ToolOutput;
use papermachine_tools::WriteStdinTool;
use serde_json::Value;
use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn context(
    workspace: &std::path::Path,
    managed: &std::path::Path,
    session_id: SessionId,
    agent_id: AgentId,
    access: AccessPreset,
) -> ToolContext {
    std::fs::create_dir_all(workspace).expect("workspace should exist");
    std::fs::create_dir_all(managed).expect("managed root should exist");
    let workspace = workspace
        .canonicalize()
        .expect("workspace should canonicalize");
    let managed = managed
        .canonicalize()
        .expect("managed root should canonicalize");
    ToolContext {
        project_id: ProjectId::new(),
        session_id,
        agent_id,
        turn_id: TurnId::new(),
        tool_call_id: "call-test".to_string(),
        action_invocation_id: None,
        action_attempt_id: None,
        sandbox_root: managed.join("sandbox").join(agent_id.to_string()),
        authorization: AuthorizationContext::materialize(
            access,
            workspace.to_string_lossy().into_owned(),
            workspace.to_string_lossy().into_owned(),
            vec![managed.to_string_lossy().into_owned()],
        )
        .expect("authorization should materialize"),
        cancellation: CancellationToken::new(),
    }
}

fn native_catalog(processes: ProcessTable) -> ToolCatalog {
    ToolCatalog::builder()
        .register_native(ExecCommandTool::new(processes.clone()))
        .expect("exec_command should register")
        .register_native(WriteStdinTool::new(processes))
        .expect("write_stdin should register")
        .register_native(ApplyPatchTool)
        .expect("apply_patch should register")
        .build()
}

#[test]
fn catalog_materializes_access_defaults_and_explicit_subsets() {
    let catalog = native_catalog(ProcessTable::default());
    let names = |access, policy: Option<&[String]>| {
        catalog
            .materialize_action_tools(policy, access, true)
            .expect("tool set should materialize")
            .definitions
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>()
    };
    assert!(names(AccessPreset::ModelOnly, None).is_empty());
    assert_eq!(
        names(AccessPreset::ReadOnly, None),
        vec!["exec_command", "write_stdin"]
    );
    assert_eq!(
        names(AccessPreset::Workspace, None),
        vec!["apply_patch", "exec_command", "write_stdin"]
    );
    assert_eq!(
        names(AccessPreset::FullAccess, None),
        vec!["apply_patch", "exec_command", "write_stdin"]
    );
    assert!(names(AccessPreset::FullAccess, Some(&[])).is_empty());
    assert_eq!(
        names(AccessPreset::Workspace, Some(&["apply_patch".to_string()])),
        vec!["apply_patch"]
    );
}

#[derive(Clone, Copy)]
struct ProbeTool(&'static str);

#[async_trait]
impl ToolExecutor for ProbeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "exec_command".to_string(),
            description: self.0.to_string(),
            input_schema: json!({"type": "object"}),
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        _context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            value: arguments,
            summary: "probe".to_string(),
        })
    }
}

#[test]
fn catalog_rejects_conflicts_unknown_tools_and_definition_drift() {
    assert!(
        ToolCatalog::builder()
            .register_native(ProbeTool("one"))
            .expect("first registration should work")
            .register_native(ProbeTool("two"))
            .is_err()
    );
    let catalog = ToolCatalog::builder()
        .register_native(ProbeTool("one"))
        .expect("probe should register")
        .build();
    assert!(
        catalog
            .materialize_action_tools(
                Some(&["missing".to_string()]),
                AccessPreset::Workspace,
                true,
            )
            .is_err()
    );
    let snapshot = catalog
        .materialize_action_tools(
            Some(&["exec_command".to_string()]),
            AccessPreset::Workspace,
            true,
        )
        .expect("snapshot should materialize");
    let changed = ToolCatalog::builder()
        .register_native(ProbeTool("two"))
        .expect("changed probe should register")
        .build();
    assert!(changed.registry_for_snapshot(&snapshot).is_err());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn command_yields_to_a_process_and_write_stdin_polls_incrementally() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let managed = fixture.path().join("managed");
    let session_id = SessionId::new();
    let agent_id = AgentId::new();
    let processes = ProcessTable::default();
    let workspace_context = context(
        &workspace,
        &managed,
        session_id,
        agent_id,
        AccessPreset::Workspace,
    );
    let exec = ExecCommandTool::new(processes.clone())
        .execute(
            workspace_context.clone(),
            json!({"cmd": "printf first; sleep 1; printf second", "yield_time_ms": 250}),
        )
        .await
        .expect("long command should start");
    assert!(
        exec.value["output"]
            .as_str()
            .is_some_and(|value| value.contains("first"))
    );
    let process_id = exec.value["process_id"]
        .as_str()
        .expect("running command should return process id");
    let mut poll = WriteStdinTool::new(processes.clone())
        .execute(
            workspace_context,
            json!({"process_id": process_id, "yield_time_ms": 2000}),
        )
        .await
        .expect("process should be pollable");
    assert!(
        poll.value["output"]
            .as_str()
            .is_some_and(|value| value.contains("second"))
    );
    if let Some(process_id) = poll.value["process_id"].as_str().map(str::to_string) {
        poll = WriteStdinTool::new(processes.clone())
            .execute(
                context(
                    &workspace,
                    &managed,
                    session_id,
                    agent_id,
                    AccessPreset::Workspace,
                ),
                json!({"process_id": process_id, "yield_time_ms": 1000}),
            )
            .await
            .expect("completed process should report its exit status");
    }
    assert!(poll.value["process_id"].is_null());
    assert_eq!(poll.value["exit_code"], 0);
    processes.shutdown().await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn tty_process_accepts_input_and_process_ownership_is_agent_scoped() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let managed = fixture.path().join("managed");
    let session_id = SessionId::new();
    let agent_id = AgentId::new();
    let processes = ProcessTable::default();
    let owner = context(
        &workspace,
        &managed,
        session_id,
        agent_id,
        AccessPreset::Workspace,
    );
    let exec = ExecCommandTool::new(processes.clone())
        .execute(
            owner.clone(),
            json!({"cmd": "read line; printf 'got:%s' \"$line\"", "tty": true, "yield_time_ms": 250}),
        )
        .await
        .expect("interactive command should start");
    let process_id = exec.value["process_id"]
        .as_str()
        .expect("interactive command should remain alive");
    let stranger = context(
        &workspace,
        &managed,
        session_id,
        AgentId::new(),
        AccessPreset::Workspace,
    );
    assert!(matches!(
        WriteStdinTool::new(processes.clone())
            .execute(stranger, json!({"process_id": process_id}))
            .await,
        Err(ToolError::PermissionDenied { .. })
    ));
    let result = WriteStdinTool::new(processes.clone())
        .execute(
            owner,
            json!({"process_id": process_id, "chars": "hello\n", "yield_time_ms": 2000}),
        )
        .await
        .expect("owner should write to its TTY");
    assert!(
        result.value["output"]
            .as_str()
            .is_some_and(|value| value.contains("got:hello"))
    );
    processes.shutdown().await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn process_limit_and_shutdown_terminate_the_process_tree() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let managed = fixture.path().join("managed");
    let marker = workspace.join("must-not-appear");
    let processes = ProcessTable::new(1, CancellationToken::new());
    let owner = context(
        &workspace,
        &managed,
        SessionId::new(),
        AgentId::new(),
        AccessPreset::Workspace,
    );
    ExecCommandTool::new(processes.clone())
        .execute(
            owner.clone(),
            json!({"cmd": "sleep 1; printf leaked > must-not-appear", "yield_time_ms": 250}),
        )
        .await
        .expect("first process should start");
    assert!(
        ExecCommandTool::new(processes.clone())
            .execute(owner, json!({"cmd": "sleep 1", "yield_time_ms": 250}),)
            .await
            .expect_err("per-Agent process limit should fail closed")
            .to_string()
            .contains("live processes")
    );
    processes.shutdown().await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn apply_patch_edits_text_and_obeys_the_workspace_boundary() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let managed = fixture.path().join("managed");
    let patch_context = context(
        &workspace,
        &managed,
        SessionId::new(),
        AgentId::new(),
        AccessPreset::Workspace,
    );
    let tool = ApplyPatchTool;
    tool.execute(
        patch_context.clone(),
        json!({"patch": "*** Begin Patch\n*** Add File: result.txt\n+first\n*** End Patch"}),
    )
    .await
    .expect("add should work");
    tool.execute(
        patch_context.clone(),
        json!({"patch": "*** Begin Patch\n*** Update File: result.txt\n@@\n-first\n+updated\n*** Add File: remove.txt\n+temporary\n*** End Patch"}),
    )
    .await
    .expect("update should work");
    tool.execute(
        patch_context.clone(),
        json!({"patch": "*** Begin Patch\n*** Delete File: remove.txt\n*** End Patch"}),
    )
    .await
    .expect("delete should work");
    assert_eq!(
        std::fs::read_to_string(workspace.join("result.txt")).expect("updated file should exist"),
        "updated\n"
    );
    assert!(!workspace.join("remove.txt").exists());

    let outside = fixture.path().join("outside.txt");
    let error = tool
        .execute(
            patch_context,
            json!({"patch": format!("*** Begin Patch\n*** Add File: {}\n+denied\n*** End Patch", outside.display())}),
        )
        .await
        .expect_err("workspace access must not patch outside its Workspace");
    assert!(matches!(error, ToolError::PathOutsideWorkspace(_)));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_tools_share_host_read_workspace_write_and_managed_deny() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let managed = fixture.path().join("managed");
    let workspace_context = context(
        &workspace,
        &managed,
        SessionId::new(),
        AgentId::new(),
        AccessPreset::Workspace,
    );
    let output = ExecCommandTool::default()
        .execute(
            workspace_context.clone(),
            json!({"cmd": "test -s /etc/hosts && printf readable"}),
        )
        .await
        .expect("workspace command should read ordinary host files");
    assert_eq!(output.value["exit_code"], 0);
    assert_eq!(output.value["output"], "readable");

    let outside = fixture.path().join("outside.txt");
    let denied = ExecCommandTool::default()
        .execute(
            workspace_context.clone(),
            json!({"cmd": format!("printf denied > '{}'", outside.display())}),
        )
        .await
        .expect("sandbox denial should be reported as command output");
    assert_ne!(denied.value["exit_code"], 0);
    assert!(!outside.exists());

    let managed_target = managed.join("project.db");
    std::fs::write(&managed_target, "managed-secret").expect("managed fixture should exist");
    let hidden = ExecCommandTool::default()
        .execute(
            workspace_context,
            json!({"cmd": format!("cat '{}'", managed_target.display())}),
        )
        .await
        .expect("managed read denial should be command output");
    assert_ne!(hidden.value["exit_code"], 0);
    assert!(
        !hidden.value["output"]
            .as_str()
            .unwrap_or_default()
            .contains("managed-secret")
    );
    let full_access = context(
        &workspace,
        &managed,
        SessionId::new(),
        AgentId::new(),
        AccessPreset::FullAccess,
    );
    let error = ApplyPatchTool
        .execute(
            full_access,
            json!({"patch": format!("*** Begin Patch\n*** Add File: {}\n+denied\n*** End Patch", managed_target.display())}),
        )
        .await
        .expect_err("managed state must remain denied under full access");
    assert!(matches!(error, ToolError::PathInsideManagedState(_)));
}
