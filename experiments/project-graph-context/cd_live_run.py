#!/usr/bin/env python3
"""Run the authorized live C-vs-D Luna comparison.

Four arm-blind Sol plans and their task versions are copied byte-for-byte from
the verified A-vs-D run.  This driver therefore makes no new Sol calls.  It
materializes fresh source repositories, contexts, worktrees, and Codex homes
for each of the 24 C/D cells, and leaves raw episode material only in the
caller-provided temporary output directory.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import random
import shutil
import sys
from collections import Counter
from pathlib import Path
from typing import Any, Mapping, Sequence

try:
    from .cd_analysis import analyze_cd, sanitized_cell_hashes
    from .corpus import materialize_task_repo, task_ids, task_manifest
    from .live_pilot import context_payload
    from .runner import EpisodeSpec, RunnerConfig, run_episode
    from .scorer import load_ledgers
except ImportError:  # pragma: no cover - direct script execution
    from cd_analysis import analyze_cd, sanitized_cell_hashes
    from corpus import materialize_task_repo, task_ids, task_manifest
    from live_pilot import context_payload
    from runner import EpisodeSpec, RunnerConfig, run_episode
    from scorer import load_ledgers


ROOT = Path(__file__).resolve().parent
ADAPTER = ROOT / "live_adapter.py"
EXPERIMENT_ID = "pgc-live-cd-pilot-20260802"
HARNESS_COMMIT = "04db2c3f86c269917ae16be38c0e3c8efa555370"
SOURCE_RUN_DEFAULT = Path("/tmp/pgc-ad-live.mTXYpP")
TASKS = tuple(task_ids())
ARMS = ("C", "D")
REPLICATES = 3
EXPECTED_CELLS = len(TASKS) * len(ARMS) * REPLICATES
TOKEN_CAP_PER_CELL = 80_000
AGGREGATE_WORKER_CAP = 1_920_000
ALL_AGENT_CAP = 2_250_000
TIMEOUT_SECONDS = 180.0
MAX_PARALLEL = 4
MIN_PAIRS = 10
SEED = 20260802


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


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _verified_oracle_sha256(source_run: Path) -> str:
    """Verify the prior run's copied checker matches this frozen harness."""

    prior = sorted(source_run.glob("episodes/*/hidden-checker/checker.py"))
    if not prior:
        raise RunError("source A-vs-D run has no verifiable hidden-oracle copy")
    prior_hashes = {sha256_file(path) for path in prior}
    if len(prior_hashes) != 1:
        raise RunError("source A-vs-D hidden-oracle copies disagree")
    current = ROOT / "fixtures" / "oracles" / "checker.py"
    if not current.is_file():
        raise RunError("current hidden oracle is missing")
    current_hash = sha256_file(current)
    source_hash = next(iter(prior_hashes))
    if current_hash != source_hash:
        raise RunError(f"hidden oracle changed since A-vs-D run: {current_hash} != {source_hash}")
    return current_hash


def pair_for(task_id: str) -> str:
    pair_id = task_manifest(task_id)["pair_id"]
    for candidate in TASKS:
        if candidate != task_id and task_manifest(candidate)["pair_id"] == pair_id:
            return candidate
    raise RunError(f"missing paired task for {task_id}")


def _source_setup(source_run: Path, seed: int) -> tuple[dict[str, Any], dict[str, Any]]:
    setup_path = source_run / "setup.json"
    report_path = source_run / "ad-live-summary.json"
    if not setup_path.is_file() or not report_path.is_file():
        raise RunError(f"verified A-vs-D artifacts are missing below {source_run}")
    setup = json.loads(setup_path.read_text(encoding="utf-8"))
    report = json.loads(report_path.read_text(encoding="utf-8"))
    if setup.get("schema_version") != "project-graph-context.ad-live-setup.v1":
        raise RunError("source setup is not the expected A-vs-D schema")
    if setup.get("tasks") != list(TASKS) or setup.get("arms") != ["A", "D"] or setup.get("replicates") != REPLICATES:
        raise RunError("source A-vs-D scope differs from the requested C-vs-D scope")
    if int(setup.get("seed")) != seed:
        raise RunError(f"source seed {setup.get('seed')} differs from requested seed {seed}")
    schedule = setup.get("schedule")
    if not isinstance(schedule, list) or len(schedule) != EXPECTED_CELLS:
        raise RunError("source A-vs-D schedule is incomplete")
    plans = setup.get("planner", {}).get("plans", [])
    report_plans = report.get("planner", {}).get("plans", [])
    if len(plans) != len(TASKS) or len(report_plans) != len(TASKS):
        raise RunError("source plan metadata is incomplete")
    report_hashes = {str(item.get("task_id")): item.get("plan_sha256") for item in report_plans}
    for item in plans:
        task_id = str(item.get("task_id"))
        if task_id not in TASKS or item.get("plan_sha256") != report_hashes.get(task_id):
            raise RunError(f"source plan metadata mismatch for {task_id}")
    return setup, report


