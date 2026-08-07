#!/usr/bin/env python3
"""Launch a benchmark-owned PaperMachine server for one resumable run."""

from __future__ import annotations

import contextlib
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.request
from collections.abc import Iterator
from pathlib import Path


def cargo_executable() -> str:
    discovered = shutil.which("cargo")
    rustup_proxy = Path.home() / ".cargo" / "bin" / "cargo"
    if discovered:
        return discovered
    if rustup_proxy.is_file():
        return str(rustup_proxy)
    raise FileNotFoundError("cargo is required to run PaperMachine benchmarks")


def server_data_dir(run_dir: Path) -> Path:
    """Return the only application-data directory used by a benchmark run."""
    return run_dir.resolve() / "server-data"


def server_command(
    repository_root: Path,
    run_dir: Path,
    config_path: Path,
    port: int,
) -> list[str]:
    root = repository_root.resolve()
    return [
        str(root / "target" / "debug" / "papermachine-server"),
        "--resource-root",
        str(root),
        "--data-dir",
        str(server_data_dir(run_dir)),
        "--config",
        str(config_path.resolve()),
        "--host",
        "127.0.0.1",
        "--port",
        str(port),
    ]


def _available_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def _wait_until_ready(
    process: subprocess.Popen[bytes], base_url: str, log_path: Path
) -> None:
    deadline = time.monotonic() + 30
    health_url = f"{base_url}/api/health"
    while time.monotonic() < deadline:
        exit_code = process.poll()
        if exit_code is not None:
            raise RuntimeError(
                f"isolated PaperMachine server exited with {exit_code}; see {log_path}"
            )
        try:
            with urllib.request.urlopen(health_url, timeout=1):
                return
        except (urllib.error.URLError, TimeoutError):
            time.sleep(0.1)
    raise RuntimeError(f"isolated PaperMachine server did not start; see {log_path}")


@contextlib.contextmanager
def isolated_server(
    repository_root: Path,
    run_dir: Path,
    config_path: Path,
) -> Iterator[str]:
    """Build and run PaperMachine with state contained by ``run_dir``."""
    root = repository_root.resolve()
    run_root = run_dir.resolve()
    run_root.mkdir(parents=True, exist_ok=True)
    data_dir = server_data_dir(run_root)
    data_dir.mkdir(parents=True, exist_ok=True)
    resolved_config = config_path.resolve()
    if not resolved_config.is_file():
        raise FileNotFoundError(f"PaperMachine config does not exist: {resolved_config}")

    subprocess.run(
        [cargo_executable(), "build", "-p", "papermachine-server"],
        cwd=root,
        check=True,
    )
    port = _available_port()
    base_url = f"http://127.0.0.1:{port}"
    log_path = run_root / "server.log"
    with log_path.open("ab") as log:
        process = subprocess.Popen(
            server_command(root, run_root, resolved_config, port),
            cwd=root,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        try:
            _wait_until_ready(process, base_url, log_path)
            yield base_url
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
