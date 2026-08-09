#!/usr/bin/env python3
"""Launch a benchmark-owned PaperMachine server for one resumable run."""

from __future__ import annotations

import contextlib
import hashlib
import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Iterator
from pathlib import Path
from typing import Any


def server_data_dir(run_dir: Path) -> Path:
    """Return the only application-data directory used by a benchmark run."""
    return run_dir.resolve() / "server-data"


def default_server_binary(repository_root: Path, *, windows: bool | None = None) -> Path:
    is_windows = os.name == "nt" if windows is None else windows
    executable = "papermachine-server.exe" if is_windows else "papermachine-server"
    return repository_root.resolve() / "target" / "debug" / executable


def runtime_artifact_fingerprints(
    config_path: Path, server_binary: Path | None
) -> dict[str, str]:
    paths = {"server-config": config_path.resolve()}
    if server_binary is not None:
        paths["server-binary"] = server_binary.resolve()
    missing = [f"{name}: {path}" for name, path in paths.items() if not path.is_file()]
    if missing:
        raise FileNotFoundError("benchmark runtime artifact is missing: " + ", ".join(missing))
    return {
        name: hashlib.sha256(path.read_bytes()).hexdigest()
        for name, path in paths.items()
    }


def install_project_workflow(api: Any, project_id: str, source_path: Path) -> Any:
    """Install one benchmark-owned Workflow through the public Project API."""
    return api.post(
        f"/projects/{project_id}/workflow-programs",
        {"source": source_path.read_text(encoding="utf-8")},
    )


def server_command(
    repository_root: Path,
    run_dir: Path,
    config_path: Path,
    server_binary: Path,
    port: int,
) -> list[str]:
    root = repository_root.resolve()
    return [
        str(server_binary.resolve()),
        "--resource-root",
        str(root),
        "--data-dir",
        str(server_data_dir(run_dir)),
        "--config",
        str(config_path.resolve()),
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
    server_binary: Path,
) -> Iterator[str]:
    """Run a compiled PaperMachine server with state contained by ``run_dir``."""
    root = repository_root.resolve()
    run_root = run_dir.resolve()
    run_root.mkdir(parents=True, exist_ok=True)
    data_dir = server_data_dir(run_root)
    data_dir.mkdir(parents=True, exist_ok=True)
    resolved_config = config_path.resolve()
    if not resolved_config.is_file():
        raise FileNotFoundError(f"PaperMachine config does not exist: {resolved_config}")
    resolved_server = server_binary.resolve()
    if not resolved_server.is_file():
        raise FileNotFoundError(
            f"PaperMachine server binary does not exist: {resolved_server}. "
            "Build papermachine-server first or pass --server-bin."
        )
    port = _available_port()
    base_url = f"http://127.0.0.1:{port}"
    log_path = run_root / "server.log"
    with log_path.open("ab") as log:
        process = subprocess.Popen(
            server_command(root, run_root, resolved_config, resolved_server, port),
            cwd=root,
            stdout=log,
            stderr=subprocess.STDOUT,
            env={
                **os.environ,
                "PAPERMACHINE_PYTHON": os.environ.get(
                    "PAPERMACHINE_PYTHON", sys.executable
                ),
            },
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
