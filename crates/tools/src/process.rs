use crate::ToolContext;
use crate::ToolError;
use crate::path::resolve_tool_path;
use papermachine_execution::SandboxExecutor;
use papermachine_execution::SandboxPolicy;
use papermachine_execution::terminate_process_tree;
use papermachine_protocol::AgentId;
use papermachine_protocol::PathOperation;
use papermachine_protocol::SessionId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const MAX_BUFFER_BYTES: usize = 1024 * 1024;
const DEFAULT_YIELD_MS: u64 = 10_000;
const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
const DEFAULT_OUTPUT_TOKENS: usize = 10_000;
const MAX_OUTPUT_TOKENS: usize = 50_000;
const MAX_ENTRIES_PER_AGENT: usize = 64;

#[derive(Clone)]
pub struct ProcessTable {
    inner: Arc<ProcessTableInner>,
}

struct ProcessTableInner {
    entries: Mutex<HashMap<String, Arc<ProcessEntry>>>,
    next_id: AtomicU64,
    max_per_agent: usize,
    shutdown: CancellationToken,
}

struct ProcessEntry {
    id: String,
    session_id: SessionId,
    agent_id: AgentId,
    authorization_sha256: String,
    tty: bool,
    backend: &'static str,
    stdin: Mutex<Option<ChildStdin>>,
    output: Mutex<OutputBuffer>,
    output_ready: Notify,
    status: watch::Receiver<ProcessStatus>,
    interaction: Mutex<()>,
    cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
enum ProcessStatus {
    Running,
    Exited(Option<i32>),
    Failed(String),
    Cancelled,
}

impl ProcessStatus {
    const fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Debug)]
pub(crate) struct ProcessOutput {
    pub process_id: Option<String>,
    pub exit_code: Option<i32>,
    pub output: String,
    pub output_truncated: bool,
    pub backend: &'static str,
}

impl ProcessTable {
    pub fn new(max_per_agent: usize, shutdown: CancellationToken) -> Self {
        Self {
            inner: Arc::new(ProcessTableInner {
                entries: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                max_per_agent: max_per_agent.max(1),
                shutdown,
            }),
        }
    }

