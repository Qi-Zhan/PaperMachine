version 1;

agent Researcher {
    access = model_only;
    role = "independent research route";
    system = "Gather concrete evidence, preserve provenance, and state uncertainty.";

    action investigate(question, perspective) {
        tools = [];
        search_context = low;
        reasoning_effort = high;
        prompt = """
Investigate the complete question from the assigned perspective with hosted
search when available. Independently verify material claims and return evidence,
counterevidence, direct URLs, and open questions.
""";
    }
}

agent Synthesizer {
    access = model_only;
    role = "research synthesis";
    system = "Compare independent findings and keep conclusions bounded by the evidence.";

    action synthesize(question, findings) {
        tools = [];
        reasoning_effort = high;
        prompt = """
Synthesize the findings into a concise answer. Identify agreements, conflicts,
missing evidence, and the strongest defensible conclusion.
""";
    }
}

workflow parallel_discovery {
    slug = "parallel-discovery";
    name = "Parallel discovery";
    description = "Investigate a request from independent directions, then combine the evidence.";
    request = required;

    params {
        perspectives = list(string, default = ["primary evidence", "counterevidence and limitations"]);
        research_model = model_profile(title = "Research model");
        synthesis_model = model_profile(title = "Synthesis model");
    }

    run(ctx) {
        let perspectives = get(ctx.params, "perspectives", ["primary evidence", "counterevidence and limitations"]);
        let findings = parallel for perspective in perspectives key perspective {
            let researcher = Researcher(
                key = perspective,
                name = perspective,
                model = get(ctx.params, "research_model", ""),
            );
            await researcher.investigate(
                question = ctx.request,
                perspective = perspective,
            )
        };
        let synthesizer = Synthesizer(
            key = "main",
            name = "Synthesis",
            model = get(ctx.params, "synthesis_model", ""),
        );
        let summary = await synthesizer.synthesize(
            question = ctx.request,
            findings = findings,
        );
        return {summary};
    }
}