def _copy_verified_plan_artifacts(output: Path, source_run: Path, source_setup: Mapping[str, Any]) -> tuple[list[dict[str, Any]], int, dict[str, str]]:
    plan_dir = output / "plans"
    plan_dir.mkdir(parents=True, exist_ok=True)
    source_plan_meta = {str(item["task_id"]): item for item in source_setup["planner"]["plans"]}
    hashes: dict[str, str] = {}
    for task_id in TASKS:
        source_plan = source_run / "plans" / f"{task_id}.json"
        source_meta = source_run / "plans" / f"{task_id}.metadata.json"
        target_plan = plan_dir / source_plan.name
        target_meta = plan_dir / source_meta.name
        if not source_plan.is_file() or not source_meta.is_file():
            raise RunError(f"missing frozen plan artifact for {task_id}")
        expected = str(source_plan_meta[task_id]["plan_sha256"])
        actual = sha256_file(source_plan)
        if actual != expected:
            raise RunError(f"plan hash mismatch for {task_id}: {actual} != {expected}")
        if target_plan.exists() and target_plan.read_bytes() != source_plan.read_bytes():
            raise RunError(f"refusing to replace a different frozen plan: {target_plan}")
        if target_meta.exists() and target_meta.read_bytes() != source_meta.read_bytes():
            raise RunError(f"refusing to replace different plan metadata: {target_meta}")
        if not target_plan.exists():
            shutil.copy2(source_plan, target_plan)
        if not target_meta.exists():
            shutil.copy2(source_meta, target_meta)
        hashes[task_id] = actual
    metadata = [dict(source_plan_meta[task_id]) for task_id in TASKS]
    total = sum(int((item.get("usage") or {}).get("total_tokens", 0) or 0) for item in metadata)
    if total != int(source_setup["planner"].get("usage_total_tokens", total)):
        raise RunError("source planner token total is inconsistent")
    return metadata, total, hashes


