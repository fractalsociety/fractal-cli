#!/usr/bin/env python3
"""Private deterministic checker used by the experiment runner.

This file is copied to a per-episode directory that is outside the worker
worktree.  It deliberately receives only task id and worktree and emits a
small JSON result; hidden expected values are not supplied through worker
context.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
import unicodedata
from pathlib import Path
from typing import Any, Callable


def _load(worktree: Path, relative: str):
    path = worktree / relative
    if not path.is_file():
        raise FileNotFoundError(relative)
    spec = importlib.util.spec_from_file_location("hidden_fixture_module", path)
    if spec is None or spec.loader is None:
        raise ImportError(relative)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _calculator_add(worktree: Path) -> None:
    module = _load(worktree, "src/calculator.py")
    assert module.add(2, 3) == 5
    assert module.add(-2, 2) == 0
    assert module.add(0, 0) == 0


def _calculator_subtract(worktree: Path) -> None:
    module = _load(worktree, "src/calculator.py")
    assert module.subtract(8, 3) == 5
    assert module.subtract(-2, 2) == -4
    assert module.subtract(0, 0) == 0


def _text_slugify(worktree: Path) -> None:
    module = _load(worktree, "src/text_utils.py")
    assert module.slugify("Hello, World!") == "hello-world"
    assert module.slugify("One   two") == "one-two"
    assert module.slugify("already-slug") == "already-slug"


def _text_slugify_edge(worktree: Path) -> None:
    module = _load(worktree, "src/text_utils.py")
    assert module.slugify("  Café au lait  ") == "cafe-au-lait"
    assert module.slugify("naïve—dash") == "naive-dash"
    assert module.slugify("A & B") == "a-b"


CHECKS: dict[str, Callable[[Path], None]] = {
    "calculator-add": _calculator_add,
    "calculator-subtract": _calculator_subtract,
    "text-slugify": _text_slugify,
    "text-slugify-edge": _text_slugify_edge,
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--task-id", required=True)
    parser.add_argument("--worktree", required=True)
    args = parser.parse_args(argv)
    result: dict[str, Any]
    try:
        check = CHECKS[args.task_id]
    except KeyError:
        result = {"passed": False, "failure_code": "oracle_unknown_task"}
    else:
        try:
            check(Path(args.worktree).resolve())
            result = {"passed": True, "failure_code": None}
        except Exception as exc:  # noqa: BLE001 - checker turns all failures into data
            result = {
                "passed": False,
                "failure_code": "oracle_assertion_failed",
                "detail": f"{type(exc).__name__}: {exc}",
            }
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

