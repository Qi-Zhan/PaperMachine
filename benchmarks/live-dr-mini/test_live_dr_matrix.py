import importlib.util
import json
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("run_matrix.py")
SPEC = importlib.util.spec_from_file_location("live_dr_run_matrix", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
run_matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(run_matrix)


class LiveDrMatrixTests(unittest.TestCase):
    def test_build_jobs_is_balanced_and_deterministic(self) -> None:
        first = run_matrix.build_jobs(
            ["0", "22"], ["single_agent", "coverage_r2"], 2, 7
        )
        second = run_matrix.build_jobs(
            ["0", "22"], ["single_agent", "coverage_r2"], 2, 7
        )
        self.assertEqual(first, second)
        self.assertEqual(len(first), 8)

    def test_json_candidates_handles_fenced_and_surrounding_text(self) -> None:
        values = run_matrix.json_candidates('Result:\n```json\n["A", "B"]\n```')
        self.assertIn(["A", "B"], values)

    def test_list_string_score_counts_precision_and_recall(self) -> None:
        score = run_matrix.score_list_strings(
            ["Maryam Mirzakhani", "Farbod Ekbatani"],
            ["Maryam Mirzakhani", "Unknown"],
        )
        self.assertEqual(score["matched_claims"], 1)
        self.assertEqual(score["precision"], 0.5)
        self.assertEqual(score["recall"], 0.5)

    def test_list_dict_score_matches_primary_key_then_fields(self) -> None:
        score = run_matrix.score_list_dicts(
            [
                {"framework": "Moral Foundations Theory", "dimension": "Fairness"},
                {"framework": "World Values Survey", "dimension": "Self-Expression"},
            ],
            [
                {"framework": "World Values Survey", "dimension": "Self Expression"},
                {"framework": "Moral Foundations Theory", "dimension": "Wrong"},
            ],
            ["framework"],
        )
        self.assertEqual(score["matched_claims"], 3)
        self.assertEqual(score["expected_claims"], 4)

    def test_field_aliases_cover_flight_output_schema(self) -> None:
        score = run_matrix.score_list_dicts(
            [{"time_(utc)": "10:00", "attempt_#": 1, "runway_#": "27"}],
            [{"time_utc": "10:00", "attempt_number": 1, "runway_number": "27"}],
            ["time_utc", "attempt_number"],
            ["time_utc", "attempt_number"],
        )
        self.assertEqual(score["f1"], 1.0)

    def test_wrong_main_claim_zeroes_identification_task(self) -> None:
        score = run_matrix.score_dict_fields(
            {"name": "BirdSet", "year": 2024},
            {"name": "OtherSet", "year": 2024},
            ["name"],
        )
        self.assertEqual(score["matched_claims"], 0)

    def test_scifacts_material_score_ignores_requested_evidence_fields(self) -> None:
        score = run_matrix.score_scifacts_materials(
            [{"paper_title": ["A paper"], "material": ["TiO2"]}],
            [
                {
                    "paper_title": "A paper",
                    "material": "TiO2",
                    "inference_basis": "evidence",
                    "property_source_table": "Table 1",
                    "property_source_passage": "passage",
                }
            ],
        )
        self.assertEqual(score["f1"], 1.0)

    def test_snapshot_ground_truths_remain_encrypted(self) -> None:
        snapshot = json.loads(Path(__file__).with_name("tasks.json").read_text())
        for task in snapshot["tasks"]:
            self.assertNotIn(task["question"], task["ground_truths"])
            ground_truths, _ = run_matrix.expected_ground_truth(task)
            self.assertIsInstance(ground_truths, list)

    def test_aggregate_keeps_semantic_and_strict_scores_separate(self) -> None:
        jobs = [
            {
                "condition": "single_agent",
                "research": {
                    "attempts": [],
                    "result": {
                        "score": {"f1": 0.0, "parse_error": None},
                        "usage": {
                            "input_tokens": 100,
                            "output_tokens": 10,
                            "cached_input_tokens": 60,
                        },
                        "model_transports": {},
                        "prompt_cache_modes": {},
                        "websocket_fallback_reasons": {},
                    },
                },
                "grade": {
                    "attempts": [],
                    "result": {
                        "precision": 1.0,
                        "recall": 1.0,
                        "f1": 1.0,
                        "usage": {
                            "input_tokens": 20,
                            "output_tokens": 5,
                            "cached_input_tokens": 0,
                        },
                    },
                },
            }
        ]
        aggregate = run_matrix.aggregate(jobs, "single_agent")
        self.assertEqual(aggregate["f1_mean"], 1.0)
        self.assertEqual(aggregate["deterministic_f1_mean"], 0.0)
        self.assertEqual(aggregate["grader_effective_mean"], 25)

    def test_report_flags_time_sensitive_references(self) -> None:
        state = {
            "experiment": {
                "conditions": ["single_agent"],
                "model": "research-profile",
                "grader_model": "grader-profile",
            },
            "jobs": [],
        }
        tasks = {
            "0": {
                "category": "entities",
                "question": "Who qualifies?",
                "reference_validity": {
                    "status": "time_sensitive",
                    "note": "The answer can change after the snapshot.",
                },
            }
        }

        report = run_matrix.render_report(state, tasks)

        self.assertIn("| 0 | entities | time_sensitive |", report)
        self.assertIn("## Reference validity warnings", report)
        self.assertIn("The answer can change after the snapshot.", report)


if __name__ == "__main__":
    unittest.main()
