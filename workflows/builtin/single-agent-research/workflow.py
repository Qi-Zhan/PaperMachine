from papermachine import Agent, action, workflow


class Researcher(Agent):
    access = "research"
    role = "single-agent deep researcher and report writer"
    system_prompt = """Independently research the user's complete question and produce the exact requested deliverable. Use hosted web search actively: reformulate queries, open relevant pages, and verify material claims against primary or authoritative sources. Do not stop at the first plausible answer, especially when the user requests a comprehensive list. Reconcile conflicting evidence and preserve exact names, numbers, dates, qualifications, JSON fields, and uncertainty. If the user requests JSON, return valid JSON with no Markdown fence or surrounding commentary. Otherwise use direct source URLs next to the claims they support. Do not expose scratch work or claim that a source says something it does not say."""

    @action(
        search_context_size="low",
        reasoning_effort="high",
        finalize="after_search",
    )
    async def research(self, question: str, prior_project_context: dict):
        """Research the complete question with live web search and return only the requested final deliverable. prior_project_context contains optional earlier Project work selected by the user: use it to continue useful leads and avoid repetition, but independently verify material claims and do not expose unrelated history. Obey any structured-output contract exactly; for reports, answer every requested part, explain the evidence-to-conclusion reasoning, include direct inline source links, and state material limitations."""


@workflow(
    slug="single-agent-research",
    name="Single-agent research",
    description="Let one persistent research Session use hosted web search, reason, and produce the exact requested deliverable without evaluator or writer handoffs.",
    params_schema={"type": "object", "properties": {}, "additionalProperties": False},
    output_schema={
        "type": "object",
        "properties": {"report": {"type": "string"}},
        "required": ["report"],
    },
)
async def main(ctx):
    researcher = Researcher(name="Single researcher")
    report = await researcher.research(ctx.request, ctx.context)
    return {"report": report}
