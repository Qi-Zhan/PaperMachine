//! Fail-closed process execution adapted from Codex's sandbox and exec layers.
//!
//! The execution layer owns process lifetime, environment isolation, output
//! limits, and platform sandbox selection. Model-visible tools only translate
//! their JSON protocol into this API.

use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkPolicy {
    Deny,
    Allow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemPolicy {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    pub filesystem: FilesystemPolicy,
    pub network: NetworkPolicy,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            filesystem: FilesystemPolicy::WorkspaceWrite,
            network: NetworkPolicy::Deny,
            timeout: Duration::from_secs(60),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxBackend {
    MacOsSeatbelt,
    Unrestricted,
}

impl SandboxBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MacOsSeatbelt => "macos_seatbelt",
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
    pub async fn run(
        &self,
        workspace_root: &Path,
        shell_command: &str,
        policy: SandboxPolicy,
        cancellation: CancellationToken,
    ) -> Result<CommandOutput, ExecutionError> {
        if shell_command.trim().is_empty() {
            return Err(ExecutionError::InvalidCommand(
                "command must not be empty".to_string(),
            ));
        }
        tokio::fs::create_dir_all(workspace_root).await?;
        let workspace = tokio::fs::canonicalize(workspace_root).await?;
        let sandbox_home = workspace.join(".sandbox-home");
        let sandbox_tmp = workspace.join(".tmp");
        tokio::fs::create_dir_all(&sandbox_home).await?;
        tokio::fs::create_dir_all(&sandbox_tmp).await?;

        let (mut command, backend) = sandboxed_shell_command(&workspace, shell_command, &policy)?;
        configure_process_group(&mut command);
        let mut child = command
            .current_dir(&workspace)
            .env_clear()
            .env(
                "PATH",
                "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            )
            .env("HOME", &sandbox_home)
            .env("TMPDIR", &sandbox_tmp)
            .env("TMPPREFIX", sandbox_tmp.join("zsh"))
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
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
        let stdout_task = tokio::spawn(drain_limited(stdout, policy.max_output_bytes));
        let stderr_task = tokio::spawn(drain_limited(stderr, policy.max_output_bytes));

        let status = tokio::select! {
            result = child.wait() => result?,
            _ = cancellation.cancelled() => {
                terminate_process_tree(&mut child).await;
                return Err(ExecutionError::Cancelled);
            },
            _ = tokio::time::sleep(policy.timeout) => {
                terminate_process_tree(&mut child).await;
                return Err(ExecutionError::Timeout(policy.timeout));
            },
        };
        finish_output(status, backend, stdout_task, stderr_task).await
    }
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

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

async fn terminate_process_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(process_id) = child.id() {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &format!("-{process_id}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(target_os = "macos")]
fn sandboxed_shell_command(
    workspace: &Path,
    shell_command: &str,
    policy: &SandboxPolicy,
) -> Result<(Command, SandboxBackend), ExecutionError> {
    if policy.filesystem == FilesystemPolicy::DangerFullAccess {
        let mut command = Command::new("/bin/zsh");
        command.args(["-c", shell_command]);
        return Ok((command, SandboxBackend::Unrestricted));
    }
    let sandbox = Path::new("/usr/bin/sandbox-exec");
    if !sandbox.is_file() {
        return Err(ExecutionError::SandboxUnavailable(
            "macOS sandbox-exec is not installed".to_string(),
        ));
    }
    let workspace = seatbelt_literal(workspace);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| seatbelt_literal(&path));
    let mut rules = vec!["(deny file-write*)".to_string()];
    if policy.network == NetworkPolicy::Deny {
        rules.push("(deny network*)".to_string());
    }
    for root in [
        "/Volumes",
        "/private/tmp",
        "/tmp",
        "/var/tmp",
        "/private/var/folders",
    ] {
        rules.push(format!("(deny file-read* (subpath \"{root}\"))"));
    }
    if let Some(home) = home {
        rules.push(format!("(deny file-read* (subpath \"{home}\"))"));
    }
    rules.push(format!("(allow file-read* (subpath \"{workspace}\"))"));
    if policy.filesystem == FilesystemPolicy::WorkspaceWrite {
        rules.push(format!("(allow file-write* (subpath \"{workspace}\"))"));
    }
    rules.push("(allow file-write* (literal \"/dev/null\"))".to_string());
    let profile = format!("(version 1)\n(allow default)\n{}", rules.join("\n"));
    let mut command = Command::new(sandbox);
    command.args(["-p", &profile, "/bin/zsh", "-c", shell_command]);
    Ok((command, SandboxBackend::MacOsSeatbelt))
}

#[cfg(not(target_os = "macos"))]
fn sandboxed_shell_command(
    _workspace: &Path,
    shell_command: &str,
    policy: &SandboxPolicy,
) -> Result<(Command, SandboxBackend), ExecutionError> {
    if policy.filesystem == FilesystemPolicy::DangerFullAccess {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", shell_command]);
        return Ok((command, SandboxBackend::Unrestricted));
    }
    Err(ExecutionError::SandboxUnavailable(
        "no fail-closed sandbox backend is implemented for this platform".to_string(),
    ))
}

#[cfg(target_os = "macos")]
fn seatbelt_literal(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
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
