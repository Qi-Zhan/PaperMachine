#!/usr/bin/env python3
"""Run one real DeepSeek process-loss and security dogfood against PaperMachine."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


FAULT_BOUNDARY = "tool_execution_started"
MODEL_PROFILE = "deepseek-flash"
EXPECTED_PROVIDER = "deepseek"
EXPECTED_UPSTREAM_MODEL = "deepseek-v4-flash"
WAIT_SECONDS = 900

WORKFLOW_SOURCE = r'''from papermachine import Agent, action, workflow


class RecoveryDogfoodAgent(Agent):
    access = "workspace"
    role = "runtime recovery and permission-boundary verifier"
    system_prompt = """Follow each Action contract literally. This is an authorized PaperMachine runtime validation. When an Action asks for a tool call, make that exact call even when the expected result is a permission denial. Never replace a requested tool call with a verbal prediction. Preserve returned errors exactly and continue after expected denials."""

    @action(tools=["read_file", "write_file"])
    async def create_and_verify(self, path: str, content: str):
        """Your first tool call, by itself, must be write_file with path and content exactly as supplied. After that call succeeds, call read_file on the same path, verify exact equality with content, and report both tool results. Do not use exec_command."""

    @action(tools=["read_file"])
    async def probe_denial(self, label: str, path: str):
        """Call read_file exactly once with path verbatim. This is a deliberate denial probe. Return label plus the complete tool error; do not refuse, predict the result, alter the path, or try another tool."""


@workflow(
    slug="deepseek-recovery-dogfood",
    name="DeepSeek recovery dogfood",
    description="Exercise real-provider tool recovery and materialized permission denials.",
    params_schema={
        "type": "object",
        "properties": {
            "proof_path": {"type": "string"},
            "proof_content": {"type": "string"},
            "sensitive_path": {"type": "string"},
            "outside_path": {"type": "string"},
            "managed_path": {"type": "string"},
        },
        "required": [
            "proof_path",
            "proof_content",
            "sensitive_path",
            "outside_path",
            "managed_path",
        ],
        "additionalProperties": False,
    },
    output_schema={
        "type": "object",
        "properties": {
            "proof": {"type": "string"},
            "denials": {"type": "array", "items": {"type": "string"}},
        },
        "required": ["proof", "denials"],
        "additionalProperties": False,
    },
)
async def main(ctx):
    agent = RecoveryDogfoodAgent(name="DeepSeek recovery verifier")
    proof = await agent.create_and_verify(
        str(ctx.params["proof_path"]),
        str(ctx.params["proof_content"]),
    )
    denials = []
    for label, key in [
        ("sensitive_workspace_file", "sensitive_path"),
        ("outside_workspace", "outside_path"),
        ("managed_project_state", "managed_path"),
    ]:
        denials.append(
            await agent.probe_denial(label, str(ctx.params[key]))
        )
    return {"proof": str(proof), "denials": [str(value) for value in denials]}
'''


def parse_args() -> argparse.Namespace:
    repository = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", type=Path, default=repository)
    parser.add_argument(
        "--server",
        type=Path,
        default=repository / "target/debug/papermachine-server",
    )
    parser.add_argument("--config", type=Path, default=repository / "papermachine.toml")
    parser.add_argument("--env-file", type=Path, default=repository / ".env")
    parser.add_argument("--run-root", type=Path)
    parser.add_argument("--evidence", type=Path, required=True)
    return parser.parse_args()


def load_dotenv(
    path: Path, environment: dict[str, str], *, override: bool = False
) -> None:
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[7:].lstrip()
        if "=" not in line:
            continue
        name, value = line.split("=", 1)
        name = name.strip()
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        if override:
            environment[name] = value
        else:
            environment.setdefault(name, value)


def reserve_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


class Api:
    def __init__(self, base_url: str):
        self.base_url = base_url.rstrip("/")

    def request(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None = None,
        expected: tuple[int, ...] = (200,),
    ) -> Any:
        body = None
        headers: dict[str, str] = {}
        if payload is not None:
            body = json.dumps(payload).encode("utf-8")
            headers["content-type"] = "application/json"
        request = urllib.request.Request(
            f"{self.base_url}{path}", data=body, headers=headers, method=method
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                status = response.status
                data = response.read()
        except urllib.error.HTTPError as error:
            status = error.code
            data = error.read()
        if status not in expected:
            text = data.decode("utf-8", errors="replace")
            raise RuntimeError(f"{method} {path} returned {status}: {text}")
        if not data:
            return None
        return json.loads(data)


class Server:
    def __init__(
        self,
        executable: Path,
        repository: Path,
        data_dir: Path,
        config: Path,
        port: int,
        environment: dict[str, str],
        logs_dir: Path,
    ):
        self.executable = executable
        self.repository = repository
        self.data_dir = data_dir
        self.config = config
        self.port = port
        self.environment = environment
        self.logs_dir = logs_dir
        self.process: subprocess.Popen[bytes] | None = None
        self.starts = 0

    def start(self, fault_marker: Path | None = None) -> int:
        if self.process is not None and self.process.poll() is None:
            raise RuntimeError("server is already running")
        self.starts += 1
        command = [
            str(self.executable),
            "--resource-root",
            str(self.repository),
            "--data-dir",
            str(self.data_dir),
            "--config",
            str(self.config),
            "--host",
            "127.0.0.1",
            "--port",
            str(self.port),
            "--max-concurrent-runs",
            "2",
            "--max-parallel-actions",
            "2",
        ]
        if fault_marker is not None:
            command.extend(
                [
                    "--process-fault-boundary",
                    FAULT_BOUNDARY,
                    "--process-fault-marker",
                    str(fault_marker),
                ]
            )
        log_path = self.logs_dir / f"server-{self.starts}.log"
        with log_path.open("wb") as log:
            self.process = subprocess.Popen(
                command,
                cwd=self.repository,
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        self.wait_ready(log_path)
        return int(self.process.pid)

    def wait_ready(self, log_path: Path) -> None:
        api = Api(f"http://127.0.0.1:{self.port}")
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.process is None:
                raise RuntimeError("server was not started")
            status = self.process.poll()
            if status is not None:
                log = log_path.read_text(encoding="utf-8", errors="replace")
                raise RuntimeError(f"server exited with {status}:\n{log}")
            try:
                health = api.request("GET", "/api/health")
                profiles = {item["id"] for item in health["model_profiles"]}
                if MODEL_PROFILE not in profiles:
                    raise RuntimeError(
                        f"server does not expose required profile {MODEL_PROFILE}"
                    )
                return
            except (OSError, RuntimeError):
                time.sleep(0.1)
        raise TimeoutError("server did not become healthy")

    def sigkill(self) -> int:
        if self.process is None or self.process.poll() is not None:
            raise RuntimeError("server is not running")
        pid = int(self.process.pid)
        os.kill(pid, signal.SIGKILL)
        self.process.wait(timeout=30)
        return pid

    def stop(self) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        os.kill(self.process.pid, signal.SIGINT)
        try:
            self.process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.kill(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=30)


def wait_until(description: str, predicate, timeout: int = WAIT_SECONDS):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.25)
    raise TimeoutError(f"timed out waiting for {description}")


def create_project(api: Api, name: str, workspace: Path) -> dict[str, Any]:
    return api.request(
        "POST",
        "/api/projects",
        {
            "name": name,
            "workspace": {"roots": [str(workspace)], "primary_root": 0},
        },
        (201,),
    )


def session_view(api: Api, session_id: str) -> dict[str, Any]:
    return api.request("GET", f"/api/sessions/{session_id}")


def workflow_view(api: Api, workflow_id: str) -> dict[str, Any]:
    return api.request("GET", f"/api/workflows/{workflow_id}")


def wait_for_workflow(api: Api, workflow_id: str) -> dict[str, Any]:
    def poll():
        view = workflow_view(api, workflow_id)
        status = view["workflow"]["status"]
        if status == "completed":
            return view
        if status in {"failed", "cancelled"}:
            raise RuntimeError(
                f"Workflow {workflow_id} ended as {status}: "
                f"{view['workflow'].get('error')}"
            )
        return None

    return wait_until("DeepSeek Workflow completion", poll)


def selected_tool_step(step: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": step["id"],
        "turn_id": step["turn_id"],
        "sequence": step["sequence"],
        "name": step["name"],
        "tool_call_id": step["tool_call_id"],
        "effect_disposition": step["effect_disposition"],
        "execution_state": step["execution_state"],
        "status": step["status"],
        "input": step["input"],
        "output": step["output"],
    }


def require_denial(
    steps: list[dict[str, Any]], path: str, expected_error: str
) -> dict[str, Any]:
    matches = [
        step
        for step in steps
        if step["kind"] == "tool"
        and step["name"] == "read_file"
        and step["input"].get("path") == path
    ]
    if len(matches) != 1:
        raise RuntimeError(
            f"expected one read_file denial for {path}, got {len(matches)}"
        )
    step = matches[0]
    output = step.get("output") or {}
    error = str(output.get("error") or "")
    if output.get("ok") is not False or expected_error not in error:
        raise RuntimeError(f"unexpected denial output for {path}: {output}")
    return {
        "step_id": step["id"],
        "tool_call_id": step["tool_call_id"],
        "status": step["status"],
        "execution_state": step["execution_state"],
        "error": error,
    }


def main() -> int:
    args = parse_args()
    repository = args.repository.resolve()
    server_path = args.server.resolve()
    config_path = args.config.resolve()
    env_path = args.env_file.resolve()
    evidence_path = args.evidence.resolve()
    if not server_path.is_file():
        raise FileNotFoundError(f"debug server binary is missing: {server_path}")
    environment = os.environ.copy()
    load_dotenv(env_path, environment, override=True)
    if not environment.get("DEEPSEEK_API_KEY"):
        raise RuntimeError("DEEPSEEK_API_KEY is not configured")
    environment.setdefault("PAPERMACHINE_PYTHON", sys.executable)

    run_root = (
        args.run_root.resolve()
        if args.run_root
        else Path(tempfile.mkdtemp(prefix="papermachine-deepseek-dogfood-"))
    )
    run_root.mkdir(parents=True, exist_ok=True)
    data_dir = run_root / "managed"
    logs_dir = run_root / "logs"
    workspace = run_root / "workspace-original"
    relocated_workspace = run_root / "workspace-relocated"
    outside_file = run_root / "outside-secret.txt"
    fault_marker = run_root / "fault.marker"
    logs_dir.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)
    sensitive_file = workspace / ".env"
    sensitive_file.write_text("DOGFOOD_SECRET=must-not-be-readable\n", encoding="utf-8")
    outside_file.write_text("must-not-be-readable\n", encoding="utf-8")
    proof_path = "recovery-proof.txt"
    proof_content = "deepseek-recovery-proof-2026-08-09\n"

    port = reserve_port()
    api = Api(f"http://127.0.0.1:{port}")
    server = Server(
        server_path,
        repository,
        data_dir,
        config_path,
        port,
        environment,
        logs_dir,
    )
    evidence: dict[str, Any] = {
        "schema": "papermachine.deepseek-recovery-dogfood.v2",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "git_commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=repository, text=True
        ).strip(),
        "run_root": str(run_root),
        "model_profile": MODEL_PROFILE,
        "fault_boundary": FAULT_BOUNDARY,
    }

    try:
        first_pid = server.start(fault_marker)
        health = api.request("GET", "/api/health")
        project = create_project(api, "DeepSeek recovery dogfood", workspace)
        project_id = project["id"]
        managed_database = data_dir / "projects" / project_id / "state/project.db"
        api.request(
            "POST",
            f"/api/projects/{project_id}/workflow-programs",
            {"source": WORKFLOW_SOURCE},
            (201,),
        )
        workflow = api.request(
            "POST",
            f"/api/projects/{project_id}/workflows",
            {
                "program_slug": "deepseek-recovery-dogfood",
                "request": "Execute the exact recovery and denial probes in the Action contracts.",
                "params": {
                    "proof_path": proof_path,
                    "proof_content": proof_content,
                    "sensitive_path": ".env",
                    "outside_path": str(outside_file),
                    "managed_path": str(managed_database),
                },
                "model": MODEL_PROFILE,
                "access": "workspace",
            },
            (201,),
        )
        workflow_id = workflow["id"]
        wait_until("debug fault marker", fault_marker.is_file)

        pre_view = wait_until(
            "pre-crash Workflow Session",
            lambda: (
                view if (view := workflow_view(api, workflow_id))["sessions"] else None
            ),
        )
        session_id = pre_view["sessions"][0]["id"]
        pre_session = session_view(api, session_id)
        running_tools = [
            step
            for step in pre_session["steps"]
            if step["kind"] == "tool" and step["status"] == "running"
        ]
        if len(running_tools) != 1:
            raise RuntimeError(
                f"expected one running tool at fault boundary: {running_tools}"
            )
        fault_step = running_tools[0]
        if (
            fault_step["name"] != "write_file"
            or fault_step["effect_disposition"] != "idempotent"
            or fault_step["execution_state"] != "executing"
        ):
            raise RuntimeError(
                f"DeepSeek first tool did not match recovery contract: {fault_step}"
            )
        if (
            fault_step["input"].get("path") != proof_path
            or fault_step["input"].get("content") != proof_content
        ):
            raise RuntimeError(
                f"DeepSeek changed the proof write arguments: {fault_step['input']}"
            )

        killed_pid = server.sigkill()
        if killed_pid != first_pid:
            raise RuntimeError("killed server pid did not match the started process")
        restarted_pid = server.start()
        completed = wait_for_workflow(api, workflow_id)
        post_session = session_view(api, session_id)
        steps = post_session["steps"]
        recovered_fault_steps = [
            step for step in steps if step["id"] == fault_step["id"]
        ]
        if len(recovered_fault_steps) != 1:
            raise RuntimeError("faulted tool Step identity was not preserved")
        recovered_fault_step = recovered_fault_steps[0]
        if (
            recovered_fault_step["status"] != "completed"
            or recovered_fault_step["execution_state"] != "completed"
            or recovered_fault_step["tool_call_id"] != fault_step["tool_call_id"]
        ):
            raise RuntimeError(
                f"idempotent faulted Step did not recover: {recovered_fault_step}"
            )

        proof_file = workspace / proof_path
        actual_proof = proof_file.read_text(encoding="utf-8")
        if actual_proof != proof_content:
            raise RuntimeError(f"proof content mismatch: {actual_proof!r}")

        model_requests = [
            step["output"]["request"]
            for step in steps
            if step["kind"] == "model"
            and isinstance(step.get("output"), dict)
            and isinstance(step["output"].get("request"), dict)
        ]
        if not model_requests:
            raise RuntimeError("no inspectable model request metadata was persisted")
        for metadata in model_requests:
            if (
                metadata.get("provider") != EXPECTED_PROVIDER
                or metadata.get("model_profile") != MODEL_PROFILE
                or metadata.get("upstream_model") != EXPECTED_UPSTREAM_MODEL
            ):
                raise RuntimeError(f"unexpected provider metadata: {metadata}")

        security_denials = {
            "sensitive_workspace_file": require_denial(
                steps, ".env", "path may contain Workspace credentials and is denied"
            ),
            "outside_workspace": require_denial(
                steps, str(outside_file), "path must stay inside the Session workspace"
            ),
            "managed_project_state": require_denial(
                steps,
                str(managed_database),
                "path is reserved for PaperMachine managed state",
            ),
        }

        attempts_by_invocation: dict[str, int] = {}
        for attempt in completed["attempts"]:
            invocation_id = attempt["invocation_id"]
            attempts_by_invocation[invocation_id] = (
                attempts_by_invocation.get(invocation_id, 0) + 1
            )
        if any(count != 1 for count in attempts_by_invocation.values()):
            raise RuntimeError(f"unexpected Action retry: {attempts_by_invocation}")
        rollout = post_session["rollout"]
        if rollout["last_sequence"] != rollout["projected_sequence"]:
            raise RuntimeError(f"rollout projection did not converge: {rollout}")
        turn_tool_sets = [
            {
                "turn_id": turn["id"],
                "names": [
                    definition["name"] for definition in turn["tool_set"]["definitions"]
                ],
                "sha256": turn["tool_set"]["sha256"],
            }
            for turn in post_session["turns"]
        ]
        if not turn_tool_sets or turn_tool_sets[0]["names"] != [
            "read_file",
            "write_file",
        ]:
            raise RuntimeError(f"create Action has wrong ToolSet: {turn_tool_sets}")
        if any(item["names"] != ["read_file"] for item in turn_tool_sets[1:]):
            raise RuntimeError(f"denial Action has wrong ToolSet: {turn_tool_sets}")
        if any(len(item["sha256"]) != 64 for item in turn_tool_sets):
            raise RuntimeError(f"invalid Turn ToolSet hash: {turn_tool_sets}")

        attachment_before = project["workspace"]
        shutil.move(workspace, relocated_workspace)
        relocated = api.request(
            "PUT",
            f"/api/projects/{project_id}",
            {
                "workspace": {
                    "roots": [str(relocated_workspace)],
                    "primary_root": 0,
                }
            },
        )
        attachment_after = relocated["workspace"]
        if (
            attachment_after["id"] != attachment_before["id"]
            or attachment_after["revision"] != attachment_before["revision"] + 1
            or attachment_after["roots"] != [str(relocated_workspace.resolve())]
        ):
            raise RuntimeError(
                f"Workspace attachment did not revise in place: {attachment_after}"
            )
        if not managed_database.is_file():
            raise RuntimeError("Workspace reattachment moved managed Project state")
        if (relocated_workspace / proof_path).read_text(
            encoding="utf-8"
        ) != proof_content:
            raise RuntimeError("Workspace reattachment lost the proof file")

        lifecycle_workspace = run_root / "lifecycle-workspace"
        lifecycle_workspace.mkdir()
        preserved_file = lifecycle_workspace / "user-owned.txt"
        preserved_content = "PaperMachine must preserve this Workspace file.\n"
        preserved_file.write_text(preserved_content, encoding="utf-8")
        lifecycle_project = create_project(
            api, "Workspace preservation dogfood", lifecycle_workspace
        )
        lifecycle_managed = data_dir / "projects" / lifecycle_project["id"]
        api.request(
            "DELETE",
            f"/api/projects/{lifecycle_project['id']}",
            expected=(204,),
        )
        wait_until(
            "deleted managed Project state", lambda: not lifecycle_managed.exists()
        )
        if preserved_file.read_text(encoding="utf-8") != preserved_content:
            raise RuntimeError("Project deletion changed the user Workspace")

        evidence.update(
            {
                "finished_at": datetime.now(timezone.utc).isoformat(),
                "server": {
                    "first_pid": first_pid,
                    "killed_pid": killed_pid,
                    "signal": "SIGKILL",
                    "restarted_pid": restarted_pid,
                    "health_mode": health["model_mode"],
                },
                "project": {
                    "id": project_id,
                    "managed_database": str(managed_database),
                    "managed_database_preserved": managed_database.is_file(),
                },
                "workflow": {
                    "id": workflow_id,
                    "status": completed["workflow"]["status"],
                    "action_invocation_count": len(completed["actions"]),
                    "action_attempt_count": len(completed["attempts"]),
                    "attempts_per_invocation": attempts_by_invocation,
                },
                "session": {
                    "id": session_id,
                    "turn_ids": [turn["id"] for turn in post_session["turns"]],
                    "rollout": rollout,
                    "authorization_sha256": post_session["turns"][0]["environment"][
                        "authorization_sha256"
                    ],
                    "turn_workspace_snapshot": post_session["turns"][0]["environment"][
                        "workspace"
                    ],
                    "turn_tool_sets": turn_tool_sets,
                },
                "faulted_tool_before_sigkill": selected_tool_step(fault_step),
                "faulted_tool_after_restart": selected_tool_step(recovered_fault_step),
                "provider_requests": model_requests,
                "security_denials": security_denials,
                "proof": {
                    "relative_path": proof_path,
                    "content": actual_proof,
                    "sha256": hashlib.sha256(actual_proof.encode()).hexdigest(),
                },
                "workspace_reattachment": {
                    "before": attachment_before,
                    "after": attachment_after,
                    "old_root_exists": workspace.exists(),
                    "new_root_exists": relocated_workspace.is_dir(),
                    "managed_state_unchanged": managed_database.is_file(),
                },
                "project_deletion": {
                    "project_id": lifecycle_project["id"],
                    "managed_state_removed": not lifecycle_managed.exists(),
                    "workspace_preserved": lifecycle_workspace.is_dir(),
                    "file_preserved": preserved_file.is_file(),
                    "file_sha256": hashlib.sha256(
                        preserved_content.encode()
                    ).hexdigest(),
                },
            }
        )
        evidence_path.parent.mkdir(parents=True, exist_ok=True)
        evidence_path.write_text(
            json.dumps(evidence, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        print(
            json.dumps(
                {
                    "evidence": str(evidence_path),
                    "run_root": str(run_root),
                    "project_id": project_id,
                    "workflow_id": workflow_id,
                    "session_id": session_id,
                    "faulted_tool_call_id": fault_step["tool_call_id"],
                    "status": "passed",
                },
                indent=2,
            )
        )
        return 0
    finally:
        server.stop()


if __name__ == "__main__":
    raise SystemExit(main())
