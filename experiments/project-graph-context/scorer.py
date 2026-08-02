#!/usr/bin/env python3
"""Oracle and metric scoring for project-graph-context episodes.

This module treats absent telemetry as ``None`` (unavailable), never as zero.
The runner performs the hidden checker invocation; this module can validate or
re-score a ledger and is also useful for analysis fixtures.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Iterable, Mapping

try:
    from .runner import _path_matches, score_path_scope, validate_usage_receipt
except ImportError:  # pragma: no cover
    from runner import _path_matches, score_path_scope, validate_usage_receipt


LEDGER_SCHEMA = "project-graph-context.event-result-ledger.v1"


class ScoreError(ValueError):
    pass


REQUIRED_RESULT_FIELDS = (
    "correctness",
    "intent_violations",
    "irrelevant_opens",
    "tokens",
    "repair_iterations",
    "repeated_failure_codes",
    "routing",
    "tool_selection",
    "changed_paths",
    "evidence_hashes",
    "timed_out",
    "exit_code",
)


def validate_ledger(ledger: Mapping[str, Any]) -> None:
    if ledger.get("schema_version") != LEDGER_SCHEMA:
        raise ScoreError("unsupported ledger schema")
    for key in ("episode_id", "experiment_id", "arm_id", "task_id", "events", "result"):
        if key not in ledger:
            raise ScoreError(f"ledger missing {key}")
    if ledger["arm_id"] not in {"A", "B", "C", "D"}:
        raise ScoreError("unknown arm")
    result = ledger["result"]
    if not isinstance(result, Mapping):
        raise ScoreError("result must be an object")
    for key in REQUIRED_RESULT_FIELDS:
        if key not in result:
            raise ScoreError(f"result missing {key}")
    events = ledger["events"]
    if not isinstance(events, list):
        raise ScoreError("events must be a list")
    expected = list(range(len(events)))
    actual = [event.get("sequence") for event in events if isinstance(event, Mapping)]
    if actual != expected:
        raise ScoreError("event sequence must be contiguous and ordered")
    tokens = result["tokens"]
    if not isinstance(tokens, Mapping) or not isinstance(tokens.get("available"), bool):
        raise ScoreError("tokens must declare availability")
    if not tokens["available"] and any(tokens.get(key) is not None for key in ("input", "output", "total", "cost_usd")):
        raise ScoreError("unavailable telemetry cannot contain values")
    if tokens["available"]:
        values = [tokens.get(key) for key in ("input", "output", "total")]
        if not all(isinstance(value, int) and not isinstance(value, bool) and value >= 0 for value in values):
            raise ScoreError("available token telemetry must be non-negative integers")
        if values[2] != values[0] + values[1]:
            raise ScoreError("available total tokens must equal input plus output")
        cost = tokens.get("cost_usd")
        if cost is not None and (isinstance(cost, bool) or not isinstance(cost, (int, float)) or cost < 0):
            raise ScoreError("available cost telemetry must be non-negative")


def score_intent(changed: Iterable[str], intent: Mapping[str, Any]) -> dict[str, Any]:
    """Score changed paths against a task intent/scope manifest."""

    return score_path_scope(sorted(set(changed)), intent)


def score_irrelevant_opens(opens: Any, intent: Mapping[str, Any]) -> int | None:
    """Count irrelevant opens when an explicit trace was emitted."""

    if not isinstance(opens, list) or not all(isinstance(item, str) for item in opens):
        return None
    allowed = intent.get("allowed_paths", [])
    return sum(1 for item in opens if not _path_matches(item, allowed))


def score_repeated_failure_codes(codes: Any) -> int | None:
    if not isinstance(codes, list) or not all(isinstance(code, str) for code in codes):
        return None
    counts: dict[str, int] = {}
    for code in codes:
        counts[code] = counts.get(code, 0) + 1
    return sum(max(0, count - 1) for count in counts.values())


def score_ledger(ledger: Mapping[str, Any], *, intent: Mapping[str, Any] | None = None) -> dict[str, Any]:
    """Return normalized metrics suitable for analysis.

    ``intent`` is optional because a completed ledger already contains the
    path-scope score.  If supplied, it is used to recompute that score and
    exposes a mismatch as an explicit diagnostic instead of silently fixing
    data.
    """

    validate_ledger(ledger)
    result = ledger["result"]
    normalized = {
        "episode_id": ledger["episode_id"],
        "arm_id": ledger["arm_id"],
        "task_id": ledger["task_id"],
        "replicate": ledger.get("replicate", 0),
        "success": bool(result["correctness"].get("passed", False)),
        "intent_violations_severe": int(result["intent_violations"].get("severe", 0)),
        "intent_violations_weighted": float(result["intent_violations"].get("weighted", 0.0)),
        "irrelevant_opens": result.get("irrelevant_opens"),
        "tokens": result["tokens"].get("total") if result["tokens"].get("available") else None,
        "cost_usd": result["tokens"].get("cost_usd") if result["tokens"].get("available") else None,
        "repair_iterations": result.get("repair_iterations"),
        "repeated_failure_codes": result.get("repeated_failure_codes"),
        "routing": result.get("routing"),
        "tool_selection": result.get("tool_selection"),
        "routing_quality": _routing_quality(result.get("routing")),
        "tool_quality": _tool_quality(result.get("tool_selection")),
        "timed_out": bool(result.get("timed_out", False)),
        "duration_ms": result.get("duration_ms"),
    }
    if intent is not None:
        recomputed = score_intent(result.get("changed_paths", []), intent)
        normalized["scope_recomputed"] = recomputed
        normalized["scope_mismatch"] = recomputed["severe"] != result["intent_violations"].get("severe") or recomputed["weighted"] != result["intent_violations"].get("weighted")
    return normalized


def _routing_quality(value: Any) -> float | None:
    if not isinstance(value, Mapping):
        return None
    quality = value.get("quality")
    if isinstance(quality, (int, float)) and not isinstance(quality, bool):
        return float(quality)
    if value.get("correct_route") is True:
        return 1.0
    if value.get("correct_route") is False:
        return 0.0
    return None


def _tool_quality(value: Any) -> float | None:
    if not isinstance(value, Mapping):
        return None
    quality = value.get("quality")
    if isinstance(quality, (int, float)) and not isinstance(quality, bool):
        return float(quality)
    if value.get("selected_relevant") is True:
        return 1.0
    if value.get("selected_relevant") is False:
        return 0.0
    return None


def load_ledgers(paths: Iterable[str | Path]) -> list[dict[str, Any]]:
    ledgers: list[dict[str, Any]] = []
    for path in paths:
        candidate = Path(path)
        if candidate.is_dir():
            candidates = sorted(candidate.rglob("ledger.json"))
        else:
            candidates = [candidate]
        for item in candidates:
            payload = json.loads(item.read_text(encoding="utf-8"))
            validate_ledger(payload)
            ledgers.append(payload)
    return sorted(ledgers, key=lambda item: (str(item["task_id"]), int(item.get("replicate", 0)), str(item["arm_id"]), str(item["episode_id"])))


def _cli() -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger", nargs="+", type=Path)
    args = parser.parse_args()
    scores = [score_ledger(ledger) for ledger in load_ledgers(args.ledger)]
    print(json.dumps(scores, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
