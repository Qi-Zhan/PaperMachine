from __future__ import annotations

import unittest

from papermachine._validate import validate


class WorkflowMetadataValidationTests(unittest.TestCase):
    def test_removed_input_schema_is_rejected_instead_of_silently_ignored(self) -> None:
        result = validate(
            '''
from papermachine import workflow

@workflow(
    slug="removed-field",
    name="Removed field",
    description="Old metadata must fail closed.",
    input_schema={"type": "object"},
)
async def main(ctx):
    return {}
'''
        )

        self.assertFalse(result["valid"])
        self.assertTrue(
            any(
                item["severity"] == "error"
                and "unknown workflow metadata: input_schema" in item["message"]
                for item in result["diagnostics"]
            )
        )

    def test_removed_budget_metadata_is_rejected(self) -> None:
        result = validate(
            '''
from papermachine import workflow

@workflow(
    slug="removed-budget",
    name="Removed budget",
    description="Removed execution quotas must fail closed.",
    params_schema={"type": "object"},
    output_schema={"type": "object"},
    budget={"max_action_steps": 8, "max_hosted_search_calls": 32},
)
async def main(ctx):
    return {}
'''
        )

        self.assertFalse(result["valid"])
        self.assertTrue(
            any(
                item["severity"] == "error"
                and "unknown workflow metadata: budget" in item["message"]
                for item in result["diagnostics"]
            )
        )

    def test_manifest_has_no_default_budget(self) -> None:
        result = validate(
            '''
from papermachine import workflow

@workflow(
    slug="plain-workflow",
    name="Plain workflow",
    description="Workflow metadata describes behavior, not quotas.",
    params_schema={"type": "object"},
    output_schema={"type": "object"},
)
async def main(ctx):
    return {}
'''
        )

        self.assertTrue(result["valid"])
        self.assertNotIn("default_budget", result["manifest"])


if __name__ == "__main__":
    unittest.main()
