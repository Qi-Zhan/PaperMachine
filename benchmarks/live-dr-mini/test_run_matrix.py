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
    def test_launches_use_project_workflow_api_with_fresh_context(self) -> None:
        class FakeApi:
            def __init__(self) -> None:
                self.requests = []

            def post(self, path, payload):
                self.requests.append((path, payload))
                return {"id": f"run-{len(self.requests)}"}

        task = json.loads(Path(__file__).with_name("tasks.json").read_text())["tasks"][0]
        api = FakeApi()
        job = {
            "task_key": str(task["key"]),
            "condition": "single_agent",
            "repeat": 1,
            "research": {"result": {"prediction": {}}},
        }
        research = run_matrix.launch_run(
            api, "project-1", job, task, "deepseek-flash"
        )
        grade = run_matrix.launch_grader_run(
            api, "project-1", job, task, "deepseek-flash"
        )

        self.assertEqual(research["workflow_id"], "run-1")
        self.assertEqual(grade["workflow_id"], "run-2")
        self.assertEqual(
            [path for path, _ in api.requests],
            [
                "/projects/project-1/workflows",
                "/projects/project-1/workflows",
            ],
        )
        for _, payload in api.requests:
            self.assertEqual(payload["context_mode"], "fresh")
            self.assertNotIn("started_from_session_id", payload)

    def test_reopen_terminal_failures_preserves_attempt_history(self) -> None:
        attempts = [{"workflow_id": "failed-run"}]
        state = {
            "jobs": [
                {
                    "failed": True,
                    "research": {"status": "failed", "attempts": attempts},
                },
                {
                    "grade_failed": True,
                    "research": {"result": {}},
                    "grade": {"status": "failed", "attempts": attempts},
                },
            ]
        }
        self.assertEqual(run_matrix.reopen_terminal_failures(state), 2)
        self.assertEqual(state["jobs"][0]["research"]["status"], "pending_retry")
        self.assertEqual(state["jobs"][0]["research"]["attempts"], attempts)
        self.assertNotIn("failed", state["jobs"][0])
        self.assertEqual(state["jobs"][1]["grade"]["status"], "pending_retry")

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

    def test_interrupt_cleanup_cancels_only_inflight_runs(self) -> None:
        class FakeApi:
            def __init__(self) -> None:
                self.paths = []

            def post_empty(self, path: str) -> None:
                self.paths.append(path)

        api = FakeApi()
        state = {
            "jobs": [
                {
                    "research": {
                        "status": "running",
                        "attempts": [{"workflow_id": "run-1"}],
                    }
                },
                {
                    "research": {
                        "status": "completed",
                        "attempts": [{"workflow_id": "run-2"}],
                    }
                },
            ]
        }
        self.assertEqual(run_matrix.cancel_inflight(api, state), 1)
        self.assertEqual(api.paths, ["/workflows/run-1/cancel"])

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
