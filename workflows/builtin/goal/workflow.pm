version 1;

schema GoalDecision = object {
    message: string,
    status: enum["active", "complete", "blocked"],
};

agent GoalAgent {
    access = workspace;
    role = "persistent goal agent";
    system = """
Own the objective until it is verified complete. Work in one persistent Session
so prior reasoning, evidence, tool results, and Workspace changes remain
available. Make concrete progress on every Turn. Preserve the full objective,
inspect the current state, verify results, and report uncertainty honestly.
Return active while required work remains, complete only after verifying the
whole objective, and blocked only after the same external condition prevents
meaningful progress for three consecutive Turns.
""";

    action work(objective) {
        search_context = high;
        finalize = if_needed;
        result = GoalDecision;
        prompt = """
Work toward the objective using the available tools.
Inspect and verify the current state.
End with a normal user-facing report of work completed, verification, and any
remaining work.
""";
    }
}

workflow goal {
    name = "Goal";
    description = "Keep working on an objective until it is complete.";
    request = required;

    params {
        session_title = string(default = "Goal", title = "Session title");
        agent_model = model_profile(title = "Agent model");
        agent_access = access(default = "workspace", title = "Agent access");
    }

    run(ctx) {
        let worker = GoalAgent(
            key = "main",
            name = trim(ctx.params.session_title),
            model = get(ctx.params, "agent_model", ""),
            access = ctx.params.agent_access,
        );
        var iterations = 0;
        var latest_result = "";

        loop {
            iterations += 1;
            let decision = await worker.work(objective = ctx.request);
            let message = trim(decision.message);
            if message != "" {
                latest_result = message;
            }
            var final_message = latest_result;
            if message != "" {
                final_message = message;
            }
            match decision.status {
                "active" => continue,
                "complete" | "blocked" => return {
                    result: final_message,
                    status: decision.status,
                    iterations,
                },
                _ => fail("Goal status is outside its validated schema"),
            }
        }
    }
}
