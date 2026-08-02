"""Decoy helpers, not a recovery path."""

from __future__ import annotations


def cache_key(key: str) -> str:
    return f"cache:{key}"


def shallow_copy(value):
    return value

