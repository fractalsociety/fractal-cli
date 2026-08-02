#!/usr/bin/env python3
"""Commands for calibration, scripted feasibility, analysis, and live planning."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from analysis import analyze  # noqa: E402
from corpus import materialize_task_repo, task_ids, task_manifest
from runner import EpisodeSpec, RunnerConfig, run_episode
from scorer import load_ledgers


ARMS = ("A", "B", "C", "D")


def _context(arm: str, task_id: str, destination: Path) -> Path:
    destination.parent.mkdir(parents=True, exist_ok=True)
    if arm == "A":
        payload: dict[str, Any] = {"schema_version": "project-graph-context.arm-context.v1", "arm_id": arm, "kind": "search_only", "search": {"queries": [task_id, "allowed source files"], "max_results": 8}}
    elif arm == "B":
        payload = {"schema_version": "project-graph-context.arm-context.v1", "arm_id": arm, "kind": "behavior_source_handbook", "behavior": [{"id": "behavior.goal", "summary": f"Satisfy observable behavior for {task_id}."}], "source": [{"id": "source.target", "path": task_manifest(task_id)["allowed_paths"][0], "summary": "Target module contains incomplete implementation."}], "handbook": {"scope": "Edit only allowed paths and run focused checks."}}
    else:
        payload = {"schema_version": "project-graph-context.c-graph-context.v1", "task_id": task_id, "layers": {"behavior": [{"id": "behavior.goal", "kind": "acceptance", "summary": "Satisfy the observable task behavior."}], "source": [{"id": "source.target", "kind": "file", "summary": "Edit the target file only.", "path": task_manifest(task_id)["allowed_paths"][0], "line": 1}], "execution": [{"id": "execution.check", "kind": "checker", "summary": "Run the focused hidden checker."}]}, "edges": [{"from": "behavior.goal", "to": "source.target", "relation": "implemented_by"}, {"from": "source.target", "to": "execution.check", "relation": "verified_by"}]}
        if arm == "D":
            payload = {"schema_version": "project-graph-context.d-prior-snapshot.v1", "prior_id": f"prior-{task_id}", "task_id": task_id, "window": {"start": "2025-01-01T00:00:00Z", "end": "2025-01-31T23:59:59Z", "timezone": "UTC"}, "evidence": [{"id": "e1", "source": "calibration", "claim": "Focused checker catches incorrect behavior.", "confidence": 0.8}], "outcomes": [{"task_id": task_id, "success": True, "failure_codes": []}], "lessons": [{"lesson": "Read target then run focused checker.", "scope": "same task family"}]}
    destination.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return destination


def _intent(task_id: str, destination: Path) -> Path:
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(task_manifest(task_id), indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return destination


def _run_pilot(*, output: Path, replicates: int, tasks: list[str], timeout_seconds: float, dry_run: bool = False) -> list[dict[str, Any]]:
    output.mkdir(parents=True, exist_ok=True)
    ledgers: list[dict[str, Any]] = []
    worker = HERE / "scripted_worker.py"
    with tempfile.TemporaryDirectory(prefix="pgc-corpus-") as scratch:
        scratch_path = Path(scratch)
        for task_id in tasks:
            source = materialize_task_repo(task_id, scratch_path / f"source-{task_id}")
            commit = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
            intent = _intent(task_id, scratch_path / "intents" / f"{task_id}.json")
            for arm in ARMS:
                context = _context(arm, task_id, scratch_path / "contexts" / f"{arm}-{task_id}.json")
                for replicate in range(replicates):
                    spec = EpisodeSpec(
                        experiment_id="pgc-scripted-pilot",
                        arm_id=arm,
                        task_id=task_id,
                        source_repo=source,
                        frozen_commit=commit,
                        worker_command=(sys.executable, str(worker), "--task-id", task_id),
                        context_source=context,
                        intent_source=intent,
                        output_root=output,
                        replicate=replicate,
                        config=RunnerConfig(timeout_seconds=timeout_seconds, max_output_bytes=1_000_000, max_repairs=8, keep_worktree=False, dry_run=dry_run),
                    )
                    ledgers.append(run_episode(spec))
    (output / "pilot-ledgers.json").write_text(json.dumps(ledgers, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return ledgers


def cmd_calibrate(args: argparse.Namespace) -> int:
    tasks = task_ids()[:2]
    ledgers = _run_pilot(output=args.output, replicates=1, tasks=tasks, timeout_seconds=10)
    report = analyze(ledgers, seed=1729, min_pairs=1, bootstrap_samples=200)
    report["mode"] = "scripted-feasibility-calibration"
    report["llm_telemetry"] = "not applicable: scripted adapter"
    (args.output / "calibration-report.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({"mode": report["mode"], "episodes": len(ledgers), "output": str(args.output), "no_go": report["no_go"]}, indent=2, sort_keys=True))
    return 0


def cmd_scripted(args: argparse.Namespace) -> int:
    tasks = task_ids()
    ledgers = _run_pilot(output=args.output, replicates=args.replicates, tasks=tasks, timeout_seconds=args.timeout_seconds)
    report = analyze(ledgers, seed=1729, min_pairs=args.min_pairs, bootstrap_samples=args.bootstrap_samples)
    report["mode"] = "scripted-feasibility-pilot"
    report["llm_telemetry"] = "not applicable: scripted adapter emits synthetic worker receipts"
    report["live_run_authorized"] = False
    (args.output / "analysis.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({"mode": report["mode"], "episodes": len(ledgers), "output": str(args.output), "no_go": report["no_go"], "reasons": report["reasons"]}, indent=2, sort_keys=True))
    return 0


def cmd_analyze(args: argparse.Namespace) -> int:
    ledgers = load_ledgers(args.ledger)
    report = analyze(ledgers, seed=args.seed, min_pairs=args.min_pairs, bootstrap_samples=args.bootstrap_samples)
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


def cmd_live_plan(args: argparse.Namespace) -> int:
    cells = 4 * len(args.task_id) * args.replicates
    command = {
        "planner": "Sol-high",
        "worker": "Luna",
        "arms": list(ARMS),
        "tasks": args.task_id,
        "replicates": args.replicates,
        "cells": cells,
        "offline": args.offline,
        "requires_root_approval": True,
        "command": "Provide the approved Sol-high planner/Luna worker argv as an explicit list to runner.py; no shell string or network fallback.",
    }
    print(json.dumps(command, indent=2, sort_keys=True))
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    calibrate = sub.add_parser("calibrate", help="run tiny deterministic scripted calibration")
    calibrate.add_argument("--output", type=Path, default=Path("experiments/project-graph-context/runs/calibration"))
    calibrate.set_defaults(func=cmd_calibrate)
    scripted = sub.add_parser("scripted-pilot", help="run deterministic non-LLM feasibility pilot")
    scripted.add_argument("--output", type=Path, default=Path("experiments/project-graph-context/runs/scripted-pilot"))
    scripted.add_argument("--replicates", type=int, default=2)
    scripted.add_argument("--timeout-seconds", type=float, default=10)
    scripted.add_argument("--min-pairs", type=int, default=10)
    scripted.add_argument("--bootstrap-samples", type=int, default=500)
    scripted.set_defaults(func=cmd_scripted)
    analyze_parser = sub.add_parser("analyze", help="analyze ledger directories/files")
    analyze_parser.add_argument("ledger", type=Path, nargs="+")
    analyze_parser.add_argument("--output", type=Path)
    analyze_parser.add_argument("--seed", type=int, default=1729)
    analyze_parser.add_argument("--min-pairs", type=int, default=10)
    analyze_parser.add_argument("--bootstrap-samples", type=int, default=2000)
    analyze_parser.set_defaults(func=cmd_analyze)
    live = sub.add_parser("live-pilot", help="print a Sol/Luna pilot plan; never runs it")
    live.add_argument("--task-id", nargs="+", default=task_ids())
    live.add_argument("--replicates", type=int, default=10)
    live.add_argument("--offline", action="store_true", default=True)
    live.set_defaults(func=cmd_live_plan)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())

