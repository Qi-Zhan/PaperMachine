import importlib.util
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("run_matrix.py")
SPEC = importlib.util.spec_from_file_location("run_matrix", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
run_matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(run_matrix)


class MatrixTests(unittest.TestCase):
    def test_workflow_control_metrics_separates_repair_and_followup_actions(
        self,
    ) -> None:
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

    def test_launches_use_project_workflow_api_with_fresh_context(self) -> None:
        class FakeApi:
            def __init__(self) -> None:
                self.requests = []

            def post(self, path, payload):
                self.requests.append((path, payload))
                return {"id": f"run-{len(self.requests)}"}

        api = FakeApi()
        job = {"task_id": 1, "condition": "single_agent", "repeat": 1}
        rubric = {"criterions": {dimension: [] for dimension in run_matrix.DIMENSIONS}}
        task = {"prompt": "Research question", "rubric": rubric, "language": "en"}
        research = run_matrix.launch_research_run(
            api,
            "project-1",
            job,
            task,
            "deepseek-flash",
            {role: "" for role in run_matrix.AGENT_MODEL_ROLES},
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

    def test_evidence_loop_can_route_each_agent_role_to_a_model(self) -> None:
        class FakeApi:
            def post(self, _path, payload):
                self.payload = payload
                return {"id": "mixed-model-run"}

        api = FakeApi()
        run_matrix.launch_research_run(
            api,
            "project-1",
            {"task_id": 68, "condition": "evidence_r2", "repeat": 1},
            {"prompt": "Research question"},
            "glm-5-2",
            {
                "planner": "glm-5-2",
                "research": "deepseek-flash",
                "evaluator": "glm-5-2",
                "writer": "glm-5-2",
            },
        )

        self.assertEqual(api.payload["model"], "glm-5-2")
        self.assertEqual(api.payload["params"]["max_rounds"], 2)
        self.assertEqual(api.payload["params"]["research_model"], "deepseek-flash")
        self.assertEqual(api.payload["params"]["writer_model"], "glm-5-2")

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

    def test_research_runtime_errors_are_retryable(self) -> None:
        self.assertTrue(
            run_matrix.is_retryable_error("model provider error: stream_read_error")
        )
        self.assertTrue(run_matrix.is_retryable_error("Broken pipe (os error 32)"))


if __name__ == "__main__":
    unittest.main()
