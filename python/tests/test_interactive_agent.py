from __future__ import annotations

import asyncio
import runpy
import unittest
from pathlib import Path
from typing import Any

from papermachine import WorkflowContext, _Runtime, _set_runtime


WORKFLOW = runpy.run_path(
    Path(__file__).resolve().parents[2]
    / "workflows"
    / "builtin"
    / "interactive-agent"
    / "workflow.py"
)
main = WORKFLOW["main"]


class InteractiveAgentWorkflowTests(unittest.TestCase):
    def test_each_agent_turn_is_preceded_by_a_durable_human_answer(self) -> None:
        effects: list[tuple[str, str, dict[str, Any]]] = []
        answers = iter(
            [
                ("human-1", "Investigate the cache miss."),
                ("human-2", "/finish"),
            ]
        )

        async def send(
            effect_id: str,
            kind: str,
            payload: dict[str, Any],
        ) -> Any:
            effects.append((effect_id, kind, payload))
            if kind == "create_agent":
                return {
                    "agent_instance_id": "agent-1",
                    "session_id": "session-1",
                    "access": payload["access"],
                }
            if kind == "ask_human":
                request_id, answer = next(answers)
                return {"human_request_id": request_id, "answer": answer}
            if kind == "invoke_action":
                return {"output": "The prefix changed.", "turn_id": "turn-1"}
            raise AssertionError(f"unexpected effect: {kind}")

        async def run() -> dict[str, Any]:
            _set_runtime(_Runtime(send))
            return await main(
                WorkflowContext(
                    objective="Persistent interactive work",
                    input={"session_title": "Cache investigation"},
                    workflow_id="workflow-1",
                )
            )

        self.assertEqual(
            asyncio.run(run()),
            {"last_response": "The prefix changed.", "turn_count": 1},
        )
        self.assertEqual(
            [kind for _, kind, _ in effects],
            ["create_agent", "ask_human", "invoke_action", "ask_human"],
        )
        action = effects[2][2]
        self.assertEqual(action["human_request_id"], "human-1")
        self.assertEqual(action["human_message_argument"], "message")
        self.assertEqual(action["arguments"]["message"], "Investigate the cache miss.")


if __name__ == "__main__":
    unittest.main()
