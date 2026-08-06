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
                self.assertEqual(payload["max_workflows"], 200)
                self.assertEqual(payload["max_text_chars"], 500_000)
                return {
                    "captured_at": "2026-08-06T12:00:00Z",
                    "project": {"name": "PaperMachine"},
                    "sessions": [{"title": "Research route"}],
                    "workflows": [],
                    "artifacts": [],
                }
            if kind == "invoke_action":
                self.assertEqual(payload["arguments"]["snapshot"]["project"]["name"], "PaperMachine")
                return {
                    "output": "<!doctype html><html><body><h1>Current progress</h1></body></html>"
                }
            if kind == "publish_artifact":
                self.assertEqual(payload["media_type"], "text/html; charset=utf-8")
                self.assertEqual(payload["metadata"]["role"], "project_summary")
                self.assertIn("<h1>Current progress</h1>", payload["content"])
                return {
                    "artifact_id": "artifact-summary",
                    "name": payload["name"],
                    "kind": payload["kind"],
                    "media_type": payload["media_type"],
                    "size_bytes": len(payload["content"]),
                }
            raise AssertionError(f"unexpected effect: {kind}")

        _set_runtime(_Runtime(send))
        output = asyncio.run(
            WORKFLOW["main"](
                WorkflowContext(
                    objective="Refresh progress.",
                    input={"interval_minutes": 0},
                    workflow_id="workflow-summary",
                )
            )
        )

        self.assertEqual(output, {"artifact_id": "artifact-summary", "refresh_count": 1})
        self.assertEqual(
            [kind for kind, _ in effects],
            ["project_snapshot", "create_agent", "invoke_action", "publish_artifact"],
        )

    def test_non_html_model_output_is_escaped_into_a_safe_document(self) -> None:
        html = WORKFLOW["_normalize_html"]("finding < uncertain & unresolved")
        self.assertIn("<!doctype html>", html)
        self.assertIn("finding &lt; uncertain &amp; unresolved", html)


if __name__ == "__main__":
    unittest.main()
