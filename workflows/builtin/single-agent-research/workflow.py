from papermachine import Agent, action, workflow


class Researcher(Agent):
    access = "research"
    role = "single-agent deep researcher and report writer"
    instructions = """Independently research the user's complete question and produce the exact requested deliverable. Use hosted web search actively: reformulate queries, open relevant pages, and verify material claims against primary or authoritative sources. Do not stop at the first plausible answer, especially when the user requests a comprehensive list. Reconcile conflicting evidence and preserve exact names, numbers, dates, qualifications, JSON fields, and uncertainty. If the user requests JSON, return valid JSON with no Markdown fence or surrounding commentary. Otherwise use direct source URLs next to the claims they support. Do not expose scratch work or claim that a source says something it does not say."""

    @action(
        max_search_calls=32,
        search_context_size="low",
        reasoning_effort="high",
    )
    async def research(self, question: str):
        """Research the complete question with live web search and return only the requested final deliverable. Obey any structured-output contract exactly; for reports, answer every requested part, explain the evidence-to-conclusion reasoning, include direct inline source links, and state material limitations."""


@workflow(
    slug="single-agent-research",
    name="Single-agent research",
    version="0.5.0",
    description="Let one persistent research Session use bounded hosted web search, reason, and produce the exact requested deliverable without evaluator or writer handoffs, with a run-level step budget sized for the full search allowance.",
    input_schema={"type": "object", "properties": {}, "additionalProperties": False},
    output_schema={
        "type": "object",
        "properties": {"report": {"type": "string"}},
        "required": ["report"],
    },
    budget={
        "max_agents": 1,
        "max_concurrent_actions": 1,
        "max_action_steps": 128,
        "max_total_tokens": 1200000,
        "max_uncached_tokens": 300000,
        "max_hosted_search_calls": 32,
        "max_wall_time_seconds": 7200,
    },
)
async def main(ctx):
    researcher = Researcher(name="Single researcher")
    report = await researcher.research(ctx.objective)
    return {"report": report}
