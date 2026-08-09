#!/usr/bin/env python3
"""Exercise Project Summary's exact per-Turn tools with GLM and DeepSeek."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from deepseek_recovery_dogfood import (
    Api,
    Server,
    create_project,
    load_dotenv,
    reserve_port,
    session_view,
    wait_for_workflow,
)


GLM_PROFILE = "glm-5-2"
DEEPSEEK_PROFILE = "deepseek-flash"
SUMMARY_TOOLS = [
    "patch_project_home",
    "preview_project_home",
    "read_project_home",
]
WORKSPACE_TOOLS = {"exec_command", "fetch_url", "read_file", "write_file"}

NOTE_WORKFLOW = r"""from papermachine import publish_artifact, workflow


@workflow(
    slug="dogfood-project-note",
    name="Dogfood project note",
    description="Add one deterministic Project fact for the refresh pass.",
    request_mode="none",
    params_schema={
        "type": "object",
        "properties": {"note": {"type": "string"}},
        "required": ["note"],
        "additionalProperties": False,
    },
)
async def main(ctx):
    artifact = await publish_artifact(
        name="verified-note.md",
        content=str(ctx.params["note"]),
        kind="report",
        metadata={"role": "dogfood_verified_note"},
    )
    return {"artifact_id": artifact.id}
