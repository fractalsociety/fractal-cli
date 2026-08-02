#!/usr/bin/env python3
"""Offline scripted worker used for harness calibration and feasibility pilots.

It intentionally models modest arm differences so scorer and analysis paths
are exercised without pretending to be LLM telemetry.  A live pilot must use a
separately approved Sol planner/Luna worker command.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import unicodedata
from pathlib import Path


def _write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def _calculator(task: str, path: Path) -> None:
    add = """def add(a, b):\n    return a + b\n"""
    subtract = """def subtract(a, b):\n    return a - b\n"""
    if task == "calculator-add":
        _write(path, '"""Calculator implementation."""\n\n' + add + "\n" + subtract)
    else:
        _write(path, '"""Calculator implementation."""\n\n' + add + "\n" + subtract)


def _text(task: str, path: Path, arm: str) -> None:
    if arm == "A" and task == "text-slugify-edge":
        # Deliberately misses accents, creating one hidden-checker failure.
        source = '''import re\n\ndef slugify(text):\n    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")\n'''
    else:
        source = '''import re\nimport unicodedata\n\ndef slugify(text):\n    normalized = unicodedata.normalize("NFKD", text)\n    normalized = normalized.replace("—", " ")\n    ascii_text = normalized.encode("ascii", "ignore").decode("ascii")\n    return re.sub(r"[^a-z0-9]+", "-", ascii_text.lower()).strip("-")\n'''
    _write(path, source)


def _trace(arm: str, task: str, trace_path: Path) -> None:
    if arm == "A":
        opens = ["src/calculator.py", "README.md", "docs/unrelated.md"]
        repairs = 2
        failures = ["oracle_assertion_failed", "oracle_assertion_failed"]
        routing = {"quality": 0.45, "correct_route": False}
        tools = {"quality": 0.40, "selected_relevant": False}
    elif arm == "B":
        opens = ["src/calculator.py"]
        repairs = 1
        failures = ["oracle_assertion_failed"]
        routing = {"quality": 0.60, "correct_route": True}
        tools = {"quality": 0.60, "selected_relevant": True}
    elif arm == "C":
        opens = ["src/calculator.py"]
        repairs = 1
        failures = ["oracle_flaky", "oracle_flaky"]
        routing = {"quality": 0.65, "correct_route": True}
        tools = {"quality": 0.65, "selected_relevant": True}
    else:
        opens = ["src/calculator.py"]
        repairs = 0
        failures = ["oracle_flaky"]
        routing = {"quality": 0.90, "correct_route": True}
        tools = {"quality": 0.90, "selected_relevant": True}
    trace_path.write_text(json.dumps({"opens": opens, "repair_iterations": repairs, "failure_codes": failures, "routing": routing, "tool_selection": tools}, sort_keys=True) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--task-id", default=os.environ.get("FRACTAL_TASK_ID"))
    args = parser.parse_args()
    task = args.task_id
    if not task:
        raise SystemExit("FRACTAL_TASK_ID or --task-id is required")
    arm = os.environ.get("FRACTAL_ARM_ID", "A")
    worktree = Path(os.environ["FRACTAL_WORKTREE"])
    if task.startswith("calculator-"):
        _calculator(task, worktree / "src/calculator.py")
    elif task.startswith("text-slugify"):
        _text(task, worktree / "src/text_utils.py", arm)
    else:
        raise SystemExit(f"unknown task: {task}")
    if arm == "A":
        # Demonstrates a detectable intent violation; the hidden checker never
        # reads this file.
        (worktree / "README.md").write_text("scripted exploratory note\n", encoding="utf-8")
    _trace(arm, task, Path(os.environ["FRACTAL_TRACE_PATH"]))
    receipt = {
        "schema_version": "project-graph-context.usage-receipt.v1",
        "source": "worker",
        "input_tokens": {"A": 600, "B": 520, "C": 450, "D": 420}.get(arm, 500),
        "output_tokens": {"A": 400, "B": 320, "C": 300, "D": 240}.get(arm, 300),
        "cost_usd": {"A": 0.010, "B": 0.008, "C": 0.007, "D": 0.006}.get(arm, 0.009),
    }
    receipt["total_tokens"] = receipt["input_tokens"] + receipt["output_tokens"]
    Path(os.environ["FRACTAL_USAGE_RECEIPT_PATH"]).write_text(json.dumps(receipt, sort_keys=True) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
