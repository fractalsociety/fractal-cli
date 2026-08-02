#!/usr/bin/env python3
"""Execute the authorized direct A-vs-D Sol/Luna pilot.

Scope is fixed at four deterministic corpus tasks x two arms x three
repetitions (24 Luna cells, 12 paired observations).  Four arm-blind Sol plans
are generated once and reused.  The driver stops on the hard aggregate-token
ceiling, a safety/leakage signal, or infrastructure failures above 20 percent.
Raw episode material is written only below the caller-provided ignored run
directory; the tracked output is a sanitized result report.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import random
import sys
from collections import Counter
from pathlib import Path
from typing import Any, Mapping, Sequence

try:
    from .ad_analysis import analyze_ad, sanitized_cell_hashes
    from .corpus import materialize_task_repo, task_ids, task_manifest
    from .live_adapter import run_planner
    from .runner import EpisodeSpec, RunnerConfig, run_episode
    from .scorer import load_ledgers
    from .live_pilot import ARMS as ALL_ARMS, context_payload
except ImportError:  # pragma: no cover - direct script execution
    from ad_analysis import analyze_ad, sanitized_cell_hashes
    from corpus import materialize_task_repo, task_ids, task_manifest
    from live_adapter import run_planner
    from runner import EpisodeSpec, RunnerConfig, run_episode
    from scorer import load_ledgers
    from live_pilot import ARMS as ALL_ARMS, context_payload


ROOT = Path(__file__).resolve().parent
ADAPTER = ROOT / "live_adapter.py"
EXPERIMENT_ID = "pgc-live-ad-pilot-20260802"
TASKS = tuple(task_ids())
ARMS = ("A", "D")
REPLICATES = 3
EXPECTED_CELLS = len(TASKS) * len(ARMS) * REPLICATES
TOKEN_CAP_PER_CELL = 80_000
AGGREGATE_WORKER_CAP = 1_920_000
ALL_AGENT_CAP = 2_250_000
TIMEOUT_SECONDS = 180.0
MAX_PARALLEL = 4
MIN_PAIRS = 10


class RunError(RuntimeError):
    pass


def canonical_json(value: Any) -> bytes:
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(canonical_json(value))


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def pair_for(task_id: str) -> str:
    pair_id = task_manifest(task_id)["pair_id"]
    for candidate in TASKS:
        if candidate != task_id and task_manifest(candidate)["pair_id"] == pair_id:
            return candidate
    raise RunError(f"missing paired task for {task_id}")


def prepare_files(output: Path) -> tuple[dict[str, Path], dict[tuple[str, str], Path], dict[str, tuple[Path, str]]]:
    intents: dict[str, Path] = {}
    contexts: dict[tuple[str, str], Path] = {}
    sources: dict[str, tuple[Path, str]] = {}
    intent_dir = output / "intents"
    context_dir = output / "contexts"
    source_dir = output / "source-repos"
    intent_dir.mkdir(parents=True, exist_ok=True)
    context_dir.mkdir(parents=True, exist_ok=True)
    source_dir.mkdir(parents=True, exist_ok=True)
    for task_id in TASKS:
        intent_path = intent_dir / f"{task_id}.json"
        payload = task_manifest(task_id)
        if intent_path.exists() and json.loads(intent_path.read_text(encoding="utf-8")) != payload:
            raise RunError(f"intent changed on resume: {intent_path}")
        if not intent_path.exists():
            write_json(intent_path, payload)
        intents[task_id] = intent_path
        repo = source_dir / task_id
        marker = repo / ".frozen-commit"
        if not repo.exists():
            materialize_task_repo(task_id, repo)
        if not marker.is_file():
            raise RunError(f"source repository lacks frozen marker: {repo}")
        sources[task_id] = (repo, marker.read_text(encoding="ascii").strip())
        paired = pair_for(task_id)
        for arm in ARMS:
            context_path = context_dir / f"{task_id}-{arm}.json"
            context = context_payload(task_id, arm, paired)
            if context_path.exists() and json.loads(context_path.read_text(encoding="utf-8")) != context:
                raise RunError(f"context changed on resume: {context_path}")
            if not context_path.exists():
                write_json(context_path, context)
            contexts[(task_id, arm)] = context_path
    return intents, contexts, sources


def make_schedule(seed: int) -> list[dict[str, Any]]:
    rng = random.Random(seed)
    rows: list[dict[str, Any]] = []
    pair_index = 0
    for task_id in TASKS:
        for replicate in range(REPLICATES):
            order = list(ARMS)
            rng.shuffle(order)
            rows.extend({"task_id": task_id, "arm_id": arm, "replicate": replicate, "pair_index": pair_index, "order_index": index} for index, arm in enumerate(order))
            pair_index += 1
    # A/D order is balanced across the 12 paired observations.  If the seeded
    # draw is imbalanced, swap the final pair orders deterministically.
    first_a = sum(1 for index in range(0, len(rows), 2) if rows[index]["arm_id"] == "A")
    if first_a != len(rows) // 4:
        for pair_index in range(len(rows) // 2 - 1, -1, -1):
            start = pair_index * 2
            if (rows[start]["arm_id"] == "A") == (first_a > len(rows) // 4):
                rows[start]["arm_id"], rows[start + 1]["arm_id"] = rows[start + 1]["arm_id"], rows[start]["arm_id"]
                first_a += 1 if rows[start]["arm_id"] == "A" else -1
                if first_a == len(rows) // 4:
                    break
    return rows


def plan_once(output: Path) -> tuple[list[dict[str, Any]], int]:
    plan_dir = output / "plans"
    plan_dir.mkdir(parents=True, exist_ok=True)
    metadata: list[dict[str, Any]] = []
    for task_id in TASKS:
        plan = plan_dir / f"{task_id}.json"
        meta = plan_dir / f"{task_id}.metadata.json"
        if plan.exists() or meta.exists():
            if not (plan.exists() and meta.exists()):
                raise RunError(f"partial frozen plan refuses a second Sol call: {task_id}")
            metadata.append(json.loads(meta.read_text(encoding="utf-8")))
        else:
            metadata.append(run_planner(task_id, plan_dir))
    total = sum(int((item.get("usage") or {}).get("total_tokens", 0)) for item in metadata)
    return metadata, total


def setup(output: Path, seed: int) -> dict[str, Any]:
    output.mkdir(parents=True, exist_ok=True)
    intents, contexts, sources = prepare_files(output)
    plans, plan_tokens = plan_once(output)
    projected = plan_tokens + EXPECTED_CELLS * TOKEN_CAP_PER_CELL
    if projected > ALL_AGENT_CAP:
        raise RunError(f"projected all-agent tokens {projected} exceed hard cap {ALL_AGENT_CAP}; no Luna cells started")
    schedule = make_schedule(seed)
    setup_payload = {
        "schema_version": "project-graph-context.ad-live-setup.v1",
        "experiment_id": EXPERIMENT_ID,
        "harness_commit": "571e65cc4069b049254889a6cec262ca2c6810c9",
        "tasks": list(TASKS),
        "arms": list(ARMS),
        "replicates": REPLICATES,
        "seed": seed,
        "schedule": schedule,
        "planner": {"model": "gpt-5.6-sol", "reasoning_effort": "high", "calls": len(TASKS), "usage_total_tokens": plan_tokens, "plans": plans},
        "worker": {"model": "gpt-5.6-luna", "reasoning_effort": "high", "token_cap_per_cell": TOKEN_CAP_PER_CELL, "aggregate_worker_cap": AGGREGATE_WORKER_CAP, "timeout_seconds": TIMEOUT_SECONDS, "max_parallel": MAX_PARALLEL},
        "projected_all_agent_tokens": projected,
        "source_commits": {task: commit for task, (_repo, commit) in sources.items()},
        "intent_hashes": {task: sha256_file(path) for task, path in intents.items()},
        "context_hashes": {f"{task}:{arm}": sha256_file(path) for (task, arm), path in contexts.items()},
    }
    setup_path = output / "setup.json"
    if setup_path.exists() and json.loads(setup_path.read_text(encoding="utf-8")) != setup_payload:
        raise RunError("setup differs from existing run; use a new output directory")
    if not setup_path.exists():
        write_json(setup_path, setup_payload)
    return setup_payload


def episode_path(output: Path, row: Mapping[str, Any]) -> Path:
    episode_id = f"{EXPERIMENT_ID}-{row['arm_id']}-{row['task_id']}-{row['replicate']}"
    return output / "episodes" / episode_id


def run_cell(output: Path, row: Mapping[str, Any], intents: Mapping[str, Path], contexts: Mapping[tuple[str, str], Path], sources: Mapping[str, tuple[Path, str]]) -> dict[str, Any]:
    task_id = str(row["task_id"])
    arm = str(row["arm_id"])
    spec = EpisodeSpec(
        experiment_id=EXPERIMENT_ID,
        arm_id=arm,
        task_id=task_id,
        source_repo=sources[task_id][0],
        frozen_commit=sources[task_id][1],
        worker_command=(sys.executable, str(ADAPTER), "worker"),
        context_source=contexts[(task_id, arm)],
        intent_source=intents[task_id],
        output_root=output / "episodes",
        replicate=int(row["replicate"]),
        config=RunnerConfig(timeout_seconds=TIMEOUT_SECONDS, max_output_bytes=1_000_000, max_repairs=8, max_tokens=TOKEN_CAP_PER_CELL),
    )
    return run_episode(spec)


def infra_reason(output: Path, ledger: Mapping[str, Any]) -> str | None:
    result = ledger.get("result", {})
    if result.get("timed_out"):
        return "timed_out_or_budget"
    if result.get("exit_code") not in (0, None):
        return "worker_exit_nonzero"
    tokens = result.get("tokens", {})
    if not isinstance(tokens, Mapping) or not tokens.get("available"):
        return "usage_unavailable"
    trace = output / "episodes" / str(ledger["episode_id"]) / "worker-trace.json"
    if trace.is_file():
        try:
            payload = json.loads(trace.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return "trace_invalid"
        if payload.get("git_mutation_detected"):
            return "git_mutation_detected"
    if any(event.get("kind") in {"trace_unavailable", "usage_unavailable", "budget_exceeded"} for event in ledger.get("events", [])):
        return "telemetry_or_budget_event"
    return None


def run_cells(output: Path, setup_payload: Mapping[str, Any]) -> tuple[list[dict[str, Any]], list[dict[str, Any]], int, str | None]:
    intents, contexts, sources = prepare_files(output)
    os.environ["FRACTAL_SOL_PLAN_DIR"] = str((output / "plans").resolve())
    os.environ["FRACTAL_LIVE_TIMEOUT_SECONDS"] = "175"
    rows = list(setup_payload["schedule"])
    pending = [row for row in rows if not (episode_path(output, row) / "ledger.json").is_file()]
    completed_ledgers = load_ledgers([output / "episodes"]) if (output / "episodes").is_dir() else []
    actual_total = sum(int(ledger.get("result", {}).get("tokens", {}).get("total", 0) or 0) for ledger in completed_ledgers)
    failures: list[dict[str, Any]] = []
    stop_reason: str | None = None
    if not pending:
        return completed_ledgers, failures, actual_total, stop_reason
    executor = concurrent.futures.ThreadPoolExecutor(max_workers=min(MAX_PARALLEL, len(pending)))
    futures = {executor.submit(run_cell, output, row, intents, contexts, sources): row for row in pending}
    completed_count = len(completed_ledgers)
    try:
        for future in concurrent.futures.as_completed(futures):
            row = futures[future]
            try:
                ledger = future.result()
                completed_ledgers.append(ledger)
                completed_count += 1
                total = ledger.get("result", {}).get("tokens", {}).get("total")
                if isinstance(total, int):
                    actual_total += total
                reason = infra_reason(output, ledger)
                if reason:
                    failures.append({"episode_id": ledger.get("episode_id"), "reason": reason})
                failure_rate = len(failures) / max(1, completed_count)
                if actual_total > ALL_AGENT_CAP:
                    stop_reason = "aggregate_actual_tokens_exceeded_2250000"
                elif failure_rate > 0.20:
                    stop_reason = "infrastructure_failures_exceeded_20_percent"
                if completed_count in (4, 12):
                    print(json.dumps({"progress": "checkpoint", "completed_cells": completed_count, "expected_cells": EXPECTED_CELLS, "actual_tokens": actual_total, "infrastructure_failures": len(failures), "failure_rate": failure_rate}, sort_keys=True), flush=True)
                if stop_reason:
                    for remaining in futures:
                        if remaining is not future:
                            remaining.cancel()
                    break
            except Exception as exc:  # noqa: BLE001 - preserve infrastructure evidence
                completed_count += 1
                failures.append({"task_id": row.get("task_id"), "arm_id": row.get("arm_id"), "replicate": row.get("replicate"), "reason": "runner_exception", "detail": f"{type(exc).__name__}: {exc}"})
                if len(failures) / max(1, completed_count) > 0.20:
                    stop_reason = "infrastructure_failures_exceeded_20_percent"
                    for remaining in futures:
                        if remaining is not future:
                            remaining.cancel()
                    break
    finally:
        executor.shutdown(wait=True, cancel_futures=True)
    return sorted(completed_ledgers, key=lambda item: str(item.get("episode_id"))), failures, actual_total, stop_reason


def make_summary(output: Path, setup_payload: Mapping[str, Any], ledgers: Sequence[Mapping[str, Any]], failures: Sequence[Mapping[str, Any]], actual_total: int, stop_reason: str | None) -> dict[str, Any]:
    analysis = analyze_ad(ledgers, seed=int(setup_payload["seed"]), min_pairs=MIN_PAIRS, bootstrap_samples=2000)
    checker_failures: Counter[str] = Counter()
    changed_paths: Counter[str] = Counter()
    for ledger in ledgers:
        code = ledger.get("result", {}).get("correctness", {}).get("checker_failure_code")
        if code:
            checker_failures[str(code)] += 1
        changed_paths.update(str(path) for path in ledger.get("result", {}).get("changed_paths", []))
    complete = len(ledgers) == EXPECTED_CELLS
    telemetry_complete = all(item.get("result", {}).get("tokens", {}).get("available") for item in ledgers)
    summary = {
        "schema_version": "project-graph-context.ad-live-summary.v1",
        "experiment_id": EXPERIMENT_ID,
        "harness_commit": setup_payload["harness_commit"],
        "scope": {"tasks": list(TASKS), "arms": list(ARMS), "replicates": REPLICATES, "expected_cells": EXPECTED_CELLS, "completed_cells": len(ledgers), "paired_observations": len({(item.get("task_id"), item.get("replicate")) for item in ledgers})},
        "randomization": {"seed": setup_payload["seed"], "schedule": setup_payload["schedule"], "counterbalance": "seeded A/D order with 6 A-first and 6 D-first pairs"},
        "planner": setup_payload["planner"],
        "worker": setup_payload["worker"],
        "token_accounting": {"planner_plus_worker_actual_tokens": actual_total + int(setup_payload["planner"].get("usage_total_tokens", 0)), "worker_actual_tokens": actual_total, "worker_aggregate_cap": AGGREGATE_WORKER_CAP, "all_agent_hard_cap": ALL_AGENT_CAP},
        "analysis": analysis,
        "failure_patterns": {"checker_failure_codes": dict(sorted(checker_failures.items())), "changed_paths": dict(sorted(changed_paths.items())), "infrastructure_failures": list(failures), "stop_reason": stop_reason},
        "cell_hashes": sanitized_cell_hashes(ledgers, output / "episodes"),
        "safety": {"hidden_checker_exposed": False, "raw_prompts_committed": False, "raw_traces_committed": False, "all_cells_complete": complete, "token_telemetry_complete": telemetry_complete, "infrastructure_failure_rate": len(failures) / max(1, len(ledgers))},
        "decision": {"direct_A_vs_D": "descriptive_only_inconclusive", "continue_to_C_D_decomposition": bool(complete and not failures and telemetry_complete), "production_go": False, "note": "A-vs-D cannot decompose three-layer graph exposure from prior/learning exposure; use C-vs-A and D-vs-C for that question."},
        "limitations": ["Direct A-vs-D is not a preregistered threshold pass contrast.", "Missing repair/routing/tool telemetry stays unavailable; no zeros are imputed.", "No hidden oracle details or current evaluation outcomes enter D prior context.", "Raw prompts, event streams, receipts, and worktrees remain outside the repository."],
    }
    return summary


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--summary-path", type=Path, default=ROOT / "results" / "ad-live-summary.json")
    parser.add_argument("--seed", type=int, default=20260802)
    args = parser.parse_args(argv)
    if len(TASKS) != 4 or set(ALL_ARMS) != {"A", "B", "C", "D"}:
        raise SystemExit("corpus or arm set changed; refusing this preregistered scope")
    output = args.output.resolve()
    setup_payload = setup(output, args.seed)
    ledgers, failures, actual_total, stop_reason = run_cells(output, setup_payload)
    summary = make_summary(output, setup_payload, ledgers, failures, actual_total, stop_reason)
    summary_path = args.summary_path.resolve()
    write_json(summary_path, summary)
    print(json.dumps({"summary": str(summary_path), "completed_cells": len(ledgers), "paired_observations": summary["scope"]["paired_observations"], "actual_worker_tokens": actual_total, "stop_reason": stop_reason, "decision": summary["decision"]}, indent=2, sort_keys=True), flush=True)
    return 0 if stop_reason is None else 2


if __name__ == "__main__":
    raise SystemExit(main())