    pub(crate) async fn exec(
        &self,
        context: &ToolContext,
        command: &str,
        workdir: Option<&str>,
        yield_time_ms: Option<u64>,
        tty: bool,
        max_output_tokens: Option<usize>,
    ) -> Result<ProcessOutput, ToolError> {
        if command.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: "exec_command".to_string(),
                message: "cmd must not be empty".to_string(),
            });
        }
        let workdir = resolve_workdir(context, workdir).await?;
        let policy = SandboxPolicy::from_authorization(
            &context.authorization,
            Duration::from_secs(24 * 60 * 60),
        )
        .map_err(map_execution_error)?;
        let prepared = SandboxExecutor
            .prepare_shell(&workdir, &context.sandbox_root, command, policy, tty)
            .await
            .map_err(map_execution_error)?;
        let backend = prepared.backend().as_str();
        let mut child = prepared.into_command();
        child
            .kill_on_drop(true)
            .stdin(if tty {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = child
            .spawn()
            .map_err(|error| ToolError::Execution(error.to_string()))?;

        let authorization_sha256 = context
            .authorization
            .policy_sha256()
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let id = format!("p{}", self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (status_tx, status) = watch::channel(ProcessStatus::Running);
        let entry = Arc::new(ProcessEntry {
            id: id.clone(),
            session_id: context.session_id,
            agent_id: context.agent_id,
            authorization_sha256,
            tty,
            backend,
            stdin: Mutex::new(child.stdin.take()),
            output: Mutex::new(OutputBuffer::default()),
            output_ready: Notify::new(),
            status,
            interaction: Mutex::new(()),
            cancellation: context.cancellation.child_token(),
        });

        let at_process_limit = {
            let mut entries = self.inner.entries.lock().await;
            let total = entries
                .values()
                .filter(|entry| {
                    entry.session_id == context.session_id && entry.agent_id == context.agent_id
                })
                .count();
            if total >= MAX_ENTRIES_PER_AGENT {
                entries.retain(|_, entry| {
                    entry.session_id != context.session_id
                        || entry.agent_id != context.agent_id
                        || entry.status.borrow().is_running()
                });
            }
            let live = entries
                .values()
                .filter(|entry| {
                    entry.session_id == context.session_id
                        && entry.agent_id == context.agent_id
                        && entry.status.borrow().is_running()
                })
                .count();
            if live < self.inner.max_per_agent {
                entries.insert(id.clone(), Arc::clone(&entry));
            }
            live >= self.inner.max_per_agent
        };
        if at_process_limit {
            terminate_process_tree(&mut child).await;
            return Err(ToolError::Execution(format!(
                "Agent already has {} live processes",
                self.inner.max_per_agent
            )));
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("command stdout is unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("command stderr is unavailable".to_string()))?;
        let stdout_task = tokio::spawn(read_output(stdout, Arc::clone(&entry), false));
        let stderr_task = tokio::spawn(read_output(stderr, Arc::clone(&entry), true));
        let turn_cancelled = entry.cancellation.clone();
        let project_shutdown = self.inner.shutdown.clone();
        let supervised = Arc::clone(&entry);
        tokio::spawn(async move {
            let status = tokio::select! {
                result = child.wait() => match result {
                    Ok(status) => ProcessStatus::Exited(status.code()),
                    Err(error) => ProcessStatus::Failed(error.to_string()),
                },
                _ = turn_cancelled.cancelled() => {
                    terminate_process_tree(&mut child).await;
                    ProcessStatus::Cancelled
                }
                _ = project_shutdown.cancelled() => {
                    terminate_process_tree(&mut child).await;
                    ProcessStatus::Cancelled
                }
            };
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            supervised.stdin.lock().await.take();
            status_tx.send_replace(status);
            supervised.output_ready.notify_waiters();
        });

        wait_until_deadline(&entry, clamp_yield(yield_time_ms)).await;
        self.collect(&entry, max_output_tokens).await
    }

    pub(crate) async fn write_stdin(
        &self,
        context: &ToolContext,
        process_id: &str,
        chars: &str,
        yield_time_ms: Option<u64>,
        max_output_tokens: Option<usize>,
    ) -> Result<ProcessOutput, ToolError> {
        let entry = self.entry_for(context, process_id).await?;
        let _interaction = entry.interaction.lock().await;
        if !chars.is_empty() {
            if !entry.tty {
                return Err(ToolError::Execution(
                    "stdin is closed; start exec_command with tty=true".to_string(),
                ));
            }
            let mut stdin = entry.stdin.lock().await;
            let stdin = stdin.as_mut().ok_or_else(|| {
                ToolError::Execution(format!("process {process_id} stdin is closed"))
            })?;
            stdin
                .write_all(chars.as_bytes())
                .await
                .map_err(|error| ToolError::Io(error.to_string()))?;
            stdin
                .flush()
                .await
                .map_err(|error| ToolError::Io(error.to_string()))?;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        wait_for_output(&entry, clamp_yield(yield_time_ms)).await;
        self.collect(&entry, max_output_tokens).await
    }

    pub async fn terminate_session(&self, session_id: SessionId) {
        let entries = self
            .inner
            .entries
            .lock()
            .await
            .values()
            .filter(|entry| entry.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            entry.cancellation.cancel();
        }
    }

    pub async fn terminate_agent(&self, session_id: SessionId, agent_id: AgentId) {
        let entries = self
            .inner
            .entries
            .lock()
            .await
            .values()
            .filter(|entry| entry.session_id == session_id && entry.agent_id == agent_id)
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            entry.cancellation.cancel();
        }
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let entries = self
            .inner
            .entries
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in &entries {
            entry.cancellation.cancel();
        }
        for entry in entries {
            let mut status = entry.status.clone();
            if status.borrow().is_running() {
                let _ = tokio::time::timeout(Duration::from_secs(2), status.changed()).await;
            }
        }
        self.inner.entries.lock().await.clear();
    }

    async fn entry_for(
        &self,
        context: &ToolContext,
        process_id: &str,
    ) -> Result<Arc<ProcessEntry>, ToolError> {
        let entry = self
            .inner
            .entries
            .lock()
            .await
            .get(process_id)
            .cloned()
            .ok_or_else(|| ToolError::Execution(format!("unknown process: {process_id}")))?;
        let authorization_sha256 = context
            .authorization
            .policy_sha256()
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        if entry.session_id != context.session_id
            || entry.agent_id != context.agent_id
            || entry.authorization_sha256 != authorization_sha256
        {
            return Err(ToolError::PermissionDenied {
                tool: "write_stdin".to_string(),
                access: context.authorization.preset,
            });
        }
        Ok(entry)
    }

    async fn collect(
        &self,
        entry: &Arc<ProcessEntry>,
        max_output_tokens: Option<usize>,
    ) -> Result<ProcessOutput, ToolError> {
        let status = entry.status.borrow().clone();
        let max_bytes = max_output_tokens
            .unwrap_or(DEFAULT_OUTPUT_TOKENS)
            .clamp(1, MAX_OUTPUT_TOKENS)
            .saturating_mul(4)
            .max(256);
        let (output, output_truncated) = entry.output.lock().await.take(max_bytes);
        let running = status.is_running();
        if !running {
            let mut entries = self.inner.entries.lock().await;
            if entries
                .get(&entry.id)
                .is_some_and(|stored| Arc::ptr_eq(stored, entry))
            {
                entries.remove(&entry.id);
            }
        }
        let (exit_code, failure) = match status {
            ProcessStatus::Running => (None, None),
            ProcessStatus::Exited(code) => (code, None),
            ProcessStatus::Failed(error) => (None, Some(error)),
            ProcessStatus::Cancelled => (None, Some("process was cancelled".to_string())),
        };
        if let Some(error) = failure {
            return Err(ToolError::Execution(error));
        }
        Ok(ProcessOutput {
            process_id: running.then(|| entry.id.clone()),
            exit_code,
            output,
            output_truncated,
            backend: entry.backend,
        })
    }
}

impl Default for ProcessTable {
    fn default() -> Self {
        Self::new(4, CancellationToken::new())
    }
}

#[derive(Default)]
struct OutputBuffer {
    bytes: Vec<u8>,
    cursor: usize,
    omitted: usize,
}

impl OutputBuffer {
    fn push(&mut self, chunk: &[u8], stderr: bool) {
        if stderr && !chunk.is_empty() {
            self.bytes.extend_from_slice(b"[stderr] ");
        }
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > MAX_BUFFER_BYTES {
            let overflow = self.bytes.len() - MAX_BUFFER_BYTES;
            self.bytes.drain(..overflow);
            if self.cursor < overflow {
                self.omitted = self.omitted.saturating_add(overflow - self.cursor);
                self.cursor = 0;
            } else {
                self.cursor -= overflow;
            }
        }
    }

    fn has_unread(&self) -> bool {
        self.omitted > 0 || self.cursor < self.bytes.len()
    }

    fn take(&mut self, max_bytes: usize) -> (String, bool) {
        let mut visible = Vec::new();
        let mut truncated = false;
        if self.omitted > 0 {
            let marker = format!("... {} bytes omitted ...\n", self.omitted);
            visible.extend_from_slice(marker.as_bytes());
            self.omitted = 0;
            truncated = true;
        }
        let remaining = max_bytes.saturating_sub(visible.len());
        let available = self.bytes.len().saturating_sub(self.cursor);
        let take = remaining.min(available);
        visible.extend_from_slice(&self.bytes[self.cursor..self.cursor + take]);
        self.cursor += take;
        truncated |= take < available;
        (String::from_utf8_lossy(&visible).into_owned(), truncated)
    }
}

async fn resolve_workdir(
    context: &ToolContext,
    workdir: Option<&str>,
) -> Result<PathBuf, ToolError> {
    let path = resolve_tool_path(
        &context.authorization,
        workdir.unwrap_or(&context.authorization.cwd),
        PathOperation::Read,
    )
    .await?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    if !metadata.is_dir() {
        return Err(ToolError::InvalidArguments {
            tool: "exec_command".to_string(),
            message: format!("workdir is not a directory: {}", path.display()),
        });
    }
    Ok(path)
}

async fn read_output<R>(mut reader: R, entry: Arc<ProcessEntry>, stderr: bool)
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                entry.output.lock().await.push(&chunk[..read], stderr);
                entry.output_ready.notify_waiters();
            }
        }
    }
}

