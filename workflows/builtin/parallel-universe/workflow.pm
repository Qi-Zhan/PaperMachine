version 1;

agent Researcher {
    access = model_only;
    role = "independent research universe";
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
    role = "cross-universe synthesis";
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

workflow parallel_universe {
    slug = "parallel-universe";
    name = "Parallel universe";
    description = "Explore a request through keyed parallel research universes, then combine their evidence.";
    request = required;

    params {
        perspectives?: list(string(min_len = 1), default = ["primary evidence", "counterevidence and limitations"], min_len = 2, max_len = 8);
        research_model?: model_profile(title = "Research model");
        synthesis_model?: model_profile(title = "Synthesis model");
    }

    run(ctx) {
        let findings = parallel for perspective in ctx.params.perspectives key perspective {
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
            name = "Cross-universe synthesis",
            model = get(ctx.params, "synthesis_model", ""),
        );
        let summary = await synthesizer.synthesize(
            question = ctx.request,
            findings = findings,
        );
        return {summary};
    }
}
