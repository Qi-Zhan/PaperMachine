import importlib.util
import json
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("run_matrix.py")
SPEC = importlib.util.spec_from_file_location("browsecomp_run_matrix", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
run_matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(run_matrix)


class BrowseCompMatrixTests(unittest.TestCase):
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
        }
        research = run_matrix.launch_research_run(
            api, "project-1", job, task, "deepseek-flash"
        )
        grade = run_matrix.launch_grader_run(
            api, "project-2", job, task, "Candidate response", "deepseek-flash"
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

    def test_snapshot_is_pinned_and_encrypted(self) -> None:
        snapshot = json.loads(Path(__file__).with_name("tasks.json").read_text())
        self.assertEqual(len(snapshot["tasks"]), 6)
        self.assertEqual(
            snapshot["source_file_sha256"],
            "7b24471cd5b3eb2a46830a14802b5c029ea62f488ff75a0f88af7923d1454abf",
        )
        for task in snapshot["tasks"]:
            question, answer = run_matrix.task_content(task)
            self.assertTrue(question.strip())
            self.assertTrue(answer.strip())
            self.assertNotIn(question, task["problem"])
            self.assertNotIn(answer, task["answer"])

    def test_research_prompt_requires_exact_answer_and_confidence(self) -> None:
        prompt = run_matrix.research_prompt("Which item?")
        self.assertIn("Explanation:", prompt)
        self.assertIn("Exact Answer:", prompt)
        self.assertIn("Confidence:", prompt)

    def test_build_jobs_is_balanced_and_deterministic(self) -> None:
        first = run_matrix.build_jobs(
            ["1", "2"], ["single_agent", "coverage_r2"], 2, 17
        )
        second = run_matrix.build_jobs(
            ["1", "2"], ["single_agent", "coverage_r2"], 2, 17
        )
        self.assertEqual(first, second)
        self.assertEqual(len(first), 8)

    def test_grader_validation_rejects_non_boolean_correct(self) -> None:
        with self.assertRaisesRegex(ValueError, "boolean"):
            run_matrix.capture_grader_result(
                {
                    "workflow": {
                        "output": {"grading": {"correct": "yes"}},
                        "workflow": {"sha256": "test"},
                        "usage": {},
                    }
                },
                Path("unused.json"),
            )

    def test_interrupt_cleanup_cancels_both_phases(self) -> None:
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
                        "attempts": [{"workflow_id": "research-run"}],
                    },
                    "grade": {
                        "status": "created",
                        "attempts": [{"workflow_id": "grade-run"}],
                    },
                }
            ]
        }
        self.assertEqual(run_matrix.cancel_inflight(api, state), 2)
        self.assertEqual(
            api.paths,
            [
                "/workflows/research-run/cancel",
                "/workflows/grade-run/cancel",
            ],
        )


if __name__ == "__main__":
    unittest.main()