async fn wait_until_deadline(entry: &ProcessEntry, yield_time_ms: u64) {
    let mut status = entry.status.clone();
    if !status.borrow().is_running() {
        return;
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(yield_time_ms)) => {}
        _ = status.changed() => {}
    }
}

async fn wait_for_output(entry: &ProcessEntry, yield_time_ms: u64) {
    let notified = entry.output_ready.notified();
    tokio::pin!(notified);
    if entry.output.lock().await.has_unread() || !entry.status.borrow().is_running() {
        return;
    }
    let mut status = entry.status.clone();
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(yield_time_ms)) => {}
        _ = &mut notified => {}
        _ = status.changed() => {}
    }
}

fn clamp_yield(value: Option<u64>) -> u64 {
    value
        .unwrap_or(DEFAULT_YIELD_MS)
        .clamp(MIN_YIELD_MS, MAX_YIELD_MS)
}

fn map_execution_error(error: papermachine_execution::ExecutionError) -> ToolError {
    match error {
        papermachine_execution::ExecutionError::SandboxUnavailable(message) => {
            ToolError::IsolationUnavailable(message)
        }
        papermachine_execution::ExecutionError::Cancelled => ToolError::Cancelled,
        papermachine_execution::ExecutionError::Io(error) => ToolError::Io(error.to_string()),
        other => ToolError::Execution(other.to_string()),
    }
}
