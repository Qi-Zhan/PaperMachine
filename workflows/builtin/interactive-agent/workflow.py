from papermachine import Agent, HumanMessage, action, ask_human, workflow


class InteractiveAgent(Agent):
    access = "research"
    role = "interactive project agent"
    system_prompt = """Work with the user as a persistent project agent. Treat each Turn as part of one continuing conversation, retain prior conclusions and tool results, and use the Project workspace and enabled skills when they help. Answer the latest human message directly. Use tools when the task requires evidence or concrete changes, make uncertainty visible, and ask the human when a consequential choice cannot be inferred safely."""

    @action(search_context_size="low")
    async def respond(self, message: HumanMessage):
        """Respond to the human's latest message as the next Turn of this persistent Session. Follow the requested task through to a useful result; do not restate this action contract or expose workflow plumbing."""


@workflow(
    slug="interactive-agent",
    name="Interactive agent",
    description="Run one persistent Agent Session that waits for a human message before every Turn. The normal New Session action uses this built-in Workflow.",
    request_mode="none",
    params_schema={
        "type": "object",
        "properties": {
            "session_title": {
                "type": "string",
                "title": "Session title",
                "default": "New project Session",
            },
            "agent_system_prompt": {
                "type": "string",
                "title": "Agent system prompt",
                "default": "",
                "description": "Persistent instructions stored on the Agent Session.",
                "x-ui-order": 2,
            },
            "agent_access": {
                "type": "string",
                "title": "Agent access",
                "enum": [
                    "model_only",
                    "read_only",
                    "workspace",
                    "research",
                    "full_access",
                ],
                "default": "research",
                "description": "Initial access profile for the persistent Agent Session.",
                "x-ui-order": 3,
            },
        },
        "additionalProperties": False,
    },
    output_schema={
        "type": "object",
        "properties": {
            "last_response": {"type": "string"},
            "turn_count": {"type": "integer"},
        },
        "required": ["last_response", "turn_count"],
    },
)
async def main(ctx):
    title = str(ctx.params.get("session_title") or "New project Session").strip()
    custom_prompt = str(ctx.params.get("agent_system_prompt") or "").strip()
    access = str(ctx.params.get("agent_access") or "research")
    system_prompt = InteractiveAgent.system_prompt
    if custom_prompt:
        system_prompt = f"{system_prompt}\n\nUser-configured Session instructions:\n{custom_prompt}"
    agent = InteractiveAgent(
        name=title or "New project Session",
        system_prompt=system_prompt,
        access=access,
    )
    last_response = ""
    turn_count = 0

    while True:
        message = await ask_human(
            "Send a message to this agent. Type /finish to close the interactive Workflow.",
            agent=agent,
        )
        if message.strip().casefold() == "/finish":
            return {"last_response": last_response, "turn_count": turn_count}
        last_response = await agent.respond(message)
        turn_count += 1
