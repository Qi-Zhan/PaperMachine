use futures::StreamExt;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_protocol::MessageRole;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelRequest;
use papermachine_protocol::ModelToolChoice;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct WorkflowGenerator {
    model: Arc<dyn ModelClient>,
    default_model: String,
}

impl WorkflowGenerator {
    pub fn new(model: Arc<dyn ModelClient>, default_model: impl Into<String>) -> Self {
        Self {
            model,
            default_model: default_model.into(),
        }
    }

    pub async fn generate(
        &self,
        request: WorkflowGenerationRequest,
        cancellation: CancellationToken,
    ) -> Result<String, WorkflowGeneratorError> {
        let description = request.description.trim();
        if description.is_empty() || description.len() > 12_000 {
            return Err(WorkflowGeneratorError::InvalidRequest(
                "workflow description must contain 1-12000 characters".to_string(),
            ));
        }
        let instructions = r#"You design PaperMachine Workflow Language v1 programs. Return only executable workflow.pm source, without Markdown fences or explanation.

The language is a single-file, Rust-like dynamic workflow language:
- Start with `version 1;`. There are no imports, modules, macros, eval, arbitrary I/O, recursion, closures, higher-order functions, or try/catch.
- Declare boundary schemas with `schema Name = object { field: string, optional?: list(string) };`. Available schema forms are any, string, bool, int, number, list(T), map(T), object {...}, enum[...], model_profile, and access. Schema options include default, title, description, min, max, min_len, and max_len.
- Declare `agent Name { access = workspace; role = "..."; system = "..."; action act(arg) { prompt = "..."; } }`. Action options are tools, search_context, reasoning_effort, finalize, and result. Omitting tools uses the normal allowed tools; `tools = []` disables local tools. Hosted search is selected with search_context = low|medium|high. Use `finalize = after_search` for a search-backed final deliverable and `finalize = if_needed` with an object/list result schema for a normal work report plus typed control result.
- Declare exactly one `workflow slug_name { slug = "optional-kebab-slug"; name = "..."; description = "..."; request = required|none; params { ... } run(ctx) { ... } }`.
- Variables are dynamic: `let` is not rebindable, `var` is. Lists and objects are immutable; use append, extend, and update to return new values. Conditions are strict booleans and missing fields fail.
- Control flow includes if/else, match with a `_` arm, finite for, while, loop, break, continue, return, and await. Every while/loop cycle must pass a durable await.
- Top-level `fn helper(args) { ... }` may await effects but cannot recurse.
- Agent construction uses named options `key`, `name`, `role`, `system`, `model`, `skills`, and `access`. Identity is template plus key; use stable unique scalar keys for dynamic instances.
- Invoke Actions with `let result = await agent.action(arg = value);`.
- Fixed concurrency is `parallel { primary => { ... }, challenge => { ... } }`. Dynamic concurrency is `parallel for item in items key item.id { ... }`; it returns results in input order. Never run two Actions on one Agent concurrently.
- Durable builtins are `await ask_human(...)`, `await wait(...)`, `await ctx.project.changes(...)`, `await publish_artifact(...)`, and `await publish_home(action = completed_action)`.
- Pure builtins: len, range, enumerate, zip, min, max, clamp, get, append, extend, update, slice, trim, string, int, number, is_*, assert, fail.
- ctx exposes request, instructions, params, trigger, session_id, and project. Project contents enter a model only when the Workflow passes them to an Action.

Prefer 2-4 clearly related Agent templates and ordinary control flow. Use model_only for evaluators and writers over supplied evidence, read_only when commands may inspect but not write, workspace for normal file work, and full_access only when truly required."#;
        let prompt = format!(
            "Create a workflow for this request:\n{description}\n\nRequested name: {}\nRequested slug: {}",
            request.name.as_deref().unwrap_or("choose a concise name"),
            request
                .slug
                .as_deref()
                .unwrap_or("derive lowercase kebab-case"),
        );
        let model_request = ModelRequest {
            model: request.model.unwrap_or_else(|| self.default_model.clone()),
            reasoning_effort: None,
            instructions: instructions.to_string(),
            input: vec![ModelInputItem::Message {
                role: MessageRole::User,
                content: prompt,
            }],
            prompt_cache: None,
            transport_session_key: None,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            web_search_context_size: None,
            parallel_tool_calls: false,
            tool_choice: ModelToolChoice::Auto,
            response_format: None,
        };
        let mut stream = tokio::select! {
            result = self.model.stream(model_request) => result?,
            _ = cancellation.cancelled() => return Err(WorkflowGeneratorError::Cancelled),
        };
        let mut source = String::new();
        let mut completed = false;
        loop {
            let next = tokio::select! {
                value = stream.next() => value,
                _ = cancellation.cancelled() => return Err(WorkflowGeneratorError::Cancelled),
            };
            let Some(event) = next else { break };
            match event? {
                ModelEvent::RequestMetadata { .. } => {}
                ModelEvent::OutputTextDelta { delta } => source.push_str(&delta),
                ModelEvent::ResponseItemCompleted { item } => {
                    if source.is_empty()
                        && let Some(text) = item
                            .pointer("/content/0/text")
                            .and_then(serde_json::Value::as_str)
                    {
                        source.push_str(text);
                    }
                }
                ModelEvent::Completed { .. } => completed = true,
                ModelEvent::ToolCallCompleted { .. } => {}
            }
        }
        if !completed || source.trim().is_empty() {
            return Err(WorkflowGeneratorError::Incomplete);
        }
        Ok(strip_code_fence(&source))
    }
}

fn strip_code_fence(source: &str) -> String {
    let trimmed = source.trim();
    if !trimmed.starts_with("```") {
        return format!("{trimmed}\n");
    }
    let mut lines = trimmed.lines();
    let _ = lines.next();
    let mut body = lines.collect::<Vec<_>>();
    if body.last().is_some_and(|line| line.trim() == "```") {
        body.pop();
    }
    format!("{}\n", body.join("\n"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkflowGenerationRequest {
    pub description: String,
    pub name: Option<String>,
    pub slug: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Error)]
pub enum WorkflowGeneratorError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("invalid workflow generation request: {0}")]
    InvalidRequest(String),
    #[error("workflow generation was cancelled")]
    Cancelled,
    #[error("model stream ended without a complete workflow source")]
    Incomplete,
}
