//! Unified fail-closed child-process sandboxing.
//!
//! The manager/transform boundary, core-only child environment, platform
//! selection, and descendant cleanup are adapted from OpenAI Codex at commit
//! `b2dc8b3e4be4fe3a453d50e13835f707b258f15b`. PaperMachine owns the smaller
//! authorization model and uses this crate for both Agent commands and
//! Agent command execution and operating-system sandbox boundaries.

mod environment;
mod manager;
mod platform;
mod policy;
mod process;

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub use manager::PreparedSandboxCommand;
pub use manager::SandboxManager;
pub use manager::SandboxRequest;
pub use policy::FilesystemPolicy;
pub use policy::NetworkPolicy;
pub use policy::SandboxPolicy;
pub use process::configure_process_group;
pub use process::terminate_process_tree;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Dispatch the hidden same-executable Windows sandbox wrapper before the
/// server creates its own Tokio runtime. Other platforms return immediately.
pub fn run_windows_sandbox_wrapper_if_requested() {
    #[cfg(target_os = "windows")]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(
            codex_windows_sandbox::CODEX_WINDOWS_SANDBOX_ARG1,
        ))
    {
        codex_windows_sandbox::run_windows_sandbox_wrapper_main();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxBackend {
    MacOsSeatbelt,
    LinuxBubblewrap,
    WindowsRestrictedToken,
    Unrestricted,
}

impl SandboxBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MacOsSeatbelt => "macos_seatbelt",
            Self::LinuxBubblewrap => "linux_bubblewrap",
            Self::WindowsRestrictedToken => "windows_restricted_token",
            Self::Unrestricted => "unrestricted",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub backend: SandboxBackend,
}

#[derive(Clone, Copy, Default)]
pub struct SandboxExecutor;

impl SandboxExecutor {
    pub async fn prepare_shell(
        &self,
        cwd: &Path,
        sandbox_root: &Path,
        shell_command: &str,
        policy: SandboxPolicy,
        tty: bool,
    ) -> Result<PreparedSandboxCommand, ExecutionError> {
        if shell_command.trim().is_empty() {
            return Err(ExecutionError::InvalidCommand(
                "command must not be empty".to_string(),
            ));
        }
        let (program, args) = shell_program(shell_command, tty);
        SandboxManager
            .prepare(SandboxRequest::new(
                program,
                args,
                cwd,
                sandbox_root,
                policy,
            ))
            .await
    }

    pub async fn run_shell(
        &self,
        cwd: &Path,
        sandbox_root: &Path,
        shell_command: &str,
        policy: SandboxPolicy,
        cancellation: CancellationToken,
    ) -> Result<CommandOutput, ExecutionError> {
        if shell_command.trim().is_empty() {
            return Err(ExecutionError::InvalidCommand(
                "command must not be empty".to_string(),
            ));
        }
        let timeout = policy.timeout;
        let max_output_bytes = policy.max_output_bytes;
        let prepared = self
            .prepare_shell(cwd, sandbox_root, shell_command, policy, false)
            .await?;
        self.run_prepared_with_limits(prepared, timeout, max_output_bytes, cancellation)
            .await
    }

    pub async fn run(
        &self,
        request: SandboxRequest,
        cancellation: CancellationToken,
    ) -> Result<CommandOutput, ExecutionError> {
        let timeout = request.policy.timeout;
        let max_output_bytes = request.policy.max_output_bytes;
        let prepared = SandboxManager.prepare(request).await?;
        self.run_prepared_with_limits(prepared, timeout, max_output_bytes, cancellation)
            .await
    }

