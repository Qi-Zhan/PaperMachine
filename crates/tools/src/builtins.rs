use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use crate::path::resolve_tool_path;
use async_trait::async_trait;
use papermachine_execution::ExecutionError;
use papermachine_execution::SandboxExecutor;
use papermachine_execution::SandboxPolicy;
use papermachine_protocol::PathOperation;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::ToolEffectDisposition;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;

const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_READ_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Default)]
pub struct ReadFileTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
    max_bytes: Option<usize>,
}

#[async_trait]
impl ToolExecutor for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description:
                "Read a UTF-8 text file from a path allowed by the current Session access profile."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_FILE_BYTES}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            supports_parallel: true,
        }
    }

    fn effect_disposition(&self) -> ToolEffectDisposition {
        ToolEffectDisposition::Pure
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if !context.authorization.tools.read_file {
            return Err(ToolError::PermissionDenied {
                tool: "read_file".to_string(),
                access: context.authorization.preset,
            });
        }
        let args: ReadFileArgs = parse_arguments("read_file", arguments)?;
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_READ_BYTES)
            .clamp(1, MAX_FILE_BYTES);
        let path =
            resolve_tool_path(&context.authorization, &args.path, PathOperation::Read).await?;
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| ToolError::Io(error.to_string()))?;
        let truncated = bytes.len() > max_bytes;
        let visible = &bytes[..bytes.len().min(max_bytes)];
        let content = String::from_utf8(visible.to_vec()).map_err(|error| {
            ToolError::Execution(format!("{} is not valid UTF-8: {error}", args.path))
        })?;
        Ok(ToolOutput {
            value: json!({
                "path": args.path,
                "content": content,
                "bytes": bytes.len(),
                "truncated": truncated,
            }),
            summary: format!("read {} bytes from {}", visible.len(), args.path),
        })
    }
}

#[derive(Clone, Copy, Default)]
pub struct WriteFileTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    path: String,
    content: String,
    #[serde(default = "default_true")]
    create_parents: bool,
}

const fn default_true() -> bool {
    true
}

#[async_trait]
impl ToolExecutor for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_string(),
            description:
                "Write a UTF-8 text file to a path allowed by the current Session access profile."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                    "create_parents": {"type": "boolean"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            supports_parallel: false,
        }
    }

    fn effect_disposition(&self) -> ToolEffectDisposition {
        ToolEffectDisposition::Idempotent
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if !context.authorization.tools.write_file {
            return Err(ToolError::PermissionDenied {
                tool: "write_file".to_string(),
                access: context.authorization.preset,
            });
        }
        let args: WriteFileArgs = parse_arguments("write_file", arguments)?;
        if args.content.len() > MAX_FILE_BYTES {
            return Err(ToolError::InvalidArguments {
                tool: "write_file".to_string(),
                message: format!("content exceeds {MAX_FILE_BYTES} bytes"),
            });
        }
        let path =
            resolve_tool_path(&context.authorization, &args.path, PathOperation::Write).await?;
        if args.create_parents
            && let Some(parent) = path.parent()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| ToolError::Io(error.to_string()))?;
        }
        tokio::fs::write(&path, args.content.as_bytes())
            .await
            .map_err(|error| ToolError::Io(error.to_string()))?;
        Ok(ToolOutput {
            value: json!({
                "path": args.path,
                "bytes_written": args.content.len(),
            }),
            summary: format!("wrote {} bytes to {}", args.content.len(), args.path),
        })
    }
}

#[derive(Clone, Copy, Default)]
pub struct ExecCommandTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecCommandArgs {
    command: String,
    timeout_seconds: Option<u64>,
}

#[async_trait]
impl ToolExecutor for ExecCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "exec_command".to_string(),
            description: "Run a shell command under the current Session access profile. Workspace and research profiles remain sandboxed.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            supports_parallel: false,
        }
    }

    fn effect_disposition(&self) -> ToolEffectDisposition {
        ToolEffectDisposition::Unknown
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if !context.authorization.tools.exec_command {
            return Err(ToolError::PermissionDenied {
                tool: "exec_command".to_string(),
                access: context.authorization.preset,
            });
        }
        let args: ExecCommandArgs = parse_arguments("exec_command", arguments)?;
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: "exec_command".to_string(),
                message: "command must not be empty".to_string(),
            });
        }
        let timeout_seconds = args.timeout_seconds.unwrap_or(60).clamp(1, 600);
        let policy = SandboxPolicy::from_authorization(
            &context.authorization,
            Duration::from_secs(timeout_seconds),
        )
        .map_err(|error| map_execution_error(error, timeout_seconds))?;
        let output = SandboxExecutor
            .run_shell(
                std::path::Path::new(&context.authorization.cwd),
                &context.sandbox_root,
                &args.command,
                policy,
                context.cancellation,
            )
            .await
            .map_err(|error| map_execution_error(error, timeout_seconds))?;
        Ok(ToolOutput {
            value: json!({
                "command": args.command,
                "exit_code": output.exit_code,
                "success": output.success,
                "stdout": output.stdout,
                "stderr": output.stderr,
                "stdout_truncated": output.stdout_truncated,
                "stderr_truncated": output.stderr_truncated,
                "sandbox_backend": output.backend.as_str(),
            }),
            summary: match output.exit_code {
                Some(code) => format!("command exited with code {code}"),
                None => "command exited without a status code".to_string(),
            },
        })
    }
}

fn map_execution_error(error: ExecutionError, timeout_seconds: u64) -> ToolError {
    match error {
        ExecutionError::InvalidCommand(message) => ToolError::InvalidArguments {
            tool: "exec_command".to_string(),
            message,
        },
        ExecutionError::SandboxUnavailable(message) => ToolError::IsolationUnavailable(message),
        ExecutionError::Timeout(_) => ToolError::Timeout {
            seconds: timeout_seconds,
        },
        ExecutionError::Cancelled => ToolError::Cancelled,
        ExecutionError::Io(error) => ToolError::Io(error.to_string()),
        other => ToolError::Execution(other.to_string()),
    }
}

fn parse_arguments<T>(tool: &str, value: Value) -> Result<T, ToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(value).map_err(|error| ToolError::InvalidArguments {
        tool: tool.to_string(),
        message: error.to_string(),
    })
}
