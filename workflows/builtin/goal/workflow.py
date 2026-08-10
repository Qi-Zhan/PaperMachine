from papermachine import Agent, Literal, TypedDict, action, workflow


class GoalDecision(TypedDict):
    message: str
    status: Literal["active", "complete", "blocked"]


class GoalAgent(Agent):
    access = "workspace"
    role = "persistent goal agent"
    system_prompt = """Own the user's objective until it is actually complete. Work in one persistent Session so prior reasoning, evidence, tool results, and workspace changes remain available across Turns. On every Turn, make concrete progress with the available tools instead of merely proposing future work. Preserve the full objective, verify the current state before claiming completion, and report uncertainty honestly. Do not wait for or ask the user to continue; make safe, reversible assumptions when possible. Use active whenever any required work remains or completion is not proved. Use complete only after auditing the whole objective against current evidence and verifying that every requested outcome is achieved. Use blocked only when the same external blocking condition has prevented meaningful progress for at least three consecutive Goal Turns."""

    @action(
        search_context_size="high",
        finalize="always",
    )
    async def work(self, objective: str) -> GoalDecision:
        """Continue working toward the objective now. Inspect the Workspace when useful, use tools whenever needed, perform concrete work rather than describing a future Turn, and verify the result. End the work phase with a normal user-facing report. The finalized result must be a JSON object containing exactly a message string and a status of active, complete, or blocked."""


@workflow(
    slug="goal",
    name="Goal",
    description="Keep working on an objective until it is complete.",
    params_schema={
        "type": "object",
        "properties": {
            "session_title": {
                "type": "string",
                "title": "Session title",
                "default": "Goal",
            },
            "agent_system_prompt": {
                "type": "string",
                "title": "Agent system prompt",
                "default": "",
                "description": "Additional instructions.",
                "x-ui-order": 2,
            },
            "agent_model": {
                "type": "string",
                "format": "model-profile",
                "title": "Agent model",
                "x-ui-order": 3,
            },
            "agent_access": {
                "type": "string",
                "title": "Agent access",
                "enum": [
                    "model_only",
                    "read_only",
                    "workspace",
                    "full_access",
                ],
                "default": "workspace",
                "x-ui-order": 4,
            },
        },
        "additionalProperties": False,
    },
)
async def main(ctx):
    title = str(ctx.params.get("session_title") or "Goal").strip()
    custom_prompt = str(ctx.params.get("agent_system_prompt") or "").strip()
    model = str(ctx.params.get("agent_model") or "")
    access = str(ctx.params.get("agent_access") or "workspace")
    system_prompt = GoalAgent.system_prompt
    if custom_prompt:
        system_prompt = f"{system_prompt}\n\nUser-configured Goal instructions:\n{custom_prompt}"
    agent = GoalAgent(
        name=title or "Goal",
        system_prompt=system_prompt,
        model=model,
        access=access,
    )

    iterations = 0
    latest_result = ""
    while True:
        iterations += 1
        decision = await agent.work(ctx.request)
        status = decision.get("status")
        result = decision.get("message")
        if status not in {"active", "complete", "blocked"}:
            raise ValueError("Goal result status must be active, complete, or blocked")
        if not isinstance(result, str):
            raise ValueError("Goal result message must be a string")
        if result.strip():
            latest_result = result.strip()
        if status != "active":
            return {
                "result": result.strip() or latest_result,
                "status": status,
                "iterations": iterations,
            }
