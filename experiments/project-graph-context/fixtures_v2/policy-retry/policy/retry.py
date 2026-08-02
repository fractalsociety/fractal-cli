"""Bounded, deterministic recovery policy benchmark seed."""

from __future__ import annotations

from .checkpoint import load_checkpoint, save_checkpoint
from .decisions import classify


def run_plan(plan, outcomes, *, max_retries=2, hard_budget=8, checkpoint=None):
    """Run named steps whose supplied outcomes are ``ok``/``transient``/``denied``.

    The seed intentionally leaves the state machine incomplete.  Outcomes are
    test input, not callbacks, so this fixture cannot create external effects.
    """
    raise NotImplementedError("task pending")

