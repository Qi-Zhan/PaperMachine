from __future__ import annotations

import runpy
import unittest
from pathlib import Path


WORKFLOW = runpy.run_path(
    Path(__file__).resolve().parents[2]
    / "workflows"
    / "builtin"
    / "live-dr-grader"
    / "workflow.py"
)


class LiveDRGraderTest(unittest.TestCase):
    def test_short_dataset_name_can_receive_full_semantic_credit(self) -> None:
        grading = WORKFLOW["_grade"](
            "novel-datasets-identification",
            {
                "name": "BirdSet: A Large-Scale Dataset for Audio Classification",
                "year": 2025,
            },
            {"name": "BirdSet", "year": 2025},
            {"main_claims": ["name"]},
            {
                "evaluations": [
                    {
                        "prediction_index": 0,
                        "ground_truth_index": 0,
                        "field_scores": {"name": 3, "year": 3},
                    }
                ]
            },
        )
        self.assertEqual(grading["f1"], 1.0)

    def test_wrong_main_claim_gates_other_dictionary_fields(self) -> None:
        grading = WORKFLOW["_grade"](
            "novel-datasets-identification",
            {"name": "Dataset A", "year": 2025},
            {"name": "Dataset B", "year": 2025},
            {"main_claims": ["name"]},
            {
                "evaluations": [
                    {
                        "prediction_index": 0,
                        "ground_truth_index": 0,
                        "field_scores": {"name": 1, "year": 3},
                    }
                ]
            },
        )
        self.assertEqual(grading["matched_claims"], 0)
        self.assertEqual(grading["f1"], 0.0)

    def test_entity_matches_are_one_to_one(self) -> None:
        grading = WORKFLOW["_grade"](
            "entities",
            ["Ada Lovelace"],
            ["Ada", "A. Lovelace"],
            {},
            {
                "evaluations": [
                    {"prediction_index": 0, "ground_truth_index": 0},
                    {"prediction_index": 1, "ground_truth_index": 0},
                ]
            },
        )
        self.assertEqual(grading["matched_claims"], 1)
        self.assertEqual(grading["precision"], 0.5)
        self.assertEqual(grading["recall"], 1.0)


if __name__ == "__main__":
    unittest.main()
