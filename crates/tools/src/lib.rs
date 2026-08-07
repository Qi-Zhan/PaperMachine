//! Model-visible tools and built-in local research operations.
//!
//! The `ToolExecutor` shape is adapted from OpenAI Codex's `codex-tools`
//! crate. PaperMachine removes deferred discovery, code mode, MCP, plugins,
//! approvals, and telemetry while retaining a spec/runtime pair.
//! Human interaction is a durable Workflow effect, never a model-visible tool.

mod builtins;
mod fetch;
mod path;
mod registry;

use async_trait::async_trait;
use papermachine_protocol::ActionAttemptId;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::AgentAccessProfile;
use papermachine_protocol::ProjectId;
use papermachine_protocol::SessionId;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::TurnId;
use papermachine_protocol::WorkflowId;
use serde_json::Value;
use std::path::PathBuf;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub use builtins::ExecCommandTool;
pub use builtins::ReadFileTool;
pub use builtins::WriteFileTool;
pub use fetch::FetchUrlTool;
pub use registry::ToolRegistry;
pub use registry::ToolRegistryBuilder;

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub workflow_id: Option<WorkflowId>,
    pub action_invocation_id: Option<ActionInvocationId>,
    pub action_attempt_id: Option<ActionAttemptId>,
    pub workspace_root: PathBuf,
    pub sandbox_root: PathBuf,
    pub protected_root: PathBuf,
    pub access: AgentAccessProfile,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub value: Value,
    pub summary: String,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("tool {tool} is not allowed by the {access} access profile")]
    PermissionDenied {
        tool: String,
        access: AgentAccessProfile,
    },
    #[error("invalid arguments for {tool}: {message}")]
    InvalidArguments { tool: String, message: String },
    #[error("path must stay inside the Session workspace: {0}")]
    PathOutsideWorkspace(String),
    #[error("path is reserved for PaperMachine managed state: {0}")]
    PathInsideManagedState(String),
    #[error("tool I/O failed: {0}")]
    Io(String),
    #[error("command timed out after {seconds} seconds")]
    Timeout { seconds: u64 },
    #[error("tool execution was cancelled")]
    Cancelled,
    #[error("tool execution failed: {0}")]
    Execution(String),
    #[error("network tool failed: {0}")]
    Network(String),
    #[error("command isolation is unavailable: {0}")]
    IsolationUnavailable(String),
    #[error("tool registry lock poisoned")]
    RegistryPoisoned,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn supports_parallel(&self) -> bool {
        false
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError>;
}
