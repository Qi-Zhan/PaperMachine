use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use papermachine_model::ModelClient;
use papermachine_model::ModelError;
use papermachine_model::ModelStream;
use papermachine_protocol::ModelEvent;
use papermachine_protocol::ModelInputItem;
use papermachine_protocol::ModelRequest;
use papermachine_protocol::TokenUsage;

#[derive(Clone, Copy, Debug, Default)]
pub struct DemoModelClient;

#[async_trait]
impl ModelClient for DemoModelClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        if request.instructions.contains("Maintain the Project home") {
            return Ok(stream::iter(project_home_response().into_iter().map(Ok)).boxed());
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

fn project_home_response() -> Vec<ModelEvent> {
    let usage = TokenUsage {
        input_tokens: 160,
        output_tokens: 40,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
    };
    vec![
        ModelEvent::OutputTextDelta {
            delta: "<header><h1>Project overview</h1><p>Demo mode verifies the Project resource and publication path without claiming evidence-bearing research results.</p></header><section><h2>Next action</h2><p>Run with a configured provider to produce a Project-specific evidence summary.</p></section>".to_string(),
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
        r#"from papermachine import Agent, action, together, workflow


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
)
async def main(ctx):
    primary = EvidenceResearcher(name="Primary evidence")
    challenge = EvidenceResearcher(name="Challenge", role="counterevidence")
    reviewer = Reviewer(name="Review")
    findings = await together(
        primary.investigate(ctx.request, "primary evidence"),
        challenge.investigate(ctx.request, "counterevidence and boundary cases"),
    )
    summary = await reviewer.review(ctx.request, list(findings))
    return {{"summary": summary}}
"#
    )
}
