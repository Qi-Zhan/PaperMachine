from __future__ import annotations

import runpy
import unittest
from pathlib import Path


WORKFLOW = runpy.run_path(
    Path(__file__).resolve().parent / "grader" / "workflow.py"
)
normalize_grading = WORKFLOW["_normalize_grading"]
grading_contract_errors = WORKFLOW["_grading_contract_errors"]
uses_alternate_shape = WORKFLOW["_uses_alternate_shape"]

CRITERIA = {
    "comprehensiveness": [{"criterion": "Coverage"}, {"criterion": "Depth"}],
    "insight": [{"criterion": "Synthesis"}],
}


def rating(index: int, score: float = 8) -> dict:
    return {
        "criterion_index": index,
        "score": score,
        "analysis": f"Evidence-based analysis for criterion {index}.",
    }


class ReportGraderContractTests(unittest.TestCase):
    def test_normalizes_flat_provider_evaluations(self) -> None:
        raw = {
            "evaluations": [
                {**rating(0), "dimension": "comprehensiveness"},
                {**rating(1), "dimension": "comprehensiveness"},
                {**rating(0), "dimension": "insight"},
            ],
            "overall_assessment": "Strong overall.",
            "major_weaknesses": ["Limited independent evidence."],
        }
        self.assertTrue(uses_alternate_shape(raw, CRITERIA))
        grading = normalize_grading(raw, CRITERIA)

        self.assertEqual(len(grading["comprehensiveness"]), 2)
        self.assertEqual(len(grading["insight"]), 1)
        self.assertEqual(grading_contract_errors(grading, CRITERIA), [])
        self.assertFalse(uses_alternate_shape(grading, CRITERIA))

    def test_assigns_indices_only_when_order_and_count_are_complete(self) -> None:
        grading = normalize_grading(
            {
                "comprehensiveness": [
                    {"score": 7, "analysis": "Coverage analysis."},
                    {"score": 8, "analysis": "Depth analysis."},
                ],
                "insight": [{"score": 6, "analysis": "Synthesis analysis."}],
                "overall_assessment": "Adequate.",
                "major_weaknesses": "Needs more synthesis.",
            },
            CRITERIA,
        )

        self.assertEqual(
            [item["criterion_index"] for item in grading["comprehensiveness"]],
            [0, 1],
        )
        self.assertEqual(grading["major_weaknesses"], ["Needs more synthesis."])
        self.assertEqual(grading_contract_errors(grading, CRITERIA), [])

    def test_rejects_incomplete_or_out_of_range_ratings(self) -> None:
        grading = {
            "comprehensiveness": [rating(0, 11)],
            "insight": [rating(0)],
            "overall_assessment": "Incomplete.",
            "major_weaknesses": [],
        }

        errors = grading_contract_errors(grading, CRITERIA)

        self.assertTrue(any("must contain 2 ratings" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
