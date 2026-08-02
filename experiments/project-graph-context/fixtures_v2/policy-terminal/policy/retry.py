"""Bounded, deterministic recovery policy benchmark seed."""

from __future__ import annotations

from .checkpoint import load_checkpoint, save_checkpoint
from .decisions import classify


def run_plan(plan, outcomes, *, max_retries=2, hard_budget=8, checkpoint=None):
    raise NotImplementedError("task pending")

