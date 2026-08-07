import tempfile
import unittest
from pathlib import Path

from benchmark_runtime import (
    default_server_binary,
    runtime_artifact_fingerprints,
    server_command,
    server_data_dir,
)


class BenchmarkRuntimeTests(unittest.TestCase):
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

            self.assertEqual(server_data_dir(run_dir), run_dir.resolve() / "server-data")
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
