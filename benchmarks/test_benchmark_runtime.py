import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from benchmark_runtime import (
    cancel_inflight,
    default_server_binary,
    drive_phase,
    ensure_project,
    launch_workflow,
    reopen_terminal_failures,
    runtime_artifact_fingerprints,
    server_command,
    server_data_dir,
    workflow_wall_time_seconds,
)


class BenchmarkRuntimeTests(unittest.TestCase):
    def test_launch_workflow_owns_the_shared_api_contract(self) -> None:
        class FakeApi:
            def __init__(self) -> None:
                self.requests = []

            def post(self, path, payload):
                self.requests.append((path, payload))
                return {"id": f"run-{len(self.requests)}"}

        api = FakeApi()
        result = launch_workflow(
            api,
            "project-1",
            program_slug="program",
            request="request",
            params={"value": 1},
            model="model",
            access="research",
        )
        self.assertEqual(result["workflow_id"], "run-1")
        self.assertIn("launched_at", result)
        self.assertEqual(
            api.requests,
            [
                (
                    "/projects/project-1/workflows",
                    {
                        "program_slug": "program",
                        "request": "request",
                        "params": {"value": 1},
                        "model": "model",
                        "access": "research",
                        "enabled_skills": [],
                        "context_mode": "fresh",
                    },
                )
            ],
        )

    def test_workflow_wall_time_includes_process_downtime(self) -> None:
        run = {
            "created_at": "2026-08-07T00:00:00Z",
            "updated_at": "2026-08-07T00:05:00Z",
            "usage": {"wall_time_seconds": 10},
        }
        self.assertEqual(workflow_wall_time_seconds(run), 300)
        run["usage"]["wall_time_seconds"] = 400
        self.assertEqual(workflow_wall_time_seconds(run), 400)

    def test_project_attachment_uses_the_exact_current_api(self) -> None:
        class FakeApi:
            def __init__(self) -> None:
                self.projects = []
                self.payload = None

            def get(self, path):
                self.assert_path(path)
                return self.projects

            def post(self, path, payload):
                self.assert_path(path)
                self.payload = payload
                return {"id": "created-project"}

            @staticmethod
            def assert_path(path):
                if path != "/projects":
                    raise AssertionError(f"unexpected path: {path}")

        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory) / "research"
            api = FakeApi()
            self.assertEqual(ensure_project(api, "Research", root), "created-project")
            self.assertEqual(
                api.payload,
                {"name": "Research", "workspace": {"path": str(root.resolve())}},
            )
            api.projects = [
                {
                    "id": "existing-project",
                    "workspace": {"path": str(root.resolve())},
                }
            ]
            self.assertEqual(ensure_project(api, "Research", root), "existing-project")

    def test_retry_and_interrupt_state_preserve_attempt_history(self) -> None:
        attempts = [{"workflow_id": "research-run"}]
        state = {
            "jobs": [
                {
                    "research_failed": True,
                    "research": {"status": "failed", "attempts": attempts},
                },
                {
                    "grade_failed": True,
                    "research": {"result": {}},
                    "grade": {
                        "status": "running",
                        "attempts": [{"workflow_id": "grade-run"}],
                    },
                },
            ]
        }
        self.assertEqual(reopen_terminal_failures(state), 2)
        self.assertEqual(state["jobs"][0]["research"]["attempts"], attempts)
        self.assertEqual(state["jobs"][0]["research"]["status"], "pending_retry")
        self.assertEqual(state["jobs"][1]["grade"]["status"], "pending_retry")

        class FakeApi:
            def __init__(self) -> None:
                self.paths = []

            def post_empty(self, path):
                self.paths.append(path)

        state["jobs"][0]["research"]["status"] = "running"
        state["jobs"][1]["grade"]["status"] = "created"
        api = FakeApi()
        self.assertEqual(cancel_inflight(api, state), 2)
        self.assertEqual(
            api.paths,
            ["/workflows/research-run/cancel", "/workflows/grade-run/cancel"],
        )

    def test_phase_driver_retries_then_commits_one_result(self) -> None:
        class FakeApi:
            def get(self, path):
                workflow_id = path.rsplit("/", 1)[-1]
                status = "failed" if workflow_id == "run-1" else "completed"
                return {
                    "workflow": {
                        "status": status,
                        "error": "transient" if status == "failed" else None,
                        "usage": {"tokens": {"input_tokens": 5}},
                        "created_at": "2026-08-09T00:00:00Z",
                        "updated_at": "2026-08-09T00:00:01Z",
                    }
                }

        launches = []

        def launch(_job, _task):
            workflow_id = f"run-{len(launches) + 1}"
            launches.append(workflow_id)
            return {"workflow_id": workflow_id}

        state = {
            "jobs": [{"key": "job", "task_key": "task"}],
            "current_runtime_source_sha256": {"runner": "hash"},
        }
        with tempfile.TemporaryDirectory() as temporary_directory:
            with patch("builtins.print"):
                drive_phase(
                    FakeApi(),
                    state,
                    Path(temporary_directory) / "state.json",
                    {"task": {}},
                    "research",
                    launch,
                    lambda _job, _view: {"answer": "done"},
                    lambda _error: True,
                    0,
                    2,
                    1,
                )

        self.assertEqual(launches, ["run-1", "run-2"])
        self.assertEqual(state["jobs"][0]["research"]["result"], {"answer": "done"})
        self.assertEqual(
            state["jobs"][0]["research"]["attempts"][0]["usage"]["input_tokens"],
            5,
        )

    def test_runtime_fingerprints_use_the_selected_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory)
            config = root / "selected.toml"
            binary = root / "server"
            config.write_text("model = 'selected'\n", encoding="utf-8")
            binary.write_bytes(b"selected-server")

            fingerprints = runtime_artifact_fingerprints(config, binary)

            self.assertEqual(set(fingerprints), {"server-config", "server-binary"})
            self.assertNotEqual(
                fingerprints["server-config"], fingerprints["server-binary"]
            )

    def test_server_state_is_owned_by_the_run_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory) / "repository"
            run_dir = root / "benchmarks" / "sample" / "runs" / "run-1"
            config = root / "papermachine.toml"
            server_binary = default_server_binary(root, windows=False)
            command = server_command(root, run_dir, config, server_binary, 9876)

            self.assertEqual(
                server_data_dir(run_dir), run_dir.resolve() / "server-data"
            )
            self.assertEqual(
                command[command.index("--data-dir") + 1],
                str(run_dir.resolve() / "server-data"),
            )
            self.assertEqual(
                command[command.index("--resource-root") + 1], str(root.resolve())
            )
            self.assertEqual(command[command.index("--port") + 1], "9876")
            self.assertEqual(command[0], str(server_binary))

    def test_default_binary_uses_the_platform_executable_name(self) -> None:
        root = Path("/repository")
        self.assertEqual(
            default_server_binary(root, windows=False).name, "papermachine-server"
        )
        self.assertEqual(
            default_server_binary(root, windows=True).name, "papermachine-server.exe"
        )


if __name__ == "__main__":
    unittest.main()
