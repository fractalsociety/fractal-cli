"""JSON codec kept separate from corruption recovery policy."""

from __future__ import annotations

import json
from typing import Any


def encode_document(document: dict[str, Any]) -> str:
    return json.dumps(document, sort_keys=True, separators=(",", ":"))


def decode_document(text: str) -> dict[str, Any]:
    value = json.loads(text)
    if not isinstance(value, dict):
        raise ValueError("document must be an object")
    return value

