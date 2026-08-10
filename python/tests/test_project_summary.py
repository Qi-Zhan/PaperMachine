from __future__ import annotations

import asyncio
import runpy
import unittest
from pathlib import Path
from typing import Any

from papermachine import SessionContext, _Runtime, _set_runtime


WORKFLOW = runpy.run_path(
    Path(__file__).resolve().parents[2]
    / "workflows"
    / "builtin"
    / "project-summary"
    / "workflow.py"
)


class ProjectSummaryWorkflowTests(unittest.TestCase):
    def test_one_shot_summary_reads_project_and_publishes_html(self) -> None:
        effects: list[tuple[str, dict[str, Any]]] = []

        async def send(_effect_id: str, kind: str, payload: dict[str, Any]) -> Any:
            effects.append((kind, payload))
            if kind == "create_agent":
                return {
                    "agent_id": "agent-summary",
                    "access": "model_only",
                }
            if kind == "project_changes":
                self.assertIsNone(payload["after_cursor"])
                return {
                    "cursor": 12,
                    "has_more": False,
                    "changed": True,
                    "resources": [{"kind": "project", "uri": "pm://project"}],
                }
            if kind == "invoke_action":
                self.assertEqual(
                    payload["arguments"]["changed_resources"],
                    [{"kind": "project", "uri": "pm://project"}],
                )
                self.assertEqual(payload["action_name"], "maintain_project_home")
                self.assertEqual(payload["requested_tools"], ["read_resource"])
                self.assertIsNone(payload["response_format"])
                return {
                    "action_invocation_id": "invocation-summary",
                    "output": "<h1>PaperMachine</h1>",
                }
            if kind == "publish_project_home":
                self.assertEqual(
                    payload["action_invocation_id"], "invocation-summary"
                )
                self.assertEqual(payload["metadata"]["project_cursor"], 12)
                self.assertEqual(payload["metadata"]["refresh_count"], 1)
                return {
                    "artifact_id": "artifact-summary",
                    "name": "project-home.html",
                    "kind": "report",
                    "media_type": "text/html; charset=utf-8",
                    "size_bytes": 128,
                }
            raise AssertionError(f"unexpected effect: {kind}")

        _set_runtime(_Runtime(send))
        output = asyncio.run(
            WORKFLOW["main"](
                SessionContext(
                    request="Refresh progress.",
                    instructions="",
                    params={"interval_minutes": 0},
                    session_id="session-summary",
                )
            )
        )

        self.assertEqual(output, {"artifact_id": "artifact-summary", "refresh_count": 1})
        self.assertEqual(
            [kind for kind, _ in effects],
            [
                "project_changes",
                "create_agent",
                "invoke_action",
                "publish_project_home",
            ],
        )

    def test_summary_uses_one_generic_project_reader(self) -> None:
        agent_type = WORKFLOW["ProjectSummaryAgent"]
        self.assertEqual(agent_type.maintain_project_home.tools, ["read_resource"])

if __name__ == "__main__":
    unittest.main()
