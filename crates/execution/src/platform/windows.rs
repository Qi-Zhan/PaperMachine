//! Native Windows wrapper transformation using the pinned Codex backend.

use crate::ExecutionError;
use crate::FilesystemPolicy;
use crate::NetworkPolicy;
use crate::SandboxBackend;
use crate::manager::ResolvedSandboxPolicy;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_windows_sandbox::WindowsSandboxProxySettingsMode;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use tokio::process::Command;

pub(crate) fn prepare(
    program: OsString,
    args: Vec<OsString>,
    cwd: &Path,
    policy: &ResolvedSandboxPolicy,
    environment: &HashMap<OsString, OsString>,
) -> Result<(Command, SandboxBackend), ExecutionError> {
    let workspace_roots = absolute_paths(&policy.workspace_roots)?;
    let cwd = absolute_path(cwd)?;
    let network = if policy.network == NetworkPolicy::Allow {
        NetworkSandboxPolicy::Enabled
    } else {
        NetworkSandboxPolicy::Restricted
    };
    let permission_profile = if policy.filesystem_read == FilesystemPolicy::Host
        && policy.filesystem_write == FilesystemPolicy::Host
    {
        PermissionProfile::from_runtime_permissions(
            &FileSystemSandboxPolicy::unrestricted(),
            network,
        )
    } else {
        PermissionProfile::workspace_write_with(&workspace_roots, network, false, false)
            .materialize_project_roots_with_workspace_roots(&workspace_roots)
    };
    let read_roots =
        (policy.filesystem_read == FilesystemPolicy::Scoped).then(|| policy.read_roots.clone());
    let write_roots =
        (policy.filesystem_write == FilesystemPolicy::Scoped).then(|| policy.write_roots.clone());
    let deny_read = policy
        .unreadable_roots
        .iter()
        .chain(policy.sensitive_paths.iter())
        .cloned()
        .collect::<Vec<_>>();
    let deny_write = policy
        .unreadable_roots
        .iter()
        .chain(policy.read_only_roots.iter())
        .chain(policy.sensitive_paths.iter())
        .cloned()
        .collect::<Vec<_>>();
    let deny_read = absolute_paths(&deny_read)?;
    let deny_write = absolute_paths(&deny_write)?;
    let environment = unicode_environment(environment)?;
    let mut inner = vec![program.to_string_lossy().into_owned()];
    inner.extend(
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    let wrapper_args =
        codex_windows_sandbox::create_windows_sandbox_command_args_for_permission_profile(
            inner,
            &cwd,
            &workspace_roots,
            &environment,
            &permission_profile,
            WindowsSandboxLevel::Elevated,
            false,
            false,
            None,
            WindowsSandboxProxySettingsMode::Reconcile,
            read_roots.as_deref(),
            true,
            write_roots.as_deref(),
            &deny_read,
            &deny_write,
            &policy.platform_state_root,
        );
    let executable = std::env::current_exe().map_err(|error| {
        ExecutionError::SandboxUnavailable(format!(
            "failed to resolve the Windows sandbox wrapper executable: {error}"
        ))
    })?;
    let mut command = Command::new(executable);
    command.args(wrapper_args);
    Ok((command, SandboxBackend::WindowsRestrictedToken))
}

fn absolute_path(path: &Path) -> Result<AbsolutePathBuf, ExecutionError> {
    AbsolutePathBuf::from_absolute_path(path).map_err(|error| {
        ExecutionError::InvalidPolicy(format!(
            "invalid native Windows sandbox path {}: {error}",
            path.display()
        ))
    })
}

fn absolute_paths(paths: &[PathBuf]) -> Result<Vec<AbsolutePathBuf>, ExecutionError> {
    paths.iter().map(|path| absolute_path(path)).collect()
}

fn unicode_environment(
    environment: &HashMap<OsString, OsString>,
) -> Result<HashMap<String, String>, ExecutionError> {
    environment
        .iter()
        .map(|(key, value)| {
            let key = key.clone().into_string().map_err(|_| {
                ExecutionError::InvalidPolicy(
                    "Windows child environment contains a non-Unicode name".to_string(),
                )
            })?;
            let value = value.clone().into_string().map_err(|_| {
                ExecutionError::InvalidPolicy(format!(
                    "Windows child environment variable {key} is non-Unicode"
                ))
            })?;
            Ok((key, value))
        })
        .collect()
}
