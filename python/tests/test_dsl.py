from __future__ import annotations

import asyncio
import unittest
from typing import Any

from papermachine import Agent, _Runtime, _set_runtime, action


class Writer(Agent):
    access = "model_only"

    @action(
        max_steps=1,
        max_search_calls=0,
        search_context_size="low",
        reasoning_effort="low",
        max_output_tokens=2_048,
    )
    async def compose(self, evidence: list[dict[str, Any]]) -> str:
        """Compose only from the supplied evidence."""


class StructuredResearcher(Agent):
    @action(max_steps=1)
    async def research(self) -> dict:
        """Return structured evidence."""


class ActionOptionsTest(unittest.TestCase):
    def test_typed_action_accepts_one_fenced_json_payload(self) -> None:
        async def send(kind: str, payload: dict[str, Any]) -> Any:
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if kind == "invoke_action":
                return {"output": "```json\n{\"answer\": 42}\n```"}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> dict:
            _set_runtime(_Runtime(send))
            return await StructuredResearcher().research()

        self.assertEqual(asyncio.run(invoke()), {"answer": 42})

    def test_typed_action_extracts_object_after_provider_commentary(self) -> None:
        async def send(kind: str, payload: dict[str, Any]) -> Any:
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if kind == "invoke_action":
                return {"output": "Result follows:\n{\"answer\": 42}\nDone."}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> dict:
            _set_runtime(_Runtime(send))
            return await StructuredResearcher().research()

        self.assertEqual(asyncio.run(invoke()), {"answer": 42})

    def test_typed_action_repairs_malformed_provider_json_in_same_session(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if kind == "invoke_action" and payload["action_name"] == "research":
                return {"output": '{"answer": "unterminated}'}
            if kind == "invoke_action" and payload["action_name"] == "research_json_repair":
                return {"output": '{"answer": 42}'}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> dict:
            _set_runtime(_Runtime(send))
            return await StructuredResearcher().research()

        self.assertEqual(asyncio.run(invoke()), {"answer": 42})
        repair = effects[-1][1]
        self.assertEqual(repair["agent_instance_id"], "agent")
        self.assertEqual(repair["max_steps"], 1)
        self.assertEqual(repair["max_search_calls"], 0)
        self.assertEqual(repair["max_output_tokens"], 32_768)

    def test_typed_action_stops_after_two_failed_repairs(self) -> None:
        calls = 0

        async def send(kind: str, payload: dict[str, Any]) -> Any:
            nonlocal calls
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if kind == "invoke_action":
                calls += 1
                return {"output": "not json"}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> dict:
            _set_runtime(_Runtime(send))
            return await StructuredResearcher().research()

        with self.assertRaisesRegex(ValueError, "returned invalid JSON"):
            asyncio.run(invoke())
        self.assertEqual(calls, 3)

    def test_max_steps_is_sent_with_the_action_effect(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session", "access": payload["access"]}
            if kind == "invoke_action":
                return {"output": "done", "turn_id": "turn"}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> str:
            _set_runtime(_Runtime(send))
            return await Writer().compose([])

        self.assertEqual(asyncio.run(invoke()), "done")
        self.assertEqual(effects[-1][0], "invoke_action")
        self.assertEqual(effects[-1][1]["max_steps"], 1)
        self.assertEqual(effects[-1][1]["max_search_calls"], 0)
        self.assertEqual(effects[-1][1]["web_search_context_size"], "low")
        self.assertEqual(effects[-1][1]["reasoning_effort"], "low")
        self.assertEqual(effects[-1][1]["max_output_tokens"], 2_048)
        self.assertEqual(effects[0][1]["access"], "model_only")

    def test_constructor_override_and_dynamic_access_change_emit_profiles(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session", "access": payload["access"]}
            if kind == "set_agent_access":
                return {"access": payload["access"]}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> None:
            _set_runtime(_Runtime(send))
            writer = Writer(access="read_only")
            await writer.set_access("workspace")
            await writer._ensure_remote()
            await writer.set_access("research")

        asyncio.run(invoke())
        self.assertEqual([kind for kind, _ in effects], ["create_agent", "set_agent_access"])
        self.assertEqual(effects[0][1]["access"], "workspace")
        self.assertEqual(effects[1][1]["access"], "research")

    def test_invalid_access_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "Agent access must be one of"):
            Writer(access="root")

    def test_max_steps_must_be_positive(self) -> None:
        with self.assertRaisesRegex(ValueError, "positive integer"):

            class Invalid(Agent):
                @action(max_steps=0)
                async def run(self) -> str:
                    """Invalid action."""

    def test_max_search_calls_must_be_non_negative(self) -> None:
        with self.assertRaisesRegex(ValueError, "non-negative integer"):

            class Invalid(Agent):
                @action(max_search_calls=-1)
                async def run(self) -> str:
                    """Invalid action."""

    def test_reasoning_effort_must_be_known(self) -> None:
        with self.assertRaisesRegex(ValueError, "reasoning_effort must be one of"):

            class Invalid(Agent):
                @action(reasoning_effort="ultra")
                async def run(self) -> str:
                    """Invalid action."""

    def test_search_context_size_must_be_known(self) -> None:
        with self.assertRaisesRegex(ValueError, "search_context_size must be one of"):

            class Invalid(Agent):
                @action(search_context_size="huge")
                async def run(self) -> str:
                    """Invalid action."""

    def test_max_output_tokens_must_be_positive(self) -> None:
        with self.assertRaisesRegex(ValueError, "max_output_tokens must be a positive integer"):

            class Invalid(Agent):
                @action(max_output_tokens=0)
                async def run(self) -> str:
                    """Invalid action."""


if __name__ == "__main__":
    unittest.main()
