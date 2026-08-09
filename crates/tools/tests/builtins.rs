use async_trait::async_trait;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::AuthorizationContext;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionId;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::ToolEffectDisposition;
use papermachine_protocol::TurnId;
use papermachine_protocol::WorkflowId;
use papermachine_tools::ExecCommandTool;
use papermachine_tools::FetchUrlTool;
use papermachine_tools::ReadFileTool;
use papermachine_tools::ToolCatalog;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolError;
use papermachine_tools::ToolExecutor;
use papermachine_tools::ToolOutput;
use papermachine_tools::WriteFileTool;
use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
struct ProjectProbeTool(&'static str);

#[async_trait]
impl ToolExecutor for ProjectProbeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "project_probe".to_string(),
            description: self.0.to_string(),
            input_schema: json!({"type": "object"}),
            supports_parallel: false,
        }
    }

    fn effect_disposition(&self) -> ToolEffectDisposition {
        ToolEffectDisposition::Pure
    }

    async fn execute(
        &self,
        _context: ToolContext,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            value: arguments,
            summary: "runtime probe completed".to_string(),
        })
    }
}

fn context(root: &std::path::Path) -> ToolContext {
    context_with_access(root, AccessPreset::Research)
}

fn context_with_access(root: &std::path::Path, access: AccessPreset) -> ToolContext {
    let fixture_root = root.parent().unwrap_or(root);
    let sandbox_root = fixture_root.join("agent-sandbox");
    let protected_root = fixture_root.join("managed-state");
    std::fs::create_dir_all(&sandbox_root).expect("sandbox fixture should be created");
    std::fs::create_dir_all(&protected_root).expect("managed fixture should be created");
    let workspace_root = root
        .canonicalize()
        .expect("workspace fixture should canonicalize");
    let protected_root = protected_root
        .canonicalize()
        .expect("managed fixture should canonicalize");
    let authorization = AuthorizationContext::materialize(
        access,
        workspace_root.to_string_lossy().into_owned(),
        workspace_root.to_string_lossy().into_owned(),
        vec![protected_root.to_string_lossy().into_owned()],
    )
    .expect("fixture policy should materialize");
    ToolContext {
        project_id: ProjectId::new(),
        session_id: SessionId::new(),
        turn_id: TurnId::new(),
        workflow_id: Some(WorkflowId::new()),
        action_invocation_id: None,
        action_attempt_id: None,
        effect_id: "test-effect".to_string(),
        sandbox_root,
        authorization,
        cancellation: CancellationToken::new(),
    }
}

#[test]
fn catalog_filters_declared_workspace_tools_for_each_access_profile() {
    let catalog = ToolCatalog::builder()
        .register_workspace(ReadFileTool)
        .expect("read tool should register")
        .register_workspace(WriteFileTool)
        .expect("write tool should register")
        .register_workspace(ExecCommandTool)
        .expect("command tool should register")
        .register_workspace(FetchUrlTool)
        .expect("fetch tool should register")
        .register_project(ProjectProbeTool("Project probe"))
        .expect("Project tool should register")
        .build();
    let names = |access| {
        catalog
            .materialize_action_tools(
                &[
                    "read_file".to_string(),
                    "write_file".to_string(),
                    "exec_command".to_string(),
                    "fetch_url".to_string(),
                ],
                access,
                true,
            )
            .expect("Action tools should materialize")
            .definitions
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>()
    };

    assert_eq!(names(AccessPreset::ModelOnly), Vec::<String>::new());
    assert_eq!(names(AccessPreset::ReadOnly), vec!["read_file"]);
    assert_eq!(
        names(AccessPreset::Workspace),
        vec!["exec_command", "read_file", "write_file"]
    );
    assert_eq!(
        names(AccessPreset::Research),
        vec!["exec_command", "fetch_url", "read_file", "write_file"]
    );
    assert_eq!(
        names(AccessPreset::FullAccess),
        vec!["exec_command", "fetch_url", "read_file", "write_file"]
    );
    assert!(!names(AccessPreset::FullAccess).contains(&"project_probe".to_string()));
}

