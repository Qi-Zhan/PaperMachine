#!/usr/bin/env python3
"""Run a resumable PaperMachine matrix on a pinned BrowseComp sample."""

from __future__ import annotations

import argparse
import hashlib
import random
import sys
from collections import Counter
from pathlib import Path
from typing import Any

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
RUNTIME_FILES = (
    "benchmarks/benchmark_runtime.py",
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
    "workflows/builtin/evidence-loop/workflow.py",
    "benchmarks/browsecomp-mini/grader/workflow.py",
    "workflows/builtin/single-agent-research/workflow.py",
)


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
            "request": research_prompt(question),
            "params": condition["params"],
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
            "request": "Blindly judge the submitted response against the supplied reference answer.",
            "params": {
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


def is_retryable_error(error: str) -> bool:
    lowered = error.casefold()
    deterministic = (
        "context window",
        "invalid grader",
        "invalid output",
        "schema",
        "permission denied",
    )
    return not any(fragment in lowered for fragment in deterministic)


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
            "- The independent grader is isolated from the researchers, but remains a model-based judge and can introduce model-specific bias.",
            "- Accuracy treats research or grading failures as incorrect; grader tokens are shown separately from research cost.",
            "- Hosted search results and indexed web content can change over time.",
            "- Dataset rows remain encrypted in tasks.json; plaintext questions and answers exist only in memory and local run records needed for execution/grading.",
            "",
        ]
    )
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-config", type=Path)
    parser.add_argument("--server-bin", type=Path)
    parser.add_argument("--task-keys", default="788,861,82,530,1047,995")
    parser.add_argument("--conditions", default="single_agent,coverage_r1,coverage_r2")
    parser.add_argument("--repeats", type=int, default=2)
    parser.add_argument("--seed", type=int, default=20260806)
    parser.add_argument("--model", default="deepseek-flash")
    parser.add_argument("--grader-model", default="deepseek-flash")
    parser.add_argument("--run-name", default="deepseek-baseline-6x3x2-2026-08-07")
    parser.add_argument("--poll-seconds", type=float, default=5.0)
    parser.add_argument("--max-attempts", type=int, default=2)
    parser.add_argument("--max-parallel-runs", type=int, default=2)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--retry-terminal-failures", action="store_true")
    return parser.parse_args()


def run_matrix(args: argparse.Namespace, api_base: str | None) -> int:
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
    runtime_artifacts = runtime_artifact_fingerprints(
        args.server_config, args.server_bin if api_base is not None else None
    )

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
            "runtime_source_sha256": runtime_fingerprint(root, runtime_artifacts),
            "projects": {},
            "jobs": jobs,
        }
        save_state(state_path, state)

    record_runtime_snapshot(state, root, runtime_artifacts)
    save_state(state_path, state)

    if args.retry_terminal_failures:
        reopened = reopen_terminal_failures(state)
        save_state(state_path, state)
        print(f"reopened {reopened} terminal benchmark phases", flush=True)

    if args.prepare_only:
        print(f"prepared {len(state['jobs'])} jobs in {run_dir}")
        return 0

    if api_base is None:
        raise RuntimeError("benchmark execution requires an isolated server")
    api = PaperMachineApi(api_base)
    health = api.get("/health")
    if health.get("model_mode") == "demo":
        raise RuntimeError("benchmark requires a substantive model provider")
    state["server_health"] = health
    if "research" not in state["projects"]:
        state["projects"]["research"] = ensure_project(
            api,
            f"BrowseComp research - {args.run_name}",
            run_dir / "projects" / "research",
        )
    if "grader" not in state["projects"]:
        state["projects"]["grader"] = ensure_project(
            api,
            f"BrowseComp graders - {args.run_name}",
            run_dir / "projects" / "grader",
        )
    install_project_workflow(
        api,
        state["projects"]["grader"],
        Path(__file__).resolve().parent / "grader" / "workflow.py",
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
            is_retryable_error,
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
            is_retryable_error,
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
    if args.prepare_only:
        return run_matrix(args, None)
    run_dir = Path(__file__).resolve().parent / "runs" / args.run_name
    with isolated_server(
        repository_root, run_dir, args.server_config, args.server_bin
    ) as api_base:
        return run_matrix(args, api_base)


if __name__ == "__main__":
    raise SystemExit(main())
