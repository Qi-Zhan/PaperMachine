from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


PYTHON_ROOT = Path(__file__).resolve().parents[1]


class RunnerProtocolTest(unittest.TestCase):
    def test_runner_exits_after_complete_ack_while_stdin_remains_open(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workflow = Path(directory) / "workflow.py"
            workflow.write_text(
                "from papermachine import workflow\n"
                "@workflow(slug='test', name='Test', description='Runner test')\n"
                "async def main(ctx):\n"
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
            assert process.stdout is not None
            process.stdin.write(
                json.dumps(
                    {"workflow_id": "test-workflow", "objective": "test", "input": {}}
                )
                + "\n"
            )
            process.stdin.flush()

            request = json.loads(process.stdout.readline())
            self.assertEqual(request["id"], "root/effect:0/complete")
            self.assertEqual(request["kind"], "complete")
            self.assertEqual(request["payload"]["output"], {"ok": True})
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


if __name__ == "__main__":
    unittest.main()
