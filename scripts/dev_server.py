#!/usr/bin/env python3
"""Run the local server without mixing development and normal user data."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path


def cargo_executable() -> str:
    discovered = shutil.which("cargo")
    rustup_proxy = Path.home() / ".cargo" / "bin" / "cargo"
    if discovered:
        return discovered
    if rustup_proxy.is_file():
        return str(rustup_proxy)
    raise FileNotFoundError("cargo is required to run the PaperMachine server")


def development_data_dir(
    platform: str = sys.platform,
    environment: dict[str, str] | None = None,
    home: Path | None = None,
) -> Path:
    env = os.environ if environment is None else environment
    user_home = Path.home() if home is None else home
    if platform == "darwin":
        return user_home / "Library" / "Application Support" / "PaperMachine" / "dev"
    if platform == "win32":
        local_app_data = env.get("LOCALAPPDATA")
        if not local_app_data:
            raise RuntimeError("LOCALAPPDATA is required on Windows")
        return Path(local_app_data) / "PaperMachine" / "dev"
    xdg_data_home = env.get("XDG_DATA_HOME")
    base = Path(xdg_data_home) if xdg_data_home else user_home / ".local" / "share"
    return base / "papermachine" / "dev"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--demo", action="store_true")
    parser.add_argument("--config", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=4310)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repository_root = Path(__file__).resolve().parent.parent
    data_dir = development_data_dir()
    command = [
        cargo_executable(),
        "run",
        "-p",
        "papermachine-server",
        "--",
        "--resource-root",
        str(repository_root),
        "--data-dir",
        str(data_dir),
        "--host",
        args.host,
        "--port",
        str(args.port),
    ]
    if args.demo:
        command.append("--demo")
    else:
        command.extend(
            ["--config", str((args.config or repository_root / "papermachine.toml").resolve())]
        )
    print(f"development data: {data_dir}", flush=True)
    return subprocess.run(command, cwd=repository_root, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
