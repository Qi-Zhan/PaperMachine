from __future__ import annotations

import asyncio
import runpy
import unittest
from pathlib import Path
from typing import Any

from papermachine import SessionContext, _Runtime, _set_runtime


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
                "Inspected the failure and identified the cause.\n"
                '```json\n{"message":"Inspected the failure.","status":"active"}\n```',
                "Fixed the failure and verified the result.\n"
                '```json\n{"message":"Fixed and verified it.","status":"complete"}\n```',
            ]
        )

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {
                    "agent_id": "agent-goal",
                    "access": payload["access"],
                }
            if kind == "invoke_action":
                return {
                    "action_invocation_id": f"invocation-{len(effects)}",
                    "output": next(responses),
                    "turn_id": f"turn-{len(effects)}",
                }
            raise AssertionError(f"unexpected effect: {kind}")

        _set_runtime(_Runtime(send))
        output = asyncio.run(
            main(
                SessionContext(
                    request="Find and fix the cache bug.",
                    instructions="",
                    params={"session_title": "Cache goal", "agent_model": "glm"},
                    session_id="workflow-goal",
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
        work_actions = [effects[1][1], effects[2][1]]
        self.assertEqual(
            {action["agent_id"] for action in work_actions},
            {"agent-goal"},
        )
        self.assertEqual(
            [action["action_name"] for action in work_actions],
            ["work", "work"],
        )
        self.assertTrue(
            all(
                action["arguments"]["objective"] == "Find and fix the cache bug."
                for action in work_actions
            )
        )
        self.assertTrue(
            all(action["response_format"] is None for action in work_actions)
        )
        self.assertTrue(all(action["tool_policy"] is None for action in work_actions))
        self.assertTrue(
            all(
                action["web_search_context_size"] == "high"
                for action in work_actions
            )
        )
        self.assertNotIn("ask_human", [kind for kind, _ in effects])

    def test_goal_can_finish_on_the_first_turn(self) -> None:
        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            if kind == "create_agent":
                return {
                    "agent_id": "agent-goal",
                    "access": payload["access"],
                }
            if kind == "invoke_action" and payload["action_name"] == "work":
                return {
                    "action_invocation_id": "invocation-work",
                    "output": (
                        "Already verified.\n"
                        '```json\n{"message":"Already verified.",'
                        '"status":"complete"}\n```'
                    ),
                    "turn_id": "turn-work",
                }
            raise AssertionError(f"unexpected effect: {kind}")

        _set_runtime(_Runtime(send))
        output = asyncio.run(
            main(
                SessionContext(
                    request="Verify the current result.",
                    instructions="",
                    params={},
                    session_id="workflow-goal",
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

    def test_invalid_goal_decision_fails_without_another_work_turn(self) -> None:
        actions: list[str] = []

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            if kind == "create_agent":
                return {"agent_id": "agent-goal", "access": payload["access"]}
            if kind == "invoke_action":
                actions.append(payload["action_name"])
                return {
                    "action_invocation_id": f"invocation-{len(actions)}",
                    "output": (
                        "Checked the current state."
                        if payload["action_name"] == "work"
                        else '{"message":"Checked.","status":"done"}'
                    ),
                }
            raise AssertionError(f"unexpected effect: {kind}")

        _set_runtime(_Runtime(send))
        with self.assertRaisesRegex(ValueError, "work.status must be one of"):
            asyncio.run(
                main(
                    SessionContext(
                        request="Verify the result.",
                        instructions="",
                        params={},
                        session_id="workflow-goal",
                    )
                )
            )
        self.assertEqual(
            actions,
            [
                "work",
                "work_finalize",
                "work_json_repair",
                "work_json_repair",
            ],
        )


if __name__ == "__main__":
    unittest.main()
