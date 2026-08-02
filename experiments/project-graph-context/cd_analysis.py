#!/usr/bin/env python3
"""Paired, preregistered analysis for the live C-vs-D pilot.

The C arm exposes the three-layer behavior/source/execution graph.  D adds
only the frozen, time-sliced paired-task prior.  This module keeps unavailable
telemetry unavailable and never treats a zero denominator as a passing ratio;
that distinction matters for the failure-count threshold in this pilot.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
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


ANALYSIS_VERSION = "project-graph-context.cd-analysis.v1"
ARMS = ("C", "D")
METRICS = (
    "success",
    "complete_task_success",
    "repeated_failure_codes",
    "tokens",
    "wall_time_ms",
    "cost_usd",
    "intent_violations_weighted",
    "irrelevant_opens",
    "repair_iterations",
    "routing_quality",
    "tool_quality",
)


def _numeric(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    value = float(value)
    return value if math.isfinite(value) else None


def _metric(score: Mapping[str, Any], name: str) -> float | None:
    if name == "success":
        return 1.0 if bool(score.get("success")) else 0.0
    if name == "complete_task_success":
        return 1.0 if bool(score.get("success")) and not bool(score.get("timed_out")) else 0.0
    if name == "wall_time_ms":
        return _numeric(score.get("duration_ms"))
    if name == "routing_quality":
        value = score.get("routing")
        if not isinstance(value, Mapping):
            return None
        quality = _numeric(value.get("quality"))
        if quality is not None:
            return quality
        if value.get("correct_route") is True:
            return 1.0
        if value.get("correct_route") is False:
            return 0.0
        return None
    if name == "tool_quality":
        value = score.get("tool_selection")
        if not isinstance(value, Mapping):
            return None
        quality = _numeric(value.get("quality"))
        if quality is not None:
            return quality
        if value.get("selected_relevant") is True:
            return 1.0
        if value.get("selected_relevant") is False:
            return 0.0
        return None
    return _numeric(score.get(name))


def _ratio(target: float | None, baseline: float | None) -> float | None:
    """Return a ratio only when the denominator is strictly positive.

    A zero denominator is not a favorable outcome: the preregistered ratio
    threshold is untestable when C has no observed events, even if D is also
    zero.  This prevents a silent ``0/0 == 1`` imputation from passing.
    """

    if target is None or baseline is None or baseline <= 0:
        return None
    return target / baseline


def _pair_rows(ledgers: Sequence[Mapping[str, Any]], metric: str) -> list[dict[str, Any]]:
    grouped: dict[tuple[str, int], dict[str, Mapping[str, Any]]] = defaultdict(dict)
    for ledger in ledgers:
        score = score_ledger(ledger)
        grouped[(str(score["task_id"]), int(score.get("replicate", 0)))][str(score["arm_id"])] = score
    rows: list[dict[str, Any]] = []
    for (task_id, replicate), arms in sorted(grouped.items()):
        c = _metric(arms["C"], metric) if "C" in arms else None
        d = _metric(arms["D"], metric) if "D" in arms else None
        rows.append(
            {
                "task_id": task_id,
                "replicate": replicate,
                "C": c,
                "D": d,
                "delta_D_minus_C": d - c if c is not None and d is not None else None,
                "ratio_D_over_C": _ratio(d, c),
            }
        )
    return rows


def _arm_metrics(ledgers: Sequence[Mapping[str, Any]], seed: int, samples: int) -> dict[str, Any]:
    scores = [score_ledger(ledger) for ledger in ledgers]
    result: dict[str, Any] = {}
    for arm in ARMS:
        arm_scores = [score for score in scores if score["arm_id"] == arm]
        raw: dict[str, Any] = {"n": len(arm_scores)}
        for offset, metric in enumerate(METRICS):
            values = [value for value in (_metric(score, metric) for score in arm_scores) if value is not None]
            raw[metric] = {
                "n_available": len(values),
                "n_missing": len(arm_scores) - len(values),
                "mean": mean(values) if values else None,
                "uncertainty": bootstrap_ci(values, seed=seed + offset + ord(arm), samples=samples),
            }
        result[arm] = raw
    return result


def _paired_metric(ledgers: Sequence[Mapping[str, Any]], metric: str, *, seed: int, samples: int) -> dict[str, Any]:
    rows = _pair_rows(ledgers, metric)
    complete = [row for row in rows if row["C"] is not None and row["D"] is not None]
    deltas = [float(row["delta_D_minus_C"]) for row in complete]
    ratios = [float(row["ratio_D_over_C"]) for row in complete if row["ratio_D_over_C"] is not None]
    return {
        "pair_total": len(rows),
        "paired_n": len(complete),
        "rows": rows,
        "deltas": deltas,
        "ratios": ratios,
        "mean_delta_D_minus_C": mean(deltas) if deltas else None,
        "mean_ratio_D_over_C": mean(ratios) if ratios else None,
        "delta_ci": bootstrap_ci(deltas, seed=seed, samples=samples),
        "ratio_ci": bootstrap_ci(ratios, seed=seed + 1, samples=samples),
        "missing": len(complete) != len(rows),
    }


def _threshold(metric: Mapping[str, Any], *, name: str, direction: str, limit: float, require_positive_denom: bool = True) -> dict[str, Any]:
    """Decorate a paired metric with a conservative threshold decision."""

    pair_total = int(metric["pair_total"])
    paired_n = int(metric["paired_n"])
    estimate = metric["mean_ratio_D_over_C"] if direction == "max_ratio" else metric["mean_delta_D_minus_C"]
    reasons: list[str] = []
    if pair_total == 0:
        reasons.append("missing:paired-observation")
    elif paired_n != pair_total:
        reasons.append("missing:metric")
    if direction == "max_ratio" and require_positive_denom:
        # A complete pair can still have ratio=None when C's metric is zero.
        if len(metric["ratios"]) != paired_n:
            reasons.append("zero-or-unavailable-C-denominator")
    if estimate is None:
        reasons.append("unavailable:estimate")
    if reasons:
        status = "untestable"
    elif direction == "max_ratio":
        status = "pass" if float(estimate) <= limit else "fail"
    elif direction == "min_delta":
        status = "pass" if float(estimate) >= limit else "fail"
    elif direction == "strict_min_delta":
        status = "pass" if float(estimate) > limit else "fail"
    else:  # pragma: no cover - defensive
        raise ValueError(direction)
    return {
        "status": status,
        "estimate": estimate,
        "threshold": {"direction": direction, "limit": limit},
        "paired_n": paired_n,
        "pair_total": pair_total,
        "reasons": sorted(set(reasons)),
    }


def analyze_cd(
    ledgers: Sequence[Mapping[str, Any]],
    *,
    seed: int = 20260802,
    min_pairs: int = 10,
    bootstrap_samples: int = 2000,
) -> dict[str, Any]:
    ordered = sorted(ledgers, key=lambda item: (str(item.get("task_id")), int(item.get("replicate", 0)), str(item.get("arm_id")), str(item.get("episode_id"))))
    arm_metrics = _arm_metrics(ordered, seed, bootstrap_samples)
    paired = {metric: _paired_metric(ordered, metric, seed=seed + 17 * index, samples=bootstrap_samples) for index, metric in enumerate(METRICS)}

    # Primary thresholds are exactly the C-vs-D learning criteria.  Cost in
    # dollars is reported as unavailable when receipts do not provide it; the
    # token and wall-clock proxies are evaluated separately and do not silently
    # relabel the dollar criterion as passed.
    thresholds = {
        "repeated_failure_codes": _threshold(paired["repeated_failure_codes"], name="repeated_failure_codes", direction="max_ratio", limit=0.85),
        "cost_usd": _threshold(paired["cost_usd"], name="cost_usd", direction="max_ratio", limit=0.90),
        "tokens_proxy": _threshold(paired["tokens"], name="tokens_proxy", direction="max_ratio", limit=0.90),
        "wall_time_proxy": _threshold(paired["wall_time_ms"], name="wall_time_proxy", direction="max_ratio", limit=0.90),
        "routing_quality": _threshold(paired["routing_quality"], name="routing_quality", direction="strict_min_delta", limit=0.0, require_positive_denom=False),
        "tool_quality": _threshold(paired["tool_quality"], name="tool_quality", direction="strict_min_delta", limit=0.0, require_positive_denom=False),
        "correctness_noninferiority": _threshold(paired["success"], name="correctness_noninferiority", direction="min_delta", limit=0.0, require_positive_denom=False),
    }
    all_pairs = len({(str(item.get("task_id")), int(item.get("replicate", 0))) for item in ordered})
    reasons: list[str] = []
    if all_pairs < min_pairs:
        reasons.append(f"underpowered:pairs<{min_pairs}")
    for name, item in thresholds.items():
        if item["status"] != "pass":
            reasons.append(f"{name}:{item['status']}")
    required_names = tuple(thresholds)
    if any(thresholds[name]["status"] == "untestable" for name in required_names):
        decision = "inconclusive"
    elif all(thresholds[name]["status"] == "pass" for name in required_names) and not reasons:
        decision = "pass"
    else:
        decision = "fail"
    return {
        "analysis_version": ANALYSIS_VERSION,
        "contrast": "D_vs_C",
        "arm_metrics": arm_metrics,
        "paired": paired,
        "thresholds": thresholds,
        "pair_count": all_pairs,
        "min_pairs": min_pairs,
        "decision": decision,
        "no_go": True,
        "reasons": sorted(set(reasons)),
        "cost_note": "Dollar cost receipts were unavailable; tokens and wall-clock are reported as separate execution-cost proxies.",
    }


def sanitized_cell_hashes(ledgers: Sequence[Mapping[str, Any]], ledger_root: Path) -> list[dict[str, Any]]:
    """Return compact, non-prompt/non-trace cell evidence for a report."""

    rows: list[dict[str, Any]] = []
    for ledger in sorted(ledgers, key=lambda item: str(item.get("episode_id"))):
        path = ledger_root / str(ledger["episode_id"]) / "ledger.json"
        result = ledger.get("result", {})
        correctness = result.get("correctness", {})
        tokens = result.get("tokens", {})
        rows.append(
            {
                "episode_id": ledger["episode_id"],
                "task_id": ledger["task_id"],
                "arm_id": ledger["arm_id"],
                "replicate": ledger.get("replicate", 0),
                "ledger_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "result": {
                    "success": correctness.get("passed"),
                    "checker_failure_code": correctness.get("checker_failure_code"),
                    "changed_paths": result.get("changed_paths", []),
                    "intent_violations": result.get("intent_violations"),
                    "tokens": tokens.get("total") if tokens.get("available") else None,
                    "timed_out": result.get("timed_out"),
                    "exit_code": result.get("exit_code"),
                    "evidence_hashes": result.get("evidence_hashes", {}),
                },
            }
        )
    return rows


def _cli() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger_root", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seed", type=int, default=20260802)
    parser.add_argument("--min-pairs", type=int, default=10)
    args = parser.parse_args()
    ledgers = load_ledgers([args.ledger_root])
    report = analyze_cd(ledgers, seed=args.seed, min_pairs=args.min_pairs)
    report["cell_hashes"] = sanitized_cell_hashes(ledgers, args.ledger_root / "episodes" if (args.ledger_root / "episodes").is_dir() else args.ledger_root)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({"output": str(args.output), "pairs": report["pair_count"], "decision": report["decision"]}, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
