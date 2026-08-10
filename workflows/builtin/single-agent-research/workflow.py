from papermachine import Agent, action, workflow


class Researcher(Agent):
    access = "model_only"
    role = "single-agent deep researcher and report writer"
    system_prompt = """Independently research the user's complete question and produce the exact requested deliverable. When hosted web search is available, reformulate queries and search actively; otherwise do not pretend to have searched. Open relevant pages and verify material claims against primary or authoritative sources. Do not stop at the first plausible answer, especially when the user requests a comprehensive list. Reconcile conflicting evidence and preserve exact names, numbers, dates, qualifications, JSON fields, and uncertainty. If the user requests JSON, return valid JSON with no Markdown fence or surrounding commentary. Otherwise use direct source URLs next to the claims they support. Do not expose scratch work or claim that a source says something it does not say."""

    @action(
        search_context_size="low",
        reasoning_effort="high",
        finalize="after_search",
        tools=[],
    )
    async def research(self, question: str):
        """Research the complete question and return only the requested final deliverable. Never claim to have searched or opened a source unless hosted search was available and used. Obey any structured-output contract exactly; for reports, answer every requested part, explain the evidence-to-conclusion reasoning, include direct inline source links, and state material limitations."""


@workflow(
    slug="single-agent-research",
    name="Single-agent research",
    description="Research a request and produce the requested deliverable.",
    params_schema={"type": "object", "properties": {}, "additionalProperties": False},
)
async def main(ctx):
    researcher = Researcher(name="Single researcher")
    report = await researcher.research(ctx.request)
    return {"report": report}
