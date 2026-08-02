#!/usr/bin/env python3
"""Analysis and preregistered threshold decisions for the pilot ledger.

The analysis is intentionally dependency-free.  It emits arm-level raw
metrics, task/replicate paired deltas and ratios, deterministic bootstrap
intervals (or an explicit ``small_n`` uncertainty method), and a conservative
no-go decision whenever a required metric is missing or the pilot is below the
pre-registered pair count.
"""

from __future__ import annotations

import argparse
import json
import math
import random
from collections import defaultdict
from pathlib import Path
from statistics import mean
from typing import Any, Iterable, Mapping, Sequence

try:
    from .scorer import load_ledgers, score_ledger
except ImportError:  # pragma: no cover
    from scorer import load_ledgers, score_ledger


ANALYSIS_VERSION = "project-graph-context.analysis.v1"
DEFAULT_MIN_PAIRS = 10
DEFAULT_BOOTSTRAPS = 2000


def _mean(values: Sequence[float]) -> float | None:
    return mean(values) if values else None


def _numeric(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    if not math.isfinite(float(value)):
        return None
    return float(value)


def bootstrap_ci(values: Sequence[float], *, seed: int, samples: int = DEFAULT_BOOTSTRAPS) -> dict[str, Any]:
    """Deterministic percentile bootstrap for a mean."""

    values = [float(value) for value in values]
    if len(values) < 2:
        return {"method": "small_n", "n": len(values), "low": None, "high": None, "estimate": _mean(values)}
    rng = random.Random(seed)
    draws: list[float] = []
    n = len(values)
    for _ in range(max(1, samples)):
        draws.append(sum(values[rng.randrange(n)] for _ in range(n)) / n)
    draws.sort()

    def percentile(p: float) -> float:
        index = (len(draws) - 1) * p
        lower = math.floor(index)
        upper = math.ceil(index)
        if lower == upper:
            return draws[lower]
        return draws[lower] + (draws[upper] - draws[lower]) * (index - lower)

    return {"method": "bootstrap_percentile", "n": n, "low": percentile(0.025), "high": percentile(0.975), "estimate": sum(values) / n}


def _ratio(target: float | None, baseline: float | None) -> float | None:
    if target is None or baseline is None:
        return None
    if baseline == 0:
        return 1.0 if target == 0 else None
    return target / baseline


def _paired(ledgers: Sequence[Mapping[str, Any]], baseline: str, target: str, metric: str) -> list[dict[str, Any]]:
    scores = [score_ledger(ledger) for ledger in ledgers]
    grouped: dict[tuple[str, int], dict[str, dict[str, Any]]] = defaultdict(dict)
    for score in scores:
        grouped[(str(score["task_id"]), int(score.get("replicate", 0)))][str(score["arm_id"])] = score
    rows: list[dict[str, Any]] = []
    for (task_id, replicate), arms in sorted(grouped.items()):
        base_raw = arms.get(baseline, {}).get(metric)
        targ_raw = arms.get(target, {}).get(metric)
        # Correctness is naturally represented as a bool in normalized scores;
        # convert it to a Bernoulli metric only here, while keeping generic
        # numeric validation strict elsewhere.
        base = (1.0 if base_raw else 0.0) if isinstance(base_raw, bool) else _numeric(base_raw)
        targ = (1.0 if targ_raw else 0.0) if isinstance(targ_raw, bool) else _numeric(targ_raw)
        rows.append({"task_id": task_id, "replicate": replicate, "baseline": base, "target": targ, "delta": targ - base if targ is not None and base is not None else None, "ratio": _ratio(targ, base)})
    return rows


def _routing_quality(value: Any) -> float | None:
    if not isinstance(value, Mapping):
        return None
    quality = value.get("quality")
    numeric = _numeric(quality)
    if numeric is not None:
        return numeric
    correct = value.get("correct_route")
    return 1.0 if correct is True else 0.0 if correct is False else None


def _tool_quality(value: Any) -> float | None:
    if not isinstance(value, Mapping):
        return None
    quality = _numeric(value.get("quality"))
    if quality is not None:
        return quality
    selected = value.get("selected_relevant")
    return 1.0 if selected is True else 0.0 if selected is False else None


def arm_metrics(ledgers: Sequence[Mapping[str, Any]], seed: int = 0) -> dict[str, Any]:
    """Compute raw arm means and metric availability."""

    scores = [score_ledger(ledger) for ledger in ledgers]
    metric_getters = {
        "success": lambda s: 1.0 if s["success"] else 0.0,
        "intent_violations_weighted": lambda s: _numeric(s["intent_violations_weighted"]),
        "irrelevant_opens": lambda s: _numeric(s["irrelevant_opens"]),
        "tokens": lambda s: _numeric(s["tokens"]),
        "repair_iterations": lambda s: _numeric(s["repair_iterations"]),
        "repeated_failure_codes": lambda s: _numeric(s["repeated_failure_codes"]),
        "cost_usd": lambda s: _numeric(s["cost_usd"]),
        "routing_quality": lambda s: _routing_quality(s["routing"]),
        "tool_quality": lambda s: _tool_quality(s["tool_selection"]),
    }
    by_arm: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for score in scores:
        by_arm[str(score["arm_id"])].append(score)
    output: dict[str, Any] = {}
    for arm in sorted(by_arm):
        raw: dict[str, Any] = {"n": len(by_arm[arm])}
        for offset, (metric, getter) in enumerate(metric_getters.items()):
            values = [value for value in (getter(score) for score in by_arm[arm]) if value is not None]
            raw[metric] = {"n_available": len(values), "n_missing": len(by_arm[arm]) - len(values), "mean": _mean(values), "uncertainty": bootstrap_ci(values, seed=seed + offset + ord(arm))}
        output[arm] = raw
    return output


THRESHOLDS: dict[str, dict[str, dict[str, Any]]] = {
    "C_vs_A": {
        "success": {"direction": "min_ratio", "threshold": 1.20},
        "intent_violations_weighted": {"direction": "max_ratio", "threshold": 0.75},
        "irrelevant_opens": {"direction": "max_ratio", "threshold": 0.80},
        "tokens": {"direction": "max_ratio", "threshold": 0.85},
        "repair_iterations": {"direction": "max_ratio", "threshold": 0.80},
    },
    "D_vs_C": {
        "repeated_failure_codes": {"direction": "max_ratio", "threshold": 0.85},
        "cost_usd": {"direction": "max_ratio", "threshold": 0.90},
        "routing_quality": {"direction": "min_delta", "threshold": 0.0, "strict": True},
        "tool_quality": {"direction": "min_delta", "threshold": 0.0, "strict": True},
        "success": {"direction": "min_delta", "threshold": 0.0},
    },
}


def _threshold_pass(value: float | None, spec: Mapping[str, Any]) -> bool | None:
    if value is None:
        return None
    direction = spec["direction"]
    threshold = float(spec["threshold"])
    if direction == "min_ratio":
        return value >= threshold
    if direction == "max_ratio":
        return value <= threshold
    if direction == "min_delta":
        return value > threshold if spec.get("strict") else value >= threshold
    raise ValueError(direction)


def compare(ledgers: Sequence[Mapping[str, Any]], *, baseline: str, target: str, comparison: str, seed: int = 0, min_pairs: int = DEFAULT_MIN_PAIRS, bootstrap_samples: int = DEFAULT_BOOTSTRAPS) -> dict[str, Any]:
    specs = THRESHOLDS[comparison]
    metrics: dict[str, Any] = {}
    reasons: list[str] = []
    for offset, (metric, spec) in enumerate(specs.items()):
        rows = _paired(ledgers, baseline, target, metric)
        complete = [row for row in rows if row["baseline"] is not None and row["target"] is not None]
        ratios = [row["ratio"] for row in complete if row["ratio"] is not None]
        deltas = [row["delta"] for row in complete if row["delta"] is not None]
        direction = spec["direction"]
        if metric == "success" and direction.endswith("ratio"):
            # A failed baseline episode has a legitimate zero Bernoulli value,
            # but its per-pair ratio is undefined.  Use the preregistered
            # aggregate success-rate ratio so those failures are not silently
            # discarded from the +20% success threshold.
            base_rate = _mean([row["baseline"] for row in complete])
            target_rate = _mean([row["target"] for row in complete])
            aggregate_ratio = _ratio(target_rate, base_rate)
            estimate = aggregate_ratio
            ratio_values_for_ci = [aggregate_ratio] if aggregate_ratio is not None else []
        else:
            estimate = _mean(ratios if direction.endswith("ratio") else deltas)
            ratio_values_for_ci = ratios if direction.endswith("ratio") else deltas
        uncertainty = bootstrap_ci(ratio_values_for_ci, seed=seed + offset * 37, samples=bootstrap_samples)
        missing = len(rows) == 0 or len(complete) != len(rows) or (direction.endswith("ratio") and metric != "success" and len(ratios) != len(complete))
        if missing:
            reasons.append(f"missing:{metric}")
        decision = _threshold_pass(estimate, spec)
        metrics[metric] = {"paired_n": len(complete), "pair_total": len(rows), "deltas": deltas, "ratios": ratios, "estimate": estimate, "threshold": spec, "pass": decision, "uncertainty": uncertainty, "missing": missing}
    all_pass = all(item["pass"] is True for item in metrics.values())
    # Every required metric needs the preregistered pair floor; using the
    # maximum would let one well-populated metric mask an underpowered one.
    underpowered = min((item["paired_n"] for item in metrics.values()), default=0) < min_pairs
    if underpowered:
        reasons.append(f"underpowered:pairs<{min_pairs}")
    if reasons:
        decision = "inconclusive" if any(reason.startswith("missing:") or reason.startswith("underpowered:") for reason in reasons) else "fail"
    else:
        decision = "pass" if all_pass else "fail"
    return {"comparison": comparison, "baseline": baseline, "target": target, "metrics": metrics, "decision": decision, "no_go": bool(reasons), "reasons": sorted(set(reasons)), "min_pairs": min_pairs}


def analyze(ledgers: Sequence[Mapping[str, Any]], *, seed: int = 0, min_pairs: int = DEFAULT_MIN_PAIRS, bootstrap_samples: int = DEFAULT_BOOTSTRAPS) -> dict[str, Any]:
    if not ledgers:
        return {"analysis_version": ANALYSIS_VERSION, "raw_arm_metrics": {}, "comparisons": {}, "no_go": True, "reasons": ["no_ledgers"]}
    # Stable ordering prevents filesystem order from changing paired output.
    ordered = sorted(ledgers, key=lambda item: (str(item.get("task_id")), int(item.get("replicate", 0)), str(item.get("arm_id")), str(item.get("episode_id"))))
    comparisons = {
        "C_vs_A": compare(ordered, baseline="A", target="C", comparison="C_vs_A", seed=seed, min_pairs=min_pairs, bootstrap_samples=bootstrap_samples),
        "D_vs_C": compare(ordered, baseline="C", target="D", comparison="D_vs_C", seed=seed + 1000, min_pairs=min_pairs, bootstrap_samples=bootstrap_samples),
    }
    reasons = sorted({reason for comparison in comparisons.values() for reason in comparison["reasons"]})
    return {"analysis_version": ANALYSIS_VERSION, "raw_arm_metrics": arm_metrics(ordered, seed), "paired": {name: {metric: item["metrics"][metric] for metric in item["metrics"]} for name, item in comparisons.items()}, "comparisons": comparisons, "no_go": bool(reasons), "reasons": reasons, "underpowered": any("underpowered:" in reason for reason in reasons)}


def _cli() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger_root", type=Path, nargs="+")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--min-pairs", type=int, default=DEFAULT_MIN_PAIRS)
    parser.add_argument("--bootstrap-samples", type=int, default=DEFAULT_BOOTSTRAPS)
    args = parser.parse_args()
    ledgers = load_ledgers(args.ledger_root)
    report = analyze(ledgers, seed=args.seed, min_pairs=args.min_pairs, bootstrap_samples=args.bootstrap_samples)
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
