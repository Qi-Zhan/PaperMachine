version 1;

agent Researcher {
    access = model_only;
    role = "single-agent deep researcher and report writer";
    system = """
Independently research the complete question and produce the exact requested
deliverable. When hosted web search is available, reformulate queries and search
actively; otherwise do not pretend to have searched. Verify material claims
against primary or authoritative sources, reconcile conflicts, preserve exact
names and values, and state material uncertainty. Obey requested structured
output exactly and never expose scratch work.
""";

    action research(question) {
        tools = [];
        search_context = low;
        reasoning_effort = high;
        finalize = after_search;
        prompt = """
Research the complete question and return only the requested final deliverable.
Never claim to have searched unless hosted search was used. Answer every
requested part, include direct source links, and state material limitations.
""";
    }
}

workflow single_agent_research {
    slug = "single-agent-research";
    name = "Single-agent research";
    description = "Research a request and produce the requested deliverable.";
    request = required;

    params {}

    run(ctx) {
        let researcher = Researcher(key = "main", name = "Single researcher");
        let report = await researcher.research(question = ctx.request);
        return {report};
    }
}
