use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_model::ModelStream;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelRequest;
use papermachine_protocol::ModelToolCall;
use papermachine_protocol::TokenUsage;
use serde_json::Value;
use serde_json::json;

#[derive(Clone, Copy, Debug, Default)]
pub struct DemoModelClient;

#[async_trait]
impl ModelClient for DemoModelClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        if request
            .tools
            .iter()
            .any(|tool| tool.name == "read_project_home")
        {
            return Ok(stream::iter(project_home_response(&request).into_iter().map(Ok)).boxed());
        }
        let prompt = request
            .input
            .iter()
            .rev()
            .find_map(|item| match item {
                ModelInputItem::Message { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .unwrap_or_default();
        let output = if request
            .instructions
            .contains("PaperMachine Python workflow DSL")
        {
            generated_workflow(prompt)
        } else {
            demo_research_response(prompt)
        };
        Ok(stream::iter([
            Ok(ModelEvent::OutputTextDelta { delta: output }),
            Ok(ModelEvent::Completed {
                usage: TokenUsage {
                    input_tokens: 240,
                    output_tokens: 180,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                },
            }),
        ])
        .boxed())
    }
}

fn project_home_response(request: &ModelRequest) -> Vec<ModelEvent> {
    let tool_output = |call_id: &str| {
        request.input.iter().rev().find_map(|item| match item {
            ModelInputItem::FunctionCallOutput {
                call_id: found,
                output,
            } if found == call_id => Some(output),
            _ => None,
        })
    };
    let usage = TokenUsage {
        input_tokens: 160,
        output_tokens: 40,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
    };
    if tool_output("demo-project-home-read").is_none() {
        return tool_call(
            "demo-project-home-read",
            "read_project_home",
            json!({}),
            usage,
        );
    }
    if tool_output("demo-project-home-patch").is_none() {
        let revision = tool_output("demo-project-home-read")
            .and_then(|output| output.pointer("/result/revision"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        return tool_call(
            "demo-project-home-patch",
            "patch_project_home",
            json!({
                "base_revision": revision,
                "operations": [{
                    "kind": "upsert",
                    "id": "overview",
                    "html": "<header><h1>Project overview</h1><p>This page was maintained through the Project-home editing and preview tool loop.</p></header><section><h2>Current state</h2><p>Demo mode verifies the runtime path but does not claim evidence-bearing research results.</p></section><section><h2>Next action</h2><p>Run with a configured provider to produce a Project-specific evidence summary.</p></section>"
                }]
            }),
            usage,
        );
    }
    if tool_output("demo-project-home-preview").is_none() {
        return tool_call(
            "demo-project-home-preview",
            "preview_project_home",
            json!({}),
            usage,
        );
    }
    vec![
        ModelEvent::OutputTextDelta {
            delta: "The Project home page has been edited and previewed.".to_string(),
        },
        ModelEvent::Completed { usage },
    ]
}

fn tool_call(call_id: &str, name: &str, arguments: Value, usage: TokenUsage) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCallCompleted {
            call: ModelToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                arguments: serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string()),
            },
        },
        ModelEvent::Completed { usage },
    ]
}

fn demo_research_response(prompt: &str) -> String {
    let focus = prompt
        .lines()
        .next()
        .unwrap_or("the requested question")
        .trim();
    format!(
        "Demo result for **{focus}**\n\n- Observation: this Turn ran through the same Session, model-step, and tool-ready agent loop used in provider mode.\n- Evidence boundary: demo mode does not perform substantive web or file research.\n- Next step: run with a configured Responses-compatible provider for evidence-bearing results."
    )
}

fn generated_workflow(prompt: &str) -> String {
    let requested_name = prompt
        .lines()
        .find_map(|line| line.strip_prefix("Requested name: "))
        .filter(|value| !value.starts_with("choose "))
        .unwrap_or("Generated evidence review");
    let requested_slug = prompt
        .lines()
        .find_map(|line| line.strip_prefix("Requested slug: "))
        .filter(|value| !value.starts_with("derive "))
        .unwrap_or("generated-evidence-review");
    format!(
        r#"from papermachine import Agent, action, relate, scope, together, workflow


class EvidenceResearcher(Agent):
    access = "research"
    role = "evidence collection"
    system_prompt = "Find concrete support and preserve uncertainty."

    @action
    async def investigate(self, question: str, perspective: str):
        """Investigate the question from the requested perspective and report evidence, counterevidence, and limitations."""


class Reviewer(Agent):
    access = "model_only"
    role = "critical synthesis"

    @action
    async def review(self, question: str, findings: list[str]):
        """Compare the findings and produce a bounded synthesis with explicit disagreements and missing evidence."""


@workflow(
    slug={requested_slug:?},
    name={requested_name:?},
    description="Run two independent evidence routes and synthesize their disagreements.",
    params_schema={{"type": "object", "additionalProperties": False}},
    output_schema={{"type": "object", "properties": {{"summary": {{"type": "string"}}}}}},
)
async def main(ctx):
    primary = EvidenceResearcher(name="Primary evidence")
    challenge = EvidenceResearcher(name="Challenge", role="counterevidence")
    reviewer = Reviewer(name="Review")
    await relate(primary, reviewer, kind="reports_to")
    await relate(challenge, reviewer, kind="challenges")
    async with scope("Independent evidence", ctx.request):
        findings = await together(
            primary.investigate(ctx.request, "primary evidence"),
            challenge.investigate(ctx.request, "counterevidence and boundary cases"),
        )
    summary = await reviewer.review(ctx.request, list(findings))
    return {{"summary": summary}}
"#
    )
}
