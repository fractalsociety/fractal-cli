#!/usr/bin/env python3
"""Reproducible two-task live Sol/Luna pilot driver.

The default scope is one related calculator pair (two tasks), four arms, one
replicate: eight Luna cells and two arm-blind Sol planning calls.  A first
invocation with ``--calibrate-only`` runs one cell; a later invocation resumes
the same output directory and runs only missing cells.  The driver never
stores raw prompts, Codex event streams, usage receipts, or worktrees in the
tracked result summary.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import random
import sys
import time
from collections import Counter
from pathlib import Path
from typing import Any, Mapping, Sequence

try:
    from .analysis import analyze
    from .corpus import materialize_task_repo, task_ids, task_manifest
    from .runner import EpisodeSpec, RunnerConfig, run_episode
    from .scorer import load_ledgers, score_ledger
    from .live_adapter import PLAN_SCHEMA_VERSION, run_planner
except ImportError:  # pragma: no cover - direct script execution
    from analysis import analyze
    from corpus import materialize_task_repo, task_ids, task_manifest
    from runner import EpisodeSpec, RunnerConfig, run_episode
    from scorer import load_ledgers, score_ledger
    from live_adapter import PLAN_SCHEMA_VERSION, run_planner


ROOT = Path(__file__).resolve().parent
ADAPTER = ROOT / "live_adapter.py"
ARMS = ("A", "B", "C", "D")
DEFAULT_TASKS = ("calculator-add", "calculator-subtract")
EXPERIMENT_ID = "pgc-live-pilot-calculator-pair"
WORKER_MODEL = "gpt-5.6-luna"
PLANNER_MODEL = "gpt-5.6-sol"
REASONING_EFFORT = "high"
CELL_TOKEN_CAP = 20_000
AGGREGATE_TOKEN_CAP = 160_000
PROJECTED_TOTAL_CAP = 210_000


class PilotError(RuntimeError):
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


def _pair_for(task_id: str, tasks: Sequence[str]) -> str:
    for candidate in tasks:
        if candidate != task_id and task_manifest(candidate).get("pair_id") == task_manifest(task_id).get("pair_id"):
            return candidate
    raise PilotError(f"tasks must contain a related pair; no pair for {task_id}")


def _graph_context(task_id: str, intent: Mapping[str, Any]) -> dict[str, Any]:
    target = str(intent["allowed_paths"][0])
    return {
        "schema_version": "project-graph-context.c-graph-context.v1",
        "arm_id": "C",
        "task_id": task_id,
        "layers": {
            "behavior": [{"id": "behavior.goal", "kind": "acceptance", "summary": str(intent["goal"])}],
            "source": [{"id": "source.target", "kind": "file", "summary": "Edit the target source file only.", "path": target, "line": 1}],
            "execution": [{"id": "execution.check", "kind": "checker", "summary": "Run focused local checks after the edit."}],
        },
        "edges": [
            {"from": "behavior.goal", "to": "source.target", "relation": "implemented_by"},
            {"from": "source.target", "to": "execution.check", "relation": "verified_by"},
        ],
        "retrieval_policy": {"top_k": 8, "include_neighbors": True},
    }


def _prior_snapshot(task_id: str, paired_task: str) -> dict[str, Any]:
    """Return a pre-registered, paired-task-only prior (never current output)."""

    prior_without_hash = {
        "schema_version": "project-graph-context.d-prior-snapshot.v1",
        "prior_id": f"prepilot-{paired_task}-fixture-v1",
        "task_id": task_id,
        "window": {"start": "2025-01-01T00:00:00Z", "end": "2025-01-31T23:59:59Z", "timezone": "UTC"},
        "evidence": [
            {
                "id": "paired-e1",
                "source": "pre-registered-paired-fixture",
                "claim": "Inspecting the target module and running a focused local check is a safe implementation workflow.",
                "confidence": 0.5,
            }
        ],
        "outcomes": [{"task_id": paired_task, "success": True, "failure_codes": []}],
        "lessons": [{"lesson": "Read the target module before editing and run the focused local check.", "scope": "paired task family"}],
    }
    source_hash = hashlib.sha256(canonical_json(prior_without_hash)).hexdigest()
    prior_without_hash["source_hash"] = source_hash
    return prior_without_hash


def context_payload(task_id: str, arm: str, paired_task: str) -> dict[str, Any]:
    intent = task_manifest(task_id)
    if arm == "A":
        # A intentionally has no curated map.  The task prompt remains
        # available to all arms, but no behavior/source/graph/prior map is.
        return {
            "schema_version": "project-graph-context.arm-context.v1",
            "arm_id": "A",
            "kind": "none",
            "disclosure": "No curated project map, handbook, graph, or prior evidence is exposed in arm A.",
        }
    if arm == "B":
        return {
            "schema_version": "project-graph-context.arm-context.v1",
            "arm_id": "B",
            "kind": "behavior_source_handbook",
            "behavior": [{"id": "behavior.goal", "summary": str(intent["goal"])}],
            "source": [{"id": "source.target", "path": str(intent["allowed_paths"][0]), "summary": "Target module contains the incomplete function."}],
            "handbook": {"scope": "Edit only the allowed target path; run focused offline checks."},
            "disclosure": "Behavior/source handbook only; no execution graph or prior outcomes.",
        }
    graph = _graph_context(task_id, intent)
    if arm == "C":
        return graph
    graph["arm_id"] = "D"
    graph["schema_version"] = "project-graph-context.d-graph-plus-prior.v1"
    graph["prior"] = _prior_snapshot(task_id, paired_task)
    graph["disclosure"] = "Three-layer graph plus a frozen, time-sliced prior from the paired training task only."
    return graph


def _write_contexts(output: Path, tasks: Sequence[str]) -> dict[tuple[str, str], Path]:
    contexts_dir = output / "contexts"
    contexts_dir.mkdir(parents=True, exist_ok=True)
    paths: dict[tuple[str, str], Path] = {}
    for task_id in tasks:
        paired = _pair_for(task_id, tasks)
        for arm in ARMS:
            path = contexts_dir / f"{task_id}-{arm}.json"
            payload = context_payload(task_id, arm, paired)
            if path.exists():
                existing = json.loads(path.read_text(encoding="utf-8"))
                if existing != payload:
                    raise PilotError(f"context already exists with different bytes: {path}")
            else:
                write_json(path, payload)
            paths[(task_id, arm)] = path
    return paths


def _write_intents(output: Path, tasks: Sequence[str]) -> dict[str, Path]:
    intent_dir = output / "intents"
    intent_dir.mkdir(parents=True, exist_ok=True)
    paths: dict[str, Path] = {}
    for task_id in tasks:
        path = intent_dir / f"{task_id}.json"
        payload = task_manifest(task_id)
        if path.exists() and json.loads(path.read_text(encoding="utf-8")) != payload:
            raise PilotError(f"intent already exists with different bytes: {path}")
        if not path.exists():
            write_json(path, payload)
        paths[task_id] = path
    return paths


def _prepare_sources(output: Path, tasks: Sequence[str]) -> dict[str, tuple[Path, str]]:
    sources: dict[str, tuple[Path, str]] = {}
    source_dir = output / "source-repos"
    source_dir.mkdir(parents=True, exist_ok=True)
    for task_id in tasks:
        repo = source_dir / task_id
        marker = repo / ".frozen-commit"
        if repo.exists() and marker.is_file():
            commit = marker.read_text(encoding="ascii").strip()
        else:
            if repo.exists():
                raise PilotError(f"source directory exists without a frozen marker: {repo}")
            materialize_task_repo(task_id, repo)
            commit = marker.read_text(encoding="ascii").strip()
        sources[task_id] = (repo, commit)
    return sources


def _schedule(tasks: Sequence[str], seed: int) -> list[dict[str, Any]]:
    if len(tasks) != 2:
        raise PilotError("this smallest pilot requires exactly two related tasks")
    rng = random.Random(seed)
    base = list(ARMS)
    rng.shuffle(base)
    rows: list[dict[str, Any]] = []
    for index, task_id in enumerate(tasks):
        order = base if index % 2 == 0 else list(reversed(base))
        for order_index, arm in enumerate(order):
            rows.append({"task_id": task_id, "arm_id": arm, "replicate": 0, "order_index": order_index})
    return rows


def _setup(output: Path, tasks: Sequence[str], seed: int) -> dict[str, Any]:
    output.mkdir(parents=True, exist_ok=True)
    intents = _write_intents(output, tasks)
    contexts = _write_contexts(output, tasks)
    sources = _prepare_sources(output, tasks)
    plans_dir = output / "plans"
    plans_dir.mkdir(parents=True, exist_ok=True)
    plan_metadata: list[dict[str, Any]] = []
    for task_id in tasks:
        plan_path = plans_dir / f"{task_id}.json"
        metadata_path = plans_dir / f"{task_id}.metadata.json"
        if plan_path.exists() and metadata_path.exists():
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        else:
            metadata = run_planner(task_id, plans_dir)
        plan_metadata.append(metadata)
    plan_tokens = sum(int((item.get("usage") or {}).get("total_tokens", 0)) for item in plan_metadata)
    projected_worker_tokens = len(tasks) * len(ARMS) * CELL_TOKEN_CAP
    projected_total = plan_tokens + projected_worker_tokens
    if projected_total > PROJECTED_TOTAL_CAP:
        raise PilotError(f"projected token budget {projected_total} exceeds cap {PROJECTED_TOTAL_CAP}; no Luna cells started")
    schedule = _schedule(tasks, seed)
    setup = {
        "experiment_id": EXPERIMENT_ID,
        "tasks": list(tasks),
        "arms": list(ARMS),
        "replicates": 1,
        "seed": seed,
        "schedule": schedule,
        "planner": {"model": PLANNER_MODEL, "reasoning_effort": REASONING_EFFORT, "calls": len(tasks), "plans": plan_metadata},
        "worker": {"model": WORKER_MODEL, "reasoning_effort": REASONING_EFFORT, "token_cap_per_cell": CELL_TOKEN_CAP, "aggregate_token_cap": AGGREGATE_TOKEN_CAP, "timeout_seconds": 120, "max_parallel": 4},
        "projected_worker_tokens": projected_worker_tokens,
        "projected_total_tokens": projected_total,
        "paths": {"plans": str((output / "plans").resolve()), "episodes": str((output / "episodes").resolve())},
        "sources": {task_id: {"repo": str(repo.resolve()), "commit": commit} for task_id, (repo, commit) in sources.items()},
        "intent_hashes": {task_id: sha256_file(path) for task_id, path in intents.items()},
        "context_hashes": {f"{task_id}:{arm}": sha256_file(path) for (task_id, arm), path in contexts.items()},
    }
    existing = output / "setup.json"
    if existing.exists() and json.loads(existing.read_text(encoding="utf-8")) != setup:
        raise PilotError(f"setup exists with different seed/config: {existing}")
    if not existing.exists():
        write_json(existing, setup)
    write_json(output / "schedule.json", {"seed": seed, "schedule": schedule})
    return setup


def _ledger_path(output: Path, row: Mapping[str, Any]) -> Path:
    episode = f"{EXPERIMENT_ID}-{row['arm_id']}-{row['task_id']}-{int(row['replicate'])}"
    return output / "episodes" / episode / "ledger.json"


def _run_cell(output: Path, row: Mapping[str, Any], intents: Mapping[str, Path], contexts: Mapping[tuple[str, str], Path], sources: Mapping[str, tuple[Path, str]]) -> dict[str, Any]:
    task_id = str(row["task_id"])
    arm = str(row["arm_id"])
    os.environ["FRACTAL_SOL_PLAN_DIR"] = str((output / "plans").resolve())
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
        config=RunnerConfig(timeout_seconds=120.0, max_output_bytes=1_000_000, max_repairs=8, max_tokens=CELL_TOKEN_CAP),
    )
    return run_episode(spec)


def _run_rows(output: Path, rows: Sequence[Mapping[str, Any]], *, max_workers: int) -> list[dict[str, Any]]:
    setup_path = output / "setup.json"
    if not setup_path.is_file():
        raise PilotError(f"pilot setup is missing: {setup_path}")
    all_tasks = tuple(str(item) for item in json.loads(setup_path.read_text(encoding="utf-8"))["tasks"])
    # Reuse the complete pair when running a one-cell calibration; context D
    # needs its paired training task even though only one row is executed.
    intents = _write_intents(output, all_tasks)
    contexts = _write_contexts(output, all_tasks)
    sources = _prepare_sources(output, all_tasks)
    pending = [row for row in rows if not _ledger_path(output, row).is_file()]
    if not pending:
        return load_ledgers([output / "episodes"])
    errors: list[str] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=min(max_workers, 4, len(pending))) as executor:
        futures = {executor.submit(_run_cell, output, row, intents, contexts, sources): row for row in pending}
        for future in concurrent.futures.as_completed(futures):
            row = futures[future]
            try:
                future.result()
            except Exception as exc:  # noqa: BLE001 - recorded as infrastructure failure
                errors.append(f"{row['task_id']}/{row['arm_id']}: {type(exc).__name__}: {exc}")
    if errors:
        write_json(output / "infrastructure-failures.json", {"failures": sorted(errors)})
    return load_ledgers([output / "episodes"]) if (output / "episodes").exists() else []


def _calibration_row(setup: Mapping[str, Any]) -> dict[str, Any]:
    for row in setup["schedule"]:
        if row["task_id"] == "calculator-add" and row["arm_id"] == "C":
            return dict(row)
    raise PilotError("calculator-add/C calibration cell is absent from schedule")


def calibration_budget_aborted(calibration: Mapping[str, Any], *, cap: int = CELL_TOKEN_CAP) -> bool:
    """Return true when a calibration cell must block all further cells."""

    result = calibration.get("result")
    if not isinstance(result, Mapping):
        return True
    tokens = result.get("tokens")
    total = tokens.get("total") if isinstance(tokens, Mapping) else None
    return bool(result.get("timed_out")) or result.get("exit_code") not in (0, None) or not isinstance(total, int) or total > cap


def _summary(output: Path, setup: Mapping[str, Any], ledgers: Sequence[Mapping[str, Any]], *, calibration_complete: bool) -> dict[str, Any]:
    report = analyze(ledgers, seed=int(setup["seed"]), min_pairs=10, bootstrap_samples=500) if ledgers else {"no_go": True, "reasons": ["no_ledgers"]}
    checker_failures: Counter[str] = Counter()
    infrastructure: list[dict[str, Any]] = []
    evidence: list[dict[str, Any]] = []
    for ledger in sorted(ledgers, key=lambda item: str(item.get("episode_id"))):
        result = ledger.get("result", {})
        code = result.get("correctness", {}).get("checker_failure_code")
        if code:
            checker_failures[str(code)] += 1
        if result.get("timed_out") or result.get("exit_code") not in (0, None):
            infrastructure.append({"episode_id": ledger.get("episode_id"), "timed_out": bool(result.get("timed_out")), "exit_code": result.get("exit_code")})
        if any(event.get("kind") in {"usage_unavailable", "trace_unavailable"} for event in ledger.get("events", [])):
            infrastructure.append({"episode_id": ledger.get("episode_id"), "telemetry_unavailable": True})
        ledger_path = output / "episodes" / str(ledger["episode_id"]) / "ledger.json"
        evidence.append({"episode_id": ledger.get("episode_id"), "ledger_sha256": sha256_file(ledger_path), "result_evidence_hashes": result.get("evidence_hashes", {})})
    complete_cells = len(ledgers)
    expected_cells = len(setup["tasks"]) * len(ARMS)
    summary = {
        "schema_version": "project-graph-context.live-pilot-summary.v1",
        "experiment_id": setup["experiment_id"],
        "base_harness_commit": "efa0833a4fd82fc527fde0f166e76d1b91a34cb1",
        "scope": {"tasks": setup["tasks"], "arms": setup["arms"], "replicates": 1, "cells_expected": expected_cells, "cells_completed": complete_cells},
        "randomization": {"seed": setup["seed"], "schedule": setup["schedule"], "counterbalance": "second task reverses first task arm order"},
        "planner": setup["planner"],
        "worker": setup["worker"],
        "calibration": {"required_cell": "calculator-add/C/0", "complete": calibration_complete},
        "raw_evidence_hashes": evidence,
        "per_arm_metrics": report.get("raw_arm_metrics", {}),
        "paired_deltas_and_ratios": report.get("comparisons", {}),
        "failure_patterns": {"checker_failure_codes": dict(sorted(checker_failures.items())), "infrastructure_events": infrastructure},
        "analysis": {"no_go": bool(report.get("no_go", True)), "underpowered": bool(report.get("underpowered", True)), "reasons": report.get("reasons", []), "min_pairs": 10},
        "go_no_go": {"decision": "inconclusive", "full_160_cell_study_justified": False, "reason": "Feasibility pilot is under the preregistered 10-pair floor and must first resolve any unavailable live telemetry/safety failures."},
        "limitations": [
            "One related pair and one replicate are exploratory only; no threshold claim is admissible.",
            "Arm D prior is a frozen pre-registered paired-task fixture, not current-episode output.",
            "Codex cost is unavailable when the CLI emits no cost field; absent trace metrics remain unavailable.",
            "Raw prompts, JSONL event streams, usage receipts, and detached worktrees are intentionally excluded from this summary.",
        ],
    }
    return summary


def _cli() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="ignored run directory for plans, ledgers, and worktrees")
    parser.add_argument("--summary-path", type=Path, default=ROOT / "results" / "live-pilot-summary.json")
    parser.add_argument("--task-id", nargs=2, default=list(DEFAULT_TASKS))
    parser.add_argument("--seed", type=int, default=20260802)
    parser.add_argument("--max-workers", type=int, default=4)
    parser.add_argument("--calibrate-only", action="store_true")
    args = parser.parse_args()
    tasks = tuple(args.task_id)
    if len(tasks) != 2 or task_manifest(tasks[0]).get("pair_id") != task_manifest(tasks[1]).get("pair_id"):
        raise SystemExit("exactly two related task ids are required")
    output = args.output.resolve()
    setup = _setup(output, tasks, args.seed)
    if args.calibrate_only:
        row = _calibration_row(setup)
        ledgers = _run_rows(output, [row], max_workers=1)
        calibration_ledger = next((item for item in ledgers if item.get("task_id") == row["task_id"] and item.get("arm_id") == row["arm_id"]), None)
        if calibration_ledger is None:
            raise SystemExit("calibration cell failed to produce a ledger")
        calibration = {
            "schema_version": "project-graph-context.live-calibration-summary.v1",
            "cell": {"task_id": row["task_id"], "arm_id": row["arm_id"], "replicate": row["replicate"]},
            "ledger_sha256": sha256_file(_ledger_path(output, row)),
            "score": score_ledger(calibration_ledger),
            "result": calibration_ledger["result"],
            "budget": {"token_cap": CELL_TOKEN_CAP, "actual_total_tokens": calibration_ledger["result"].get("tokens", {}).get("total"), "aborted": calibration_budget_aborted({"result": calibration_ledger["result"]})},
            "safety_events": [event for event in calibration_ledger.get("events", []) if event.get("kind") in {"context_mounted", "usage_unavailable", "trace_unavailable", "checker_finished"}],
        }
        write_json(output / "calibration-summary.json", calibration)
        print(json.dumps(calibration, indent=2, sort_keys=True))
        return 0
    calibration_path = output / "calibration-summary.json"
    if not calibration_path.is_file():
        raise SystemExit("run --calibrate-only first and inspect its safety/oracle/telemetry summary")
    calibration = json.loads(calibration_path.read_text(encoding="utf-8"))
    if calibration_budget_aborted(calibration):
        # Do not start additional LLM cells after a cap or safety failure.
        blocked = {
            "schema_version": "project-graph-context.live-pilot-summary.v1",
            "experiment_id": setup["experiment_id"],
            "status": "blocked_after_calibration",
            "calibration": calibration,
            "decision": "inconclusive_no_go",
            "reason": "The calibration worker exceeded the actual 20,000-token cell ceiling; remaining cells were not started.",
            "recommendation": "Reduce Codex input overhead or use a CLI-supported hard token budget, then repeat calibration before any A/B/C/D pilot.",
        }
        write_json(args.summary_path.resolve(), blocked)
        print(json.dumps({"summary": str(args.summary_path.resolve()), "cells": 1, "no_go": True, "decision": "inconclusive"}, indent=2, sort_keys=True))
        return 2
    ledgers = _run_rows(output, setup["schedule"], max_workers=args.max_workers)
    summary = _summary(output, setup, ledgers, calibration_complete=True)
    summary_path = args.summary_path.resolve()
    write_json(summary_path, summary)
    print(json.dumps({"summary": str(summary_path), "cells": len(ledgers), "no_go": summary["analysis"]["no_go"], "decision": summary["go_no_go"]["decision"]}, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
