#!/usr/bin/env python3
"""Launch a benchmark-owned PaperMachine server for one resumable run."""

from __future__ import annotations

import contextlib
import base64
import hashlib
import json
import math
import os
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections import Counter
from collections.abc import Iterator
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable


class ApiError(RuntimeError):
    pass


class PaperMachineApi:
    def __init__(self, base_url: str) -> None:
        self.base_url = base_url.rstrip("/") + "/api"

    def request(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None = None,
    ) -> Any:
        body = None if payload is None else json.dumps(payload).encode("utf-8")
        request = urllib.request.Request(
            self.base_url + path,
            data=body,
            method=method,
            headers={"content-type": "application/json"} if body is not None else {},
        )
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                data = response.read()
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", errors="replace")
            raise ApiError(
                f"{method} {path} returned HTTP {error.code}: {detail}"
            ) from error
        except urllib.error.URLError as error:
            raise ApiError(f"{method} {path} failed: {error}") from error
        return None if not data else json.loads(data)

    def get(self, path: str) -> Any:
        return self.request("GET", path)

    def post(self, path: str, payload: dict[str, Any]) -> Any:
        return self.request("POST", path, payload)

    def post_empty(self, path: str) -> Any:
        return self.request("POST", path)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def workflow_wall_time_seconds(run: dict[str, Any]) -> int:
    usage = run.get("usage") or {}
    runtime_seconds = max(0, int(usage.get("wall_time_seconds", 0)))
    try:
        created_at = datetime.fromisoformat(
            str(run["created_at"]).replace("Z", "+00:00")
        )
        updated_at = datetime.fromisoformat(
            str(run["updated_at"]).replace("Z", "+00:00")
        )
        observed_seconds = max(0, math.ceil((updated_at - created_at).total_seconds()))
    except (KeyError, TypeError, ValueError):
        observed_seconds = 0
    return max(runtime_seconds, observed_seconds)


def atomic_write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        "w", encoding="utf-8", dir=path.parent, delete=False
    ) as handle:
        handle.write(content)
        handle.flush()
        os.fsync(handle.fileno())
        temporary = Path(handle.name)
    os.replace(temporary, path)


