from __future__ import annotations

import json
import os
import select
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


PYTHON_ROOT = Path(__file__).resolve().parents[1]


class RunnerProtocolTest(unittest.TestCase):
    def read_request(
        self, process: subprocess.Popen[str], timeout: int = 3
    ) -> dict[str, object]:
        assert process.stdout is not None
        ready, _, _ = select.select([process.stdout], [], [], timeout)
        self.assertTrue(ready, "runner did not emit its next protocol request")
        return json.loads(process.stdout.readline())

    def test_runner_exits_after_complete_ack_while_stdin_remains_open(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "workflow.py"
            workflow.write_text(
                "from papermachine import workflow\n"
                "@workflow(slug='test', name='Test', description='Runner test')\n"
                "async def main(ctx):\n"
                "    return {\n"
                "        'request': ctx.request,\n"
                "        'params': ctx.params,\n"
                "        'trigger': ctx.trigger,\n"
                "        'context': ctx.context,\n"
                "    }\n",
                encoding="utf-8",
            )
            environment = os.environ.copy()
            environment["PYTHONPATH"] = str(PYTHON_ROOT)
            process = subprocess.Popen(
                [
                    sys.executable,
                    "-B",
                    "-m",
                    "papermachine._runner",
                    str(workflow),
                    "main",
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
            )

            def cleanup() -> None:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                for stream in (process.stdin, process.stdout, process.stderr):
                    if stream is not None:
                        stream.close()

            self.addCleanup(cleanup)
            assert process.stdin is not None
            assert process.stdout is not None
            process.stdin.write(
                json.dumps(
                    {
                        "workflow_id": "test-workflow",
                        "request": "concrete user task",
                        "params": {"route_count": 2},
                        "trigger": {
                            "kind": "user",
                            "source_session_id": "origin-session",
                        },
                        "context": {"project": {"name": "Context project"}},
                    }
                )
                + "\n"
            )
            process.stdin.flush()

            request = self.read_request(process)
            self.assertEqual(request["id"], "root/effect:0/complete")
            self.assertEqual(request["kind"], "complete")
            self.assertEqual(
                request["payload"]["output"],
                {
                    "request": "concrete user task",
                    "params": {"route_count": 2},
                    "trigger": {
                        "kind": "user",
                        "source_session_id": "origin-session",
                    },
                    "context": {"project": {"name": "Context project"}},
                },
            )
            process.stdin.write(
                json.dumps(
                    {"id": request["id"], "ok": True, "result": None, "error": None}
                )
                + "\n"
            )
            process.stdin.flush()

            # Keeping stdin open reproduces the old deadlock: asyncio.to_thread()
            # left a blocking readline worker that asyncio.run() waited on forever.
            self.assertEqual(process.wait(timeout=3), 0, process.stderr.read())

    def test_runner_accepts_effect_response_larger_than_default_stream_limit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "workflow.py"
            workflow.write_text(
                "from papermachine import _effect, workflow\n"
                "@workflow(slug='test', name='Test', description='Runner test')\n"
                "async def main(ctx):\n"
                "    result = await _effect('project_snapshot', {})\n"
                "    return {'size': len(result['blob'])}\n",
                encoding="utf-8",
            )
            environment = os.environ.copy()
            environment["PYTHONPATH"] = str(PYTHON_ROOT)
            process = subprocess.Popen(
                [
                    sys.executable,
                    "-B",
                    "-m",
                    "papermachine._runner",
                    str(workflow),
                    "main",
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
            )

            def cleanup() -> None:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                for stream in (process.stdin, process.stdout, process.stderr):
                    if stream is not None:
                        stream.close()

            self.addCleanup(cleanup)
            assert process.stdin is not None
            assert process.stderr is not None
            process.stdin.write(
                json.dumps(
                    {
                        "workflow_id": "test-workflow",
                        "request": "test",
                        "params": {},
                        "trigger": {"kind": "manual"},
                    }
                )
                + "\n"
            )
            process.stdin.flush()

            request = self.read_request(process)
            self.assertEqual(request["kind"], "project_snapshot")
            process.stdin.write(
                json.dumps(
                    {
                        "id": request["id"],
                        "ok": True,
                        "result": {"blob": "x" * 100_000},
                        "error": None,
                    }
                )
                + "\n"
            )
            process.stdin.flush()

            complete = self.read_request(process)
            self.assertEqual(complete["kind"], "complete")
            self.assertEqual(complete["payload"]["output"], {"size": 100_000})
            process.stdin.write(
                json.dumps(
                    {"id": complete["id"], "ok": True, "result": None, "error": None}
                )
                + "\n"
            )
            process.stdin.flush()
            self.assertEqual(process.wait(timeout=3), 0, process.stderr.read())

    def test_runner_propagates_protocol_reader_failure_to_pending_effect(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "workflow.py"
            workflow.write_text(
                "from papermachine import _effect, workflow\n"
                "@workflow(slug='test', name='Test', description='Runner test')\n"
                "async def main(ctx):\n"
                "    await _effect('project_snapshot', {})\n"
                "    return {'ok': True}\n",
                encoding="utf-8",
            )
            environment = os.environ.copy()
            environment["PYTHONPATH"] = str(PYTHON_ROOT)
            process = subprocess.Popen(
                [
                    sys.executable,
                    "-B",
                    "-m",
                    "papermachine._runner",
                    str(workflow),
                    "main",
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
            )

            def cleanup() -> None:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                for stream in (process.stdin, process.stdout, process.stderr):
                    if stream is not None:
                        stream.close()

            self.addCleanup(cleanup)
            assert process.stdin is not None
            assert process.stderr is not None
            process.stdin.write(
                json.dumps(
                    {
                        "workflow_id": "test-workflow",
                        "request": "test",
                        "params": {},
                        "trigger": {"kind": "manual"},
                    }
                )
                + "\n"
            )
            process.stdin.flush()
            self.assertEqual(self.read_request(process)["kind"], "project_snapshot")

            process.stdin.write("not-json\n")
            process.stdin.flush()
            self.assertNotEqual(process.wait(timeout=3), 0)
            self.assertIn(
                "Rust workflow protocol reader failed while waiting for",
                process.stderr.read(),
            )


if __name__ == "__main__":
    unittest.main()
