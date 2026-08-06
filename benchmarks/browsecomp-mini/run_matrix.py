#!/usr/bin/env python3
"""Run a resumable PaperMachine matrix on a pinned BrowseComp sample."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import math
import os
import random
import statistics
import tempfile
import time
import urllib.error
import urllib.request
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable


CONDITIONS = {
    "single_agent": {
        "program_slug": "single-agent-research",
        "input": {},
    },
    "coverage_r1": {
        "program_slug": "evidence-loop",
        "input": {
            "route_count": 2,
            "max_rounds": 1,
            "max_followups_per_round": 2,
        },
    },
    "coverage_r2": {
        "program_slug": "evidence-loop",
        "input": {
            "route_count": 2,
            "max_rounds": 2,
            "max_followups_per_round": 2,
        },
    },
    "coverage_r3": {
        "program_slug": "evidence-loop",
        "input": {
            "route_count": 3,
            "minimum_route_count": 3,
            "max_rounds": 3,
            "max_followups_per_round": 3,
        },
    },
    "coverage_r4": {
        "program_slug": "evidence-loop",
        "input": {
            "route_count": 4,
            "minimum_route_count": 4,
            "max_rounds": 4,
            "max_followups_per_round": 4,
        },
    },
}
RUNTIME_FILES = (
    "benchmarks/browsecomp-mini/run_matrix.py",
    "crates/agent/src/lib.rs",
    "crates/model/src/openai.rs",
    "crates/model/src/providers.rs",
    "crates/protocol/src/event.rs",
    "crates/protocol/src/model.rs",
    "crates/protocol/src/project.rs",
    "crates/workflow/src/runtime.rs",
    "crates/workflow/src/scheduler.rs",
    "crates/server/src/lib.rs",
    "crates/session/src/lib.rs",
    "crates/store/src/database.rs",
    "python/papermachine/__init__.py",
    "python/papermachine/_runner.py",
    "python/papermachine/_validate.py",
    "papermachine.toml",
    "workflows/builtin/evidence-loop/workflow.py",
    "workflows/builtin/short-answer-grader/workflow.py",
    "workflows/builtin/single-agent-research/workflow.py",
)


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
    """Return end-to-end elapsed time, including time lost across restarts."""
    usage = run.get("usage") or {}
    runtime_seconds = max(0, int(usage.get("wall_time_seconds", 0)))
    try:
        created_at = datetime.fromisoformat(str(run["created_at"]).replace("Z", "+00:00"))
        updated_at = datetime.fromisoformat(str(run["updated_at"]).replace("Z", "+00:00"))
        observed_seconds = max(
            0,
            math.ceil((updated_at - created_at).total_seconds()),
        )
    except (KeyError, TypeError, ValueError):
        observed_seconds = 0
    return max(runtime_seconds, observed_seconds)


def atomic_write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        "w", encoding="utf-8", dir=path.parent, delete=False
    ) as handle:
        handle.write(content)
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


def task_content(task: dict[str, Any]) -> tuple[str, str]:
    password = str(task["canary"])
    return decrypt(str(task["problem"]), password), decrypt(
        str(task["answer"]), password
    )


def research_prompt(question: str) -> str:
    return (
        f"{question.strip()}\n\n"
        "Return the final response in exactly this format:\n"
        "Explanation: {a concise evidence-based explanation}\n"
        "Exact Answer: {your succinct final answer}\n"
        "Confidence: {your confidence from 0% to 100%}"
    )


def ensure_project(
    api: PaperMachineApi,
    name: str,
    description: str,
    root_path: Path,
) -> str:
    root_path.mkdir(parents=True, exist_ok=True)
    canonical_root = str(root_path.resolve())
    for project in api.get("/projects"):
        if project["root_path"] == canonical_root:
            return str(project["id"])
    return str(
        api.post(
            "/projects",
            {"name": name, "description": description, "root_path": canonical_root},
        )["id"]
    )


def build_jobs(
    task_keys: list[str], conditions: list[str], repeats: int, seed: int
) -> list[dict[str, Any]]:
    jobs = [
        {
            "key": f"task-{task_key}__{condition}__repeat-{repeat}",
            "task_key": task_key,
            "condition": condition,
            "repeat": repeat,
        }
        for task_key in task_keys
        for repeat in range(1, repeats + 1)
        for condition in conditions
    ]
    random.Random(seed).shuffle(jobs)
    return jobs


def launch_research_run(
    api: PaperMachineApi,
    project_id: str,
    job: dict[str, Any],
    task: dict[str, Any],
    model: str,
) -> dict[str, Any]:
    condition = CONDITIONS[job["condition"]]
    question, _ = task_content(task)
    run = api.post(
        f"/projects/{project_id}/workflows",
        {
            "program_slug": condition["program_slug"],
            "objective": research_prompt(question),
            "input": condition["input"],
            "model": model,
            "access": "research",
            "enabled_skills": [],
            "context_mode": "fresh",
        },
    )
    return {
        "workflow_id": str(run["id"]),
        "launched_at": utc_now(),
    }


def launch_grader_run(
    api: PaperMachineApi,
    project_id: str,
    job: dict[str, Any],
    task: dict[str, Any],
    response: str,
    model: str,
) -> dict[str, Any]:
    question, correct_answer = task_content(task)
    run = api.post(
        f"/projects/{project_id}/workflows",
        {
            "program_slug": "short-answer-grader",
            "objective": "Blindly judge the submitted response against the supplied reference answer.",
            "input": {
                "question": question,
                "correct_answer": correct_answer,
                "response": response,
                "grader_model": model,
            },
            "model": model,
            "access": "model_only",
            "enabled_skills": [],
            "context_mode": "fresh",
        },
    )
    return {
        "workflow_id": str(run["id"]),
        "launched_at": utc_now(),
    }


def token_usage(tokens: dict[str, Any]) -> dict[str, int | float]:
    input_tokens = int(tokens.get("input_tokens", 0))
    output_tokens = int(tokens.get("output_tokens", 0))
    cached = int(tokens.get("cached_input_tokens", 0))
    return {
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cached_input_tokens": cached,
        "cache_write_input_tokens": int(tokens.get("cache_write_input_tokens", 0)),
        "uncached_input_tokens": max(0, input_tokens - cached),
        "effective_tokens": max(0, input_tokens - cached) + output_tokens,
        "cache_read_ratio": cached / input_tokens if input_tokens else 0.0,
    }


def session_metrics(view: dict[str, Any]) -> dict[str, Any]:
    turns = view.get("turns", [])
    steps = view.get("steps", [])
    usage = {
        key: sum(int(turn.get("usage", {}).get(key, 0)) for turn in turns)
        for key in (
            "input_tokens",
            "output_tokens",
            "cached_input_tokens",
            "cache_write_input_tokens",
        )
    }
    hosted = [
        step
        for step in steps
        if step.get("kind") == "tool" and step.get("name") == "web_search"
    ]
    continuation_hits = 0
    continuation_misses = Counter()
    transports = Counter()
    cache_modes = Counter()
    model_profiles = Counter()
    model_providers = Counter()
    upstream_models = Counter()
    fallback_reasons = Counter()
    cache_keys = set()
    explicit_breakpoints = 0
    for step in steps:
        if step.get("kind") != "model":
            continue
        metadata = (step.get("output") or {}).get("request") or {}
        if metadata.get("transport"):
            transports[str(metadata["transport"])] += 1
        if metadata.get("prompt_cache_mode"):
            cache_modes[str(metadata["prompt_cache_mode"])] += 1
        if metadata.get("model_profile"):
            model_profiles[str(metadata["model_profile"])] += 1
        if metadata.get("provider"):
            model_providers[str(metadata["provider"])] += 1
        if metadata.get("upstream_model"):
            upstream_models[str(metadata["upstream_model"])] += 1
        if metadata.get("prompt_cache_key"):
            cache_keys.add(str(metadata["prompt_cache_key"]))
        if metadata.get("prompt_cache_breakpoint"):
            explicit_breakpoints += 1
        if metadata.get("websocket_fallback_reason"):
            fallback_reasons[str(metadata["websocket_fallback_reason"])] += 1
        if metadata.get("used_previous_response_id"):
            continuation_hits += 1
        elif metadata.get("continuation_miss_reason"):
            continuation_misses[str(metadata["continuation_miss_reason"])] += 1
    return {
        "name": view["session"]["title"],
        "session_id": view["session"]["id"],
        "turns": len(turns),
        "steps": len(steps),
        **token_usage(usage),
        "hosted_search_calls": len(hosted),
        "continuation_hits": continuation_hits,
        "continuation_misses": dict(continuation_misses),
        "model_transports": dict(transports),
        "prompt_cache_modes": dict(cache_modes),
        "model_profiles": dict(model_profiles),
        "model_providers": dict(model_providers),
        "upstream_models": dict(upstream_models),
        "prompt_cache_keys": sorted(cache_keys),
        "explicit_cache_breakpoints": explicit_breakpoints,
        "websocket_fallback_reasons": dict(fallback_reasons),
    }


def capture_research_result(
    api: PaperMachineApi,
    view: dict[str, Any],
    article_path: Path,
) -> dict[str, Any]:
    run = view["workflow"]
    output = run.get("output") or {}
    report = output.get("report")
    if not isinstance(report, str) or not report.strip():
        raise ValueError("completed research run did not return a non-empty report")
    atomic_write_text(article_path, report.rstrip() + "\n")
    agents = [
        session_metrics(api.get(f"/sessions/{session['id']}"))
        for session in view.get("sessions", [])
    ]
    usage = run.get("usage") or {}
    result = {
        "status": run["status"],
        "workflow_sha256": run["program"]["sha256"],
        "created_at": run["created_at"],
        "completed_at": run["updated_at"],
        "runtime_wall_time_seconds": int(usage.get("wall_time_seconds", 0)),
        "wall_time_seconds": workflow_wall_time_seconds(run),
        "agents_created": int(usage.get("agents_created", 0)),
        "actions_completed": int(usage.get("actions_completed", 0)),
        "action_steps": int(usage.get("action_steps", 0)),
        "usage": token_usage(usage.get("tokens") or {}),
        "report_characters": len(report),
        "report_sha256": hashlib.sha256(report.encode()).hexdigest(),
        "article_path": str(article_path),
        "hosted_search_calls": sum(agent["hosted_search_calls"] for agent in agents),
        "continuation_hits": sum(agent["continuation_hits"] for agent in agents),
        "model_transports": dict(
            sum((Counter(agent["model_transports"]) for agent in agents), Counter())
        ),
        "prompt_cache_modes": dict(
            sum((Counter(agent["prompt_cache_modes"]) for agent in agents), Counter())
        ),
        "model_profiles": dict(
            sum((Counter(agent["model_profiles"]) for agent in agents), Counter())
        ),
        "model_providers": dict(
            sum((Counter(agent["model_providers"]) for agent in agents), Counter())
        ),
        "upstream_models": dict(
            sum((Counter(agent["upstream_models"]) for agent in agents), Counter())
        ),
        "prompt_cache_key_count": len(
            {key for agent in agents for key in agent["prompt_cache_keys"]}
        ),
        "explicit_cache_breakpoints": sum(
            agent["explicit_cache_breakpoints"] for agent in agents
        ),
        "websocket_fallback_reasons": dict(
            sum(
                (Counter(agent["websocket_fallback_reasons"]) for agent in agents),
                Counter(),
            )
        ),
        "per_agent": agents,
    }
    for key in (
        "rounds",
        "route_sessions_reused",
        "plan",
        "evaluation",
        "draft_audit",
        "completion",
    ):
        if key in output:
            result[key] = output[key]
    return result


def capture_grader_result(view: dict[str, Any], grade_path: Path) -> dict[str, Any]:
    run = view["workflow"]
    grading = (run.get("output") or {}).get("grading")
    if not isinstance(grading, dict):
        raise ValueError("grader output is missing grading object")
    correct = grading.get("correct")
    if not isinstance(correct, bool):
        raise ValueError("grader correct field must be boolean")
    extracted = grading.get("extracted_final_answer")
    if extracted is not None and not isinstance(extracted, str):
        raise ValueError("grader extracted_final_answer must be a string or null")
    confidence = grading.get("confidence", 100)
    if isinstance(confidence, bool) or not isinstance(confidence, (int, float)):
        raise ValueError("grader confidence must be numeric")
    if not 0 <= float(confidence) <= 100:
        raise ValueError("grader confidence must be between 0 and 100")
    atomic_write_json(grade_path, grading)
    usage = run.get("usage") or {}
    return {
        "correct": correct,
        "extracted_final_answer": extracted,
        "confidence": float(confidence),
        "reasoning": str(grading.get("reasoning", "")),
        "workflow_sha256": run["program"]["sha256"],
        "usage": token_usage(usage.get("tokens") or {}),
        "created_at": run["created_at"],
        "completed_at": run["updated_at"],
        "runtime_wall_time_seconds": int(usage.get("wall_time_seconds", 0)),
        "wall_time_seconds": workflow_wall_time_seconds(run),
        "grade_path": str(grade_path),
    }


def record_failed_attempt(attempt: dict[str, Any], run: dict[str, Any]) -> None:
    usage = run.get("usage") or {}
    attempt["usage"] = token_usage(usage.get("tokens") or {})
    attempt["runtime_wall_time_seconds"] = int(usage.get("wall_time_seconds", 0))
    attempt["wall_time_seconds"] = workflow_wall_time_seconds(run)


def is_retryable_error(error: str) -> bool:
    lowered = error.casefold()
    deterministic = (
        "budget exceeded",
        "budget exhausted",
        "context window",
        "invalid grader",
        "invalid output",
        "schema",
        "permission denied",
    )
    return not any(fragment in lowered for fragment in deterministic)


Capture = Callable[[dict[str, Any], dict[str, Any]], dict[str, Any]]
Launch = Callable[[dict[str, Any], dict[str, Any]], dict[str, Any]]


def drive_phase(
    api: PaperMachineApi,
    state: dict[str, Any],
    state_path: Path,
    tasks: dict[str, dict[str, Any]],
    phase: str,
    launch: Launch,
    capture: Capture,
    poll_seconds: float,
    max_attempts: int,
    max_parallel_runs: int,
) -> None:
    failure_key = f"{phase}_failed"
    for job in state["jobs"]:
        if phase == "grade" and job.get("research_failed"):
            continue
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
            error: str | None = None
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
                    len(phase_state["attempts"]) < max_attempts
                    and is_retryable_error(error)
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
            (
                "skipped"
                if phase == "grade" and job.get("research_failed")
                else (
                    "failed"
                    if job.get(failure_key)
                    else job.get(phase, {}).get("status", "pending")
                )
            )
            for job in state["jobs"]
        )
        print(f"{phase}: {dict(counts)}", flush=True)
        if unfinished == 0:
            return
        time.sleep(poll_seconds)


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


def cancel_inflight(api: PaperMachineApi, state: dict[str, Any]) -> int:
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


def runtime_fingerprint(root: Path) -> dict[str, str]:
    return {
        relative: hashlib.sha256((root / relative).read_bytes()).hexdigest()
        for relative in RUNTIME_FILES
    }


def record_runtime_snapshot(state: dict[str, Any], root: Path) -> dict[str, str]:
    current = runtime_fingerprint(root)
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
    result = phase_state.get("result") or {}
    result_usage = result.get("usage") or {}
    for key in keys:
        total[key] += int(result_usage.get(key, 0))
    for attempt in phase_state.get("attempts") or []:
        usage = attempt.get("usage") or {}
        for key in keys:
            total[key] += int(usage.get(key, 0))
    total["uncached_input_tokens"] = max(
        0, total["input_tokens"] - total["cached_input_tokens"]
    )
    total["effective_tokens"] = total["uncached_input_tokens"] + total["output_tokens"]
    return total


def mean(values: list[float]) -> float:
    return statistics.mean(values) if values else 0.0


def aggregate(jobs: list[dict[str, Any]], condition: str) -> dict[str, Any]:
    selected = [job for job in jobs if job["condition"] == condition]
    graded = [job for job in selected if "result" in job.get("grade", {})]
    research_results = [job.get("research", {}).get("result") or {} for job in selected]
    draft_audits = [
        result["draft_audit"]
        for result in research_results
        if isinstance(result.get("draft_audit"), dict)
    ]
    research_usage = [operational_usage(job, "research") for job in selected]
    grader_usage = [operational_usage(job, "grade") for job in selected]
    total_input = sum(usage["input_tokens"] for usage in research_usage)
    total_cached = sum(usage["cached_input_tokens"] for usage in research_usage)
    transports = sum(
        (
            Counter(
                (job.get("research", {}).get("result") or {}).get(
                    "model_transports", {}
                )
            )
            for job in selected
        ),
        Counter(),
    )
    cache_modes = sum(
        (
            Counter(
                (job.get("research", {}).get("result") or {}).get(
                    "prompt_cache_modes", {}
                )
            )
            for job in selected
        ),
        Counter(),
    )
    return {
        "runs": len(selected),
        "graded": len(graded),
        "accuracy": (
            sum(job["grade"]["result"]["correct"] for job in graded) / len(selected)
            if selected
            else 0.0
        ),
        "raw_input_mean": mean([usage["input_tokens"] for usage in research_usage]),
        "effective_mean": mean([usage["effective_tokens"] for usage in research_usage]),
        "output_mean": mean([usage["output_tokens"] for usage in research_usage]),
        "cache_read_ratio": total_cached / total_input if total_input else 0.0,
        "grader_effective_mean": mean(
            [usage["effective_tokens"] for usage in grader_usage]
        ),
        "search_calls_mean": mean(
            [
                float(
                    (job.get("research", {}).get("result") or {}).get(
                        "hosted_search_calls", 0
                    )
                )
                for job in selected
            ]
        ),
        "research_failures": sum(bool(job.get("research_failed")) for job in selected),
        "grade_failures": sum(bool(job.get("grade_failed")) for job in selected),
        "draft_revisions": sum(
            audit.get("revision_performed") is True for audit in draft_audits
        ),
        "final_audit_failures": sum(
            audit.get("pass") is not True for audit in draft_audits
        ),
        "model_transports": dict(transports),
        "prompt_cache_modes": dict(cache_modes),
        "websocket_ratio": (
            transports["responses_websocket"] / sum(transports.values())
            if transports
            else 0.0
        ),
        "continuation_hits": sum(
            int(
                (job.get("research", {}).get("result") or {}).get(
                    "continuation_hits", 0
                )
            )
            for job in selected
        ),
        "fallback_steps": sum(
            sum(
                (
                    (job.get("research", {}).get("result") or {}).get(
                        "websocket_fallback_reasons", {}
                    )
                ).values()
            )
            for job in selected
        ),
        "explicit_cache_breakpoints": sum(
            int(
                (job.get("research", {}).get("result") or {}).get(
                    "explicit_cache_breakpoints", 0
                )
            )
            for job in selected
        ),
    }


def render_report(state: dict[str, Any], tasks: dict[str, dict[str, Any]]) -> str:
    conditions = list(state["experiment"]["conditions"])
    aggregates = {
        condition: aggregate(state["jobs"], condition) for condition in conditions
    }
    lines = [
        "# PaperMachine BrowseComp mini report",
        "",
        f"Generated: {utc_now()}",
        "",
        f"Research model profile: `{state['experiment']['model']}`; grader profile: `{state['experiment']['grader_model']}`.",
        "",
        "This is a pinned six-question development slice, not an official BrowseComp score. Research runs never receive reference answers. Each final response is graded afterward in a separate no-tool Session using the upstream answer-equivalence criteria.",
        "",
        "## Questions",
        "",
        "| Key | Topic | Question |",
        "|---:|---|---|",
    ]
    for key, task in tasks.items():
        question, _ = task_content(task)
        compact = " ".join(question.split()).replace("|", "\\|")
        if len(compact) > 260:
            compact = compact[:257] + "..."
        lines.append(f"| {key} | {task['problem_topic']} | {compact} |")
    lines.extend(
        [
            "",
            "## Aggregate",
            "",
            "| Condition | Runs | Graded | Accuracy | Raw input | Effective tokens | Output | Cache read | Search calls | Revised | Audit fail | Grader effective | Research failures | Grade failures |",
            "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
        ]
    )
    for condition, values in aggregates.items():
        lines.append(
            f"| {condition} | {values['runs']} | {values['graded']} | "
            f"{values['accuracy']:.1%} | {values['raw_input_mean']:.0f} | "
            f"{values['effective_mean']:.0f} | {values['output_mean']:.0f} | "
            f"{values['cache_read_ratio']:.1%} | {values['search_calls_mean']:.1f} | "
            f"{values['draft_revisions']} | {values['final_audit_failures']} | "
            f"{values['grader_effective_mean']:.0f} | {values['research_failures']} | "
            f"{values['grade_failures']} |"
        )
    lines.extend(
        [
            "",
            "## Transport and cache path",
            "",
            "| Condition | WebSocket model steps | HTTP SSE model steps | WebSocket ratio | Continuation hits | Fallback steps | Implicit-cache steps | Explicit breakpoints |",
            "|---|---:|---:|---:|---:|---:|---:|---:|",
        ]
    )
    for condition, values in aggregates.items():
        transports = values["model_transports"]
        cache_modes = values["prompt_cache_modes"]
        lines.append(
            f"| {condition} | {transports.get('responses_websocket', 0)} | "
            f"{transports.get('http_sse', 0)} | {values['websocket_ratio']:.1%} | "
            f"{values['continuation_hits']} | {values['fallback_steps']} | "
            f"{cache_modes.get('implicit', 0)} | {values['explicit_cache_breakpoints']} |"
        )
    lines.extend(
        [
            "",
            "## Per run",
            "",
            "| Task | Condition | Repeat | Correct | Raw input | Cached | Effective | Searches | Rounds | Revised | Audit |",
            "|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---|",
        ]
    )
    for job in sorted(
        state["jobs"],
        key=lambda item: (int(item["task_key"]), item["condition"], item["repeat"]),
    ):
        research = job.get("research", {}).get("result") or {}
        grade = job.get("grade", {}).get("result") or {}
        usage = operational_usage(job, "research")
        correct = "yes" if grade.get("correct") else "no"
        draft_audit = research.get("draft_audit")
        revised = (
            "yes"
            if isinstance(draft_audit, dict)
            and draft_audit.get("revision_performed") is True
            else "no"
        )
        audit_status = (
            "-"
            if not isinstance(draft_audit, dict)
            else ("pass" if draft_audit.get("pass") is True else "fail")
        )
        lines.append(
            f"| {job['task_key']} | {job['condition']} | {job['repeat']} | {correct} | "
            f"{usage['input_tokens']} | {usage['cached_input_tokens']} | "
            f"{usage['effective_tokens']} | {research.get('hosted_search_calls', 0)} | "
            f"{research.get('rounds', 1)} | {revised} | {audit_status} |"
        )
    lines.extend(
        [
            "",
            "## Validity limits",
            "",
            "- The six questions are a deterministic sample from the 1,266-task test set and are too small for leaderboard comparison.",
            "- The independent grader uses the same model family as the researchers, so judge-model bias remains possible.",
            "- Accuracy treats research or grading failures as incorrect; grader tokens are shown separately from research cost.",
            "- Hosted search results and indexed web content can change over time.",
            "- Dataset rows remain encrypted in tasks.json; plaintext questions and answers exist only in memory and local run records needed for execution/grading.",
            "",
        ]
    )
    return "\n".join(lines)


def validate_workflows(
    api: PaperMachineApi,
    project_id: str,
    required: set[str],
) -> None:
    available = {
        item["manifest"]["slug"]
        for item in api.get(f"/projects/{project_id}/workflow-programs")
    }
    missing = sorted(required - available)
    if missing:
        raise RuntimeError(
            f"server is missing workflows {missing}; restart PaperMachine"
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--api-base", default="http://127.0.0.1:4310")
    parser.add_argument("--task-keys", default="788,861,82,530,1047,995")
    parser.add_argument(
        "--conditions", default="single_agent,coverage_r1,coverage_r2"
    )
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--seed", type=int, default=20260806)
    parser.add_argument("--model", default="deepseek-flash")
    parser.add_argument("--grader-model", default="deepseek-flash")
    parser.add_argument(
        "--run-name", default="deepseek-baseline-6x3x2-2026-08-07"
    )
    parser.add_argument("--poll-seconds", type=float, default=5.0)
    parser.add_argument("--max-attempts", type=int, default=2)
    parser.add_argument("--max-parallel-runs", type=int, default=2)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--retry-terminal-failures", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.repeats < 1 or args.max_attempts < 1 or args.max_parallel_runs < 1:
        raise ValueError(
            "repeats, max-attempts, and max-parallel-runs must be positive"
        )
    root = Path(__file__).resolve().parents[2]
    snapshot = load_json(Path(__file__).with_name("tasks.json"))
    available_tasks = {str(task["key"]): task for task in snapshot["tasks"]}
    task_keys = [value.strip() for value in args.task_keys.split(",") if value.strip()]
    conditions = [
        value.strip() for value in args.conditions.split(",") if value.strip()
    ]
    unknown_tasks = sorted(set(task_keys) - available_tasks.keys())
    unknown_conditions = sorted(set(conditions) - CONDITIONS.keys())
    if unknown_tasks or unknown_conditions:
        raise ValueError(
            f"unknown tasks={unknown_tasks}, conditions={unknown_conditions}"
        )
    tasks = {key: available_tasks[key] for key in task_keys}
    jobs = build_jobs(task_keys, conditions, args.repeats, args.seed)
    run_dir = Path(__file__).with_name("runs") / args.run_name
    state_path = run_dir / "state.json"
    report_path = run_dir / "report.md"

    if state_path.exists():
        state = load_json(state_path)
        expected = state["experiment"]
        current = {
            "task_keys": task_keys,
            "conditions": conditions,
            "repeats": args.repeats,
            "seed": args.seed,
            "model": args.model,
            "grader_model": args.grader_model,
            "max_parallel_runs": args.max_parallel_runs,
        }
        for key, value in current.items():
            if expected[key] != value:
                raise ValueError(
                    f"existing run differs for {key}: {expected[key]!r} != {value!r}"
                )
    else:
        state = {
            "schema_version": 1,
            "created_at": utc_now(),
            "experiment": {
                "name": args.run_name,
                "task_keys": task_keys,
                "conditions": conditions,
                "condition_config": {key: CONDITIONS[key] for key in conditions},
                "repeats": args.repeats,
                "seed": args.seed,
                "model": args.model,
                "grader_model": args.grader_model,
                "max_parallel_runs": args.max_parallel_runs,
                "scoring_method": "upstream_browsecomp_blind_answer_equivalence_grader",
            },
            "upstream": {
                key: snapshot[key]
                for key in (
                    "source_repo",
                    "source_commit",
                    "source_file",
                    "source_file_sha256",
                    "evaluator_file",
                    "evaluator_sha256",
                    "selection",
                )
            },
            "runtime_source_sha256": runtime_fingerprint(root),
            "projects": {},
            "jobs": jobs,
        }
        save_state(state_path, state)

    record_runtime_snapshot(state, root)
    save_state(state_path, state)

    if args.retry_terminal_failures:
        reopened = reopen_terminal_failures(state)
        save_state(state_path, state)
        print(f"reopened {reopened} terminal benchmark phases", flush=True)

    if args.prepare_only:
        print(f"prepared {len(state['jobs'])} jobs in {run_dir}")
        return 0

    api = PaperMachineApi(args.api_base)
    health = api.get("/health")
    if health.get("model_mode") == "demo":
        raise RuntimeError("benchmark requires a substantive model provider")
    state["server_health"] = health
    if "research" not in state["projects"]:
        state["projects"]["research"] = ensure_project(
            api,
            f"BrowseComp research - {args.run_name}",
            "Pinned hard short-answer browsing tasks; reference answers are withheld.",
            run_dir / "projects" / "research",
        )
    if "grader" not in state["projects"]:
        state["projects"]["grader"] = ensure_project(
            api,
            f"BrowseComp graders - {args.run_name}",
            "Independent no-tool answer-equivalence grading Sessions.",
            run_dir / "projects" / "grader",
        )
    validate_workflows(
        api,
        state["projects"]["research"],
        {CONDITIONS[condition]["program_slug"] for condition in conditions},
    )
    validate_workflows(api, state["projects"]["grader"], {"short-answer-grader"})
    save_state(state_path, state)

    try:
        drive_phase(
            api,
            state,
            state_path,
            tasks,
            "research",
            lambda job, task: launch_research_run(
                api, state["projects"]["research"], job, task, args.model
            ),
            lambda job, view: capture_research_result(
                api, view, run_dir / "articles" / f"{job['key']}.txt"
            ),
            args.poll_seconds,
            args.max_attempts,
            args.max_parallel_runs,
        )
        drive_phase(
            api,
            state,
            state_path,
            tasks,
            "grade",
            lambda job, task: launch_grader_run(
                api,
                state["projects"]["grader"],
                job,
                task,
                Path(job["research"]["result"]["article_path"]).read_text(
                    encoding="utf-8"
                ),
                args.grader_model,
            ),
            lambda job, view: capture_grader_result(
                view, run_dir / "grades" / f"{job['key']}.json"
            ),
            args.poll_seconds,
            args.max_attempts,
            args.max_parallel_runs,
        )
    except KeyboardInterrupt:
        cancelled = cancel_inflight(api, state)
        save_state(state_path, state)
        print(f"cancelled {cancelled} in-flight Workflows", flush=True)
        return 130

    atomic_write_text(report_path, render_report(state, tasks))
    print(report_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
