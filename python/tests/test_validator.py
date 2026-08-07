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
    output_schema={"type": "object"},
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
        self.assertEqual(
            result["agents"],
            [
                {
                    "class_name": "Reviewer",
                    "actions": ["assess"],
                    "access": "model_only",
                }
            ],
        )
        self.assertEqual(result["features"]["human_checkpoints"], 1)

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
    output_schema={"type": "object"},
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
