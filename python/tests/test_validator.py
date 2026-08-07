from __future__ import annotations

import unittest

from papermachine._validate import validate


class WorkflowBudgetValidationTests(unittest.TestCase):
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

    def test_warns_when_search_allowance_cannot_fit_in_step_budget(self) -> None:
        result = validate(
            '''
from papermachine import workflow

@workflow(
    slug="budget-warning",
    name="Budget warning",
    description="Exercise the budget diagnostic.",
    params_schema={"type": "object"},
    output_schema={"type": "object"},
    budget={"max_action_steps": 8, "max_hosted_search_calls": 32},
)
async def main(ctx):
    return {}
'''
        )

        self.assertTrue(result["valid"])
        self.assertTrue(
            any(
                item["severity"] == "warning"
                and "cannot accommodate max_hosted_search_calls" in item["message"]
                for item in result["diagnostics"]
            )
        )

    def test_sized_budget_has_no_search_step_warning(self) -> None:
        result = validate(
            '''
from papermachine import workflow

@workflow(
    slug="sized-budget",
    name="Sized budget",
    description="Exercise a sufficient budget.",
    params_schema={"type": "object"},
    output_schema={"type": "object"},
    budget={"max_action_steps": 128, "max_hosted_search_calls": 32},
)
async def main(ctx):
    return {}
'''
        )

        self.assertTrue(result["valid"])
        self.assertFalse(
            any("max_hosted_search_calls" in item["message"] for item in result["diagnostics"])
        )


if __name__ == "__main__":
    unittest.main()
