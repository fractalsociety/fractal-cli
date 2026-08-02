#!/usr/bin/env python3
"""Descriptive preregistered analysis for the direct A-vs-D pilot.

The protocol's primary contrasts are C-vs-A and D-vs-C.  A-vs-D therefore
cannot decompose graph exposure from learning/prior exposure; this module
reports paired descriptive deltas/ratios and uncertainty without relabelling
them as a preregistered pass.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import random
from collections import Counter, defaultdict
from pathlib import Path
from statistics import mean
from typing import Any, Mapping, Sequence

try:
    from .analysis import bootstrap_ci
    from .scorer import load_ledgers, score_ledger
except ImportError:  # pragma: no cover - direct script execution
    from analysis import bootstrap_ci
    from scorer import load_ledgers, score_ledger


ANALYSIS_VERSION = "project-graph-context.ad-analysis.v1"


def _numeric(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(float(value)):
        return None
    return float(value)


def _routing(value: Any) -> float | None:
    if not isinstance(value, Mapping):
        return None
    direct = _numeric(value.get("quality"))
    if direct is not None:
        return direct
    if value.get("correct_route") is True:
        return 1.0
    if value.get("correct_route") is False:
        return 0.0
    return None


def _tool(value: Any) -> float | None:
    if not isinstance(value, Mapping):
        return None
    direct = _numeric(value.get("quality"))
    if direct is not None:
        return direct
    if value.get("selected_relevant") is True:
        return 1.0
    if value.get("selected_relevant") is False:
        return 0.0
    return None


def _metric(score: Mapping[str, Any], name: str) -> float | None:
    if name in {"success", "complete_task_success"}:
        passed = bool(score.get("success"))
        complete = passed and not bool(score.get("timed_out"))
        return 1.0 if (complete if name == "complete_task_success" else passed) else 0.0
    if name == "wall_time_ms":
        return _numeric(score.get("duration_ms"))
    if name == "routing_quality":
        return _routing(score.get("routing"))
    if name == "tool_quality":
        return _tool(score.get("tool_selection"))
    mapping = {
        "intent_violations_weighted": "intent_violations_weighted",
        "irrelevant_opens": "irrelevant_opens",
        "tokens": "tokens",
        "repair_iterations": "repair_iterations",
        "repeated_failure_codes": "repeated_failure_codes",
        "cost_usd": "cost_usd",
    }
    return _numeric(score.get(mapping.get(name, name)))


def _ratio(target: float | None, baseline: float | None) -> float | None:
    if target is None or baseline is None or baseline == 0:
        return 1.0 if target == baseline == 0 else None
    return target / baseline


def paired_rows(ledgers: Sequence[Mapping[str, Any]], metric: str) -> list[dict[str, Any]]:
    grouped: dict[tuple[str, int], dict[str, Mapping[str, Any]]] = defaultdict(dict)
    for ledger in ledgers:
        grouped[(str(ledger["task_id"]), int(ledger.get("replicate", 0)))][str(ledger["arm_id"])] = score_ledger(ledger)
    rows: list[dict[str, Any]] = []
    for (task_id, replicate), arms in sorted(grouped.items()):
        a = _metric(arms["A"], metric) if "A" in arms else None
        d = _metric(arms["D"], metric) if "D" in arms else None
        rows.append({"task_id": task_id, "replicate": replicate, "A": a, "D": d, "delta_D_minus_A": d - a if d is not None and a is not None else None, "ratio_D_over_A": _ratio(d, a)})
    return rows


METRICS = (
    "success",
    "complete_task_success",
    "intent_violations_weighted",
    "irrelevant_opens",
    "tokens",
    "wall_time_ms",
    "repair_iterations",
    "repeated_failure_codes",
    "routing_quality",
    "tool_quality",
    "cost_usd",
)


def analyze_ad(ledgers: Sequence[Mapping[str, Any]], *, seed: int = 20260802, min_pairs: int = 10, bootstrap_samples: int = 2000) -> dict[str, Any]:
    ordered = sorted(ledgers, key=lambda item: (str(item.get("task_id")), int(item.get("replicate", 0)), str(item.get("arm_id")), str(item.get("episode_id"))))
    scores = [score_ledger(item) for item in ordered]
    arm_metrics: dict[str, Any] = {}
    for arm in ("A", "D"):
        arm_scores = [score for score in scores if score["arm_id"] == arm]
        raw: dict[str, Any] = {"n": len(arm_scores)}
        for offset, metric in enumerate(METRICS):
            values = [value for value in (_metric(score, metric) for score in arm_scores) if value is not None]
            raw[metric] = {"n_available": len(values), "n_missing": len(arm_scores) - len(values), "mean": mean(values) if values else None, "uncertainty": bootstrap_ci(values, seed=seed + offset + ord(arm), samples=bootstrap_samples)}
        arm_metrics[arm] = raw
    comparisons: dict[str, Any] = {}
    for offset, metric in enumerate(METRICS):
        rows = paired_rows(ordered, metric)
        complete = [row for row in rows if row["A"] is not None and row["D"] is not None]
        deltas = [float(row["delta_D_minus_A"]) for row in complete]
        ratios = [float(row["ratio_D_over_A"]) for row in complete if row["ratio_D_over_A"] is not None]
        aggregate_rate_ratio: float | None = None
        ratio_uncertainty_values = ratios
        if metric in {"success", "complete_task_success"} and complete:
            a_rate = mean(float(row["A"]) for row in complete)
            d_rate = mean(float(row["D"]) for row in complete)
            aggregate_rate_ratio = _ratio(d_rate, a_rate)
            # A zero A episode has no pairwise ratio, but it is valid in the
            # Bernoulli rate.  Preserve both the pairwise list and the
            # aggregate estimand explicitly instead of silently dropping it.
            ratio_uncertainty_values = [aggregate_rate_ratio] if aggregate_rate_ratio is not None else []
        comparisons[metric] = {
            "pair_total": len(rows),
            "paired_n": len(complete),
            "deltas": deltas,
            "ratios": ratios,
            "mean_delta_D_minus_A": mean(deltas) if deltas else None,
            "mean_ratio_D_over_A": aggregate_rate_ratio if aggregate_rate_ratio is not None else (mean(ratios) if ratios else None),
            "aggregate_rate_ratio_D_over_A": aggregate_rate_ratio,
            "delta_ci": bootstrap_ci(deltas, seed=seed + offset * 13, samples=bootstrap_samples),
            "ratio_ci": bootstrap_ci(ratio_uncertainty_values, seed=seed + offset * 17 + 3, samples=bootstrap_samples),
            "missing": len(complete) != len(rows) or (metric not in {"success", "complete_task_success"} and len(ratios) != len(complete)),
        }
    reasons: list[str] = []
    if len({(str(item.get("task_id")), int(item.get("replicate", 0))) for item in ordered}) < min_pairs:
        reasons.append(f"underpowered:pairs<{min_pairs}")
    if any(item["missing"] for item in comparisons.values()):
        reasons.append("missing:required-metrics")
    return {
        "analysis_version": ANALYSIS_VERSION,
        "contrast": "D_vs_A",
        "estimand_note": "Descriptive direct A-vs-D only; cannot decompose graph exposure from prior/learning exposure.",
        "raw_arm_metrics": arm_metrics,
        "paired": comparisons,
        "pair_count": len({(str(item.get("task_id")), int(item.get("replicate", 0))) for item in ordered}),
        "min_pairs": min_pairs,
        "decision": "inconclusive" if reasons else "descriptive_only",
        "no_go": True,
        "reasons": sorted(set(reasons)) or ["direct_A_vs_D_is_not_a_preregistered_pass_contrast"],
    }


def sanitized_cell_hashes(ledgers: Sequence[Mapping[str, Any]], ledger_root: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for ledger in sorted(ledgers, key=lambda item: str(item.get("episode_id"))):
        path = ledger_root / str(ledger["episode_id"]) / "ledger.json"
        rows.append({"episode_id": ledger["episode_id"], "task_id": ledger["task_id"], "arm_id": ledger["arm_id"], "replicate": ledger.get("replicate", 0), "ledger_sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "result": {"success": ledger["result"]["correctness"]["passed"], "checker_failure_code": ledger["result"]["correctness"].get("checker_failure_code"), "changed_paths": ledger["result"].get("changed_paths", []), "intent_violations": ledger["result"].get("intent_violations"), "tokens": ledger["result"].get("tokens"), "timed_out": ledger["result"].get("timed_out"), "exit_code": ledger["result"].get("exit_code"), "evidence_hashes": ledger["result"].get("evidence_hashes", {})}})
    return rows


def _cli() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger_root", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seed", type=int, default=20260802)
    parser.add_argument("--min-pairs", type=int, default=10)
    args = parser.parse_args()
    ledgers = load_ledgers([args.ledger_root])
    report = analyze_ad(ledgers, seed=args.seed, min_pairs=args.min_pairs)
    report["cell_hashes"] = sanitized_cell_hashes(ledgers, args.ledger_root / "episodes" if (args.ledger_root / "episodes").is_dir() else args.ledger_root)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({"output": str(args.output), "pairs": report["pair_count"], "decision": report["decision"], "no_go": report["no_go"]}, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
