from __future__ import annotations

import asyncio
import runpy
import unittest
from pathlib import Path
from typing import Any

from papermachine import WorkflowContext, _Runtime, _set_runtime


WORKFLOW_PATH = (
    Path(__file__).resolve().parents[2]
    / "workflows"
    / "builtin"
    / "goal"
    / "workflow.py"
)
WORKFLOW = runpy.run_path(WORKFLOW_PATH)
main = WORKFLOW["main"]


class GoalWorkflowTests(unittest.TestCase):
    def test_goal_reuses_one_agent_and_continues_without_human_wait(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []
        responses = iter(
            [
                "Inspected the failure.\n<!-- papermachine-goal:active -->",
                "Fixed and verified it.\n<!-- papermachine-goal:complete -->",
            ]
        )

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {
                    "agent_instance_id": "agent-goal",
                    "session_id": "session-goal",
                    "access": payload["access"],
                }
            if kind == "invoke_action":
                return {
                    "output": next(responses),
                    "turn_id": f"turn-{len(effects)}",
                }
            raise AssertionError(f"unexpected effect: {kind}")

        _set_runtime(_Runtime(send))
        output = asyncio.run(
            main(
                WorkflowContext(
                    request="Find and fix the cache bug.",
                    instructions="",
                    params={"session_title": "Cache goal", "agent_model": "glm"},
                    workflow_id="workflow-goal",
                    context={"sessions": [{"title": "Earlier investigation"}]},
                )
            )
        )

        self.assertEqual(
            [kind for kind, _ in effects],
            [
                "create_agent",
                "invoke_action",
                "invoke_action",
            ],
        )
        self.assertEqual(
            output,
            {
                "result": "Fixed and verified it.",
                "status": "complete",
                "iterations": 2,
            },
        )
        create = effects[0][1]
        self.assertEqual(create["model"], "glm")
        self.assertEqual(create["name"], "Cache goal")
        first = effects[1][1]
        second = effects[2][1]
        self.assertEqual(
            {
                first["agent_instance_id"],
                second["agent_instance_id"],
            },
            {"agent-goal"},
        )
        self.assertEqual(first["action_name"], "work")
        self.assertEqual(second["action_name"], "work")
        self.assertEqual(first["arguments"]["objective"], "Find and fix the cache bug.")
        self.assertEqual(second["arguments"]["objective"], "Find and fix the cache bug.")
        self.assertEqual(
            first["arguments"]["initial_project_context"]["sessions"][0]["title"],
            "Earlier investigation",
        )
        self.assertEqual(second["arguments"]["initial_project_context"], {})
        self.assertNotIn("ask_human", [kind for kind, _ in effects])

    def test_goal_can_finish_on_the_first_turn(self) -> None:
        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            if kind == "create_agent":
                return {
                    "agent_instance_id": "agent-goal",
                    "session_id": "session-goal",
                    "access": payload["access"],
                }
            if kind == "invoke_action":
                return {
                    "output": "Already verified.\n<!-- papermachine-goal:complete -->",
                    "turn_id": "turn-work",
                }
            raise AssertionError(f"unexpected effect: {kind}")

        _set_runtime(_Runtime(send))
        output = asyncio.run(
            main(
                WorkflowContext(
                    request="Verify the current result.",
                    instructions="",
                    params={},
                    workflow_id="workflow-goal",
                )
            )
        )
        self.assertEqual(
            output,
            {
                "result": "Already verified.",
                "status": "complete",
                "iterations": 1,
            },
        )

    def test_goal_turn_status_matches_codex_active_until_updated_semantics(self) -> None:
        parse = WORKFLOW["_parse_goal_turn"]
        self.assertEqual(
            parse("Progress.\n<!-- papermachine-goal:active -->\n"),
            ("Progress.", "active"),
        )
        self.assertEqual(
            parse("Done.\n<!-- papermachine-goal:complete -->"),
            ("Done.", "complete"),
        )
        self.assertEqual(
            parse("External access is still unavailable.\n<!-- papermachine-goal:blocked -->"),
            ("External access is still unavailable.", "blocked"),
        )
        self.assertEqual(parse("No control update."), ("No control update.", "active"))


if __name__ == "__main__":
    unittest.main()
