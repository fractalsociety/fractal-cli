"""Durable state benchmark seed; see README for the corruption contract."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable

from .codec import decode_document, encode_document


class StateStore:
    VERSION = 1

    def __init__(self, path: str | Path, clock: Callable[[], float] | None = None):
        self.path = Path(path)
        self.clock = clock or __import__("time").time
        self._entries: dict[str, dict[str, Any]] = {}

    def put(self, key: str, value: Any, ttl: float | None = None) -> None:
        raise NotImplementedError("task pending")

    def get(self, key: str, default: Any = None) -> Any:
        raise NotImplementedError("task pending")

    def save(self) -> None:
        raise NotImplementedError("task pending")

    @classmethod
    def load(cls, path: str | Path, clock: Callable[[], float] | None = None) -> "StateStore":
        raise NotImplementedError("task pending")

    def _purge_expired(self) -> None:
        raise NotImplementedError("task pending")

