"""Deterministic clock helper; intentionally separate from the store module."""

from __future__ import annotations


class ManualClock:
    def __init__(self, value: float = 0.0):
        self.value = float(value)

    def now(self) -> float:
        return self.value

    def advance(self, seconds: float) -> None:
        self.value += float(seconds)

