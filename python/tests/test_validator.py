from __future__ import annotations

import unittest

from papermachine._validate import validate


class WorkflowValidationTests(unittest.TestCase):
    def test_extracts_current_manifest_agents_and_human_checkpoint(self) -> None:
        result = validate(
            '''
from papermachine import Agent, action, ask_human, workflow

class Reviewer(Agent):
    access = "model_only"

    @action
    async def assess(self, report: str) -> dict:
        """Assess the report."""

@workflow(
    slug="review-report",
    name="Review report",
    description="Review a report and request an explicit decision.",
    params_schema={"type": "object"},
)
async def main(ctx):
    reviewer = Reviewer(name="Reviewer")
    assessment = await reviewer.assess(ctx.request)
    decision = await ask_human("Accept the assessment?", agent=reviewer)
    return {"assessment": assessment, "decision": decision}
'''
        )

        self.assertTrue(result["valid"])
        self.assertEqual(result["manifest"]["slug"], "review-report")
        self.assertEqual(result["manifest"]["request_mode"], "required")
        self.assertEqual(
            result["agents"],
            [
                {
                    "class_name": "Reviewer",
                    "actions": [{"name": "assess", "tools": []}],
                    "access": "model_only",
                }
            ],
        )

    def test_declares_workflow_without_a_launch_user_task(self) -> None:
        result = validate(
            '''
from papermachine import ask_human, workflow

@workflow(
    slug="interactive-session",
    name="Interactive session",
    description="Wait for messages in a persistent session.",
    request_mode="none",
)
async def main(ctx):
    return {"message": await ask_human("Send a message")}
'''
        )

        self.assertTrue(result["valid"])
        self.assertEqual(result["manifest"]["request_mode"], "none")

    def test_extracts_literal_action_tools(self) -> None:
        result = validate(
            '''
from papermachine import Agent, action, workflow

class Curator(Agent):
    access = "model_only"

    @action(tools=["read_resource", "fetch_url"])
    async def maintain(self):
        """Maintain the page."""

@workflow(
    slug="maintain-page",
    name="Maintain page",
    description="Maintain one Project page.",
)
async def main(ctx):
    await Curator().maintain()
'''
        )

        self.assertTrue(result["valid"])
        self.assertEqual(
            result["agents"][0]["actions"],
            [
                {
                    "name": "maintain",
                    "tools": ["read_resource", "fetch_url"],
                }
            ],
        )

    def test_rejects_dynamic_duplicate_and_invalid_action_tools(self) -> None:
        template = '''
from papermachine import Agent, action, workflow

TOOLS = {tools}

class Worker(Agent):
    @action(tools={declaration})
    async def work(self):
        """Work."""

@workflow(slug="tool-check", name="Tool check", description="Check tools.")
async def main(ctx):
    await Worker().work()
'''
        cases = [
            ("['read_file']", "TOOLS", "literal list"),
            ("[]", "['read_file', 'read_file']", "duplicates"),
            ("[]", "['']", "non-empty"),
        ]
        for tools, declaration, expected in cases:
            with self.subTest(expected=expected):
                result = validate(template.format(tools=tools, declaration=declaration))
                self.assertFalse(result["valid"])
                self.assertTrue(
                    any(
                        expected in item["message"]
                        for item in result["diagnostics"]
                    )
                )

    def test_rejects_forbidden_imports(self) -> None:
        result = validate(
            '''
import os
from papermachine import workflow

@workflow(
    slug="unsafe-workflow",
    name="Unsafe workflow",
    description="Attempt forbidden host access.",
    params_schema={"type": "object"},
)
async def main(ctx):
    return {"cwd": os.getcwd()}
'''
        )

        self.assertFalse(result["valid"])
        messages = [item["message"] for item in result["diagnostics"]]
        self.assertTrue(any("arbitrary imports are disabled" in item for item in messages))
        self.assertTrue(any("access through `os` is disabled" in item for item in messages))


if __name__ == "__main__":
    unittest.main()
