use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use async_trait::async_trait;
use papermachine_protocol::ActionInvocationId;
use papermachine_protocol::ToolDefinition;
use papermachine_protocol::WorkflowId;
use papermachine_store::ProjectHomeDraft;
use papermachine_store::ProjectHomePatchOperation;
use papermachine_store::Store;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
pub struct ReadProjectHomeTool {
    store: Arc<Store>,
}

impl ReadProjectHomeTool {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[derive(Clone)]
pub struct PatchProjectHomeTool {
    store: Arc<Store>,
}

impl PatchProjectHomeTool {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[derive(Clone)]
pub struct PreviewProjectHomeTool {
    store: Arc<Store>,
}

impl PreviewProjectHomeTool {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    base_revision: String,
    operations: Vec<ProjectHomePatchOperation>,
}

#[async_trait]
impl ToolExecutor for ReadProjectHomeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_project_home".to_string(),
            description: "Read the current staged Project home page as stable semantic blocks. Call this before editing so patches use the current revision.".to_string(),
            input_schema: empty_schema(),
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let _: EmptyArgs = parse_arguments("read_project_home", arguments)?;
        let (workflow_id, action_invocation_id) = action_context(&context)?;
        let draft = self
            .store
            .read_project_home_draft(workflow_id, action_invocation_id)
            .map_err(store_error)?;
        Ok(ToolOutput {
            value: draft_value(&draft),
            summary: format!(
                "read Project home revision {} with {} blocks",
                draft.revision,
                draft.blocks.len()
            ),
        })
    }
}

#[async_trait]
impl ToolExecutor for PatchProjectHomeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "patch_project_home".to_string(),
            description: "Incrementally edit the staged Project home page. Use upsert with id and html, remove with id, or reorder with an order containing every current block ID. Use the revision returned by read_project_home or the preceding patch.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "base_revision": {"type": "string"},
                    "operations": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 128,
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": {"type": "string", "enum": ["upsert", "remove", "reorder"]},
                                "id": {"type": "string"},
                                "html": {"type": "string"},
                                "order": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["kind"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["base_revision", "operations"],
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
        let args: PatchArgs = parse_arguments("patch_project_home", arguments)?;
        let (workflow_id, action_invocation_id) = action_context(&context)?;
        let draft = self
            .store
            .patch_project_home_draft(
                workflow_id,
                action_invocation_id,
                &args.base_revision,
                args.operations,
            )
            .map_err(store_error)?;
        Ok(ToolOutput {
            value: json!({
                "revision": draft.revision,
                "block_ids": draft.blocks.iter().map(|block| block.id.as_str()).collect::<Vec<_>>(),
                "block_count": draft.blocks.len(),
            }),
            summary: format!(
                "updated Project home to revision {} with {} blocks",
                draft.revision,
                draft.blocks.len()
            ),
        })
    }
}

#[async_trait]
impl ToolExecutor for PreviewProjectHomeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "preview_project_home".to_string(),
            description: "Inspect the complete materialized Project home page after edits. Returns the exact semantic HTML that will be published, a rendered text view, block order, and validation diagnostics.".to_string(),
            input_schema: empty_schema(),
            supports_parallel: false,
        }
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let _: EmptyArgs = parse_arguments("preview_project_home", arguments)?;
        let (workflow_id, action_invocation_id) = action_context(&context)?;
        let draft = self
            .store
            .read_project_home_draft(workflow_id, action_invocation_id)
            .map_err(store_error)?;
        let html = draft.html();
        let rendered_text = html2text::from_read(html.as_bytes(), 100).map_err(|error| {
            ToolError::Execution(format!("failed to render Project-home preview: {error}"))
        })?;
        let mut diagnostics = Vec::new();
        if draft.blocks.is_empty() {
            diagnostics.push("The page has no content blocks.".to_string());
        }
        if !html.to_ascii_lowercase().contains("<h1") {
            diagnostics.push("The page has no h1 heading.".to_string());
        }
        if rendered_text.trim().is_empty() {
            diagnostics.push("The rendered page has no readable text.".to_string());
        }
        Ok(ToolOutput {
            value: json!({
                "revision": draft.revision,
                "block_ids": draft.blocks.iter().map(|block| block.id.as_str()).collect::<Vec<_>>(),
                "html": html,
                "rendered_text": rendered_text,
                "diagnostics": diagnostics,
            }),
            summary: format!(
                "previewed Project home revision {} with {} diagnostics",
                draft.revision,
                diagnostics.len()
            ),
        })
    }
}

fn empty_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

fn action_context(context: &ToolContext) -> Result<(WorkflowId, ActionInvocationId), ToolError> {
    let workflow_id = context.workflow_id.ok_or_else(|| {
        ToolError::Execution(
            "Project-home tools are available only inside a Workflow Action".to_string(),
        )
    })?;
    let action_invocation_id = context.action_invocation_id.ok_or_else(|| {
        ToolError::Execution("Project-home tools require a durable ActionInvocation".to_string())
    })?;
    Ok((workflow_id, action_invocation_id))
}

fn draft_value(draft: &ProjectHomeDraft) -> Value {
    json!({
        "revision": draft.revision,
        "base_artifact_id": draft.base_artifact_id,
        "blocks": draft.blocks,
        "materialized_html": draft.html(),
    })
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

fn store_error(error: papermachine_store::StoreError) -> ToolError {
    ToolError::Execution(error.to_string())
}
