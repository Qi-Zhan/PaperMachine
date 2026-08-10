from papermachine import Agent, action, together, workflow


class Researcher(Agent):
    access = "research"
    role = "independent research route"
    system_prompt = "Gather concrete evidence, preserve provenance, and state uncertainty."

    @action(
        search_context_size="low",
        reasoning_effort="high",
        tools=["read_file", "write_file", "exec_command", "fetch_url", "read_resource"],
    )
    async def investigate(
        self,
        question: str,
        perspective: str,
    ):
        """Investigate the question from the assigned perspective. Read relevant Project resources when earlier work may help, but independently verify material claims. Use tools when useful and return evidence, counterevidence, and open questions."""


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
    findings = await together(
        *(
            researcher.investigate(
                ctx.request,
                perspective,
            )
            for researcher, perspective in zip(researchers, perspectives)
        )
    )

    summary = await synthesizer.synthesize(ctx.request, list(findings))
    return {"summary": summary}
