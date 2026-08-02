"""Checkpoint serialization helper (stdlib only)."""

from __future__ import annotations

import json
from pathlib import Path


def save_checkpoint(path, completed):
    Path(path).write_text(json.dumps({"version": 1, "completed": list(completed)}, sort_keys=True), encoding="utf-8")


def load_checkpoint(path):
    candidate = Path(path)
    if not candidate.exists():
        return []
    try:
        value = json.loads(candidate.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    if value.get("version") != 1 or not isinstance(value.get("completed"), list):
        return []
    return [item for item in value["completed"] if isinstance(item, str)]

