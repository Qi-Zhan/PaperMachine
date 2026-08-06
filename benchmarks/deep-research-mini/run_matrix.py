#!/usr/bin/env python3
"""Run a resumable PaperMachine research and post-write grading matrix."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import random
import re
import statistics
import sys
import tempfile
import time
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


UPSTREAM_REPO = "Ayanami0730/deep_research_bench"
QUERY_PATH = "data/prompt_data/query.jsonl"
CRITERIA_PATH = "data/criteria_data/criteria.jsonl"
DIMENSIONS = (
    "comprehensiveness",
    "insight",
    "instruction_following",
    "readability",
)
CONDITIONS = {
    "single_agent": {
        "program_slug": "single-agent-research",
        "input": {},
    },
    "evidence_r1": {
        "program_slug": "evidence-loop",
        "input": {"route_count": 2, "max_rounds": 1, "max_followups_per_round": 2},
    },
    "evidence_r2": {
        "program_slug": "evidence-loop",
        "input": {"route_count": 2, "max_rounds": 2, "max_followups_per_round": 2},
    },
    "evidence_r3": {
        "program_slug": "evidence-loop",
        "input": {
            "route_count": 3,
            "minimum_route_count": 3,
            "max_rounds": 3,
            "max_followups_per_round": 3,
        },
    },
    "evidence_r4": {
        "program_slug": "evidence-loop",
        "input": {
            "route_count": 4,
            "minimum_route_count": 4,
            "max_rounds": 4,
            "max_followups_per_round": 4,
        },
    },
}
CONDITION_DESCRIPTIONS = {
    "single_agent": "one persistent research Session searches, reasons, and writes directly, with no evaluator or handoff",
    "evidence_r1": "at least two independent research Sessions run in parallel; a fixed evaluator assesses their evidence once; a separate writer composes the report",
    "evidence_r2": "the same graph, with up to two evaluator-directed follow-ups on the existing route Sessions before a second assessment",
    "evidence_r3": "at least three parallel evidence routes, up to three evaluator assessments, and up to three directed follow-ups per failed assessment",
    "evidence_r4": "four parallel evidence routes, up to four evaluator assessments, and up to four directed follow-ups per failed assessment",
}
RUNTIME_FILES = (
    "benchmarks/deep-research-mini/run_matrix.py",
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
    "workflows/builtin/evidence-loop/workflow.py",
    "workflows/builtin/single-agent-research/workflow.py",
    "workflows/builtin/report-grader/workflow.py",
    "papermachine.toml",
)
TERMINAL_STATUSES = {"completed", "failed", "cancelled"}
URL_RE = re.compile(r"https?://[^\s)\]>]+")


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


def atomic_write_text(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        "w",
        encoding="utf-8",
        dir=path.parent,
        delete=False,
    ) as handle:
        handle.write(content)
        temporary = Path(handle.name)
    os.replace(temporary, path)


def atomic_write_json(path: Path, value: Any) -> None:
    atomic_write_text(path, json.dumps(value, ensure_ascii=False, indent=2) + "\n")


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


def load_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def fetch_json(url: str) -> Any:
    request = urllib.request.Request(
        url, headers={"user-agent": "papermachine-benchmark/0.1"}
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        return json.loads(response.read())


def fetch_text(url: str) -> str:
    request = urllib.request.Request(
        url, headers={"user-agent": "papermachine-benchmark/0.1"}
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        return response.read().decode("utf-8")


def parse_jsonl(text: str) -> list[dict[str, Any]]:
    return [json.loads(line) for line in text.splitlines() if line.strip()]


def prepare_upstream_snapshot(
    task_ids: list[int],
    destination: Path,
    refresh: bool = False,
) -> dict[str, Any]:
    if destination.exists() and not refresh:
        snapshot = load_json(destination)
        available = {int(task["id"]) for task in snapshot["tasks"]}
        if set(task_ids).issubset(available):
            return snapshot

    commit = fetch_json(f"https://api.github.com/repos/{UPSTREAM_REPO}/commits/main")[
        "sha"
    ]
    raw_root = f"https://raw.githubusercontent.com/{UPSTREAM_REPO}/{commit}"
    query_text = fetch_text(f"{raw_root}/{QUERY_PATH}")
    criteria_text = fetch_text(f"{raw_root}/{CRITERIA_PATH}")
    query_map = {int(item["id"]): item for item in parse_jsonl(query_text)}
    criteria_map = {int(item["id"]): item for item in parse_jsonl(criteria_text)}
    missing = sorted(
        set(task_ids) - query_map.keys() | set(task_ids) - criteria_map.keys()
    )
    if missing:
        raise ValueError(f"upstream snapshot is missing task IDs: {missing}")

    tasks = []
    for task_id in task_ids:
        query = query_map[task_id]
        rubric = criteria_map[task_id]
        if query["prompt"] != rubric["prompt"]:
            raise ValueError(f"prompt mismatch for task {task_id}")
        tasks.append({**query, "rubric": rubric})
    snapshot = {
        "schema_version": 1,
        "source_repo": f"https://github.com/{UPSTREAM_REPO}",
        "source_commit": commit,
        "query_sha256": hashlib.sha256(query_text.encode()).hexdigest(),
        "criteria_sha256": hashlib.sha256(criteria_text.encode()).hexdigest(),
        "fetched_at": utc_now(),
        "tasks": tasks,
    }
    atomic_write_json(destination, snapshot)
    return snapshot


def visible_criteria(rubric: dict[str, Any]) -> dict[str, list[dict[str, str]]]:
    return {
        dimension: [
            {
                "criterion": str(item["criterion"]),
                "explanation": str(item["explanation"]),
            }
            for item in rubric["criterions"][dimension]
        ]
        for dimension in DIMENSIONS
    }


def build_jobs(
    task_ids: list[int], conditions: list[str], repeats: int, seed: int
) -> list[dict[str, Any]]:
    jobs = [
        {
            "key": f"task-{task_id}__{condition}__repeat-{repeat}",
            "task_id": task_id,
            "condition": condition,
            "repeat": repeat,
        }
        for task_id in task_ids
        for repeat in range(1, repeats + 1)
        for condition in conditions
    ]
    random.Random(seed).shuffle(jobs)
    return jobs


def ensure_project(
    api: PaperMachineApi,
    name: str,
    description: str,
    root_path: Path,
) -> str:
    canonical_root = str(root_path.resolve())
    for project in api.get("/projects"):
        if project["root_path"] == canonical_root:
            return str(project["id"])
    created = api.post(
        "/projects",
        {"name": name, "description": description, "root_path": canonical_root},
    )
    return str(created["id"])


def create_origin_session(
    api: PaperMachineApi,
    project_id: str,
    title: str,
    model: str,
) -> str:
    session = api.post(
        f"/projects/{project_id}/sessions",
        {
            "title": title,
            "instructions": "",
            "model": model,
            "enabled_skills": [],
        },
    )
    return str(session["id"])


def launch_research_run(
    api: PaperMachineApi,
    project_id: str,
    job: dict[str, Any],
    task: dict[str, Any],
    model: str,
) -> dict[str, Any]:
    condition = CONDITIONS[job["condition"]]
    title = f"T{job['task_id']} {job['condition']} repeat {job['repeat']}"
    session_id = create_origin_session(api, project_id, title, model)
    run = api.post(
        f"/projects/{project_id}/workflows",
        {
            "program_slug": condition["program_slug"],
            "objective": task["prompt"],
            "input": condition["input"],
            "started_from_session_id": session_id,
            "model": model,
            "access": "research",
            "enabled_skills": [],
        },
    )
    return {
        "session_id": session_id,
        "workflow_id": str(run["id"]),
        "launched_at": utc_now(),
    }


def token_usage(tokens: dict[str, Any]) -> dict[str, int | float]:
    input_tokens = int(tokens.get("input_tokens", 0))
    cached = int(tokens.get("cached_input_tokens", 0))
    return {
        "input_tokens": input_tokens,
        "output_tokens": int(tokens.get("output_tokens", 0)),
        "cached_input_tokens": cached,
        "cache_write_input_tokens": int(tokens.get("cache_write_input_tokens", 0)),
        "uncached_input_tokens": max(0, input_tokens - cached),
        "cache_read_ratio": cached / input_tokens if input_tokens else 0.0,
    }


def combined_token_usage(usages: list[dict[str, Any]]) -> dict[str, int | float]:
    return token_usage(
        {
            key: sum(int(usage.get(key, 0)) for usage in usages)
            for key in (
                "input_tokens",
                "output_tokens",
                "cached_input_tokens",
                "cache_write_input_tokens",
            )
        }
    )


def record_attempt_metrics(attempt: dict[str, Any], run: dict[str, Any]) -> None:
    usage = run.get("usage") or {}
    attempt["usage"] = token_usage(usage.get("tokens") or {})
    attempt["wall_time_seconds"] = int(usage.get("wall_time_seconds", 0))
    attempt["agents_created"] = int(usage.get("agents_created", 0))
    attempt["actions_completed"] = int(usage.get("actions_completed", 0))
    attempt["action_steps"] = int(usage.get("action_steps", 0))


def operational_usage(job: dict[str, Any], phase: str) -> dict[str, int | float]:
    phase_state = job[phase]
    successful = (
        phase_state["result"]["usage"] if phase == "research" else phase_state["usage"]
    )
    failed = [
        attempt["usage"]
        for attempt in phase_state.get("attempts", [])
        if attempt.get("status") != "completed" and "usage" in attempt
    ]
    invalid_completed = [
        attempt["usage"]
        for attempt in phase_state.get("attempts", [])
        if attempt.get("status") == "completed"
        and attempt.get("error")
        and "usage" in attempt
    ]
    return combined_token_usage([successful, *failed, *invalid_completed])


def operational_wall_time(job: dict[str, Any], phase: str) -> int:
    phase_state = job[phase]
    successful = (
        int(phase_state["result"]["wall_time_seconds"])
        if phase == "research"
        else int(phase_state["wall_time_seconds"])
    )
    retry_time = sum(
        int(attempt.get("wall_time_seconds", 0))
        for attempt in phase_state.get("attempts", [])
        if attempt.get("error")
    )
    return successful + retry_time


def capture_per_agent(
    api: PaperMachineApi, view: dict[str, Any]
) -> list[dict[str, Any]]:
    agents = []
    for session in view.get("sessions", []):
        session_view = api.get(f"/sessions/{session['id']}")
        turns = session_view.get("turns", [])
        steps = session_view.get("steps", [])
        usage = {
            key: sum(int(turn.get("usage", {}).get(key, 0)) for turn in turns)
            for key in (
                "input_tokens",
                "output_tokens",
                "cached_input_tokens",
                "cache_write_input_tokens",
            )
        }
        agents.append(
            {
                "name": session_view["session"]["title"],
                "session_id": session["id"],
                "turns": len(turns),
                "steps": len(steps),
                **token_usage(usage),
                **model_step_metadata(steps),
            }
        )
    return agents


def model_step_metadata(steps: list[dict[str, Any]]) -> dict[str, Any]:
    counters = {
        "model_transports": Counter(),
        "prompt_cache_modes": Counter(),
        "model_profiles": Counter(),
        "model_providers": Counter(),
        "upstream_models": Counter(),
        "websocket_fallback_reasons": Counter(),
    }
    field_map = {
        "transport": "model_transports",
        "prompt_cache_mode": "prompt_cache_modes",
        "model_profile": "model_profiles",
        "provider": "model_providers",
        "upstream_model": "upstream_models",
        "websocket_fallback_reason": "websocket_fallback_reasons",
    }
    continuation_hits = 0
    continuation_misses = Counter()
    cache_keys = set()
    for step in steps:
        if step.get("kind") != "model":
            continue
        metadata = (step.get("output") or {}).get("request") or {}
        for source, target in field_map.items():
            if metadata.get(source):
                counters[target][str(metadata[source])] += 1
        if metadata.get("used_previous_response_id"):
            continuation_hits += 1
        elif metadata.get("continuation_miss_reason"):
            continuation_misses[str(metadata["continuation_miss_reason"])] += 1
        if metadata.get("prompt_cache_key"):
            cache_keys.add(str(metadata["prompt_cache_key"]))
    return {
        **{key: dict(value) for key, value in counters.items()},
        "continuation_hits": continuation_hits,
        "continuation_misses": dict(continuation_misses),
        "prompt_cache_keys": sorted(cache_keys),
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
    usage = run.get("usage") or {}
    agents = capture_per_agent(api, view)
    metadata_keys = (
        "model_transports",
        "prompt_cache_modes",
        "model_profiles",
        "model_providers",
        "upstream_models",
        "websocket_fallback_reasons",
        "continuation_misses",
    )
    result = {
        "status": run["status"],
        "workflow_sha256": run["program"]["sha256"],
        "created_at": run["created_at"],
        "completed_at": run["updated_at"],
        "wall_time_seconds": int(usage.get("wall_time_seconds", 0)),
        "agents_created": int(usage.get("agents_created", 0)),
        "actions_completed": int(usage.get("actions_completed", 0)),
        "action_steps": int(usage.get("action_steps", 0)),
        "report_characters": len(report),
        "report_sha256": hashlib.sha256(report.encode()).hexdigest(),
        "unique_direct_urls": len(
            {match.rstrip(".,;:") for match in URL_RE.findall(report)}
        ),
        "article_path": str(article_path),
        "usage": token_usage(usage.get("tokens") or {}),
        "per_agent": agents,
        **{
            key: dict(sum((Counter(agent[key]) for agent in agents), Counter()))
            for key in metadata_keys
        },
        "continuation_hits": sum(agent["continuation_hits"] for agent in agents),
    }
    if "rounds" in output:
        result["rounds"] = int(output["rounds"])
    if isinstance(output.get("evaluation"), dict):
        result["internal_evaluation"] = output["evaluation"]
    if isinstance(output.get("draft_audit"), dict):
        result["draft_audit"] = output["draft_audit"]
    if isinstance(output.get("evidence_ledger"), list):
        result["evidence_packets"] = len(output["evidence_ledger"])
    return result


def validate_and_score_grading(
    grading: dict[str, Any],
    rubric: dict[str, Any],
) -> dict[str, Any]:
    dimension_scores: dict[str, float] = {}
    normalized: dict[str, list[dict[str, Any]]] = {}
    for dimension in DIMENSIONS:
        expected = rubric["criterions"][dimension]
        ratings = grading.get(dimension)
        if not isinstance(ratings, list) or len(ratings) != len(expected):
            raise ValueError(
                f"{dimension} expected {len(expected)} ratings, got "
                f"{len(ratings) if isinstance(ratings, list) else type(ratings).__name__}"
            )
        indexed: dict[int, dict[str, Any]] = {}
        for rating in ratings:
            if not isinstance(rating, dict):
                raise ValueError(f"{dimension} contains a non-object rating")
            index = rating.get("criterion_index")
            score = rating.get("score")
            if isinstance(index, bool) or not isinstance(index, int):
                raise ValueError(f"{dimension} contains an invalid criterion_index")
            if isinstance(score, bool) or not isinstance(score, (int, float)):
                raise ValueError(f"{dimension}[{index}] contains a non-numeric score")
            if not 0 <= float(score) <= 10 or not math.isfinite(float(score)):
                raise ValueError(f"{dimension}[{index}] score is outside 0..10")
            if index in indexed:
                raise ValueError(f"{dimension} repeats criterion_index {index}")
            indexed[index] = {**rating, "score": float(score)}
        if set(indexed) != set(range(len(expected))):
            raise ValueError(f"{dimension} criterion indices are incomplete")
        ordered = [indexed[index] for index in range(len(expected))]
        total_weight = sum(float(item["weight"]) for item in expected)
        if total_weight <= 0:
            raise ValueError(f"{dimension} has no positive criterion weight")
        dimension_scores[dimension] = (
            sum(
                rating["score"] * float(criterion["weight"])
                for rating, criterion in zip(ordered, expected)
            )
            / total_weight
        )
        normalized[dimension] = ordered

    weights = rubric["dimension_weight"]
    weight_total = sum(float(weights[dimension]) for dimension in DIMENSIONS)
    overall_10 = (
        sum(
            dimension_scores[dimension] * float(weights[dimension])
            for dimension in DIMENSIONS
        )
        / weight_total
    )
    return {
        "overall_score_100": overall_10 * 10,
        "dimension_scores_100": {
            dimension: score * 10 for dimension, score in dimension_scores.items()
        },
        "ratings": normalized,
        "overall_assessment": str(grading.get("overall_assessment", "")),
        "major_weaknesses": grading.get("major_weaknesses", []),
    }


def launch_grader_run(
    api: PaperMachineApi,
    project_id: str,
    job: dict[str, Any],
    task: dict[str, Any],
    report: str,
    model: str,
) -> dict[str, Any]:
    title = f"Grade T{job['task_id']} {job['condition']} repeat {job['repeat']}"
    session_id = create_origin_session(api, project_id, title, model)
    run = api.post(
        f"/projects/{project_id}/workflows",
        {
            "program_slug": "report-grader",
            "objective": "Blindly grade the supplied final report against the full external rubric.",
            "input": {
                "question": task["prompt"],
                "report": report,
                "criteria": visible_criteria(task["rubric"]),
                "language": task["language"],
                "grader_model": model,
            },
            "started_from_session_id": session_id,
            "model": model,
            "access": "model_only",
            "enabled_skills": [],
        },
    )
    return {
        "session_id": session_id,
        "workflow_id": str(run["id"]),
        "launched_at": utc_now(),
    }


def aggregate_condition(jobs: list[dict[str, Any]], condition: str) -> dict[str, Any]:
    selected = [
        job
        for job in jobs
        if job["condition"] == condition
        and "result" in job.get("research", {})
        and "score" in job.get("grade", {})
    ]
    scores = [float(job["grade"]["score"]["overall_score_100"]) for job in selected]
    draft_audits = [
        job["research"]["result"]["draft_audit"]
        for job in selected
        if isinstance(job["research"]["result"].get("draft_audit"), dict)
    ]
    successful_usage = [job["research"]["result"]["usage"] for job in selected]
    research_usage = [operational_usage(job, "research") for job in selected]
    grader_usage = [operational_usage(job, "grade") for job in selected]
    total_input = sum(int(usage["input_tokens"]) for usage in research_usage)
    total_cached = sum(int(usage["cached_input_tokens"]) for usage in research_usage)
    grader_input = sum(int(usage["input_tokens"]) for usage in grader_usage)
    grader_cached = sum(int(usage["cached_input_tokens"]) for usage in grader_usage)
    return {
        "runs": len(selected),
        "score_mean": statistics.mean(scores) if scores else 0.0,
        "score_stdev": statistics.stdev(scores) if len(scores) > 1 else 0.0,
        "successful_input_mean": (
            statistics.mean(int(usage["input_tokens"]) for usage in successful_usage)
            if successful_usage
            else 0.0
        ),
        "successful_uncached_mean": (
            statistics.mean(
                int(usage["uncached_input_tokens"]) for usage in successful_usage
            )
            if successful_usage
            else 0.0
        ),
        "input_mean": (
            statistics.mean(int(usage["input_tokens"]) for usage in research_usage)
            if research_usage
            else 0.0
        ),
        "uncached_input_mean": (
            statistics.mean(
                int(usage["uncached_input_tokens"]) for usage in research_usage
            )
            if research_usage
            else 0.0
        ),
        "output_mean": (
            statistics.mean(int(usage["output_tokens"]) for usage in research_usage)
            if research_usage
            else 0.0
        ),
        "wall_time_mean": (
            statistics.mean(operational_wall_time(job, "research") for job in selected)
            if selected
            else 0.0
        ),
        "cache_read_ratio": total_cached / total_input if total_input else 0.0,
        "report_characters_mean": (
            statistics.mean(
                int(job["research"]["result"]["report_characters"]) for job in selected
            )
            if selected
            else 0.0
        ),
        "direct_urls_mean": (
            statistics.mean(
                int(job["research"]["result"]["unique_direct_urls"]) for job in selected
            )
            if selected
            else 0.0
        ),
        "draft_revisions": sum(
            audit.get("revision_performed") is True for audit in draft_audits
        ),
        "final_audit_failures": sum(
            audit.get("pass") is not True for audit in draft_audits
        ),
        "dimension_means": {
            dimension: (
                statistics.mean(
                    float(job["grade"]["score"]["dimension_scores_100"][dimension])
                    for job in selected
                )
                if selected
                else 0.0
            )
            for dimension in DIMENSIONS
        },
        "grader_input_mean": (
            statistics.mean(int(usage["input_tokens"]) for usage in grader_usage)
            if grader_usage
            else 0.0
        ),
        "grader_output_mean": (
            statistics.mean(int(usage["output_tokens"]) for usage in grader_usage)
            if grader_usage
            else 0.0
        ),
        "grader_cache_read_ratio": (
            grader_cached / grader_input if grader_input else 0.0
        ),
        "grader_alternate_shapes": sum(
            job["grade"].get("contract", {}).get("alternate_shape_normalized") is True
            for job in selected
        ),
        "grader_semantic_repairs": sum(
            int(job["grade"].get("contract", {}).get("semantic_repair_attempts", 0))
            for job in selected
        ),
    }


def mean(values: list[float]) -> float:
    return statistics.mean(values) if values else 0.0


def compact_error(error: str) -> str:
    if "token budget exceeded" in error.lower():
        return "token_budget_exceeded"
    if "stream_read_error" in error:
        return "stream_read_error"
    if "Broken pipe" in error:
        return "broken_pipe"
    return error.strip().splitlines()[-1][:120]


def is_retryable_error(error: str) -> bool:
    return "budget exceeded" not in error.lower()


def render_report(state: dict[str, Any], tasks: dict[int, dict[str, Any]]) -> str:
    stored_conditions = state["experiment"]["conditions"]
    conditions = list(stored_conditions)
    jobs = [
        job
        for job in state["jobs"]
        if "result" in job.get("research", {}) and "score" in job.get("grade", {})
    ]
    aggregates = {
        condition: aggregate_condition(jobs, condition) for condition in conditions
    }
    lines = [
        "# PaperMachine auto-research benchmark pilot",
        "",
        f"Generated: {utc_now()}",
        "",
        "## Experiment",
        "",
        f"- Tasks: {len(tasks)} ({', '.join(str(task_id) for task_id in tasks)})",
        f"- Research runs: {len(jobs)}",
        f"- Repeats per task and condition: {state['experiment']['repeats']}",
        f"- Research model: `{state['experiment']['model']}`",
        f"- Grader model: `{state['experiment']['grader_model']}` in an independent no-tool Session",
        f"- Upstream commit: `{state['upstream']['source_commit']}`",
        "- Score: absolute point-wise post-write score using every upstream criterion and its two-level weights; this is not the upstream reference-normalized official RACE score.",
        "- Research agents received only the original question. The full rubric was visible only to the post-write grader.",
        "",
        "## Conditions",
        "",
    ]
    lines.extend(
        f"- `{condition}`: {CONDITION_DESCRIPTIONS[condition]}."
        for condition in conditions
    )
    lines.extend(
        [
            "- `report-grader`: a separate no-tool Session sees the question, final report, and every upstream criterion, but not the condition, internal evaluator result, or rubric weights. Python validates all criterion indices and applies criterion and dimension weights.",
            "",
            "## Questions",
            "",
            "| ID | Topic | Lang | Question |",
            "|---:|---|---|---|",
        ]
    )
    for task_id, task in tasks.items():
        question = str(task["prompt"]).replace("|", "\\|").replace("\n", " ")
        lines.append(
            f"| {task_id} | {task['topic']} | {task['language']} | {question} |"
        )

    lines.extend(
        [
            "",
            "## Aggregate results",
            "",
            "| Condition | Runs | Score mean | Score SD | Successful input mean | Operational input mean | Operational uncached mean | Operational output mean | Cache read | Operational wall mean | Final report chars | Direct URLs | Revised | Audit fail |",
            "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
        ]
    )
    for condition, aggregate in aggregates.items():
        lines.append(
            f"| {condition} | {aggregate['runs']} | {aggregate['score_mean']:.2f} | "
            f"{aggregate['score_stdev']:.2f} | {aggregate['successful_input_mean']:.0f} | "
            f"{aggregate['input_mean']:.0f} | "
            f"{aggregate['uncached_input_mean']:.0f} | {aggregate['output_mean']:.0f} | "
            f"{aggregate['cache_read_ratio']:.1%} | {aggregate['wall_time_mean']:.0f}s | "
            f"{aggregate['report_characters_mean']:.0f} | {aggregate['direct_urls_mean']:.1f} | "
            f"{aggregate['draft_revisions']} | {aggregate['final_audit_failures']} |"
        )

    lines.extend(
        [
            "",
            "## Provider and cache path",
            "",
            "| Condition | Profiles | Providers | Upstream models | Transports | Cache modes | Continuation hits |",
            "|---|---|---|---|---|---|---:|",
        ]
    )
    for condition in conditions:
        results = [
            job["research"]["result"] for job in jobs if job["condition"] == condition
        ]
        combined = {
            key: dict(
                sum((Counter(result.get(key, {})) for result in results), Counter())
            )
            for key in (
                "model_profiles",
                "model_providers",
                "upstream_models",
                "model_transports",
                "prompt_cache_modes",
            )
        }
        lines.append(
            f"| {condition} | {json.dumps(combined['model_profiles'], sort_keys=True)} | "
            f"{json.dumps(combined['model_providers'], sort_keys=True)} | "
            f"{json.dumps(combined['upstream_models'], sort_keys=True)} | "
            f"{json.dumps(combined['model_transports'], sort_keys=True)} | "
            f"{json.dumps(combined['prompt_cache_modes'], sort_keys=True)} | "
            f"{sum(int(result.get('continuation_hits', 0)) for result in results)} |"
        )

    lines.extend(
        [
            "",
            "## Rubric dimensions",
            "",
            "| Condition | Comprehensiveness | Insight | Instruction following | Readability |",
            "|---|---:|---:|---:|---:|",
        ]
    )
    for condition, aggregate in aggregates.items():
        dimensions = aggregate["dimension_means"]
        lines.append(
            f"| {condition} | {dimensions['comprehensiveness']:.2f} | "
            f"{dimensions['insight']:.2f} | {dimensions['instruction_following']:.2f} | "
            f"{dimensions['readability']:.2f} |"
        )

    lines.extend(
        [
            "",
            "Post-write grader usage is reported separately and excluded from workflow cost comparisons.",
            "",
            "| Condition graded | Grader input mean | Grader output mean | Grader cache read | Alternate shape | Semantic repairs |",
            "|---|---:|---:|---:|---:|---:|",
        ]
    )
    for condition, aggregate in aggregates.items():
        lines.append(
            f"| {condition} | {aggregate['grader_input_mean']:.0f} | "
            f"{aggregate['grader_output_mean']:.0f} | "
            f"{aggregate['grader_cache_read_ratio']:.1%} | "
            f"{aggregate['grader_alternate_shapes']} | "
            f"{aggregate['grader_semantic_repairs']} |"
        )

    lines.extend(
        [
            "",
            "## Per-task post-write score",
            "",
            "| Task | " + " | ".join(conditions) + " |",
            "|---:|" + "---:|" * len(conditions),
        ]
    )
    per_task: dict[int, dict[str, list[float]]] = defaultdict(lambda: defaultdict(list))
    for job in jobs:
        per_task[int(job["task_id"])][job["condition"]].append(
            float(job["grade"]["score"]["overall_score_100"])
        )
    for task_id in tasks:
        values = [mean(per_task[task_id][condition]) for condition in conditions]
        lines.append(
            f"| {task_id} | " + " | ".join(f"{value:.2f}" for value in values) + " |"
        )

    by_pair = {(job["task_id"], job["repeat"], job["condition"]): job for job in jobs}
    lines.extend(
        [
            "",
            "## Paired deltas",
            "",
        ]
    )
    for left, right in zip(conditions, conditions[1:]):
        score_deltas: list[float] = []
        uncached_deltas: list[float] = []
        for task_id in tasks:
            for repeat in range(1, int(state["experiment"]["repeats"]) + 1):
                left_job = by_pair.get((task_id, repeat, left))
                right_job = by_pair.get((task_id, repeat, right))
                if left_job is None or right_job is None:
                    continue
                score_deltas.append(
                    float(right_job["grade"]["score"]["overall_score_100"])
                    - float(left_job["grade"]["score"]["overall_score_100"])
                )
                uncached_deltas.append(
                    operational_usage(right_job, "research")["uncached_input_tokens"]
                    - operational_usage(left_job, "research")["uncached_input_tokens"]
                )
        lines.append(
            f"- `{right} - {left}`: {mean(score_deltas):+.2f} score points and "
            f"{mean(uncached_deltas):+.0f} uncached tokens across {len(score_deltas)} complete "
            f"pairs; `{right}` won {sum(value > 0 for value in score_deltas)}/{len(score_deltas)}."
        )
    if len(conditions) < 2:
        lines.append(
            "- Only one condition was selected, so there is no paired comparison."
        )
    for condition in conditions:
        if not condition.startswith("evidence_"):
            continue
        condition_jobs = [job for job in jobs if job["condition"] == condition]
        followup_runs = sum(
            int(job["research"]["result"].get("rounds", 1)) > 1
            for job in condition_jobs
        )
        lines.append(
            f"- `{condition}` opened at least one evaluator follow-up round in "
            f"{followup_runs}/{len(condition_jobs)} completed runs."
        )
    lines.extend(
        [
            "",
            "## Repeat stability",
            "",
            "| Task | Condition | Score repeat 1 | Score repeat 2 | Absolute gap |",
            "|---:|---|---:|---:|---:|",
        ]
    )
    for task_id in tasks:
        for condition in conditions:
            scores = sorted(
                (
                    int(job["repeat"]),
                    float(job["grade"]["score"]["overall_score_100"]),
                )
                for job in jobs
                if int(job["task_id"]) == task_id and job["condition"] == condition
            )
            if len(scores) >= 2:
                lines.append(
                    f"| {task_id} | {condition} | {scores[0][1]:.2f} | {scores[1][1]:.2f} | "
                    f"{abs(scores[0][1] - scores[1][1]):.2f} |"
                )

    repeat_gaps: dict[str, list[float]] = defaultdict(list)
    for task_id in tasks:
        for condition in conditions:
            values = sorted(
                float(job["grade"]["score"]["overall_score_100"])
                for job in jobs
                if int(job["task_id"]) == task_id and job["condition"] == condition
            )
            if len(values) >= 2:
                repeat_gaps[condition].append(abs(values[0] - values[1]))
    lines.extend(
        [
            "",
            "Mean absolute repeat gap: "
            + "; ".join(
                f"{condition} {mean(repeat_gaps[condition]):.2f}"
                for condition in conditions
            )
            + ".",
        ]
    )

    research_failures = [job for job in state["jobs"] if job.get("research_failed")]
    grade_failures = [job for job in state["jobs"] if job.get("grade_failed")]
    research_retries = sum(
        max(0, len(job.get("research", {}).get("attempts", [])) - 1)
        for job in state["jobs"]
    )
    grade_retries = sum(
        max(0, len(job.get("grade", {}).get("attempts", [])) - 1)
        for job in state["jobs"]
    )
    failed_attempts = [
        (job, attempt)
        for job in state["jobs"]
        for attempt in job.get("research", {}).get("attempts", [])
        if attempt.get("error")
    ]
    lines.extend(
        [
            "",
            "## Operational retries",
            "",
            "| Job | Error | Input | Cached input | Output | Wall | Agents | Steps |",
            "|---|---|---:|---:|---:|---:|---:|---:|",
        ]
    )
    for job, attempt in failed_attempts:
        usage = attempt.get("usage") or token_usage({})
        lines.append(
            f"| {job['key']} | {compact_error(str(attempt['error']))} | "
            f"{usage['input_tokens']} | {usage['cached_input_tokens']} | "
            f"{usage['output_tokens']} | {attempt.get('wall_time_seconds', 0)}s | "
            f"{attempt.get('agents_created', 0)} | {attempt.get('action_steps', 0)} |"
        )

    best_condition = max(
        conditions, key=lambda condition: aggregates[condition]["score_mean"]
    )
    baseline = conditions[0]
    lines.extend(
        [
            "",
            "## Observed result",
            "",
            f"- `{best_condition}` had the highest mean post-write score "
            f"({aggregates[best_condition]['score_mean']:.2f}) among the selected conditions.",
        ]
    )
    for condition in conditions[1:]:
        lines.append(
            f"- `{condition}` versus `{baseline}`: "
            f"{aggregates[condition]['score_mean'] - aggregates[baseline]['score_mean']:+.2f} "
            f"score points and "
            f"{aggregates[condition]['uncached_input_mean'] - aggregates[baseline]['uncached_input_mean']:+.0f} "
            "mean operational uncached input tokens."
        )
    lines.extend(
        [
            "",
            "## Validity limits",
            "",
            f"- {len(tasks)} tasks and {state['experiment']['repeats']} repeats are an experiment, not a definitive benchmark ranking.",
            "- The point-wise grader is independent by Session and prompt, but currently uses the same model family as the research runs.",
            "- This experiment does not run the upstream reference-relative RACE judge or FACT citation scraper, so its absolute scores are not official DeepResearch Bench scores.",
            "- Prompt cache keys are stable routing-affinity keys scoped to one Session and model. They remain stable across that Session's turns, tools, schemas, and compaction, while provider cache reuse still requires a matching prompt prefix; cache telemetry is reported separately from Responses continuation hits.",
            "- Token and wall-time comparisons use operational totals, including failed or invalid attempts before a successful retry.",
            "- Web content can change during the experiment; deterministic ordering reduces but cannot remove temporal drift.",
            f"- Terminal research failures: {len(research_failures)}; terminal grading failures: {len(grade_failures)}.",
            f"- Research retries: {research_retries}; grading retries: {grade_retries}.",
            "",
        ]
    )
    return "\n".join(lines)


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


def status_counts(state: dict[str, Any], phase: str) -> Counter[str]:
    statuses = Counter()
    for job in state["jobs"]:
        if phase in job:
            statuses[str(job[phase].get("status", "launched"))] += 1
        else:
            statuses["pending"] += 1
    return statuses


def run_research_phase(
    api: PaperMachineApi,
    state: dict[str, Any],
    state_path: Path,
    tasks: dict[int, dict[str, Any]],
    articles_dir: Path,
    project_id: str,
    model: str,
    poll_seconds: float,
    max_attempts: int,
    max_parallel_runs: int,
) -> None:
    for job in state["jobs"]:
        job.setdefault("research", {"attempts": [], "status": "pending"})

    while True:
        active = 0
        for job in state["jobs"]:
            research = job["research"]
            if "result" in research or job.get("research_failed"):
                continue
            if not research["attempts"] or research.get("status") == "pending_retry":
                continue
            attempt = research["attempts"][-1]
            view = api.get(f"/workflows/{attempt['workflow_id']}")
            status = str(view["workflow"]["status"])
            research["status"] = status
            attempt["status"] = status
            if status in {"created", "running", "paused"}:
                active += 1
                continue
            if status == "completed":
                try:
                    article_path = articles_dir / f"{job['key']}.md"
                    research["result"] = capture_research_result(
                        api, view, article_path
                    )
                except (KeyError, TypeError, ValueError) as error:
                    status = "invalid_output"
                    attempt["error"] = f"invalid research output: {error}"
                else:
                    research["status"] = "completed"
                    save_state(state_path, state)
                    continue
            if status in {"failed", "cancelled", "invalid_output"}:
                attempt["error"] = str(
                    attempt.get("error") or view["workflow"].get("error") or status
                )
                record_attempt_metrics(attempt, view["workflow"])
                if attempt.get("cancelled_by_runner") or (
                    len(research["attempts"]) < max_attempts
                    and is_retryable_error(attempt["error"])
                ):
                    research["status"] = "pending_retry"
                else:
                    job["research_failed"] = True
                save_state(state_path, state)

        for job in state["jobs"]:
            if active >= max_parallel_runs:
                break
            research = job["research"]
            if "result" in research or job.get("research_failed"):
                continue
            if research["attempts"] and research.get("status") != "pending_retry":
                continue
            attempt = launch_research_run(
                api,
                project_id,
                job,
                tasks[int(job["task_id"])],
                model,
            )
            attempt["runtime_source_sha256"] = state.get(
                "current_runtime_source_sha256", {}
            )
            research["attempts"].append(attempt)
            research["status"] = "created"
            active += 1
            save_state(state_path, state)

        unfinished = sum(
            "result" not in job["research"] and not job.get("research_failed")
            for job in state["jobs"]
        )
        print(f"research: {dict(status_counts(state, 'research'))}", flush=True)
        if unfinished == 0:
            return
        time.sleep(poll_seconds)


def run_grading_phase(
    api: PaperMachineApi,
    state: dict[str, Any],
    state_path: Path,
    tasks: dict[int, dict[str, Any]],
    grades_dir: Path,
    project_id: str,
    model: str,
    poll_seconds: float,
    max_attempts: int,
    max_parallel_runs: int,
) -> None:
    for job in state["jobs"]:
        if job.get("research_failed"):
            continue
        job.setdefault("grade", {"attempts": [], "status": "pending"})

    while True:
        active = 0
        for job in state["jobs"]:
            if job.get("research_failed"):
                continue
            grade = job["grade"]
            if "score" in grade or job.get("grade_failed"):
                continue
            if not grade["attempts"] or grade.get("status") == "pending_retry":
                continue
            attempt = grade["attempts"][-1]
            view = api.get(f"/workflows/{attempt['workflow_id']}")
            run = view["workflow"]
            status = str(run["status"])
            grade["status"] = status
            attempt["status"] = status
            if status in {"created", "running", "paused"}:
                active += 1
                continue
            retry_error = None
            if status == "completed":
                try:
                    grading = run["output"]["grading"]
                    score = validate_and_score_grading(
                        grading,
                        tasks[int(job["task_id"])]["rubric"],
                    )
                except (KeyError, TypeError, ValueError) as error:
                    retry_error = f"invalid grader output: {error}"
                else:
                    usage = run.get("usage") or {}
                    grade["score"] = score
                    grade["usage"] = token_usage(usage.get("tokens") or {})
                    grade["wall_time_seconds"] = int(usage.get("wall_time_seconds", 0))
                    grade["workflow_sha256"] = run["program"]["sha256"]
                    contract = run["output"].get("contract")
                    if isinstance(contract, dict):
                        grade["contract"] = contract
                    grade["status"] = "completed"
                    atomic_write_json(grades_dir / f"{job['key']}.json", grading)
                    save_state(state_path, state)
                    continue
            elif status in {"failed", "cancelled"}:
                retry_error = str(run.get("error") or status)

            if retry_error is not None:
                attempt["error"] = retry_error
                record_attempt_metrics(attempt, run)
                if attempt.get("cancelled_by_runner") or (
                    len(grade["attempts"]) < max_attempts
                    and is_retryable_error(retry_error)
                ):
                    grade["status"] = "pending_retry"
                else:
                    grade["status"] = (
                        "invalid_output"
                        if retry_error.startswith("invalid grader output:")
                        else status
                    )
                    job["grade_failed"] = True
                save_state(state_path, state)

        for job in state["jobs"]:
            if active >= max_parallel_runs:
                break
            if job.get("research_failed"):
                continue
            grade = job["grade"]
            if "score" in grade or job.get("grade_failed"):
                continue
            if grade["attempts"] and grade.get("status") != "pending_retry":
                continue
            report = Path(job["research"]["result"]["article_path"]).read_text(
                encoding="utf-8"
            )
            attempt = launch_grader_run(
                api,
                project_id,
                job,
                tasks[int(job["task_id"])],
                report,
                model,
            )
            attempt["runtime_source_sha256"] = state.get(
                "current_runtime_source_sha256", {}
            )
            grade["attempts"].append(attempt)
            grade["status"] = "created"
            active += 1
            save_state(state_path, state)

        unfinished = sum(
            not job.get("research_failed")
            and "score" not in job["grade"]
            and not job.get("grade_failed")
            for job in state["jobs"]
        )
        print(f"grading: {dict(status_counts(state, 'grade'))}", flush=True)
        if unfinished == 0:
            return
        time.sleep(poll_seconds)


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
            f"PaperMachine server is missing workflows: {missing}; restart it"
        )


def backfill_retry_metrics(api: PaperMachineApi, state: dict[str, Any]) -> bool:
    changed = False
    for job in state["jobs"]:
        for phase in ("research", "grade"):
            for attempt in job.get(phase, {}).get("attempts", []):
                if not attempt.get("error") or all(
                    key in attempt
                    for key in (
                        "usage",
                        "wall_time_seconds",
                        "agents_created",
                        "actions_completed",
                        "action_steps",
                    )
                ):
                    continue
                view = api.get(f"/workflows/{attempt['workflow_id']}")
                record_attempt_metrics(attempt, view["workflow"])
                changed = True
    return changed


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--api-base", default="http://127.0.0.1:4310")
    parser.add_argument("--task-ids", default="19,59,66,68,69")
    parser.add_argument("--conditions", default=",".join(CONDITIONS))
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--seed", type=int, default=20260805)
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--grader-model", default="gpt-5.6-sol")
    parser.add_argument("--run-name", default="pilot-5x3x2-2026-08-05")
    parser.add_argument("--poll-seconds", type=float, default=10.0)
    parser.add_argument("--max-attempts", type=int, default=3)
    parser.add_argument("--max-parallel-runs", type=int, default=2)
    parser.add_argument("--refresh-upstream", action="store_true")
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--retry-terminal-failures", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.repeats < 2:
        raise ValueError("repeats must be at least 2")
    if args.max_attempts < 1 or args.max_parallel_runs < 1:
        raise ValueError("max-attempts and max-parallel-runs must be positive")
    task_ids = [
        int(value.strip()) for value in args.task_ids.split(",") if value.strip()
    ]
    conditions = [
        value.strip() for value in args.conditions.split(",") if value.strip()
    ]
    unknown_conditions = sorted(set(conditions) - CONDITIONS.keys())
    if not conditions or unknown_conditions:
        raise ValueError(
            f"conditions must be non-empty and known; unknown={unknown_conditions}"
        )
    if len(task_ids) < 4:
        raise ValueError("the matrix must contain more than three tasks")

    benchmark_root = Path(__file__).resolve().parent
    repository_root = Path(__file__).resolve().parents[2]
    run_dir = benchmark_root / "runs" / args.run_name
    snapshot_path = run_dir / "upstream-snapshot.json"
    state_path = run_dir / "state.json"
    report_path = run_dir / "report.md"
    snapshot = prepare_upstream_snapshot(task_ids, snapshot_path, args.refresh_upstream)
    tasks = {
        int(task["id"]): task
        for task in snapshot["tasks"]
        if int(task["id"]) in task_ids
    }
    jobs = build_jobs(task_ids, conditions, args.repeats, args.seed)

    if state_path.exists():
        state = load_json(state_path)
        expected = state["experiment"]
        current = {
            "task_ids": task_ids,
            "conditions": conditions,
            "repeats": args.repeats,
            "seed": args.seed,
            "model": args.model,
            "grader_model": args.grader_model,
            "max_parallel_runs": args.max_parallel_runs,
        }
        for key, value in current.items():
            expected_value = expected[key]
            if key == "conditions" and isinstance(expected_value, dict):
                expected_value = list(expected_value)
            if expected_value != value:
                raise ValueError(
                    f"existing run differs for {key}: {expected_value!r} != {value!r}"
                )
    else:
        state = {
            "schema_version": 1,
            "created_at": utc_now(),
            "experiment": {
                "name": args.run_name,
                "task_ids": task_ids,
                "conditions": conditions,
                "condition_config": {key: CONDITIONS[key] for key in conditions},
                "repeats": args.repeats,
                "seed": args.seed,
                "model": args.model,
                "grader_model": args.grader_model,
                "max_parallel_runs": args.max_parallel_runs,
                "scoring_method": "absolute_pointwise_full_upstream_rubric",
            },
            "upstream": {
                key: snapshot[key]
                for key in (
                    "source_repo",
                    "source_commit",
                    "query_sha256",
                    "criteria_sha256",
                    "fetched_at",
                )
            },
            "runtime_source_sha256": runtime_fingerprint(repository_root),
            "projects": {},
            "jobs": jobs,
        }
        save_state(state_path, state)

    record_runtime_snapshot(state, repository_root)
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
    if "server_health" not in state:
        state["server_health"] = health
        save_state(state_path, state)
    if backfill_retry_metrics(api, state):
        save_state(state_path, state)

    if "research" not in state["projects"]:
        state["projects"]["research"] = ensure_project(
            api,
            f"Benchmark research - {args.run_name}",
            "Controlled research runs for the auto-research benchmark matrix.",
            run_dir / "projects" / "research",
        )
    if "grader" not in state["projects"]:
        state["projects"]["grader"] = ensure_project(
            api,
            f"Benchmark graders - {args.run_name}",
            "Independent post-write grading Sessions using the full upstream rubric.",
            run_dir / "projects" / "grader",
        )
    validate_workflows(
        api,
        state["projects"]["research"],
        {CONDITIONS[condition]["program_slug"] for condition in conditions},
    )
    validate_workflows(api, state["projects"]["grader"], {"report-grader"})
    save_state(state_path, state)

    try:
        run_research_phase(
            api,
            state,
            state_path,
            tasks,
            run_dir / "articles",
            state["projects"]["research"],
            args.model,
            args.poll_seconds,
            args.max_attempts,
            args.max_parallel_runs,
        )
        run_grading_phase(
            api,
            state,
            state_path,
            tasks,
            run_dir / "grades",
            state["projects"]["grader"],
            args.grader_model,
            args.poll_seconds,
            args.max_attempts,
            args.max_parallel_runs,
        )
    except KeyboardInterrupt:
        cancelled = cancel_inflight(api, state)
        save_state(state_path, state)
        print(f"cancelled {cancelled} in-flight Workflows", file=sys.stderr)
        return 130
    report = render_report(state, tasks)
    atomic_write_text(report_path, report)
    print(f"report: {report_path}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("interrupted; rerun the same command to resume", file=sys.stderr)
        raise SystemExit(130)
