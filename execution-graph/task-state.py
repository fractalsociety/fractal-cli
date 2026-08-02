#!/usr/bin/env python3
"""Claim and attribute Fractal execution-graph tasks from agent workflows."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path

from server import (
    DEFAULT_PRD,
    DEFAULT_STATE,
    TaskStateError,
    mutate_task_state,
    parse_prd,
)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--legacy-mac-runtime", action="store_true")
    parser.add_argument("action", choices=("checkout", "complete", "release", "status"))
    parser.add_argument("task_id", help="PRD task or gate id, for example M3.13 or M3.G1")
    parser.add_argument("--agent-id", default=os.environ.get("FRACTAL_AGENT_ID"))
    parser.add_argument("--agent-label", default=os.environ.get("FRACTAL_AGENT_LABEL"))
    parser.add_argument("--prd", type=Path, default=DEFAULT_PRD)
    parser.add_argument("--state", type=Path, default=DEFAULT_STATE)
    args = parser.parse_args()
    if not args.legacy_mac_runtime:
        parser.error(
            "task-state.py is archived; use `fractal node NODE "
            "--checkout|--complete|--release|--show --repo REPO`"
        )

    if args.action == "status":
        graph = parse_prd(args.prd.resolve(), args.state.resolve())
        task = next(
            (
                task
                for group in graph["groups"]
                for task in group["tasks"]
                if task["id"] == args.task_id
            ),
            None,
        )
        if task is None:
            parser.error(f"unknown PRD task: {args.task_id}")
        print(
            json.dumps(
                {
                    "task_id": args.task_id,
                    "status": task["status"],
                    "assignment": task["assignment"],
                },
                indent=2,
            )
        )
        return

    if not args.agent_id:
        parser.error("--agent-id or FRACTAL_AGENT_ID is required")
    try:
        assignment = mutate_task_state(
            args.action,
            args.task_id,
            args.agent_id,
            args.agent_label,
            prd_path=args.prd.resolve(),
            state_path=args.state.resolve(),
        )
    except TaskStateError as error:
        parser.error(str(error))
    print(
        json.dumps(
            {"ok": True, "task_id": args.task_id, "assignment": assignment},
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
