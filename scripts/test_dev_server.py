import tempfile
import unittest
from pathlib import Path

from dev_server import development_data_dir


class DevelopmentDataDirectoryTests(unittest.TestCase):
    def test_platform_paths_have_a_dedicated_dev_namespace(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            home = Path(temporary_directory)
            self.assertEqual(
                development_data_dir("darwin", {}, home),
                home / "Library" / "Application Support" / "PaperMachine" / "dev",
            )
            self.assertEqual(
                development_data_dir("linux", {}, home),
                home / ".local" / "share" / "papermachine" / "dev",
            )
            self.assertEqual(
                development_data_dir(
                    "win32", {"LOCALAPPDATA": str(home / "AppData")}, home
                ),
                home / "AppData" / "PaperMachine" / "dev",
            )


if __name__ == "__main__":
    unittest.main()
