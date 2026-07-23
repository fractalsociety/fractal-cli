#!/usr/bin/env python3
"""Router evolution: recommend the cheapest *acceptable* model for a task-kind
from accumulated outcome memory, using DataEvol's real
`build_cheapest_acceptable_rows` (the genuine cheapest-acceptable-target
machinery, not a reimplementation).

Reads one JSON object on stdin:
  { "dataevol_src", "memory_path", "counterfactual_group_id" }
Prints one JSON object on stdout:
  { "chosen_option_id", "observed_options", "acceptable_option_ids", "samples" }
or `{}` when memory has no causal cheapest-acceptable target for the group yet.
Exit 2 = DataEvol not importable (caller treats as "no recommendation").
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    try:
        payload = json.loads(sys.stdin.read() or "{}")
    except json.JSONDecodeError as error:
        print(f"invalid recommend payload: {error}", file=sys.stderr)
        return 1

    src = Path(payload.get("dataevol_src", "")).expanduser()
    memory_path = Path(payload.get("memory_path", "")).expanduser()
    group_id = str(payload.get("counterfactual_group_id", ""))
    if not src.is_dir() or not memory_path.is_file():
        print("{}")
        return 0
    sys.path.insert(0, str(src))
    try:
        from dataevol.datasets.codex_execution_outcomes import (  # noqa: E402
            build_cheapest_acceptable_rows,
        )
    except Exception as error:  # noqa: BLE001
        print(f"cannot import DataEvol normalizer: {error}", file=sys.stderr)
        return 2

    outcomes = []
    for line in memory_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            try:
                outcomes.append(json.loads(line))
            except json.JSONDecodeError:
                continue

    try:
        targets = build_cheapest_acceptable_rows(outcomes)
    except Exception as error:  # noqa: BLE001 - malformed memory should not crash the run
        print(f"cheapest-target computation failed: {error}", file=sys.stderr)
        print("{}")
        return 0

    for target in targets:
        if str(target.get("counterfactual_group_id")) == group_id:
            print(
                json.dumps(
                    {
                        "chosen_option_id": target["chosen_option_id"],
                        "observed_options": target.get("observed_options", []),
                        "acceptable_option_ids": target.get("acceptable_option_ids", []),
                        "samples": len(target.get("source_record_hashes", [])),
                    }
                )
            )
            return 0
    print("{}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
