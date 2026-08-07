from __future__ import annotations

import asyncio
import unittest
from typing import Any

from papermachine import (
    Agent,
    HumanMessage,
    _Runtime,
    _set_runtime,
    action,
    ask_human,
    together,
)


class Writer(Agent):
    access = "model_only"

    @action(
        search_context_size="low",
        reasoning_effort="low",
    )
    async def compose(self, evidence: list[dict[str, Any]]) -> str:
        """Compose only from the supplied evidence."""


class StructuredResearcher(Agent):
    @action
    async def research(self) -> dict:
        """Return structured evidence."""


class RouteResearcher(Agent):
    @action
    async def investigate(self, route: str) -> str:
        """Investigate one route."""


class FinalizingResearcher(Agent):
    @action(finalize="after_search")
    async def research(self, question: str) -> str:
        """Research and return the final answer."""


class ConversationalAgent(Agent):
    @action
    async def respond(self, message: HumanMessage) -> str:
        """Respond to the human message."""


class ActionOptionsTest(unittest.TestCase):
    def test_after_search_finalization_uses_same_session_without_tools(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if payload["action_name"] == "research":
                return {
                    "output": "I finished searching and will now write the answer.",
                    "hosted_search_calls_used": 3,
                }
            if payload["action_name"] == "research_finalize":
                return {"output": "The complete final answer."}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> str:
            _set_runtime(_Runtime(send))
            return await FinalizingResearcher().research("Question")

        self.assertEqual(asyncio.run(invoke()), "The complete final answer.")
        action_effects = [payload for kind, payload in effects if kind == "invoke_action"]
        self.assertEqual(
            [payload["action_name"] for payload in action_effects],
            ["research", "research_finalize"],
        )
        self.assertTrue(action_effects[0]["tools_enabled"])
        self.assertFalse(action_effects[1]["tools_enabled"])
        self.assertEqual(action_effects[1]["agent_instance_id"], "agent")

    def test_after_search_finalization_skips_search_free_result(self) -> None:
        actions = 0

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            nonlocal actions
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if kind == "invoke_action":
                actions += 1
                return {"output": "Already final.", "hosted_search_calls_used": 0}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> str:
            _set_runtime(_Runtime(send))
            return await FinalizingResearcher().research("Question")

        self.assertEqual(asyncio.run(invoke()), "Already final.")
        self.assertEqual(actions, 1)

    def test_string_human_answer_preserves_provenance_for_a_user_turn(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {
                    "agent_instance_id": "agent",
                    "session_id": "session",
                    "access": payload["access"],
                }
            if kind == "ask_human":
                return {
                    "human_request_id": "request-1",
                    "answer": "Please inspect the cache behavior.",
                }
            if kind == "invoke_action":
                return {"output": "I will inspect it.", "turn_id": "turn-1"}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> str:
            _set_runtime(_Runtime(send))
            agent = ConversationalAgent("Interactive")
            message = await ask_human("Next message", agent=agent)
            self.assertIsInstance(message, HumanMessage)
            self.assertEqual(message.request_id, "request-1")
            return await agent.respond(message)

        self.assertEqual(asyncio.run(invoke()), "I will inspect it.")
        action_payload = effects[-1][1]
        self.assertEqual(action_payload["arguments"]["message"], "Please inspect the cache behavior.")
        self.assertEqual(action_payload["human_request_id"], "request-1")
        self.assertEqual(action_payload["human_message_argument"], "message")

    def test_human_message_parameter_rejects_unattributed_strings(self) -> None:
        with self.assertRaisesRegex(TypeError, "returned by ask_human"):
            ConversationalAgent().respond("not a durable human answer")

    def test_effect_ids_are_stable_when_parallel_completion_order_changes(self) -> None:
        async def run(delays: dict[str, float]) -> dict[tuple[str, str], str]:
            observed: dict[tuple[str, str], str] = {}

            async def send(
                effect_id: str,
                kind: str,
                payload: dict[str, Any],
            ) -> Any:
                identity = str(
                    payload.get("name") or payload.get("arguments", {}).get("route")
                )
                observed[(kind, identity)] = effect_id
                await asyncio.sleep(delays.get(identity, 0))
                if kind == "create_agent":
                    return {
                        "agent_instance_id": f"agent-{payload['name']}",
                        "session_id": f"session-{payload['name']}",
                    }
                if kind == "invoke_action":
                    return {"output": identity, "turn_id": f"turn-{identity}"}
                raise AssertionError(f"unexpected effect: {kind}")

            _set_runtime(_Runtime(send))
            left = RouteResearcher("left")
            right = RouteResearcher("right")
            await together(left.investigate("left"), right.investigate("right"))
            return observed

        left_first = asyncio.run(run({"right": 0.01}))
        right_first = asyncio.run(run({"left": 0.01}))
        self.assertEqual(left_first, right_first)
        self.assertEqual(len(set(left_first.values())), 4)
        self.assertTrue(
            left_first[("invoke_action", "left")].startswith(
                "root/together:2/branch:0/"
            )
        )
        self.assertTrue(
            left_first[("invoke_action", "right")].startswith(
                "root/together:2/branch:1/"
            )
        )

    def test_typed_action_accepts_one_fenced_json_payload(self) -> None:
        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if kind == "invoke_action":
                return {"output": '```json\n{"answer": 42}\n```'}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> dict:
            _set_runtime(_Runtime(send))
            return await StructuredResearcher().research()

        self.assertEqual(asyncio.run(invoke()), {"answer": 42})

    def test_typed_action_extracts_object_after_provider_commentary(self) -> None:
        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if kind == "invoke_action":
                return {"output": 'Result follows:\n{"answer": 42}\nDone.'}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> dict:
            _set_runtime(_Runtime(send))
            return await StructuredResearcher().research()

        self.assertEqual(asyncio.run(invoke()), {"answer": 42})

    def test_typed_action_repairs_malformed_provider_json_in_same_session(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {"agent_instance_id": "agent", "session_id": "session"}
            if kind == "invoke_action" and payload["action_name"] == "research":
                return {"output": '{"answer": "unterminated}'}
            if (
                kind == "invoke_action"
                and payload["action_name"] == "research_json_repair"
            ):
                return {"output": '{"answer": 42}'}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> dict:
            _set_runtime(_Runtime(send))
            return await StructuredResearcher().research()

        self.assertEqual(asyncio.run(invoke()), {"answer": 42})
        repair = effects[-1][1]
        self.assertEqual(repair["agent_instance_id"], "agent")
        self.assertFalse(repair["tools_enabled"])

    def test_typed_action_stops_after_two_failed_repairs(self) -> None:
        calls = 0

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
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

    def test_action_policy_is_sent_with_the_action_effect(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {
                    "agent_instance_id": "agent",
                    "session_id": "session",
                    "access": payload["access"],
                }
            if kind == "invoke_action":
                return {"output": "done", "turn_id": "turn"}
            raise AssertionError(f"unexpected effect: {kind}")

        async def invoke() -> str:
            _set_runtime(_Runtime(send))
            return await Writer().compose([])

        self.assertEqual(asyncio.run(invoke()), "done")
        self.assertEqual(effects[-1][0], "invoke_action")
        self.assertTrue(effects[-1][1]["tools_enabled"])
        self.assertEqual(effects[-1][1]["web_search_context_size"], "low")
        self.assertEqual(effects[-1][1]["reasoning_effort"], "low")
        self.assertEqual(effects[0][1]["access"], "model_only")

    def test_constructor_override_and_dynamic_access_change_emit_profiles(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {
                    "agent_instance_id": "agent",
                    "session_id": "session",
                    "access": payload["access"],
                }
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
        self.assertEqual(
            [kind for kind, _ in effects], ["create_agent", "set_agent_access"]
        )
        self.assertEqual(effects[0][1]["access"], "workspace")
        self.assertEqual(effects[1][1]["access"], "research")

    def test_invalid_access_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "Agent access must be one of"):
            Writer(access="root")

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

    def test_finalize_policy_must_be_known(self) -> None:
        with self.assertRaisesRegex(ValueError, "finalize must be one of"):

            class Invalid(Agent):
                @action(finalize="sometimes")
                async def run(self) -> str:
                    """Invalid action."""


if __name__ == "__main__":
    unittest.main()