#[tokio::test]
async fn project_tools_are_admitted_only_for_declared_workflow_actions() {
    let directory = tempdir().expect("temporary directory should be created");
    let context = context_with_access(directory.path(), AccessPreset::ModelOnly);
    let catalog = ToolCatalog::builder()
        .register_workspace(ReadFileTool)
        .expect("read tool should register")
        .register_project(ProjectProbeTool("Project probe"))
        .expect("Project tool should register")
        .build();

    let undeclared = catalog
        .materialize_action_tools(&[], AccessPreset::FullAccess, true)
        .expect("empty Action tools should materialize");
    assert_eq!(
        undeclared
            .definitions
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new()
    );
    let snapshot = catalog
        .materialize_action_tools(
            &["read_file".to_string(), "project_probe".to_string()],
            AccessPreset::ModelOnly,
            true,
        )
        .expect("Action tools should materialize");
    assert_eq!(
        snapshot
            .definitions
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["project_probe"]
    );
    let registry = catalog
        .registry_for_snapshot(&snapshot)
        .expect("Workflow registry should rebuild");
    let output = registry
        .execute("project_probe", context.clone(), json!({"value": 7}))
        .await
        .expect("declared Project tool should execute");
    assert_eq!(output.value, json!({"value": 7}));
    let denied = registry
        .execute("read_file", context, json!({"path": "anything"}))
        .await
        .expect_err("filtered Workspace tool must not enter the registry");
    assert!(matches!(denied, ToolError::UnknownTool(_)));
}

#[test]
fn catalog_rejects_registration_conflicts_unknown_requests_and_definition_drift() {
    assert!(
        ToolCatalog::builder()
            .register_workspace(ReadFileTool)
            .expect("first registration should succeed")
            .register_project(ReadFileTool)
            .is_err()
    );

    let catalog = ToolCatalog::builder()
        .register_project(ProjectProbeTool("version one"))
        .expect("Project tool should register")
        .build();
    let unknown = catalog
        .materialize_action_tools(&["missing_tool".to_string()], AccessPreset::Research, true)
        .expect_err("unknown Action tool must fail validation");
    assert!(matches!(unknown, ToolError::UnknownTool(_)));
    assert!(
        catalog
            .materialize_action_tools(
                &["project_probe".to_string(), "project_probe".to_string()],
                AccessPreset::Research,
                true,
            )
            .is_err()
    );

    let snapshot = catalog
        .materialize_action_tools(&["project_probe".to_string()], AccessPreset::Research, true)
        .expect("snapshot should materialize");
    assert!(
        ToolCatalog::default()
            .registry_for_snapshot(&snapshot)
            .is_err()
    );
    let mut corrupt = snapshot.clone();
    corrupt.sha256 = "0".repeat(64);
    assert!(catalog.registry_for_snapshot(&corrupt).is_err());
    let changed = ToolCatalog::builder()
        .register_project(ProjectProbeTool("version two"))
        .expect("changed Project tool should register")
        .build();
    assert!(changed.registry_for_snapshot(&snapshot).is_err());
}

#[test]
fn one_persistent_agent_can_receive_different_exact_registries_per_action() {
    let catalog = ToolCatalog::builder()
        .register_workspace(ReadFileTool)
        .expect("read tool should register")
        .register_workspace(WriteFileTool)
        .expect("write tool should register")
        .build();
    let read_turn = catalog
        .materialize_action_tools(&["read_file".to_string()], AccessPreset::Workspace, true)
        .expect("read Action should materialize");
    let write_turn = catalog
        .materialize_action_tools(&["write_file".to_string()], AccessPreset::Workspace, true)
        .expect("write Action should materialize");

    assert_ne!(read_turn.sha256, write_turn.sha256);
    assert_eq!(
        catalog
            .registry_for_snapshot(&read_turn)
            .expect("read Registry should rebuild")
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>(),
        vec!["read_file"]
    );
    assert_eq!(
        catalog
            .registry_for_snapshot(&write_turn)
            .expect("write Registry should rebuild")
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>(),
        vec!["write_file"]
    );
}

#[tokio::test]
async fn builtins_recheck_access_without_the_registry() {
    let directory = tempdir().expect("temporary directory should be created");
    let write_error = WriteFileTool
        .execute(
            context_with_access(directory.path(), AccessPreset::ReadOnly),
            json!({"path": "forbidden.txt", "content": "no"}),
        )
        .await
        .expect_err("read-only profile must reject direct writes");
    let command_error = ExecCommandTool
        .execute(
            context_with_access(directory.path(), AccessPreset::ReadOnly),
            json!({"command": "true"}),
        )
        .await
        .expect_err("read-only profile must reject direct commands");
    let fetch_error = FetchUrlTool
        .execute(
            context_with_access(directory.path(), AccessPreset::Workspace),
            json!({"url": "https://example.com"}),
        )
        .await
        .expect_err("workspace profile must reject direct network fetches");
    assert!(matches!(write_error, ToolError::PermissionDenied { .. }));
    assert!(matches!(command_error, ToolError::PermissionDenied { .. }));
    assert!(matches!(fetch_error, ToolError::PermissionDenied { .. }));
}

