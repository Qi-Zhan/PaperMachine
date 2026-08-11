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
use serde_json::json;

#[derive(Clone, Copy, Debug, Default)]
pub struct DemoModelClient;

#[async_trait]
impl ModelClient for DemoModelClient {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ModelError> {
        if request.instructions.contains("Maintain the Project home") {
            return Ok(stream::iter(project_home_response().into_iter().map(Ok)).boxed());
        }
        if let Some(response_format) = &request.response_format {
            let output = demo_structured_response(&response_format.name);
            return Ok(completed_text(output));
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
            .contains("PaperMachine Workflow Language v1")
        {
            generated_workflow(prompt)
        } else {
            demo_research_response(prompt)
        };
        Ok(completed_text(output))
    }
}

fn completed_text(output: String) -> ModelStream {
    stream::iter([
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
    .boxed()
}

fn demo_structured_response(name: &str) -> String {
    let value = match name {
        "work_result" => json!({
            "message": "Demo mode completed and verified the requested runtime exercise.",
            "status": "complete"
        }),
        "plan_result" => json!({
            "deliverable": "A bounded demo synthesis",
            "acceptance_criteria": ["Run two independent routes", "Preserve the demo evidence boundary"],
            "routes": [
                {"key": "primary", "name": "Primary route", "objective": "Exercise the primary evidence worldline"},
                {"key": "challenge", "name": "Challenge route", "objective": "Exercise the counterevidence worldline"}
            ],
            "verification_notes": ["Demo mode validates orchestration, not external evidence"]
        }),
        "assess_result" => json!({
            "complete": true,
            "rationale": "Both demo routes completed; no evidence-bearing claim is made.",
            "supported_conclusions": ["The Workflow runtime completed both named routes"],
            "unresolved_gaps": ["External evidence was not collected in demo mode"],
            "contradictions": [],
            "follow_ups": []
        }),
        "review_draft_result" => json!({
            "complete": true,
            "feedback": "The draft preserves the demo evidence boundary."
        }),
        _ => json!({}),
    };
    serde_json::to_string(&value).expect("demo structured response should serialize")
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
            delta: "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Project overview</title></head><body><header><h1>Project overview</h1><p>Demo mode verifies the Project resource and publication path without claiming evidence-bearing research results.</p></header><section><h2>Next action</h2><p>Run with a configured provider to produce a Project-specific evidence summary.</p></section></body></html>".to_string(),
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
        r#"version 1;

agent EvidenceResearcher {{
    access = model_only;
    role = "evidence collection";
    system = "Find concrete support and preserve uncertainty.";
    action investigate(question, perspective) {{
        prompt = "Investigate the question from the requested perspective and report evidence, counterevidence, and limitations.";
    }}
}}

agent Reviewer {{
    access = model_only;
    role = "critical synthesis";
    system = "Compare independent findings and keep conclusions bounded.";
    action review(question, findings) {{
        prompt = "Compare the findings and produce a bounded synthesis with explicit disagreements and missing evidence.";
    }}
}}

workflow generated {{
    slug = {requested_slug:?};
    name = {requested_name:?};
    description = "Run two independent evidence routes and synthesize their disagreements.";
    request = required;
    params {{}}
    run(ctx) {{
        let findings = parallel {{
            primary => {{
                let worker = EvidenceResearcher(key = "primary", name = "Primary evidence");
                await worker.investigate(question = ctx.request, perspective = "primary evidence")
            }},
            challenge => {{
                let worker = EvidenceResearcher(key = "challenge", name = "Challenge");
                await worker.investigate(question = ctx.request, perspective = "counterevidence and boundary cases")
            }},
        }};
        let reviewer = Reviewer(key = "main", name = "Review");
        let summary = await reviewer.review(question = ctx.request, findings = [findings.primary, findings.challenge]);
        return {{summary}};
    }}
}}
"#
    )
}
