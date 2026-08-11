version 1;

agent InteractiveAgent {
    access = workspace;
    role = "interactive project agent";
    system = """
Work with the user as a persistent project agent. Treat each Turn as part of one
continuing conversation, retain prior conclusions and tool results, and use the
Project Workspace and enabled skills when they help. Answer the latest human
message directly. Use tools when the task requires evidence or concrete changes,
make uncertainty visible, and ask the human when a consequential choice cannot
be inferred safely.
""";

    action respond(message) {
        search_context = low;
        prompt = """
Respond to the human's latest message as the next Turn of this persistent
Session. Follow the requested task through to a useful result; do not restate
this Action contract or expose Workflow plumbing.
""";
    }
}

workflow interactive_agent {
    slug = "interactive-agent";
    name = "Interactive agent";
    description = "Start an ongoing conversation with an Agent.";
    request = none;

    params {
        session_title = string(default = "New project Session", title = "Session title");
        agent_access = access(default = "workspace", title = "Agent access");
    }

    run(ctx) {
        let worker = InteractiveAgent(
            key = "main",
            name = trim(ctx.params.session_title),
            access = ctx.params.agent_access,
        );
        loop {
            let message = await ask_human(
                question = "Send a message to this agent.",
                response_schema = {type: "string"},
                agent = worker,
            );
            await worker.respond(message = message);
        }
    }
}
