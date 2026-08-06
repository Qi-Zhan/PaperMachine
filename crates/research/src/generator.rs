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
        let instructions = r#"You design PaperMachine Python workflow DSL programs. Return only executable workflow.py source, without Markdown fences or explanation.

Available API:
- Agent base class; class attributes: role, instructions, model, skills.
- @action, @action("prompt"), or @action(max_steps=N, max_search_calls=M, search_context_size="low", reasoning_effort="high", max_output_tokens=K) on async Agent methods. The method body is declarative; its docstring/prompt and arguments become a Codex-like Session Turn. Use max_steps=1 for evaluators and synthesizers that must reason only over supplied evidence without calling tools. Give every research action a finite max_search_calls allowance and normally use low search context for bounded parallel routes. Assign lower reasoning effort and a bounded output ceiling to planning/routing actions, and reserve higher effort for evidence judgment and final synthesis.
- Annotate an action with -> dict, -> list, -> bool, -> int, or -> float when workflow control flow needs parsed JSON rather than raw text.
- @workflow(slug=..., name=..., version="0.1.0", description=..., input_schema={...}, output_schema={...}, budget={...}) on exactly one async main(ctx).
- ctx.objective, ctx.input, ctx.run_id.
- await together(a.action(...), b.action(...)) for explicit concurrency. Never put two actions from the same Agent in one together().
- Team(name, *agents), await team.add(agent), await team.remove(agent), await agent.retire().
- await relate(source, target, kind="reviews", instructions="...").
- async with scope(name, objective): ...
- Channel(name, schema={...}); await channel.publish(value, sender=agent); await channel.receive().
- await ask_human(question, response_schema={...}, agent=optional_agent).
- @every(seconds=..., policy="coalesce") on a nested async callback for periodic work.
- background(coroutine) returns a handle with await handle.join().

Use ordinary Python if/for/while for long-running control logic. All imports must be a single `from papermachine import ...` statement. Do not import or access files, network, subprocesses, environment variables, reflection, MCP, plugins, or external libraries. Every Agent class must declare one access profile: `model_only`, `read_only`, `workspace`, `research`, or `full_access`. Use `research` only for Agents that gather external evidence, `model_only` for evaluators and writers that should use supplied evidence, and request `full_access` only when the user's description truly requires unrestricted host access because it pauses for explicit human approval. Prefer 2-4 clearly related Agent classes and keep the program understandable to a researcher editing it."#;
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
            max_tool_calls: None,
            max_output_tokens: Some(16_384),
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
