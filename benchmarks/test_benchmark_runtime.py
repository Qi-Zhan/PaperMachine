import tempfile
import unittest
from pathlib import Path

from benchmark_runtime import server_command, server_data_dir


class BenchmarkRuntimeTests(unittest.TestCase):
    def test_server_state_is_owned_by_the_run_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            root = Path(temporary_directory) / "repository"
            run_dir = root / "benchmarks" / "sample" / "runs" / "run-1"
            config = root / "papermachine.toml"
            command = server_command(root, run_dir, config, 9876)

            self.assertEqual(server_data_dir(run_dir), run_dir.resolve() / "server-data")
            self.assertEqual(
                command[command.index("--data-dir") + 1],
                str(run_dir.resolve() / "server-data"),
            )
            self.assertEqual(
                command[command.index("--resource-root") + 1], str(root.resolve())
            )
            self.assertEqual(command[command.index("--port") + 1], "9876")


if __name__ == "__main__":
    unittest.main()
