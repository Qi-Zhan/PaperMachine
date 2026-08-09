#!/usr/bin/env python3
"""Run a resumable PaperMachine matrix on a pinned LiveDRBench slice.

Research never receives the encrypted references. Every parseable final answer
is graded afterward in a separate no-tool Session using the upstream semantic
claim-matching rubric. A strict deterministic score remains as a cheap diagnostic.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import sys
import unicodedata
from collections import Counter
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit, urlunsplit

BENCHMARKS_ROOT = Path(__file__).resolve().parent.parent
if str(BENCHMARKS_ROOT) not in sys.path:
    sys.path.insert(0, str(BENCHMARKS_ROOT))

from benchmark_runtime import (  # noqa: E402
    PaperMachineApi,
    atomic_write_json,
    atomic_write_text,
    cancel_inflight,
    default_server_binary,
    decrypt,
    drive_phase,
    ensure_project,
    install_project_workflow,
    isolated_server,
    launch_workflow,
    load_json,
    mean,
    operational_usage,
    record_runtime_snapshot as record_shared_runtime_snapshot,
    reopen_terminal_failures,
    runtime_fingerprint as shared_runtime_fingerprint,
    runtime_artifact_fingerprints,
    save_state,
    token_usage,
    utc_now,
    validate_workflows,
    workflow_wall_time_seconds,
)


CONDITIONS = {
    "single_agent": {
        "program_slug": "single-agent-research",
        "params": {},
    },
    "coverage_r1": {
        "program_slug": "evidence-loop",
        "params": {
            "route_count": 2,
            "max_rounds": 1,
            "max_followups_per_round": 2,
        },
    },
    "coverage_r2": {
        "program_slug": "evidence-loop",
        "params": {
            "route_count": 2,
            "max_rounds": 2,
            "max_followups_per_round": 2,
        },
    },
    "coverage_r3": {
        "program_slug": "evidence-loop",
        "params": {
            "route_count": 3,
            "max_rounds": 3,
            "max_followups_per_round": 3,
        },
    },
    "coverage_r4": {
        "program_slug": "evidence-loop",
        "params": {
            "route_count": 4,
            "max_rounds": 4,
            "max_followups_per_round": 4,
        },
    },
}
TERMINAL_STATUSES = {"completed", "failed", "cancelled"}
RUNTIME_FILES = (
    "benchmarks/benchmark_runtime.py",
    "benchmarks/live-dr-mini/run_matrix.py",
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
    "workflows/builtin/evidence-loop/workflow.py",
    "benchmarks/live-dr-mini/grader/workflow.py",
    "workflows/builtin/single-agent-research/workflow.py",
)
UPSTREAM_EVALUATOR = {
    "repo": "https://github.com/microsoft/LiveDRBench",
    "revision": "6ff85b67b35fa303907f6f275417622338acd1f6",
    "files_sha256": {
        "src/evaluate.py": "083ad8c264659c5d5b2810920855476a8b314673dc6998fc5a8f0cf1fbf384d0",
        "src/evals/datasets_flights.py": "eb9b52c3c8d3de544b04c1dc173eb7f078b72abe8f3bbe17924cd211e013aa5e",
        "src/evals/entities.py": "94a83fa7212b986038f9a576560432fa56fda8e9de21cc2b212cf3df8ba9d461",
        "src/evals/priorart.py": "8b5a0fe527a7a12cb15a08200809d60ac2ca1ece7f296b6e76d9c519426915cc",
        "src/evals/scifacts.py": "ee88d2c1b8378d46a4b6035c69bf0c089b8d0d12f4778282240b6a836bcda610",
    },
}


def normalize_text(value: Any) -> str:
    text = unicodedata.normalize("NFKC", str(value)).casefold().strip()
    return "".join(character for character in text if character.isalnum())


def normalize_url(value: Any) -> str:
    try:
        parts = urlsplit(str(value).strip())
    except ValueError:
        return normalize_text(value)
    return urlunsplit(
        (
            parts.scheme.casefold(),
            parts.netloc.casefold(),
            parts.path.rstrip("/"),
            parts.query,
            "",
        )
    )


def equivalent(left: Any, right: Any) -> bool:
    if isinstance(left, bool) or isinstance(right, bool):
        return type(left) is type(right) and left == right
    if isinstance(right, list):
        return any(equivalent(left, candidate) for candidate in right)
    if isinstance(left, list):
        return any(equivalent(candidate, right) for candidate in left)
    if isinstance(left, (int, float)) and isinstance(right, (int, float)):
        left_number = float(left)
        right_number = float(right)
        tolerance = max(abs(right_number) * 0.01, 1e-9)
        return abs(left_number - right_number) <= tolerance
    if isinstance(left, str) and isinstance(right, str):
        if left.startswith("http") or right.startswith("http"):
            return normalize_url(left) == normalize_url(right)
        normalized_left = normalize_text(left)
        normalized_right = normalize_text(right)
        if normalized_left == normalized_right:
            return True
        left_words = {normalize_text(word) for word in re.findall(r"[\w-]+", left)}
        right_words = {normalize_text(word) for word in re.findall(r"[\w-]+", right)}
        left_words.discard("")
        right_words.discard("")
        shorter, longer = sorted((left_words, right_words), key=len)
        return len(shorter) >= 2 and shorter.issubset(longer)
    return left == right


_MISSING = object()


def normalize_field_name(value: str) -> str:
    tokens = re.findall(r"[a-z0-9]+", str(value).casefold().replace("#", " number "))
    normalized = []
    for token in tokens:
        if len(token) > 3 and token.endswith("s") and not token.endswith("ss"):
            token = token[:-1]
        normalized.append(token)
    return "".join(normalized)


def field_value(value: dict[str, Any], key: str) -> Any:
    if key in value:
        return value[key]
    normalized = normalize_field_name(key)
    for candidate, candidate_value in value.items():
        if normalize_field_name(candidate) == normalized:
            return candidate_value
    return _MISSING


def selected_keys(value: dict[str, Any], ignored: set[str]) -> list[str]:
    ignored_normalized = {normalize_field_name(key) for key in ignored}
    return [key for key in value if normalize_field_name(key) not in ignored_normalized]


def json_candidates(text: str) -> list[Any]:
    cleaned = text.strip()
    if cleaned.startswith("```"):
        cleaned = re.sub(r"^```(?:json)?\s*", "", cleaned, flags=re.IGNORECASE)
        cleaned = re.sub(r"\s*```$", "", cleaned)
    candidates = []
    try:
        candidates.append(json.loads(cleaned))
    except json.JSONDecodeError:
        pass
    decoder = json.JSONDecoder()
    for index, character in enumerate(cleaned):
        if character not in "[{":
            continue
        try:
            value, _ = decoder.raw_decode(cleaned[index:])
        except json.JSONDecodeError:
            continue
        if value not in candidates:
            candidates.append(value)
    return candidates


def expected_ground_truth(task: dict[str, Any]) -> tuple[list[Any], dict[str, Any]]:
    ground_truths = json.loads(decrypt(task["ground_truths"], task["canary"]))
    misc = json.loads(decrypt(task["misc"], task["canary"]))
    return ground_truths, misc


def score_list_strings(expected: list[str], predicted: Any) -> dict[str, Any]:
    if not isinstance(predicted, list) or any(
        not isinstance(item, str) for item in predicted
    ):
        raise ValueError("expected a JSON array of strings")
    remaining = list(expected)
    matched = 0
    for item in predicted:
        index = next(
            (
                index
                for index, target in enumerate(remaining)
                if equivalent(item, target)
            ),
            None,
        )
        if index is not None:
            matched += 1
            remaining.pop(index)
    return metric_counts(matched, len(predicted), len(expected))


def score_dict_fields(
    expected: dict[str, Any],
    predicted: Any,
    main_claims: list[str] | None = None,
    ignore_keys: list[str] | None = None,
) -> dict[str, Any]:
    if not isinstance(predicted, dict):
        raise ValueError("expected a JSON object")
    ignored = set(ignore_keys or [])
    expected_keys = selected_keys(expected, ignored)
    predicted_keys = selected_keys(predicted, ignored)
    for key in main_claims or []:
        actual = field_value(predicted, key)
        target = field_value(expected, key)
        if actual is _MISSING or target is _MISSING or not equivalent(actual, target):
            return metric_counts(0, len(predicted_keys), len(expected_keys))
    matched = sum(
        (actual := field_value(predicted, key)) is not _MISSING
        and equivalent(actual, expected[key])
        for key in expected_keys
    )
    return metric_counts(matched, len(predicted_keys), len(expected_keys))


def score_list_dicts(
    expected: list[dict[str, Any]],
    predicted: Any,
    primary_keys: list[str],
    main_claims: list[str] | None = None,
    ignore_keys: list[str] | None = None,
) -> dict[str, Any]:
    if not isinstance(predicted, list) or any(
        not isinstance(item, dict) for item in predicted
    ):
        raise ValueError("expected a JSON array of objects")
    ignored = set(ignore_keys or [])
    remaining = list(expected)
    matched_claims = 0
    predicted_claims = sum(len(selected_keys(item, ignored)) for item in predicted)
    expected_claims = sum(len(selected_keys(item, ignored)) for item in expected)
    for item in predicted:
        index = next(
            (
                index
                for index, target in enumerate(remaining)
                if dicts_match_on_primary_keys(target, item, primary_keys)
            ),
            None,
        )
        if index is None:
            continue
        target = remaining.pop(index)
        if any(
            (actual := field_value(item, key)) is _MISSING
            or (expected_value := field_value(target, key)) is _MISSING
            or not equivalent(actual, expected_value)
            for key in main_claims or []
        ):
            continue
        matched_claims += sum(
            (actual := field_value(item, key)) is not _MISSING
            and equivalent(actual, target[key])
            for key in selected_keys(target, ignored)
        )
    return metric_counts(matched_claims, predicted_claims, expected_claims)


def dicts_match_on_primary_keys(
    expected: dict[str, Any], predicted: dict[str, Any], primary_keys: list[str]
) -> bool:
    usable = [
        key
        for key in primary_keys
        if field_value(expected, key) is not _MISSING
        and field_value(predicted, key) is not _MISSING
    ]
    if not usable:
        usable = [
            key for key in expected if field_value(predicted, key) is not _MISSING
        ][:1]
    return bool(usable) and all(
        equivalent(field_value(predicted, key), field_value(expected, key))
        for key in usable
    )


def score_list_key(
    expected: list[dict[str, Any]], predicted: Any, key: str, unique: bool = False
) -> dict[str, Any]:
    if not isinstance(predicted, list) or any(
        not isinstance(item, dict) for item in predicted
    ):
        raise ValueError("expected a JSON array of objects")
    predicted_values = [field_value(item, key) for item in predicted]
    predicted_values = [value for value in predicted_values if value is not _MISSING]
    if unique:
        remaining = [field_value(item, key) for item in expected]
        matched = 0
        for value in predicted_values:
            index = next(
                (
                    index
                    for index, target in enumerate(remaining)
                    if equivalent(value, target)
                ),
                None,
            )
            if index is not None:
                matched += 1
                remaining.pop(index)
    else:
        expected_values = [field_value(item, key) for item in expected]
        matched = sum(
            any(equivalent(value, target) for target in expected_values)
            for value in predicted_values
        )
    recall_matches = (
        matched
        if unique
        else sum(
            any(equivalent(value, field_value(item, key)) for value in predicted_values)
            for item in expected
        )
    )
    precision = matched / len(predicted_values) if predicted_values else 0.0
    recall = recall_matches / len(expected) if expected else 0.0
    f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
    return {
        "precision": precision,
        "recall": recall,
        "f1": f1,
        "matched_claims": matched,
        "predicted_claims": len(predicted_values),
        "expected_claims": len(expected),
    }


def score_scifacts_materials(
    expected: list[dict[str, Any]], predicted: Any
) -> dict[str, Any]:
    material = score_list_key(expected, predicted, "material")
    paper = score_list_key(expected, predicted, "paper_title")
    return {
        "precision": material["precision"] * paper["precision"],
        "recall": material["recall"] * paper["recall"],
        "f1": material["f1"] * paper["f1"],
        "matched_claims": min(material["matched_claims"], paper["matched_claims"]),
        "predicted_claims": max(
            material["predicted_claims"], paper["predicted_claims"]
        ),
        "expected_claims": len(expected),
        "per_key": {"material": material, "paper_title": paper},
    }


def metric_counts(matched: int, predicted: int, expected: int) -> dict[str, Any]:
    precision = matched / predicted if predicted else 0.0
    recall = matched / expected if expected else 0.0
    f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
    return {
        "precision": precision,
        "recall": recall,
        "f1": f1,
        "matched_claims": matched,
        "predicted_claims": predicted,
        "expected_claims": expected,
    }


def prediction_from_report(task: dict[str, Any], report: str) -> Any:
    ground_truths, _ = expected_ground_truth(task)
    expected = ground_truths[0]
    candidates = json_candidates(report)
    expected_type = list if isinstance(expected, list) else dict
    candidate = next(
        (value for value in candidates if isinstance(value, expected_type)), None
    )
    if candidate is None:
        raise ValueError("no compatible JSON value")
    return candidate


def score_report(task: dict[str, Any], report: str) -> dict[str, Any]:
    ground_truths, misc = expected_ground_truth(task)
    expected = ground_truths[0]
    try:
        candidate = prediction_from_report(task, report)
    except ValueError as error:
        return {**metric_counts(0, 0, claim_count(expected)), "parse_error": str(error)}
    try:
        eval_info = misc.get("eval_info", {})
        main_claims = list(eval_info.get("main_claims") or [])
        ignore_keys = list(eval_info.get("ignore_keys") or [])
        if task["scorer"] == "list_strings":
            score = score_list_strings(expected, candidate)
        elif task["scorer"] == "dict_fields":
            score = score_dict_fields(expected, candidate, main_claims, ignore_keys)
        elif task["scorer"] == "list_dicts":
            primary_keys = list(eval_info.get("primary_keys") or [])
            if not primary_keys:
                primary_keys = [next(iter(expected[0]))]
            score = score_list_dicts(
                expected,
                candidate,
                primary_keys,
                main_claims,
                ignore_keys,
            )
        elif task["scorer"] == "list_key":
            score = score_list_key(
                expected,
                candidate,
                str(task["score_key"]),
                bool(task.get("unique_matches")),
            )
        elif task["scorer"] == "scifacts_materials":
            score = score_scifacts_materials(expected, candidate)
        else:
            raise ValueError(f"unknown scorer {task['scorer']}")
    except (TypeError, ValueError) as error:
        return {**metric_counts(0, 0, claim_count(expected)), "parse_error": str(error)}
    score["parse_error"] = None
    return score


def claim_count(value: Any) -> int:
    if isinstance(value, dict):
        return len(value)
    if isinstance(value, list):
        return sum(len(item) if isinstance(item, dict) else 1 for item in value)
    return 1


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


def launch_run(
    api: PaperMachineApi,
    project_id: str,
    job: dict[str, Any],
    task: dict[str, Any],
    model: str,
) -> dict[str, Any]:
    condition = CONDITIONS[job["condition"]]
    return launch_workflow(
        api,
        project_id,
        program_slug=condition["program_slug"],
        request=task["question"],
        params=condition["params"],
        model=model,
        access="research",
    )


def launch_grader_run(
    api: PaperMachineApi,
    project_id: str,
    job: dict[str, Any],
    task: dict[str, Any],
    grader_model: str,
) -> dict[str, Any]:
    ground_truths, misc = expected_ground_truth(task)
    if len(ground_truths) != 1:
        raise ValueError("LiveDR mini expects exactly one reference output per task")
    prediction = (job.get("research", {}).get("result") or {}).get("prediction")
    if prediction is None:
        raise ValueError("cannot launch semantic grader without a parsed prediction")
    return launch_workflow(
        api,
        project_id,
        program_slug="live-dr-grader",
        request="Blindly apply the pinned upstream LiveDRBench claim-matching rubric.",
        params={
            "category": task["category"],
            "ground_truth": ground_truths[0],
            "prediction": prediction,
            "eval_info": misc.get("eval_info") or {},
            "grader_model": grader_model,
        },
        model=grader_model,
        access="model_only",
    )


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
    hosted_actions = Counter(
        str((step.get("input") or {}).get("type", "unknown")) for step in hosted
    )
    queries = []
    for step in hosted:
        payload = step.get("input") or {}
        values = payload.get("queries") or (
            [payload["query"]] if payload.get("query") else []
        )
        queries.extend(str(value) for value in values)
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
        "hosted_search_actions": dict(hosted_actions),
        "unique_search_queries": len(set(queries)),
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


def capture_result(
    api: PaperMachineApi,
    view: dict[str, Any],
    task: dict[str, Any],
    article_path: Path,
) -> dict[str, Any]:
    run = view["workflow"]
    output = run.get("output") or {}
    report = output.get("report")
    if not isinstance(report, str) or not report.strip():
        raise ValueError("completed run did not return a non-empty report")
    atomic_write_text(article_path, report.rstrip() + "\n")
    agents = [
        session_metrics(api.get(f"/sessions/{session['id']}"))
        for session in view.get("sessions", [])
    ]
    usage = run.get("usage") or {}
    try:
        prediction = prediction_from_report(task, report)
    except ValueError:
        prediction = None
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
        "hosted_search_actions": dict(
            sum(
                (Counter(agent["hosted_search_actions"]) for agent in agents),
                Counter(),
            )
        ),
        "unique_search_queries": sum(
            agent["unique_search_queries"] for agent in agents
        ),
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
        "prediction": prediction,
        "score": score_report(task, report),
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
    for key in ("precision", "recall", "f1"):
        value = grading.get(key)
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ValueError(f"grader {key} must be numeric")
        if not 0 <= float(value) <= 1:
            raise ValueError(f"grader {key} must be between zero and one")
    atomic_write_json(grade_path, grading)
    usage = run.get("usage") or {}
    return {
        **grading,
        "workflow_sha256": run["program"]["sha256"],
        "usage": token_usage(usage.get("tokens") or {}),
        "created_at": run["created_at"],
        "completed_at": run["updated_at"],
        "runtime_wall_time_seconds": int(usage.get("wall_time_seconds", 0)),
        "wall_time_seconds": workflow_wall_time_seconds(run),
        "grade_path": str(grade_path),
    }


def is_retryable_error(error: str) -> bool:
    lowered = error.casefold()
    deterministic = (
        "context window",
        "invalid",
        "schema",
        "permission denied",
    )
    return not any(fragment in lowered for fragment in deterministic)


def run_research_matrix(
    api: PaperMachineApi,
    state: dict[str, Any],
    state_path: Path,
    tasks: dict[str, dict[str, Any]],
    articles_dir: Path,
    project_id: str,
    model: str,
    poll_seconds: float,
    max_attempts: int,
    max_parallel_runs: int,
) -> None:
    drive_phase(
        api,
        state,
        state_path,
        tasks,
        "research",
        lambda job, task: launch_run(api, project_id, job, task, model),
        lambda job, view: capture_result(
            api,
            view,
            tasks[job["task_key"]],
            articles_dir / f"{job['key']}.txt",
        ),
        is_retryable_error,
        poll_seconds,
        max_attempts,
        max_parallel_runs,
    )


def run_grading_matrix(
    api: PaperMachineApi,
    state: dict[str, Any],
    state_path: Path,
    tasks: dict[str, dict[str, Any]],
    grades_dir: Path,
    project_id: str,
    grader_model: str,
    poll_seconds: float,
    max_attempts: int,
    max_parallel_runs: int,
) -> None:
    for job in state["jobs"]:
        if job.get("research_failed"):
            continue
        grade = job.setdefault("grade", {"attempts": [], "status": "pending"})
        result = job.get("research", {}).get("result") or {}
        if result.get("prediction") is None and "result" not in grade:
            expected, _ = expected_ground_truth(tasks[job["task_key"]])
            grade["result"] = {
                **metric_counts(0, 0, claim_count(expected[0])),
                "parse_error": (result.get("score") or {}).get("parse_error")
                or "no compatible JSON value",
                "usage": token_usage({}),
                "wall_time_seconds": 0,
                "method": "parse_failure_no_judge",
            }
            grade["status"] = "completed"
    save_state(state_path, state)
    drive_phase(
        api,
        state,
        state_path,
        tasks,
        "grade",
        lambda job, task: launch_grader_run(api, project_id, job, task, grader_model),
        lambda job, view: capture_grader_result(
            view, grades_dir / f"{job['key']}.json"
        ),
        is_retryable_error,
        poll_seconds,
        max_attempts,
        max_parallel_runs,
    )


def runtime_fingerprint(
    root: Path, runtime_artifacts: dict[str, str] | None = None
) -> dict[str, str]:
    return shared_runtime_fingerprint(root, RUNTIME_FILES, runtime_artifacts)


def record_runtime_snapshot(
    state: dict[str, Any], root: Path, runtime_artifacts: dict[str, str]
) -> dict[str, str]:
    return record_shared_runtime_snapshot(state, root, RUNTIME_FILES, runtime_artifacts)


def aggregate(jobs: list[dict[str, Any]], condition: str) -> dict[str, Any]:
    selected = [job for job in jobs if job["condition"] == condition]
    research_results = [job.get("research", {}).get("result") or {} for job in selected]
    draft_audits = [
        result["draft_audit"]
        for result in research_results
        if isinstance(result.get("draft_audit"), dict)
    ]
    grades = [job.get("grade", {}).get("result") or {} for job in selected]
    research_usage = [operational_usage(job, "research") for job in selected]
    grader_usage = [operational_usage(job, "grade") for job in selected]
    total_input = sum(usage["input_tokens"] for usage in research_usage)
    total_cached = sum(usage["cached_input_tokens"] for usage in research_usage)
    transports = sum(
        (Counter(result.get("model_transports", {})) for result in research_results),
        Counter(),
    )
    cache_modes = sum(
        (Counter(result.get("prompt_cache_modes", {})) for result in research_results),
        Counter(),
    )
    return {
        "runs": len(selected),
        "graded": sum(bool(grade) for grade in grades),
        "precision_mean": mean(
            [float(grade.get("precision", 0.0)) for grade in grades]
        ),
        "recall_mean": mean([float(grade.get("recall", 0.0)) for grade in grades]),
        "f1_mean": mean([float(grade.get("f1", 0.0)) for grade in grades]),
        "deterministic_f1_mean": mean(
            [
                float((result.get("score") or {}).get("f1", 0.0))
                for result in research_results
            ]
        ),
        "input_mean": mean([usage["input_tokens"] for usage in research_usage]),
        "uncached_mean": mean(
            [usage["uncached_input_tokens"] for usage in research_usage]
        ),
        "output_mean": mean([usage["output_tokens"] for usage in research_usage]),
        "cache_read_ratio": total_cached / total_input if total_input else 0.0,
        "wall_mean": mean(
            [float(result.get("wall_time_seconds", 0)) for result in research_results]
        ),
        "search_calls_mean": mean(
            [float(result.get("hosted_search_calls", 0)) for result in research_results]
        ),
        "search_queries_mean": mean(
            [
                float(result.get("unique_search_queries", 0))
                for result in research_results
            ]
        ),
        "continuation_hits": sum(
            int(result.get("continuation_hits", 0)) for result in research_results
        ),
        "report_characters_mean": mean(
            [float(result.get("report_characters", 0)) for result in research_results]
        ),
        "parse_failures": sum(
            bool((result.get("score") or {}).get("parse_error"))
            for result in research_results
        ),
        "two_round_runs": sum(
            int(result.get("rounds", 1)) > 1 for result in research_results
        ),
        "draft_revisions": sum(
            audit.get("revision_performed") is True for audit in draft_audits
        ),
        "final_audit_failures": sum(
            audit.get("pass") is not True for audit in draft_audits
        ),
        "grader_effective_mean": mean(
            [usage["effective_tokens"] for usage in grader_usage]
        ),
        "research_failures": sum(bool(job.get("research_failed")) for job in selected),
        "grade_failures": sum(bool(job.get("grade_failed")) for job in selected),
        "model_transports": dict(transports),
        "prompt_cache_modes": dict(cache_modes),
        "websocket_ratio": (
            transports["responses_websocket"] / sum(transports.values())
            if transports
            else 0.0
        ),
        "fallback_steps": sum(
            sum((result.get("websocket_fallback_reasons") or {}).values())
            for result in research_results
        ),
        "explicit_cache_breakpoints": sum(
            int(result.get("explicit_cache_breakpoints", 0))
            for result in research_results
        ),
    }


def render_report(state: dict[str, Any], tasks: dict[str, dict[str, Any]]) -> str:
    complete = [job for job in state["jobs"] if "result" in job.get("research", {})]
    conditions = list(state["experiment"]["conditions"])
    aggregates = {
        condition: aggregate(state["jobs"], condition) for condition in conditions
    }
    lines = [
        "# PaperMachine LiveDRBench mini report",
        "",
        f"Generated: {utc_now()}",
        "",
        f"Research model profile: `{state['experiment']['model']}`; grader profile: `{state['experiment']['grader_model']}`.",
        "",
        "This is a pinned development slice, not a leaderboard score. Research runs never receive references. Each parseable answer is judged afterward in an independent no-tool Session using the complete upstream semantic claim-matching rubric; metric arithmetic is deterministic. A stricter local scorer is retained only as a diagnostic.",
        "",
        "## Tasks",
        "",
        "| Key | Category | Reference | Question |",
        "|---:|---|---|---|",
    ]
    for key, task in tasks.items():
        question = task["question"].replace("|", "\\|").replace("\n", " ")
        validity = task.get("reference_validity") or {}
        reference_status = str(validity.get("status", "pinned"))
        lines.append(
            f"| {key} | {task['category']} | {reference_status} | {question} |"
        )
    lines.extend(
        [
            "",
            "## Aggregate",
            "",
            "| Condition | Runs | Graded | Rubric P | Rubric R | Rubric F1 | Strict F1 | Input | Uncached | Output | Cache read | Wall | Search calls | Revised | Audit fail | Grader effective | Research fail | Grade fail |",
            "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
        ]
    )
    for condition, values in aggregates.items():
        lines.append(
            f"| {condition} | {values['runs']} | {values['graded']} | {values['precision_mean']:.3f} | "
            f"{values['recall_mean']:.3f} | {values['f1_mean']:.3f} | "
            f"{values['deterministic_f1_mean']:.3f} | "
            f"{values['input_mean']:.0f} | {values['uncached_mean']:.0f} | "
            f"{values['output_mean']:.0f} | {values['cache_read_ratio']:.1%} | "
            f"{values['wall_mean']:.0f}s | {values['search_calls_mean']:.1f} | "
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
            "## Per task",
            "",
            "| Task | Condition | Repeat | Rubric P | Rubric R | Rubric F1 | Strict F1 | Input | Cached | Search calls | Rounds | Revised | Audit | Continuation hits |",
            "|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|",
        ]
    )
    for job in sorted(
        complete,
        key=lambda item: (int(item["task_key"]), item["condition"], item["repeat"]),
    ):
        result = job["research"]["result"]
        score = job.get("grade", {}).get("result") or {}
        strict = result["score"]
        usage = result["usage"]
        draft_audit = result.get("draft_audit")
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
            f"| {job['task_key']} | {job['condition']} | {job['repeat']} | "
            f"{float(score.get('precision', 0)):.3f} | {float(score.get('recall', 0)):.3f} | "
            f"{float(score.get('f1', 0)):.3f} | {strict['f1']:.3f} | "
            f"{usage['input_tokens']} | {usage['cached_input_tokens']} | "
            f"{result['hosted_search_calls']} | {result.get('rounds', 1)} | "
            f"{revised} | {audit_status} | "
            f"{result['continuation_hits']} |"
        )
    warnings = [
        (key, task["reference_validity"])
        for key, task in tasks.items()
        if task.get("reference_validity")
    ]
    if warnings:
        lines.extend(
            [
                "",
                "## Reference validity warnings",
                "",
                "Scores on the tasks below are reported for reproducibility, but should not be used to compare workflows without live adjudication:",
                "",
            ]
        )
        for key, warning in warnings:
            lines.append(f"- Task {key} (`{warning['status']}`): {warning['note']}")
    lines.extend(
        [
            "",
            "## Validity limits",
            "",
            f"- The development slice contains {len(tasks)} tasks and is for workflow diagnosis, not leaderboard comparison.",
            "- The semantic judge is isolated and has no tools, but remains a model-based judge and can introduce model-specific bias.",
            "- The grader ports the upstream rubric to Responses and batches each task into one judgment action, so it is rubric-faithful but not byte-for-byte execution of the upstream Chat Completions script.",
            "- Research or grading failures count as zero in aggregate means; grader tokens are reported separately.",
            "- Hosted search results and indexed web content can change over time.",
            "- Runtime source hashes are recorded in state.json because workflow source hash alone does not identify tool/runtime behavior.",
            "",
        ]
    )
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-config", type=Path)
    parser.add_argument("--server-bin", type=Path)
    parser.add_argument("--task-keys", default="0,20,22,23,40,47,66,83")
    parser.add_argument("--conditions", default="single_agent,coverage_r1,coverage_r2")
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--seed", type=int, default=20260806)
    parser.add_argument("--model", default="deepseek-flash")
    parser.add_argument("--grader-model", default="deepseek-flash")
    parser.add_argument("--run-name", default="deepseek-baseline-8x3x2-2026-08-07")
    parser.add_argument("--poll-seconds", type=float, default=5.0)
    parser.add_argument("--max-attempts", type=int, default=2)
    parser.add_argument("--max-parallel-runs", type=int, default=2)
    parser.add_argument("--retry-terminal-failures", action="store_true")
    return parser.parse_args()


def run_matrix(args: argparse.Namespace, api_base: str | None) -> int:
    if args.repeats < 1:
        raise ValueError("repeats must be positive")
    if args.max_attempts < 1 or args.max_parallel_runs < 1:
        raise ValueError("max-attempts and max-parallel-runs must be positive")
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
    articles_dir = run_dir / "articles"
    grades_dir = run_dir / "grades"
    runtime_artifacts = runtime_artifact_fingerprints(
        args.server_config, args.server_bin if api_base is not None else None
    )

    if api_base is None:
        raise RuntimeError("benchmark execution requires an isolated server")
    api = PaperMachineApi(api_base)
    health = api.get("/health")
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
                "scoring_method": "independent_upstream_rubric_semantic_judge_plus_deterministic_metrics",
            },
            "upstream": {
                key: snapshot[key]
                for key in (
                    "source_dataset",
                    "source_repo",
                    "source_revision",
                    "source_file",
                    "source_file_sha256",
                )
            },
            "upstream_evaluator": UPSTREAM_EVALUATOR,
            "server_health": health,
            "runtime_source_sha256": runtime_fingerprint(root, runtime_artifacts),
            "jobs": jobs,
        }
        save_state(state_path, state)

    record_runtime_snapshot(state, root, runtime_artifacts)
    save_state(state_path, state)

    if args.retry_terminal_failures:
        reopened = reopen_terminal_failures(state)
        save_state(state_path, state)
        print(f"reopened {reopened} terminal benchmark phases", flush=True)

    project_id = ensure_project(
        api,
        f"LiveDRBench research - {args.run_name}",
        run_dir / "project",
    )
    install_project_workflow(
        api,
        project_id,
        Path(__file__).resolve().parent / "grader" / "workflow.py",
    )
    validate_workflows(
        api,
        project_id,
        {CONDITIONS[condition]["program_slug"] for condition in conditions}
        | {"live-dr-grader"},
    )
    state["project_id"] = project_id
    save_state(state_path, state)
    try:
        run_research_matrix(
            api,
            state,
            state_path,
            tasks,
            articles_dir,
            project_id,
            args.model,
            args.poll_seconds,
            args.max_attempts,
            args.max_parallel_runs,
        )
        run_grading_matrix(
            api,
            state,
            state_path,
            tasks,
            grades_dir,
            project_id,
            args.grader_model,
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


def main() -> int:
    args = parse_args()
    repository_root = Path(__file__).resolve().parents[2]
    args.server_config = (
        args.server_config or repository_root / "papermachine.toml"
    ).resolve()
    args.server_bin = (
        args.server_bin or default_server_binary(repository_root)
    ).resolve()
    run_dir = Path(__file__).resolve().parent / "runs" / args.run_name
    with isolated_server(
        repository_root, run_dir, args.server_config, args.server_bin
    ) as api_base:
        return run_matrix(args, api_base)


if __name__ == "__main__":
    raise SystemExit(main())
