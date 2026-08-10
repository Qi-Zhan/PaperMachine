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
                    "access": "workspace",
                }
            if kind == "project_changes":
                self.assertTrue(payload["exclude_current_program"])
                cursor = payload["after_cursor"]
                self.assertIn(cursor, (None, "pc1_snapshot", "pc2_snapshot"))
                page = {None: 1, "pc1_snapshot": 2, "pc2_snapshot": 3}[cursor]
                if page == 3:
                    return {
                        "cursor": "pc3_snapshot",
                        "has_more": False,
                        "changed": False,
                        "resources": [],
                    }
                return {
                    "cursor": f"pc{page}_snapshot",
                    "has_more": True,
                    "changed": True,
                    "resources": [{
                        "kind": "project" if page == 1 else "session",
                        "id": f"resource-{page}",
                        "session_id": None if page == 1 else "session-2",
                        "deleted": False,
                        "data": {"page": page},
                    }],
                }
            if kind == "invoke_action":
                page = payload["arguments"]["changed_resources"][0]["data"]["page"]
                self.assertEqual(
                    payload["arguments"]["changed_resources"],
                    [{
                        "kind": "project" if page == 1 else "session",
                        "id": f"resource-{page}",
                        "session_id": None if page == 1 else "session-2",
                        "deleted": False,
                        "data": {"page": page},
                    }],
                )
                self.assertEqual(payload["action_name"], "maintain_project_home")
                self.assertIsNone(payload["tool_policy"])
                self.assertEqual(payload["web_search_context_size"], "low")
                self.assertIsNone(payload["response_format"])
                return {
                    "action_invocation_id": f"invocation-summary-{page}",
                    "output": f"<h1>PaperMachine page {page}</h1>",
                }
            if kind == "publish_project_home":
                page = payload["metadata"]["refresh_count"]
                self.assertEqual(
                    payload["action_invocation_id"], f"invocation-summary-{page}"
                )
                self.assertEqual(
                    payload["metadata"]["project_cursor"], f"pc{page}_snapshot"
                )
                return {
                    "artifact_id": f"artifact-summary-{page}",
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

        self.assertEqual(output, {"artifact_id": "artifact-summary-2", "refresh_count": 2})
        self.assertEqual(
            [kind for kind, _ in effects],
            [
                "project_changes",
                "create_agent",
                "invoke_action",
                "publish_project_home",
                "project_changes",
                "invoke_action",
                "publish_project_home",
                "project_changes",
            ],
        )

    def test_summary_uses_normal_agent_defaults(self) -> None:
        agent_type = WORKFLOW["ProjectSummaryAgent"]
        self.assertEqual(agent_type.access, "workspace")
        self.assertIsNone(agent_type.maintain_project_home.tools)

if __name__ == "__main__":
    unittest.main()
