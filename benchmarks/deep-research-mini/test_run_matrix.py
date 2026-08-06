import importlib.util
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("run_matrix.py")
SPEC = importlib.util.spec_from_file_location("run_matrix", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
run_matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(run_matrix)


class MatrixTests(unittest.TestCase):
    def test_workflow_control_metrics_separates_repair_and_followup_actions(self) -> None:
        actions = [
            {"id": "plan", "action_name": "plan", "status": "completed"},
            *[
                {
                    "id": f"initial-{index}",
                    "action_name": "research",
                    "status": "completed",
                    "arguments": {"phase": "initial"},
                }
                for index in range(4)
            ],
            *[
                {
                    "id": f"followup-{index}",
                    "action_name": "research",
                    "status": "completed",
                    "arguments": {"phase": "evaluator_follow_up"},
                }
                for index in range(2)
            ],
        ]
        view = {
            "workflow": {"output": {"plan": {"routes": [{}, {}, {}]}}},
            "actions": actions,
            "attempts": [
                {"invocation_id": "initial-0", "status": "failed"},
                {"invocation_id": "initial-0", "status": "completed"},
                *[
                    {"invocation_id": action["id"], "status": "completed"}
                    for action in actions[2:]
                ],
            ],
        }

        metrics = run_matrix.workflow_control_metrics(view)

        self.assertEqual(
            metrics["research_phase_counts"],
            {"initial": 4, "evaluator_follow_up": 2},
        )
        self.assertEqual(metrics["initial_route_count"], 3)
        self.assertEqual(metrics["initial_contract_repairs"], 1)
        self.assertEqual(metrics["action_attempt_retries"], 1)
        self.assertEqual(metrics["failed_action_attempts"], 1)

    def test_workflow_wall_time_survives_runtime_restart(self) -> None:
        run = {
            "created_at": "2026-08-07T00:00:00Z",
            "updated_at": "2026-08-07T00:05:00Z",
            "usage": {"wall_time_seconds": 10},
        }
        self.assertEqual(run_matrix.workflow_wall_time_seconds(run), 300)
        run["usage"]["wall_time_seconds"] = 400
        self.assertEqual(run_matrix.workflow_wall_time_seconds(run), 400)

    def test_ensure_project_repairs_missing_reused_root(self) -> None:
        class FakeApi:
            def __init__(self, root_path: str) -> None:
                self.root_path = root_path

            def get(self, path):
                self.assert_path(path)
                return [{"id": "existing-project", "root_path": self.root_path}]

            def post(self, path, payload):
                raise AssertionError(f"unexpected project creation: {path} {payload}")

            @staticmethod
            def assert_path(path):
                if path != "/projects":
                    raise AssertionError(f"unexpected path: {path}")

        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory) / "missing" / "research"
            api = FakeApi(str(root.resolve()))
            self.assertFalse(root.exists())
            self.assertEqual(
                run_matrix.ensure_project(api, "Research", "Benchmark", root),
                "existing-project",
            )
            self.assertTrue(root.is_dir())

    def test_launches_use_project_workflow_api_with_fresh_context(self) -> None:
        class FakeApi:
            def __init__(self) -> None:
                self.requests = []

            def post(self, path, payload):
                self.requests.append((path, payload))
                return {"id": f"run-{len(self.requests)}"}

        api = FakeApi()
        job = {"task_id": 1, "condition": "single_agent", "repeat": 1}
        rubric = {
            "criterions": {dimension: [] for dimension in run_matrix.DIMENSIONS}
        }
        task = {"prompt": "Research question", "rubric": rubric, "language": "en"}
        research = run_matrix.launch_research_run(
            api, "project-1", job, task, "deepseek-flash"
        )
        grade = run_matrix.launch_grader_run(
            api, "project-2", job, task, "Final report", "deepseek-flash"
        )

        self.assertEqual(research["workflow_id"], "run-1")
        self.assertEqual(grade["workflow_id"], "run-2")
        self.assertEqual(
            [path for path, _ in api.requests],
            [
                "/projects/project-1/workflows",
                "/projects/project-2/workflows",
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
                    "research_failed": True,
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
        self.assertNotIn("research_failed", state["jobs"][0])
        self.assertEqual(state["jobs"][1]["grade"]["status"], "pending_retry")

    def test_build_jobs_is_balanced_and_deterministic(self) -> None:
        conditions = ["single_agent", "evidence_r3"]
        first = run_matrix.build_jobs([1, 2, 3, 4], conditions, 2, 17)
        second = run_matrix.build_jobs([1, 2, 3, 4], conditions, 2, 17)
        self.assertEqual(first, second)
        self.assertEqual(len(first), 16)
        self.assertEqual(
            {
                condition: sum(job["condition"] == condition for job in first)
                for condition in conditions
            },
            {condition: 8 for condition in conditions},
        )

    def test_visible_criteria_hides_weights(self) -> None:
        rubric = {
            "criterions": {
                dimension: [
                    {
                        "criterion": "A",
                        "explanation": "B",
                        "weight": 1.0,
                        "comment": "hidden",
                    }
                ]
                for dimension in run_matrix.DIMENSIONS
            }
        }
        visible = run_matrix.visible_criteria(rubric)
        self.assertEqual(
            visible["comprehensiveness"],
            [{"criterion": "A", "explanation": "B"}],
        )

    def test_weighted_score_uses_both_weight_levels(self) -> None:
        rubric = {
            "dimension_weight": {
                "comprehensiveness": 0.4,
                "insight": 0.3,
                "instruction_following": 0.2,
                "readability": 0.1,
            },
            "criterions": {
                dimension: [
                    {"criterion": "A", "explanation": "", "weight": 0.25},
                    {"criterion": "B", "explanation": "", "weight": 0.75},
                ]
                for dimension in run_matrix.DIMENSIONS
            },
        }
        grading = {
            dimension: [
                {"criterion_index": 0, "score": 10, "analysis": ""},
                {"criterion_index": 1, "score": 6, "analysis": ""},
            ]
            for dimension in run_matrix.DIMENSIONS
        }
        score = run_matrix.validate_and_score_grading(grading, rubric)
        self.assertAlmostEqual(score["overall_score_100"], 70.0)
        self.assertEqual(
            score["dimension_scores_100"],
            {dimension: 70.0 for dimension in run_matrix.DIMENSIONS},
        )

    def test_weighted_score_rejects_missing_criterion(self) -> None:
        rubric = {
            "dimension_weight": {
                dimension: 0.25 for dimension in run_matrix.DIMENSIONS
            },
            "criterions": {
                dimension: [{"criterion": "A", "explanation": "", "weight": 1.0}]
                for dimension in run_matrix.DIMENSIONS
            },
        }
        grading = {
            dimension: (
                []
                if dimension == "insight"
                else [{"criterion_index": 0, "score": 5, "analysis": ""}]
            )
            for dimension in run_matrix.DIMENSIONS
        }
        with self.assertRaisesRegex(ValueError, "insight expected 1 ratings"):
            run_matrix.validate_and_score_grading(grading, rubric)

    def test_aggregate_ignores_incomplete_grades(self) -> None:
        complete = {
            "condition": "single_agent",
            "research": {
                "result": {
                    "wall_time_seconds": 10,
                    "report_characters": 1000,
                    "unique_direct_urls": 3,
                    "rounds": 2,
                    "completion": {"status": "warning"},
                    "workflow_control": {
                        "action_counts": {"research": 3, "assess": 2},
                        "research_phase_counts": {
                            "initial": 2,
                            "evaluator_follow_up": 1,
                        },
                        "initial_route_count": 2,
                        "initial_contract_repairs": 0,
                        "action_attempt_retries": 1,
                        "failed_action_attempts": 1,
                    },
                    "usage": run_matrix.token_usage(
                        {
                            "input_tokens": 100,
                            "output_tokens": 20,
                            "cached_input_tokens": 40,
                        }
                    ),
                }
            },
            "grade": {
                "score": {
                    "overall_score_100": 75,
                    "dimension_scores_100": {
                        dimension: 75 for dimension in run_matrix.DIMENSIONS
                    },
                },
                "usage": run_matrix.token_usage(
                    {"input_tokens": 30, "output_tokens": 5, "cached_input_tokens": 0}
                ),
            },
        }
        incomplete = {
            "condition": "single_agent",
            "research": complete["research"],
            "grade": {"attempts": []},
        }
        aggregate = run_matrix.aggregate_condition(
            [complete, incomplete], "single_agent"
        )
        self.assertEqual(aggregate["runs"], 1)
        self.assertEqual(aggregate["score_mean"], 75)
        self.assertEqual(aggregate["uncached_input_mean"], 60)
        self.assertEqual(aggregate["grader_input_mean"], 30)
        self.assertEqual(aggregate["completion_statuses"], {"warning": 1})
        self.assertEqual(aggregate["round_distribution"], {2: 1})
        self.assertEqual(aggregate["followup_research_actions"], 1)
        self.assertEqual(aggregate["action_attempt_retries"], 1)

    def test_operational_usage_includes_failed_attempt(self) -> None:
        job = {
            "research": {
                "result": {
                    "usage": run_matrix.token_usage(
                        {
                            "input_tokens": 100,
                            "output_tokens": 20,
                            "cached_input_tokens": 40,
                        }
                    ),
                    "wall_time_seconds": 10,
                },
                "attempts": [
                    {
                        "status": "failed",
                        "error": "stream_read_error",
                        "usage": run_matrix.token_usage(
                            {
                                "input_tokens": 25,
                                "output_tokens": 5,
                                "cached_input_tokens": 8,
                            }
                        ),
                        "wall_time_seconds": 3,
                    },
                    {"status": "completed"},
                ],
            }
        }
        usage = run_matrix.operational_usage(job, "research")
        self.assertEqual(usage["input_tokens"], 125)
        self.assertEqual(usage["cached_input_tokens"], 48)
        self.assertEqual(usage["uncached_input_tokens"], 77)
        self.assertEqual(run_matrix.operational_wall_time(job, "research"), 13)

    def test_budget_errors_are_not_retried(self) -> None:
        self.assertFalse(
            run_matrix.is_retryable_error(
                "Workflow token budget exceeded: used 515213 of 500000 tokens"
            )
        )
        self.assertTrue(
            run_matrix.is_retryable_error("model provider error: stream_read_error")
        )
        self.assertTrue(run_matrix.is_retryable_error("Broken pipe (os error 32)"))


if __name__ == "__main__":
    unittest.main()