def atomic_write_json(path: Path, value: Any) -> None:
    atomic_write_text(path, json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def derive_key(password: str, length: int) -> bytes:
    digest = hashlib.sha256(password.encode()).digest()
    return digest * (length // len(digest)) + digest[: length % len(digest)]


def decrypt(ciphertext_b64: str, password: str) -> str:
    encrypted = base64.b64decode(ciphertext_b64)
    key = derive_key(password, len(encrypted))
    return bytes(left ^ right for left, right in zip(encrypted, key)).decode()


def ensure_project(api: Any, name: str, workspace_root: Path) -> str:
    canonical_root = str(workspace_root.resolve())
    for project in api.get("/projects"):
        if project["workspace"]["path"] == canonical_root:
            return str(project["id"])
    workspace_root.mkdir(parents=True, exist_ok=True)
    return str(
        api.post(
            "/projects",
            {"name": name, "workspace": {"path": canonical_root}},
        )["id"]
    )


def token_usage(tokens: dict[str, Any]) -> dict[str, int | float]:
    input_tokens = int(tokens.get("input_tokens", 0))
    output_tokens = int(tokens.get("output_tokens", 0))
    cached = int(tokens.get("cached_input_tokens", 0))
    uncached = max(0, input_tokens - cached)
    return {
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cached_input_tokens": cached,
        "cache_write_input_tokens": int(tokens.get("cache_write_input_tokens", 0)),
        "uncached_input_tokens": uncached,
        "effective_tokens": uncached + output_tokens,
        "cache_read_ratio": cached / input_tokens if input_tokens else 0.0,
    }


def save_state(path: Path, state: dict[str, Any]) -> None:
    state["updated_at"] = utc_now()
    atomic_write_json(path, state)


def reopen_terminal_failures(state: dict[str, Any]) -> int:
    reopened = 0
    for job in state["jobs"]:
        if job.pop("research_failed", False):
            job.setdefault("research", {"attempts": []})["status"] = "pending_retry"
            reopened += 1
        if job.pop("grade_failed", False) and "result" in job.get("research", {}):
            job.setdefault("grade", {"attempts": []})["status"] = "pending_retry"
            reopened += 1
    return reopened


def cancel_inflight(api: Any, state: dict[str, Any]) -> int:
    cancelled = 0
    for job in state.get("jobs", []):
        for phase in ("research", "grade"):
            phase_state = job.get(phase) or {}
            if phase_state.get("status") not in {"created", "running", "paused"}:
                continue
            attempts = phase_state.get("attempts") or []
            if not attempts:
                continue
            attempt = attempts[-1]
            try:
                api.post_empty(f"/workflows/{attempt['workflow_id']}/cancel")
            except ApiError as error:
                attempt["cancel_error"] = str(error)
            else:
                attempt["status"] = "cancel_requested"
                attempt["cancelled_by_runner"] = True
                phase_state["status"] = "cancel_requested"
                cancelled += 1
    return cancelled


def runtime_fingerprint(
    root: Path,
    runtime_files: tuple[str, ...],
    runtime_artifacts: dict[str, str] | None = None,
) -> dict[str, str]:
    fingerprint = {
        relative: hashlib.sha256((root / relative).read_bytes()).hexdigest()
        for relative in runtime_files
    }
    fingerprint.update(runtime_artifacts or {})
    return fingerprint


def record_runtime_snapshot(
    state: dict[str, Any],
    root: Path,
    runtime_files: tuple[str, ...],
    runtime_artifacts: dict[str, str],
) -> dict[str, str]:
    current = runtime_fingerprint(root, runtime_files, runtime_artifacts)
    history = state.setdefault("runtime_source_history", [])
    original = state.get("runtime_source_sha256")
    if original and not any(item.get("files_sha256") == original for item in history):
        history.insert(
            0,
            {
                "observed_at": state.get("created_at", utc_now()),
                "files_sha256": original,
            },
        )
    if not history or history[-1].get("files_sha256") != current:
        history.append({"observed_at": utc_now(), "files_sha256": current})
    state["current_runtime_source_sha256"] = current
    return current


def operational_usage(job: dict[str, Any], phase: str) -> dict[str, int]:
    keys = ("input_tokens", "output_tokens", "cached_input_tokens")
    total = {key: 0 for key in keys}
    phase_state = job.get(phase) or {}
    result_usage = (phase_state.get("result") or {}).get("usage") or {}
    for usage in [
        result_usage,
        *(item.get("usage") or {} for item in phase_state.get("attempts") or []),
    ]:
        for key in keys:
            total[key] += int(usage.get(key, 0))
    total["uncached_input_tokens"] = max(
        0, total["input_tokens"] - total["cached_input_tokens"]
    )
    total["effective_tokens"] = total["uncached_input_tokens"] + total["output_tokens"]
    return total


def mean(values: list[float]) -> float:
    return statistics.mean(values) if values else 0.0


def validate_workflows(api: Any, project_id: str, required: set[str]) -> None:
    available = {
        item["manifest"]["slug"]
        for item in api.get(f"/projects/{project_id}/workflow-programs")
    }
    missing = sorted(required - available)
    if missing:
        raise RuntimeError(f"PaperMachine server is missing Workflows: {missing}")


Capture = Callable[[dict[str, Any], dict[str, Any]], dict[str, Any]]
Launch = Callable[[dict[str, Any], dict[str, Any]], dict[str, Any]]


def record_failed_attempt(attempt: dict[str, Any], run: dict[str, Any]) -> None:
    usage = run.get("usage") or {}
    attempt["usage"] = token_usage(usage.get("tokens") or {})
    attempt["runtime_wall_time_seconds"] = int(usage.get("wall_time_seconds", 0))
    attempt["wall_time_seconds"] = workflow_wall_time_seconds(run)


def drive_phase(
    api: Any,
    state: dict[str, Any],
    state_path: Path,
    tasks: dict[str, dict[str, Any]],
    phase: str,
    launch: Launch,
    capture: Capture,
    retryable: Callable[[str], bool],
    poll_seconds: float,
    max_attempts: int,
    max_parallel_runs: int,
) -> None:
    failure_key = f"{phase}_failed"
    for job in state["jobs"]:
        if phase != "grade" or not job.get("research_failed"):
            job.setdefault(phase, {"attempts": [], "status": "pending"})

    while True:
        active = 0
        for job in state["jobs"]:
            if phase == "grade" and job.get("research_failed"):
                continue
            phase_state = job[phase]
            if "result" in phase_state or job.get(failure_key):
                continue
            if (
                not phase_state["attempts"]
                or phase_state.get("status") == "pending_retry"
            ):
                continue
            attempt = phase_state["attempts"][-1]
            view = api.get(f"/workflows/{attempt['workflow_id']}")
            run = view["workflow"]
            status = str(run["status"])
            phase_state["status"] = status
            attempt["status"] = status
            if status in {"created", "running", "paused"}:
                active += 1
                continue
            error = None
            if status == "completed":
                try:
                    phase_state["result"] = capture(job, view)
                except (KeyError, TypeError, ValueError) as capture_error:
                    error = f"invalid {phase} output: {capture_error}"
                else:
                    phase_state["status"] = "completed"
                    save_state(state_path, state)
                    continue
            elif status in {"failed", "cancelled"}:
                error = str(run.get("error") or status)
            if error is not None:
                attempt["error"] = error
                record_failed_attempt(attempt, run)
                if attempt.get("cancelled_by_runner") or (
                    len(phase_state["attempts"]) < max_attempts and retryable(error)
                ):
                    phase_state["status"] = "pending_retry"
                else:
                    job[failure_key] = True
                save_state(state_path, state)

        for job in state["jobs"]:
            if active >= max_parallel_runs:
                break
            if phase == "grade" and job.get("research_failed"):
                continue
            phase_state = job[phase]
            if "result" in phase_state or job.get(failure_key):
                continue
            if phase_state["attempts"] and phase_state.get("status") != "pending_retry":
                continue
            attempt = launch(job, tasks[job["task_key"]])
            attempt["runtime_source_sha256"] = state.get(
                "current_runtime_source_sha256", {}
            )
            phase_state["attempts"].append(attempt)
            phase_state["status"] = "created"
            active += 1
            save_state(state_path, state)

        unfinished = sum(
            not (phase == "grade" and job.get("research_failed"))
            and "result" not in job[phase]
            and not job.get(failure_key)
            for job in state["jobs"]
        )
        counts = Counter(
            "skipped"
            if phase == "grade" and job.get("research_failed")
            else "failed"
            if job.get(failure_key)
            else job.get(phase, {}).get("status", "pending")
            for job in state["jobs"]
        )
        print(f"{phase}: {dict(counts)}", flush=True)
        if unfinished == 0:
            return
        time.sleep(poll_seconds)


def server_data_dir(run_dir: Path) -> Path:
    """Return the only application-data directory used by a benchmark run."""
    return run_dir.resolve() / "server-data"


def default_server_binary(
    repository_root: Path, *, windows: bool | None = None
) -> Path:
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
        raise FileNotFoundError(
            "benchmark runtime artifact is missing: " + ", ".join(missing)
        )
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
        raise FileNotFoundError(
            f"PaperMachine config does not exist: {resolved_config}"
        )
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
