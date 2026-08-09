from papermachine import Agent, action, workflow


class GoalAgent(Agent):
    access = "research"
    role = "persistent goal agent"
    system_prompt = """Own the user's objective until it is actually complete. Work in one persistent Session so prior reasoning, evidence, tool results, and workspace changes remain available across Turns. On every Turn, make concrete progress with the available tools instead of merely proposing future work. Preserve the full objective, verify the current state before claiming completion, and report uncertainty honestly. Do not wait for or ask the user to continue; make safe, reversible assumptions when possible.

End every response with exactly one of these control lines as its final non-empty line:
<!-- papermachine-goal:active -->
<!-- papermachine-goal:complete -->
<!-- papermachine-goal:blocked -->

Use active whenever any required work remains or completion is not proved. Use complete only after auditing the whole objective against current evidence and verifying that every requested outcome is achieved. Use blocked only when the same external blocking condition has prevented meaningful progress for at least three consecutive Goal Turns. The control line is for the Workflow runtime; do not explain or quote it. The runtime automatically starts another Turn only while the status remains active."""

    @action(
        search_context_size="high",
        tools=["read_file", "write_file", "exec_command", "fetch_url"],
    )
    async def work(
        self,
        objective: str,
        initial_project_context: dict,
    ):
        """Continue working toward objective now. initial_project_context contains a captured Project snapshot only on the first Turn and is empty thereafter; the persistent Session already retains prior Turns. Use tools whenever they are needed, perform concrete work rather than describing what a future Turn could do, verify the results you produced, and return a normal user-facing progress update or final result followed by the required Goal control line."""


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
                    "research",
                    "full_access",
                ],
                "default": "research",
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
    access = str(ctx.params.get("agent_access") or "research")
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
    initial_context = ctx.context
    latest_result = ""
    while True:
        iterations += 1
        result, status = _parse_goal_turn(
            await agent.work(ctx.request, initial_context)
        )
        if result:
            latest_result = result
        if status != "active":
            return {
                "result": result or latest_result,
                "status": status,
                "iterations": iterations,
            }
        initial_context = {}


def _parse_goal_turn(value):
    text = str(value or "")
    lines = text.splitlines()
    last_nonempty = next(
        (index for index in range(len(lines) - 1, -1, -1) if lines[index].strip()),
        None,
    )
    if last_nonempty is None:
        return "", "active"

    statuses = {
        "<!-- papermachine-goal:active -->": "active",
        "<!-- papermachine-goal:complete -->": "complete",
        "<!-- papermachine-goal:blocked -->": "blocked",
    }
    status = statuses.get(lines[last_nonempty].strip())
    if status is None:
        # This is equivalent to a Codex Goal Turn that did not call update_goal:
        # the persisted status remains active, so the runtime continues.
        return text, "active"
    return "\n".join(lines[:last_nonempty]).rstrip(), status
