use papermachine_protocol::AccessPreset;
use papermachine_protocol::AuthorizationContext;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionId;
use papermachine_protocol::ToolEffectDisposition;
use papermachine_protocol::TurnId;
use papermachine_protocol::WorkflowId;
use papermachine_tools::ExecCommandTool;
use papermachine_tools::FetchUrlTool;
use papermachine_tools::ReadFileTool;
use papermachine_tools::ToolContext;
use papermachine_tools::ToolError;
use papermachine_tools::ToolExecutor;
use papermachine_tools::ToolRegistry;
use papermachine_tools::WriteFileTool;
use serde_json::json;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

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
        vec![workspace_root.to_string_lossy().into_owned()],
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
fn registry_exposes_exact_tools_for_each_access_profile() {
    let registry = ToolRegistry::builder()
        .register(ReadFileTool)
        .expect("read tool should register")
        .register(WriteFileTool)
        .expect("write tool should register")
        .register(ExecCommandTool)
        .expect("command tool should register")
        .register(FetchUrlTool)
        .expect("fetch tool should register")
        .build();
    let names = |access| {
        let directory = tempdir().expect("temporary workspace should be created");
        let context = context_with_access(directory.path(), access);
        registry
            .definitions_for(&context.authorization)
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
    assert_eq!(
        registry.effect_disposition("read_file"),
        Some(ToolEffectDisposition::Pure)
    );
    assert_eq!(
        registry.effect_disposition("fetch_url"),
        Some(ToolEffectDisposition::Pure)
    );
    assert_eq!(
        registry.effect_disposition("write_file"),
        Some(ToolEffectDisposition::Idempotent)
    );
    assert_eq!(
        registry.effect_disposition("exec_command"),
        Some(ToolEffectDisposition::Unknown)
    );
}

#[tokio::test]
async fn registry_rejects_a_hidden_tool_call() {
    let directory = tempdir().expect("temporary directory should be created");
    std::fs::write(directory.path().join("evidence.txt"), "private")
        .expect("probe should be written");
    let registry = ToolRegistry::builder()
        .register(ReadFileTool)
        .expect("read tool should register")
        .build();
    let error = registry
        .execute(
            "read_file",
            context_with_access(directory.path(), AccessPreset::ModelOnly),
            json!({"path": "evidence.txt"}),
        )
        .await
        .expect_err("model-only profile must reject a forged read call");
    assert!(matches!(error, ToolError::PermissionDenied { .. }));
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
