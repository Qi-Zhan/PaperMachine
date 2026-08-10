use crate::ProcessTable;
use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use async_trait::async_trait;
use papermachine_protocol::ToolDefinition;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

#[derive(Clone, Default)]
pub struct ExecCommandTool {
    processes: ProcessTable,
}

impl ExecCommandTool {
    pub fn new(processes: ProcessTable) -> Self {
        Self { processes }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecCommandArgs {
    cmd: String,
    workdir: Option<String>,
    yield_time_ms: Option<u64>,
    #[serde(default)]
    tty: bool,
    max_output_tokens: Option<usize>,
}

#[async_trait]
impl ToolExecutor for ExecCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "exec_command".to_string(),
            description: "Run a shell command under the current access boundary. Short commands return directly; longer commands return a process_id for write_stdin.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string"},
                    "workdir": {"type": "string"},
                    "yield_time_ms": {"type": "integer", "minimum": 250, "maximum": 30000},
                    "tty": {"type": "boolean"},
                    "max_output_tokens": {"type": "integer", "minimum": 1, "maximum": 50000}
                },
                "required": ["cmd"],
                "additionalProperties": false
            }),
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        ensure_allowed(&context, "exec_command")?;
        let args: ExecCommandArgs = parse_arguments("exec_command", arguments)?;
        let output = self
            .processes
            .exec(
                &context,
                &args.cmd,
                args.workdir.as_deref(),
                args.yield_time_ms,
                args.tty,
                args.max_output_tokens,
            )
            .await?;
        Ok(ToolOutput {
            summary: match (&output.process_id, output.exit_code) {
                (Some(process_id), _) => format!("command is running as {process_id}"),
                (None, Some(code)) => format!("command exited with code {code}"),
                (None, None) => "command finished".to_string(),
            },
            value: json!({
                "process_id": output.process_id,
                "exit_code": output.exit_code,
                "output": output.output,
                "output_truncated": output.output_truncated,
                "sandbox_backend": output.backend,
            }),
        })
    }
}

#[derive(Clone, Default)]
pub struct WriteStdinTool {
    processes: ProcessTable,
}

impl WriteStdinTool {
    pub fn new(processes: ProcessTable) -> Self {
        Self { processes }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteStdinArgs {
    process_id: String,
    #[serde(default)]
    chars: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<usize>,
}

#[async_trait]
impl ToolExecutor for WriteStdinTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_stdin".to_string(),
            description: "Write to or poll a process created by this Agent with exec_command. Non-empty writes require tty=true on the original command.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "process_id": {"type": "string"},
                    "chars": {"type": "string"},
                    "yield_time_ms": {"type": "integer", "minimum": 250, "maximum": 30000},
                    "max_output_tokens": {"type": "integer", "minimum": 1, "maximum": 50000}
                },
                "required": ["process_id"],
                "additionalProperties": false
            }),
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        ensure_allowed(&context, "write_stdin")?;
        let args: WriteStdinArgs = parse_arguments("write_stdin", arguments)?;
        let output = self
            .processes
            .write_stdin(
                &context,
                &args.process_id,
                &args.chars,
                args.yield_time_ms,
                args.max_output_tokens,
            )
            .await?;
        Ok(ToolOutput {
            summary: match (&output.process_id, output.exit_code) {
                (Some(process_id), _) => format!("process {process_id} is still running"),
                (None, Some(code)) => format!("process exited with code {code}"),
                (None, None) => "process finished".to_string(),
            },
            value: json!({
                "process_id": output.process_id,
                "exit_code": output.exit_code,
                "output": output.output,
                "output_truncated": output.output_truncated,
                "sandbox_backend": output.backend,
            }),
        })
    }
}

fn ensure_allowed(context: &ToolContext, tool: &str) -> Result<(), ToolError> {
    if context.authorization.preset.allows_local_tool(tool) {
        Ok(())
    } else {
        Err(ToolError::PermissionDenied {
            tool: tool.to_string(),
            access: context.authorization.preset,
        })
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