#[tokio::test]
async fn full_access_can_read_and_write_outside_the_workspace() {
    let directory = tempdir().expect("temporary directory should be created");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace should be created");
    let outside = directory.path().join("outside.txt");
    std::fs::write(&outside, "host data").expect("outside probe should be written");
    let access = context_with_access(&workspace, AccessPreset::FullAccess);
    let read = ReadFileTool
        .execute(access.clone(), json!({"path": outside}))
        .await
        .expect("full access should read an absolute host path");
    assert_eq!(read.value["content"], "host data");

    let command = format!("/bin/cat '{}'", outside.display());
    #[cfg(target_os = "macos")]
    let command = ExecCommandTool
        .execute(access.clone(), json!({"command": command}))
        .await
        .expect("full access should run an isolated host command");
    #[cfg(target_os = "macos")]
    assert_eq!(command.value["stdout"], "host data");
    #[cfg(target_os = "macos")]
    assert_eq!(command.value["sandbox_backend"], "macos_seatbelt");
    #[cfg(not(target_os = "macos"))]
    assert!(matches!(
        ExecCommandTool
            .execute(access.clone(), json!({"command": command}))
            .await
            .expect_err("full access must fail closed without a protected sandbox"),
        ToolError::IsolationUnavailable(_)
    ));

    let written = directory.path().join("written-outside.txt");
    WriteFileTool
        .execute(
            access,
            json!({"path": written, "content": "allowed by explicit grant"}),
        )
        .await
        .expect("full access should write an absolute host path");
    assert_eq!(
        std::fs::read_to_string(written).expect("outside write should exist"),
        "allowed by explicit grant"
    );
}

#[tokio::test]
async fn full_access_still_excludes_papermachine_managed_state() {
    let directory = tempdir().expect("temporary directory should be created");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace should be created");
    let access = context_with_access(&workspace, AccessPreset::FullAccess);
    let managed_secret =
        std::path::PathBuf::from(access.authorization.filesystem.managed_roots[0].clone())
            .join("project.db");
    std::fs::write(&managed_secret, "private state").expect("managed fixture should be written");

    let read_error = ReadFileTool
        .execute(access.clone(), json!({"path": managed_secret}))
        .await
        .expect_err("managed state must not be model-readable");
    let write_error = WriteFileTool
        .execute(
            access,
            json!({"path": managed_secret, "content": "corrupted"}),
        )
        .await
        .expect_err("managed state must not be model-writable");
    assert!(matches!(read_error, ToolError::PathInsideManagedState(_)));
    assert!(matches!(write_error, ToolError::PathInsideManagedState(_)));
}

#[tokio::test]
async fn write_then_read_stays_in_workspace() {
    let directory = tempdir().expect("temporary directory should be created");
    WriteFileTool
        .execute(
            context(directory.path()),
            json!({"path": "notes/result.md", "content": "evidence"}),
        )
        .await
        .expect("write should succeed");
    let output = ReadFileTool
        .execute(
            context(directory.path()),
            json!({"path": "notes/result.md"}),
        )
        .await
        .expect("read should succeed");
    assert_eq!(output.value["content"], "evidence");
}

#[tokio::test]
async fn parent_path_escape_is_rejected() {
    let directory = tempdir().expect("temporary directory should be created");
    let error = ReadFileTool
        .execute(context(directory.path()), json!({"path": "../secret"}))
        .await
        .expect_err("escape should fail");
    assert!(matches!(error, ToolError::PathOutsideWorkspace(_)));
}

