from papermachine import Agent, Team, action, relate, scope, together, workflow


class Researcher(Agent):
    access = "research"
    role = "independent research route"
    system_prompt = "Gather concrete evidence, preserve provenance, and state uncertainty."

    @action(
        max_search_calls=16,
        search_context_size="low",
        reasoning_effort="high",
    )
    async def investigate(self, question: str, perspective: str):
        """Investigate the question from the assigned perspective. Use tools when useful and return evidence, counterevidence, and open questions."""


class Synthesizer(Agent):
    access = "model_only"
    role = "research synthesis"
    system_prompt = "Compare independent findings and keep conclusions bounded by the evidence."

    @action(max_steps=1, reasoning_effort="high", max_output_tokens=16_384)
    async def synthesize(self, question: str, findings: list[str]):
        """Synthesize the findings into a concise answer. Identify agreements, conflicts, missing evidence, and the strongest defensible conclusion."""


@workflow(
    slug="parallel-discovery",
    name="Parallel discovery",
    description="Run bounded independent research Sessions concurrently, then synthesize their evidence in a dedicated Session.",
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
                "description": "Optional model profile for every Researcher; empty inherits the Run model.",
            },
            "synthesis_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Synthesis model",
                "description": "Optional model profile for the Synthesizer; empty inherits the Run model.",
            },
        },
        "additionalProperties": False,
    },
    output_schema={
        "type": "object",
        "properties": {"summary": {"type": "string"}},
        "required": ["summary"],
    },
    budget={
        "max_agents": 8,
        "max_concurrent_actions": 4,
        "max_action_steps": 24,
        "max_total_tokens": 1500000,
        "max_uncached_tokens": 400000,
        "max_hosted_search_calls": 64,
        "max_wall_time_seconds": 7200,
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
    team = Team("Discovery routes", *researchers)
    synthesizer = Synthesizer(name="Synthesis", model=synthesis_model)
    await team.activate()
    for researcher in researchers:
        await relate(
            researcher,
            synthesizer,
            kind="reports_to",
            instructions="Send evidence and uncertainty to the synthesis Session.",
        )

    async with scope("Independent discovery", ctx.request):
        findings = await together(
            *(
                researcher.investigate(ctx.request, perspective)
                for researcher, perspective in zip(researchers, perspectives)
            )
        )

    summary = await synthesizer.synthesize(ctx.request, list(findings))
    return {"summary": summary}
