use crate::ExecutionError;
use crate::SandboxBackend;
use crate::manager::ResolvedSandboxPolicy;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use tokio::process::Command;

#[cfg(target_os = "linux")]
#[path = "platform/linux.rs"]
mod current;
#[cfg(target_os = "macos")]
#[path = "platform/macos.rs"]
mod current;
#[cfg(target_os = "windows")]
#[path = "platform/windows.rs"]
mod current;

pub(crate) fn prepare(
    program: OsString,
    args: Vec<OsString>,
    cwd: &Path,
    policy: &ResolvedSandboxPolicy,
    environment: &HashMap<OsString, OsString>,
) -> Result<(Command, SandboxBackend), ExecutionError> {
    if !policy.requires_platform_sandbox {
        let mut command = Command::new(program);
        command.args(args);
        return Ok((command, SandboxBackend::Unrestricted));
    }
    prepare_restricted(program, args, cwd, policy, environment)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn prepare_restricted(
    program: OsString,
    args: Vec<OsString>,
    cwd: &Path,
    policy: &ResolvedSandboxPolicy,
    environment: &HashMap<OsString, OsString>,
) -> Result<(Command, SandboxBackend), ExecutionError> {
    current::prepare(program, args, cwd, policy, environment)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn prepare_restricted(
    _program: OsString,
    _args: Vec<OsString>,
    _cwd: &Path,
    _policy: &ResolvedSandboxPolicy,
    _environment: &HashMap<OsString, OsString>,
) -> Result<(Command, SandboxBackend), ExecutionError> {
    Err(ExecutionError::SandboxUnavailable(
        "no fail-closed sandbox backend is implemented for this platform".to_string(),
    ))
}