def _write_or_copy(path: Path, payload: Mapping[str, Any] | None = None, source: Path | None = None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if source is not None:
        if not source.is_file():
            raise RunError(f"missing source artifact: {source}")
        if path.exists() and path.read_bytes() != source.read_bytes():
            raise RunError(f"refusing to replace a different artifact: {path}")
        if not path.exists():
            shutil.copy2(source, path)
        return
    assert payload is not None
    encoded = canonical_json(payload)
    if path.exists() and path.read_bytes() != encoded:
        raise RunError(f"artifact differs from deterministic payload: {path}")
    if not path.exists():
        path.write_bytes(encoded)


def _prepare_files(output: Path, source_run: Path, source_setup: Mapping[str, Any]) -> tuple[dict[str, Path], dict[tuple[str, str], Path], dict[str, tuple[Path, str]]]:
    intents: dict[str, Path] = {}
    contexts: dict[tuple[str, str], Path] = {}
    sources: dict[str, tuple[Path, str]] = {}
    intent_dir = output / "intents"
    context_dir = output / "contexts"
    source_dir = output / "source-repos"
    intent_dir.mkdir(parents=True, exist_ok=True)
    context_dir.mkdir(parents=True, exist_ok=True)
    source_dir.mkdir(parents=True, exist_ok=True)
    source_intent_hashes = source_setup.get("intent_hashes", {})
    source_context_hashes = source_setup.get("context_hashes", {})
    source_commits = source_setup.get("source_commits", {})
    for task_id in TASKS:
        intent_path = intent_dir / f"{task_id}.json"
        source_intent = source_run / "intents" / intent_path.name
        _write_or_copy(intent_path, source=source_intent)
        if sha256_file(intent_path) != source_intent_hashes.get(task_id):
            raise RunError(f"task-version hash changed for {task_id}")
        if json.loads(intent_path.read_text(encoding="utf-8")) != task_manifest(task_id):
            raise RunError(f"source intent is not the current frozen task manifest: {task_id}")
        intents[task_id] = intent_path

        repo = source_dir / task_id
        marker = repo / ".frozen-commit"
        if not repo.exists():
            materialize_task_repo(task_id, repo)
        if not marker.is_file():
            raise RunError(f"source repository lacks frozen marker: {repo}")
        commit = marker.read_text(encoding="ascii").strip()
        if commit != source_commits.get(task_id):
            raise RunError(f"source commit changed for {task_id}: {commit}")
        sources[task_id] = (repo, commit)

        paired = pair_for(task_id)
        c_path = context_dir / f"{task_id}-C.json"
        _write_or_copy(c_path, payload=context_payload(task_id, "C", paired))
        contexts[(task_id, "C")] = c_path
        d_path = context_dir / f"{task_id}-D.json"
        source_d = source_run / "contexts" / d_path.name
        _write_or_copy(d_path, source=source_d)
        if sha256_file(d_path) != source_context_hashes.get(f"{task_id}:D"):
            raise RunError(f"D context hash changed for {task_id}")
        contexts[(task_id, "D")] = d_path
    return intents, contexts, sources


def _schedule(source_setup: Mapping[str, Any], seed: int) -> list[dict[str, Any]]:
    source_rows = source_setup["schedule"]
    rows: list[dict[str, Any]] = []
    for source_row in source_rows:
        arm = str(source_row.get("arm_id"))
        if arm not in {"A", "D"}:
            raise RunError(f"unexpected arm in frozen schedule: {arm}")
        row = dict(source_row)
        row["arm_id"] = "C" if arm == "A" else "D"
        rows.append(row)
    groups: dict[int, list[dict[str, Any]]] = {}
    for row in rows:
        groups.setdefault(int(row["pair_index"]), []).append(row)
    if len(groups) != len(TASKS) * REPLICATES or any(len(pair) != 2 or {item["arm_id"] for item in pair} != {"C", "D"} for pair in groups.values()):
        raise RunError("mapped schedule does not contain exactly one C/D observation per pair")
    c_first = sum(1 for pair in groups.values() if min(pair, key=lambda item: int(item["order_index"]))["arm_id"] == "C")
    if c_first != len(groups) // 2:
        raise RunError(f"counterbalance is not 6 C-first / 6 D-first: {c_first}")
    # Keep the source's deterministic order; the seed is recorded and checked.
    if int(source_setup.get("seed")) != seed:
        raise RunError("schedule seed mismatch")
    return rows


def _setup(output: Path, source_run: Path, seed: int) -> dict[str, Any]:
    output.mkdir(parents=True, exist_ok=True)
    source_setup, source_report = _source_setup(source_run, seed)
    oracle_sha256 = _verified_oracle_sha256(source_run)
    intents, contexts, sources = _prepare_files(output, source_run, source_setup)
    planner_plans, planner_tokens, plan_hashes = _copy_verified_plan_artifacts(output, source_run, source_setup)
    schedule = _schedule(source_setup, seed)
    projected = planner_tokens + EXPECTED_CELLS * TOKEN_CAP_PER_CELL
    if projected > ALL_AGENT_CAP:
        raise RunError(f"projected all-agent tokens {projected} exceed hard cap {ALL_AGENT_CAP}; no Luna cells started")
    setup_payload = {
        "schema_version": "project-graph-context.cd-live-setup.v1",
        "experiment_id": EXPERIMENT_ID,
        "harness_commit": HARNESS_COMMIT,
        "tasks": list(TASKS),
        "arms": list(ARMS),
        "replicates": REPLICATES,
        "seed": seed,
        "schedule": schedule,
        "planner": {
            "model": "gpt-5.6-sol",
            "reasoning_effort": "high",
            "calls": 0,
            "reused_calls": len(planner_plans),
            "usage_total_tokens": planner_tokens,
            "plans": planner_plans,
            "plan_hashes": plan_hashes,
            "source_run_artifact": "verified A-vs-D pilot plans (byte-for-byte)",
            "source_harness_commit": source_setup.get("harness_commit"),
        },
        "worker": {
            "model": "gpt-5.6-luna",
            "reasoning_effort": "high",
            "token_cap_per_cell": TOKEN_CAP_PER_CELL,
            "aggregate_worker_cap": AGGREGATE_WORKER_CAP,
            "timeout_seconds": TIMEOUT_SECONDS,
            "max_parallel": MAX_PARALLEL,
        },
        "projected_all_agent_tokens": projected,
        "source_commits": {task: commit for task, (_repo, commit) in sources.items()},
        "intent_hashes": {task: sha256_file(path) for task, path in intents.items()},
        "context_hashes": {f"{task}:{arm}": sha256_file(path) for (task, arm), path in contexts.items()},
        "reused_source": {
            "scope": "A-vs-D Sol plan/task-version/schedule artifacts",
            "source_setup_schema": source_setup.get("schema_version"),
            "source_report_schema": source_report.get("schema_version"),
            "source_seed": source_setup.get("seed"),
            "hidden_oracle_sha256": oracle_sha256,
        },
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


def _context_hash_from_events(ledger: Mapping[str, Any]) -> str | None:
    for event in ledger.get("events", []):
        if event.get("kind") == "context_mounted":
            return str(event.get("data", {}).get("context_sha256"))
    return None


def infra_reason(output: Path, ledger: Mapping[str, Any], setup_payload: Mapping[str, Any]) -> str | None:
    result = ledger.get("result", {})
    if result.get("timed_out"):
        return "timed_out_or_budget"
    if result.get("exit_code") not in (0, None):
        return "worker_exit_nonzero"
    tokens = result.get("tokens", {})
    if not isinstance(tokens, Mapping) or not tokens.get("available"):
        return "usage_unavailable"
    expected_context = setup_payload.get("context_hashes", {}).get(f"{ledger.get('task_id')}:{ledger.get('arm_id')}")
    mounted_context = _context_hash_from_events(ledger)
    if expected_context and mounted_context and mounted_context != expected_context:
        return "context_hash_mismatch"
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


def safety_reason(output: Path, ledger: Mapping[str, Any]) -> str | None:
    trace = output / "episodes" / str(ledger["episode_id"]) / "worker-trace.json"
    if trace.is_file():
        try:
            payload = json.loads(trace.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return "trace_invalid"
        if payload.get("git_mutation_detected"):
            return "git_mutation_detected"
    violations = ledger.get("result", {}).get("intent_violations", {})
    if isinstance(violations, Mapping) and int(violations.get("severe", 0) or 0) > 0:
        return "scope_violation_safety"
    return None


def _token_total(ledger: Mapping[str, Any]) -> int:
    value = ledger.get("result", {}).get("tokens", {}).get("total")
    return int(value) if isinstance(value, int) and not isinstance(value, bool) else 0


def run_cells(output: Path, setup_payload: Mapping[str, Any]) -> tuple[list[dict[str, Any]], list[dict[str, Any]], int, str | None, list[dict[str, Any]]]:
    # The setup already validated all artifacts.  Resolve them without touching
    # the verified source run again; this also makes resume deterministic.
    intents = {task: output / "intents" / f"{task}.json" for task in TASKS}
    contexts = {(task, arm): output / "contexts" / f"{task}-{arm}.json" for task in TASKS for arm in ARMS}
    sources = {task: (output / "source-repos" / task, str(setup_payload["source_commits"][task])) for task in TASKS}
    os.environ["FRACTAL_SOL_PLAN_DIR"] = str((output / "plans").resolve())
    os.environ["FRACTAL_LIVE_TIMEOUT_SECONDS"] = "175"
    rows = list(setup_payload["schedule"])
    pending = [row for row in rows if not (episode_path(output, row) / "ledger.json").is_file()]
    completed_ledgers = load_ledgers([output / "episodes"]) if (output / "episodes").is_dir() else []
    actual_total = sum(_token_total(ledger) for ledger in completed_ledgers)
    failures: list[dict[str, Any]] = []
    safety_events: list[dict[str, Any]] = []
    stop_reason: str | None = None
    processed = len(completed_ledgers)
    planner_tokens = int(setup_payload["planner"].get("usage_total_tokens", 0) or 0)
    if planner_tokens + actual_total > ALL_AGENT_CAP or actual_total > AGGREGATE_WORKER_CAP:
        return sorted(completed_ledgers, key=lambda item: str(item.get("episode_id"))), failures, actual_total, "aggregate_actual_tokens_exceeded", safety_events
    for start in range(0, len(pending), MAX_PARALLEL):
        if stop_reason:
            break
        batch = pending[start : start + MAX_PARALLEL]
        with concurrent.futures.ThreadPoolExecutor(max_workers=len(batch)) as executor:
            futures = {executor.submit(run_cell, output, row, intents, contexts, sources): row for row in batch}
            for future in concurrent.futures.as_completed(futures):
                row = futures[future]
                processed += 1
                try:
                    ledger = future.result()
                    completed_ledgers.append(ledger)
                    actual_total += _token_total(ledger)
                    reason = infra_reason(output, ledger, setup_payload)
                    if reason:
                        failures.append({"episode_id": ledger.get("episode_id"), "reason": reason})
                    safety = safety_reason(output, ledger)
                    if safety:
                        safety_events.append({"episode_id": ledger.get("episode_id"), "reason": safety})
                except Exception as exc:  # noqa: BLE001 - retain evidence and stop safely
                    failures.append({"task_id": row.get("task_id"), "arm_id": row.get("arm_id"), "replicate": row.get("replicate"), "reason": "runner_exception", "detail": f"{type(exc).__name__}: {exc}"})
                failure_rate = len(failures) / max(1, processed)
                if actual_total > AGGREGATE_WORKER_CAP or planner_tokens + actual_total > ALL_AGENT_CAP:
                    stop_reason = "aggregate_actual_tokens_exceeded"
                elif safety_events:
                    stop_reason = "safety_or_git_mutation_signal"
                elif failure_rate > 0.20:
                    stop_reason = "infrastructure_failures_exceeded_20_percent"
        completed_count = len(completed_ledgers)
        if completed_count in (4, 12):
            failure_rate = len(failures) / max(1, processed)
            print(json.dumps({"progress": "checkpoint", "completed_cells": completed_count, "expected_cells": EXPECTED_CELLS, "processed_cells": processed, "actual_worker_tokens": actual_total, "all_agent_actual_tokens_including_reused_plans": planner_tokens + actual_total, "infrastructure_failures": len(failures), "failure_rate": failure_rate, "safety_signals": len(safety_events)}, sort_keys=True), flush=True)
        if len(failures) / max(1, processed) > 0.20 and stop_reason is None:
            stop_reason = "infrastructure_failures_exceeded_20_percent"
        if stop_reason:
            break
    return sorted(completed_ledgers, key=lambda item: str(item.get("episode_id"))), failures, actual_total, stop_reason, safety_events


def make_summary(output: Path, setup_payload: Mapping[str, Any], ledgers: Sequence[Mapping[str, Any]], failures: Sequence[Mapping[str, Any]], actual_total: int, stop_reason: str | None, safety_events: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    analysis = analyze_cd(ledgers, seed=int(setup_payload["seed"]), min_pairs=MIN_PAIRS, bootstrap_samples=2000)
    checker_failures: Counter[str] = Counter()
    changed_paths: Counter[str] = Counter()
    for ledger in ledgers:
        code = ledger.get("result", {}).get("correctness", {}).get("checker_failure_code")
        if code:
            checker_failures[str(code)] += 1
        changed_paths.update(str(path) for path in ledger.get("result", {}).get("changed_paths", []))
    complete = len(ledgers) == EXPECTED_CELLS
    telemetry_complete = all(item.get("result", {}).get("tokens", {}).get("available") for item in ledgers) if ledgers else False
    cell_hashes = sanitized_cell_hashes(ledgers, output / "episodes")
    aggregate_hash = sha256_bytes(canonical_json([{"episode_id": row["episode_id"], "ledger_sha256": row["ledger_sha256"]} for row in cell_hashes]))
    safe_run = complete and not failures and not safety_events and telemetry_complete and stop_reason is None
    summary = {
        "schema_version": "project-graph-context.cd-live-summary.v1",
        "experiment_id": EXPERIMENT_ID,
        "harness_commit": setup_payload["harness_commit"],
        "scope": {"tasks": list(TASKS), "arms": list(ARMS), "replicates": REPLICATES, "expected_cells": EXPECTED_CELLS, "completed_cells": len(ledgers), "paired_observations": len({(item.get("task_id"), item.get("replicate")) for item in ledgers})},
        "randomization": {"seed": setup_payload["seed"], "schedule": setup_payload["schedule"], "counterbalance": "reused A/D order mapped A→C; 6 C-first and 6 D-first pairs"},
        "planner": setup_payload["planner"],
        "worker": setup_payload["worker"],
        "provenance": {"reused": "A-vs-D plans, task versions, hidden-oracle corpus, and counterbalanced schedule", "new": "C contexts and all 24 isolated Luna episodes", "plan_hashes": setup_payload["planner"]["plan_hashes"], "context_hashes": setup_payload["context_hashes"], "source_commits": setup_payload["source_commits"], "hidden_oracle_sha256": setup_payload["reused_source"].get("hidden_oracle_sha256")},
        "token_accounting": {"reused_planner_tokens": int(setup_payload["planner"].get("usage_total_tokens", 0) or 0), "worker_actual_tokens": actual_total, "all_agent_actual_tokens": actual_total + int(setup_payload["planner"].get("usage_total_tokens", 0) or 0), "worker_aggregate_cap": AGGREGATE_WORKER_CAP, "all_agent_hard_cap": ALL_AGENT_CAP},
        "analysis": analysis,
        "failure_patterns": {"checker_failure_codes": dict(sorted(checker_failures.items())), "changed_paths": dict(sorted(changed_paths.items())), "infrastructure_failures": list(failures), "safety_signals": list(safety_events), "stop_reason": stop_reason},
        "cell_hashes": cell_hashes,
        "aggregate_cell_ledger_sha256": aggregate_hash,
        "safety": {"hidden_checker_exposed": False, "current_eval_outcomes_in_D": False, "raw_prompts_committed": False, "raw_traces_committed": False, "all_cells_complete": complete, "token_telemetry_complete": telemetry_complete, "infrastructure_failure_rate": len(failures) / max(1, len(ledgers)), "safety_signals": len(safety_events), "isolated_codex_home_per_cell": True, "no_external_side_effects": True},
        "decision": {"cd_thresholds": analysis["decision"], "production_go": False, "recommendation": "No production go from this pilot; report threshold status and treat unavailable/zero-denominator criteria as untestable." if safe_run else "No production go; run stopped or incomplete and requires review.", "run_integrity": "complete" if safe_run else "incomplete_or_flagged"},
        "limitations": ["Dollar cost receipts were unavailable; token and wall-clock proxies are separate metrics.", "Repeated-failure ratio is untestable when C's count is zero; zero is never imputed as a pass.", "Routing/tool quality remains untestable when workers emit no structured routing/tool telemetry.", "Raw prompts, event streams, usage receipts, hidden oracles, and worktrees remain outside the repository."],
    }
    return summary


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-run", type=Path, default=SOURCE_RUN_DEFAULT)
    parser.add_argument("--summary-path", type=Path, default=ROOT / "results" / "cd-live-summary.json")
    parser.add_argument("--seed", type=int, default=SEED)
    args = parser.parse_args(argv)
    if len(TASKS) != 4 or set(ARMS) != {"C", "D"}:
        raise SystemExit("corpus or arm set changed; refusing this preregistered scope")
    output = args.output.resolve()
    setup_payload = _setup(output, args.source_run.resolve(), args.seed)
    ledgers, failures, actual_total, stop_reason, safety_events = run_cells(output, setup_payload)
    summary = make_summary(output, setup_payload, ledgers, failures, actual_total, stop_reason, safety_events)
    summary_path = args.summary_path.resolve()
    write_json(summary_path, summary)
    print(json.dumps({"summary": str(summary_path), "completed_cells": len(ledgers), "paired_observations": summary["scope"]["paired_observations"], "actual_worker_tokens": actual_total, "all_agent_actual_tokens": summary["token_accounting"]["all_agent_actual_tokens"], "stop_reason": stop_reason, "decision": summary["decision"]}, indent=2, sort_keys=True), flush=True)
    return 0 if stop_reason is None and len(ledgers) == EXPECTED_CELLS else 2


if __name__ == "__main__":
    raise SystemExit(main())