    async fn run_prepared_with_limits(
        &self,
        prepared: PreparedSandboxCommand,
        timeout: Duration,
        max_output_bytes: usize,
        cancellation: CancellationToken,
    ) -> Result<CommandOutput, ExecutionError> {
        let backend = prepared.backend();
        let mut command = prepared.into_command();
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| ExecutionError::Spawn(error.to_string()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or(ExecutionError::MissingPipe("stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ExecutionError::MissingPipe("stderr"))?;
        let stdout_task = tokio::spawn(drain_limited(stdout, max_output_bytes));
        let stderr_task = tokio::spawn(drain_limited(stderr, max_output_bytes));

        let status = tokio::select! {
            result = child.wait() => result?,
            _ = cancellation.cancelled() => {
                terminate_process_tree(&mut child).await;
                return Err(ExecutionError::Cancelled);
            },
            _ = tokio::time::sleep(timeout) => {
                terminate_process_tree(&mut child).await;
                return Err(ExecutionError::Timeout(timeout));
            },
        };
        finish_output(status, backend, stdout_task, stderr_task).await
    }
}

#[cfg(target_os = "windows")]
fn shell_program(shell_command: &str, _tty: bool) -> (OsString, Vec<OsString>) {
    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"));
    (
        shell,
        vec![
            OsString::from("/D"),
            OsString::from("/S"),
            OsString::from("/C"),
            OsString::from(shell_command),
        ],
    )
}

#[cfg(target_os = "macos")]
fn shell_program(shell_command: &str, tty: bool) -> (OsString, Vec<OsString>) {
    if tty {
        (
            OsString::from("/usr/bin/script"),
            vec![
                OsString::from("-q"),
                OsString::from("/dev/null"),
                OsString::from("/bin/zsh"),
                OsString::from("-lc"),
                OsString::from(shell_command),
            ],
        )
    } else {
        (
            OsString::from("/bin/zsh"),
            vec![OsString::from("-lc"), OsString::from(shell_command)],
        )
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn shell_program(shell_command: &str, tty: bool) -> (OsString, Vec<OsString>) {
    if tty {
        (
            OsString::from("/usr/bin/script"),
            vec![
                OsString::from("-qfec"),
                OsString::from(shell_command),
                OsString::from("/dev/null"),
            ],
        )
    } else {
        (
            OsString::from("/bin/sh"),
            vec![OsString::from("-c"), OsString::from(shell_command)],
        )
    }
}

#[cfg(not(any(unix, target_os = "windows")))]
fn shell_program(shell_command: &str, _tty: bool) -> (OsString, Vec<OsString>) {
    (
        OsString::from("sh"),
        vec![OsString::from("-c"), OsString::from(shell_command)],
    )
}

async fn finish_output(
    status: ExitStatus,
    backend: SandboxBackend,
    stdout_task: tokio::task::JoinHandle<Result<LimitedOutput, std::io::Error>>,
    stderr_task: tokio::task::JoinHandle<Result<LimitedOutput, std::io::Error>>,
) -> Result<CommandOutput, ExecutionError> {
    let stdout = stdout_task
        .await
        .map_err(|error| ExecutionError::Output(error.to_string()))??;
    let stderr = stderr_task
        .await
        .map_err(|error| ExecutionError::Output(error.to_string()))??;
    Ok(CommandOutput {
        exit_code: status.code(),
        success: status.success(),
        stdout: stdout.text,
        stderr: stderr.text,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        backend,
    })
}

struct LimitedOutput {
    text: String,
    truncated: bool,
}

async fn drain_limited<R>(mut reader: R, limit: usize) -> Result<LimitedOutput, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut kept = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        let take = read.min(remaining);
        kept.extend_from_slice(&buffer[..take]);
        truncated |= take < read;
    }
    Ok(LimitedOutput {
        text: String::from_utf8_lossy(&kept).into_owned(),
        truncated,
    })
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("invalid command: {0}")]
    InvalidCommand(String),
    #[error("invalid sandbox policy: {0}")]
    InvalidPolicy(String),
    #[error("sandbox cwd is not a directory: {0}")]
    InvalidWorkspace(String),
    #[error("sandbox is unavailable: {0}")]
    SandboxUnavailable(String),
    #[error("command timed out after {0:?}")]
    Timeout(Duration),
    #[error("command was cancelled")]
    Cancelled,
    #[error("failed to spawn command: {0}")]
    Spawn(String),
    #[error("command {0} pipe is missing")]
    MissingPipe(&'static str),
    #[error("failed to collect command output: {0}")]
    Output(String),
    #[error("execution I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
