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
                    "agent_instance_id": "agent-summary",
                    "session_id": "session-summary",
                    "access": "model_only",
                }
            if kind == "project_snapshot":
                self.assertTrue(payload["include_artifact_content"])
                self.assertIsNone(payload["after_cursor"])
                self.assertEqual(payload["max_workflows"], 200)
                self.assertEqual(payload["max_text_chars"], 500_000)
                return {
                    "cursor": 12,
                    "has_more": False,
                    "changed": True,
                    "mode": "full",
                    "after_cursor": None,
                    "project": {"name": "PaperMachine"},
                    "sessions": [{"title": "Research route"}],
                    "workflows": [],
                    "artifacts": [],
                }
            if kind == "invoke_action":
                self.assertEqual(
                    payload["arguments"]["project_changes"]["project"]["name"],
                    "PaperMachine",
                )
                self.assertEqual(payload["action_name"], "maintain_project_home")
                self.assertEqual(
                    payload["requested_tools"],
                    [
                        "read_project_home",
                        "patch_project_home",
                        "preview_project_home",
                    ],
                )
                self.assertIsNone(payload["response_format"])
                return {"output": "The Project home page is current."}
            if kind == "publish_project_home":
                self.assertEqual(payload["agent_instance_id"], "agent-summary")
                self.assertEqual(payload["metadata"]["snapshot_mode"], "full")
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
                WorkflowContext(
                    request="Refresh progress.",
                    instructions="",
                    params={"interval_minutes": 0},
                    workflow_id="workflow-summary",
                )
            )
        )

        self.assertEqual(output, {"artifact_id": "artifact-summary", "refresh_count": 1})
        self.assertEqual(
            [kind for kind, _ in effects],
            [
                "project_snapshot",
                "create_agent",
                "invoke_action",
                "publish_project_home",
            ],
        )

    def test_summary_prompt_delegates_iteration_to_one_tool_capable_action(self) -> None:
        agent_type = WORKFLOW["ProjectSummaryAgent"]
        self.assertEqual(
            agent_type.maintain_project_home.tools,
            [
                "read_project_home",
                "patch_project_home",
                "preview_project_home",
            ],
        )
        prompt = agent_type.system_prompt
        self.assertIn("as many editing and inspection passes as needed", prompt)
        self.assertNotIn("complete or continue", prompt.lower())

if __name__ == "__main__":
    unittest.main()
