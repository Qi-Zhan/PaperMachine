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
        waiting_for_next_message = asyncio.Event()
        human_requests = 0

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
                nonlocal human_requests
                human_requests += 1
                if human_requests == 1:
                    return {
                        "human_request_id": "human-1",
                        "answer": "Investigate the cache miss.",
                    }
                waiting_for_next_message.set()
                await asyncio.Future()
            if kind == "invoke_action":
                return {"output": "The prefix changed.", "turn_id": "turn-1"}
            raise AssertionError(f"unexpected effect: {kind}")

        async def run() -> None:
            _set_runtime(_Runtime(send))
            task = asyncio.create_task(
                main(
                    WorkflowContext(
                        request="",
                        instructions="",
                        params={"session_title": "Cache investigation"},
                        workflow_id="workflow-1",
                    )
                )
            )
            await waiting_for_next_message.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task

        asyncio.run(run())
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
