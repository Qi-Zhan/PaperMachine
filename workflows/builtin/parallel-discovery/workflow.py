from papermachine import Agent, action, together, workflow


class Researcher(Agent):
    access = "research"
    role = "independent research route"
    system_prompt = "Gather concrete evidence, preserve provenance, and state uncertainty."

    @action(
        search_context_size="low",
        reasoning_effort="high",
        tools=["read_file", "write_file", "exec_command", "fetch_url"],
    )
    async def investigate(
        self,
        question: str,
        perspective: str,
        prior_context_brief: str,
    ):
        """Investigate the question from the assigned perspective. prior_context_brief contains unverified leads from earlier Project work; use relevant leads and avoid repeated dead ends, but independently verify material claims. Use tools when useful and return evidence, counterevidence, and open questions."""


class ContextAnalyst(Agent):
    access = "model_only"
    role = "prior Project context analyst"
    system_prompt = "Extract a compact, provenance-preserving set of relevant leads from prior Project work. Never turn an earlier conclusion into verified evidence and never include unrelated history."

    @action(reasoning_effort="medium", tools=[])
    async def distill(self, question: str, project_context: dict):
        """Return a compact brief covering relevant prior findings, source leads, contradictions, unresolved gaps, and work to avoid repeating for this question."""


class Synthesizer(Agent):
    access = "model_only"
    role = "research synthesis"
    system_prompt = "Compare independent findings and keep conclusions bounded by the evidence."

    @action(reasoning_effort="high", tools=[])
    async def synthesize(self, question: str, findings: list[str]):
        """Synthesize the findings into a concise answer. Identify agreements, conflicts, missing evidence, and the strongest defensible conclusion."""


@workflow(
    slug="parallel-discovery",
    name="Parallel discovery",
    description="Investigate a request from independent directions, then combine the evidence.",
    params_schema={
        "type": "object",
        "properties": {
            "perspectives": {
                "type": "array",
                "items": {"type": "string"},
                "default": ["primary evidence", "counterevidence and limitations"],
            },
            "research_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Research model",
            },
            "synthesis_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Synthesis model",
            },
        },
        "additionalProperties": False,
    },
)
async def main(ctx):
    perspectives = ctx.params.get("perspectives") or [
        "primary evidence",
        "counterevidence and limitations",
    ]
    research_model = str(ctx.params.get("research_model") or "")
    synthesis_model = str(ctx.params.get("synthesis_model") or "")
    researchers = [
        Researcher(
            name=f"Route {index + 1}",
            role=perspective,
            model=research_model,
        )
        for index, perspective in enumerate(perspectives)
    ]
    synthesizer = Synthesizer(name="Synthesis", model=synthesis_model)
    context_analyst = ContextAnalyst(name="Prior context", model=synthesis_model)
    prior_context_brief = ""
    if ctx.context:
        prior_context_brief = await context_analyst.distill(ctx.request, ctx.context)
    findings = await together(
        *(
            researcher.investigate(
                ctx.request,
                perspective,
                prior_context_brief,
            )
            for researcher, perspective in zip(researchers, perspectives)
        )
    )

    summary = await synthesizer.synthesize(ctx.request, list(findings))
    return {"summary": summary}