"""


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


def model_request_metadata(steps: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        step["output"]["request"]
        for step in steps
        if step["kind"] == "model"
        and isinstance(step.get("output"), dict)
        and isinstance(step["output"].get("request"), dict)
    ]


def canonical_project_home(api: Api, project_id: str) -> dict[str, Any] | None:
    return api.request("GET", f"/api/projects/{project_id}").get(
        "project_home_artifact"
    )


def artifact_content(api: Api, artifact_id: str) -> str:
    with urllib.request.urlopen(
        f"{api.base_url}/api/artifacts/{artifact_id}/content", timeout=30
    ) as response:
        return response.read().decode("utf-8")


def run_summary(
    api: Api,
    project_id: str,
    model: str,
    instruction: str,
    expected_provider: str,
) -> dict[str, Any]:
    launched = api.request(
        "POST",
        f"/api/projects/{project_id}/workflows",
        {
            "program_slug": "project-summary",
            "request": "Refresh the Project home page from current Project evidence.",
            "instructions": instruction,
            "params": {
                "interval_minutes": 0,
                "max_sessions": 50,
                "turns_per_session": 12,
                "max_artifacts": 50,
            },
            "model": model,
            "access": "model_only",
        },
        (201,),
    )
    view = wait_for_workflow(api, launched["id"])
    if view["workflow"]["status"] != "completed":
        raise RuntimeError(f"Summary did not complete: {view['workflow']}")
    if len(view["actions"]) != 1 or len(view["attempts"]) != 1:
        raise RuntimeError(
            "Summary must complete through one ActionInvocation and one ActionAttempt"
        )
    participant = view["participants"][0]
    session = session_view(api, participant["session_id"])
    if len(session["turns"]) != 1:
        raise RuntimeError(f"Summary Session has unexpected Turns: {session['turns']}")
    turn = session["turns"][0]
    definitions = turn["tool_set"]["definitions"]
    names = [definition["name"] for definition in definitions]
    if names != SUMMARY_TOOLS:
        raise RuntimeError(f"Summary Turn materialized wrong tools: {names}")
    if set(names) & WORKSPACE_TOOLS:
        raise RuntimeError(f"Summary Turn received Workspace tools: {names}")
    if len(turn["tool_set"]["sha256"]) != 64:
        raise RuntimeError("Summary Turn ToolSet hash is not SHA-256")
    if turn["environment"]["authorization"]["preset"] != "model_only":
        raise RuntimeError("Summary Turn did not preserve model_only Workspace access")

    steps = session["steps"]
    failed_model_steps = [
        step
        for step in steps
        if step["kind"] == "model" and step["status"] != "completed"
    ]
    if failed_model_steps:
        raise RuntimeError(f"Summary has failed model Steps: {failed_model_steps}")
    tool_steps = [step for step in steps if step["kind"] == "tool"]
    called = {step["name"] for step in tool_steps}
    missing = set(SUMMARY_TOOLS) - called
    if missing:
        raise RuntimeError(
            f"Summary did not exercise required tools: {sorted(missing)}"
        )
    previews = [step for step in tool_steps if step["name"] == "preview_project_home"]
    diagnostics = previews[-1]["output"].get("result", {}).get("diagnostics")
    if diagnostics != []:
        raise RuntimeError(f"final Project-home preview has diagnostics: {diagnostics}")
    reads = [step for step in tool_steps if step["name"] == "read_project_home"]
    base_artifact_id = reads[0]["output"].get("result", {}).get("base_artifact_id")

    metadata = model_request_metadata(steps)
    if not metadata:
        raise RuntimeError("Summary has no inspectable real-provider request metadata")
    if any(item.get("provider") != expected_provider for item in metadata):
        raise RuntimeError(f"Summary used the wrong provider: {metadata}")
    if any(item.get("model_profile") != model for item in metadata):
        raise RuntimeError(f"Summary used the wrong model profile: {metadata}")

    artifacts = view["artifacts"]
    page = next(
        item for item in artifacts if item["metadata"].get("role") == "project_summary"
    )
    source = next(
        item
        for item in artifacts
        if item["metadata"].get("role") == "project_summary_source"
    )
    html = artifact_content(api, page["id"])
    if not html.strip() or "<h1" not in html.lower():
        raise RuntimeError("published Project home is empty or lacks an h1")

    return {
        "workflow_id": view["workflow"]["id"],
        "session_id": session["session"]["id"],
        "action_invocation_id": view["actions"][0]["id"],
        "action_attempt_id": view["attempts"][0]["id"],
        "turn_id": turn["id"],
        "tool_set": turn["tool_set"],
        "tool_steps": [
            {
                "name": step["name"],
                "status": step["status"],
                "tool_call_id": step["tool_call_id"],
                "execution_state": step["execution_state"],
            }
            for step in tool_steps
        ],
        "tool_trial_failures": sum(
            1 for step in tool_steps if step["status"] != "completed"
        ),
        "base_artifact_id": base_artifact_id,
        "page_artifact_id": page["id"],
        "source_artifact_id": source["id"],
        "page_sha256": hashlib.sha256(html.encode("utf-8")).hexdigest(),
        "provider_requests": metadata,
        "usage": view["workflow"]["usage"],
    }


def main() -> int:
    args = parse_args()
    repository = args.repository.resolve()
    server_path = args.server.resolve()
    config_path = args.config.resolve()
    evidence_path = args.evidence.resolve()
    if not server_path.is_file():
        raise FileNotFoundError(f"debug server binary is missing: {server_path}")

    environment = os.environ.copy()
    load_dotenv(args.env_file.resolve(), environment, override=True)
    for name in ["AEROIDES_API_KEY", "DEEPSEEK_API_KEY"]:
        if not environment.get(name):
            raise RuntimeError(f"{name} is not configured")
    environment.setdefault("PAPERMACHINE_PYTHON", sys.executable)

    run_root = (
        args.run_root.resolve()
        if args.run_root
        else Path(tempfile.mkdtemp(prefix="papermachine-summary-toolset-dogfood-"))
    )
    data_dir = run_root / "managed"
    workspace = run_root / "workspace"
    logs = run_root / "logs"
    workspace.mkdir(parents=True, exist_ok=True)
    logs.mkdir(parents=True, exist_ok=True)

    port = reserve_port()
    api = Api(f"http://127.0.0.1:{port}")
    server = Server(
        server_path,
        repository,
        data_dir,
        config_path,
        port,
        environment,
        logs,
    )
    evidence: dict[str, Any] = {
        "schema": "papermachine.project-summary-toolset-dogfood.v1",
        "started_at": datetime.now(timezone.utc).isoformat(),
        "git_commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=repository, text=True
        ).strip(),
        "run_root": str(run_root),
    }

    try:
        server.start()
        project = create_project(api, "Project Summary ToolSet dogfood", workspace)
        first = run_summary(
            api,
            project["id"],
            GLM_PROFILE,
            "Create a concise first Project page. Use the normal tool loop and verify the final preview.",
            "aeroides",
        )

        api.request(
            "POST",
            f"/api/projects/{project['id']}/workflow-programs",
            {"source": NOTE_WORKFLOW},
            (201,),
        )
        note = api.request(
            "POST",
            f"/api/projects/{project['id']}/workflows",
            {
                "program_slug": "dogfood-project-note",
                "params": {
                    "note": "Verified milestone: per-Action ToolRegistry dogfood reached the refresh phase."
                },
                "model": GLM_PROFILE,
                "access": "model_only",
            },
            (201,),
        )
        wait_for_workflow(api, note["id"])

        second = run_summary(
            api,
            project["id"],
            DEEPSEEK_PROFILE,
            "Refresh the existing Project page from current evidence, including the verified milestone when supported. Inspect and correct the final preview.",
            "deepseek",
        )
        if first["base_artifact_id"] is not None:
            raise RuntimeError(
                "first Summary unexpectedly started from an existing page"
            )
        if second["base_artifact_id"] != first["page_artifact_id"]:
            raise RuntimeError(
                "DeepSeek refresh did not start from the GLM-published Project page"
            )
        if first["tool_set"]["sha256"] != second["tool_set"]["sha256"]:
            raise RuntimeError(
                "same Summary declaration produced different ToolSet hashes"
            )
        canonical_home = canonical_project_home(api, project["id"])
        if not canonical_home or canonical_home["id"] != second["page_artifact_id"]:
            raise RuntimeError("Project overview does not reference the refreshed page")

        evidence.update(
            {
                "finished_at": datetime.now(timezone.utc).isoformat(),
                "project_id": project["id"],
                "first_write_glm": first,
                "existing_page_refresh_deepseek": second,
                "canonical_page_artifact_id": canonical_home["id"],
                "assertions": {
                    "fresh_data_dir": True,
                    "one_attempt_per_summary": True,
                    "no_model_or_terminal_failure": True,
                    "final_preview_diagnostics": 0,
                    "no_workspace_tools": True,
                    "same_exact_tool_set": True,
                    "refresh_used_first_page_as_base": True,
                },
            }
        )
    finally:
        server.stop()

    evidence_path.parent.mkdir(parents=True, exist_ok=True)
    evidence_path.write_text(
        json.dumps(evidence, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    print(evidence_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