#[tokio::test]
async fn workspace_presets_cannot_read_credentials_or_write_protected_metadata() {
    let directory = tempdir().expect("temporary directory should be created");
    std::fs::write(directory.path().join(".env"), "API_KEY=secret")
        .expect("credential fixture should be written");
    std::fs::create_dir(directory.path().join(".git"))
        .expect("Git metadata fixture should be created");

    let read_error = ReadFileTool
        .execute(
            context_with_access(directory.path(), AccessPreset::Research),
            json!({"path": ".env"}),
        )
        .await
        .expect_err("research preset must not expose Workspace credentials");
    let write_error = WriteFileTool
        .execute(
            context_with_access(directory.path(), AccessPreset::Workspace),
            json!({"path": ".git/config", "content": "[malicious]"}),
        )
        .await
        .expect_err("workspace preset must not mutate protected metadata");

    assert!(matches!(read_error, ToolError::SensitiveWorkspacePath(_)));
    assert!(matches!(
        write_error,
        ToolError::ProtectedWorkspaceMetadata(_)
    ));
    assert!(!directory.path().join(".git/config").exists());
}

#[tokio::test]
async fn command_output_is_structured() {
    let directory = tempdir().expect("temporary directory should be created");
    let output = ExecCommandTool
        .execute(
            context(directory.path()),
            json!({"command": "printf 'paper-machine'"}),
        )
        .await
        .expect("command should run");
    assert_eq!(output.value["exit_code"], 0);
    assert_eq!(output.value["stdout"], "paper-machine");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn command_heredoc_uses_the_session_temp_directory() {
    let directory = tempdir().expect("temporary directory should be created");
    let output = ExecCommandTool
        .execute(
            context(directory.path()),
            json!({"command": "python3 - <<'PY'\nprint('heredoc-ok')\nPY"}),
        )
        .await
        .expect("heredoc command should run inside the sandbox");
    assert_eq!(output.value["exit_code"], 0);
    assert_eq!(output.value["stdout"], "heredoc-ok\n");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn command_cannot_read_or_write_outside_its_workspace() {
    let directory = tempdir().expect("temporary directory should be created");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace should be created");
    let secret = directory.path().join("host-secret.txt");
    std::fs::write(&secret, "must-not-leak").expect("probe secret should be written");
    let outside_write = directory.path().join("escaped.txt");
    let command = format!(
        "if /bin/cat '{}' >/dev/null 2>&1; then printf read-leaked; else printf read-denied; fi; if printf escaped >'{}' 2>/dev/null; then printf write-leaked; else printf write-denied; fi",
        secret.display(),
        outside_write.display(),
    );
    let output = ExecCommandTool
        .execute(context(&workspace), json!({"command": command}))
        .await
        .expect("sandboxed command should run");
    assert_eq!(output.value["stdout"], "read-deniedwrite-denied");
    assert!(!outside_write.exists());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn command_obeys_the_same_credential_and_metadata_policy_as_direct_tools() {
    let directory = tempdir().expect("temporary directory should be created");
    std::fs::write(directory.path().join(".env.local"), "API_KEY=must-not-leak")
        .expect("credential fixture should be written");
    std::fs::create_dir(directory.path().join(".git"))
        .expect("Git metadata fixture should be created");
    std::fs::write(directory.path().join(".git/config"), "original")
        .expect("Git config fixture should be written");
    let command = "if /bin/cat .env.local >/dev/null 2>&1; then printf credential-leaked; else printf credential-denied; fi; if printf changed >.git/config 2>/dev/null; then printf metadata-written; else printf metadata-denied; fi";
    let output = ExecCommandTool
        .execute(context(directory.path()), json!({"command": command}))
        .await
        .expect("sandboxed command should run");
    assert_eq!(output.value["stdout"], "credential-deniedmetadata-denied");
    assert_eq!(
        std::fs::read_to_string(directory.path().join(".git/config"))
            .expect("Git config should remain readable"),
        "original"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn full_access_command_cannot_read_papermachine_managed_state() {
    let directory = tempdir().expect("temporary directory should be created");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace should be created");
    let access = context_with_access(&workspace, AccessPreset::FullAccess);
    let managed_secret =
        std::path::PathBuf::from(access.authorization.filesystem.managed_roots[0].clone())
            .join("project.db");
    std::fs::write(&managed_secret, "private state").expect("managed fixture should be written");
    let command = format!(
        "if /bin/cat '{}' >/dev/null 2>&1; then printf leaked; else printf denied; fi",
        managed_secret.display()
    );
    let output = ExecCommandTool
        .execute(access, json!({"command": command}))
        .await
        .expect("protected full-access command should run");
    assert_eq!(output.value["stdout"], "denied");
}
