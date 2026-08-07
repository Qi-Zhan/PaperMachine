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
- Agent base class; class attributes: role, system_prompt, model, skills, access.
- @action, @action("prompt"), or @action(search_context_size="low", reasoning_effort="high", finalize="after_search") on async Agent methods. The method body is declarative; its docstring/prompt and arguments become a Codex-like Session Turn. An Action continues its model/tool loop until the model returns a terminal answer, the user interrupts it, or runtime infrastructure fails. Use a model_only Agent for evaluators and synthesizers that must reason only over supplied evidence without calling tools. Use finalize="after_search" when an action itself must return a user-facing deliverable: after a hosted-search Turn, the same Session gets one separate model-only finalization Turn, so provider progress narration cannot silently become the deliverable. Reserve higher reasoning effort for evidence judgment and final synthesis.
- Annotate an action with -> dict, -> list, -> bool, -> int, or -> float when workflow control flow needs parsed JSON rather than raw text.
- @workflow(slug=..., name=..., description=..., request_mode="required", params_schema={...}, output_schema={...}) on exactly one async main(ctx). Use request_mode="none" only for a persistent interaction that deliberately starts without a user task and obtains each user message through ask_human.
- ctx.request, ctx.params, ctx.trigger, ctx.context, ctx.workflow_id, and await ctx.project.snapshot(max_sessions=..., max_turns_per_session=..., max_artifacts=...) for a bounded view of existing Project research.
- await together(a.action(...), b.action(...)) for explicit concurrency. Never put two actions from the same Agent in one together().
- Team(name, *agents), await team.add(agent), await team.remove(agent), await agent.retire().
- await relate(source, target, kind="reviews", instructions="...").
- async with scope(name, objective): ...
- Channel(name, schema={...}); await channel.publish(value, sender=agent); await channel.receive().
- await ask_human(question, response_schema={...}, agent=optional_agent).
- @every(seconds=..., policy="coalesce") on a nested async callback for periodic work.
- await wait(seconds=... or minutes=..., name="...") for a sequential durable timer wait.
- background(coroutine) returns a handle with await handle.join().
- await publish_artifact(name, text, kind="report", media_type="text/html; charset=utf-8", metadata={...}, agent=optional_agent) for durable text output. Generated HTML must be self-contained and script-free.

Use ordinary Python if/for/while for long-running control logic. Human, timer, and Channel waits are durable and may coexist in background branches; the runtime releases idle Python processes only after every branch reaches a replayable wait. All imports must be a single `from papermachine import ...` statement. Do not import or access files, network, subprocesses, environment variables, reflection, MCP, plugins, or external libraries. Every Agent class must declare one access profile: `model_only`, `read_only`, `workspace`, `research`, or `full_access`. Use `research` only for Agents that gather external evidence, `model_only` for evaluators and writers that should use supplied evidence, and request `full_access` only when the user's description truly requires unrestricted host access because it pauses for explicit human approval. Prefer 2-4 clearly related Agent classes and keep the program understandable to a researcher editing it."#;
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
