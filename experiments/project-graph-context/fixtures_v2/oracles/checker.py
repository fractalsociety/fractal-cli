#!/usr/bin/env python3
"""Private v2 checker launcher.

The launcher deliberately prints only the sanitized clause summary returned by
``task_quality``.  It is copied outside episode worktrees by ``corpus_v2`` and
must never be mounted as worker context.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--task-id", required=True)
    parser.add_argument("--worktree", required=True)
    args = parser.parse_args(argv)
    root = Path(os.environ.get("PGC_CORPUS_ROOT", "")).resolve()
    if not root.is_dir():
        print(json.dumps({"passed": False, "failure_code": "checker_root_unavailable"}, sort_keys=True))
        return 2
    sys.path.insert(0, str(root))
    from task_quality import check_behavior  # imported only in private checker

    payload = check_behavior(args.task_id, Path(args.worktree).resolve())
    # Keep result intentionally narrow: no expected values, answer snippets,
    # source paths outside the fixture, or hidden evaluator trace.
    sanitized = {
        "passed": bool(payload.get("passed", False)),
        "failure_code": payload.get("failure_code"),
        "clauses_passed": payload.get("clauses_passed", 0),
        "clauses_total": payload.get("clauses_total", 0),
    }
    print(json.dumps(sanitized, sort_keys=True))
    return 0 if sanitized["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

