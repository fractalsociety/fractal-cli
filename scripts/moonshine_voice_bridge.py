#!/usr/bin/env python3
"""Fractal's narrow bridge to Moonshine Voice's native on-device runtime."""

from __future__ import annotations

import argparse
import importlib.metadata
import json
import sys
from pathlib import Path


def model(cache_root: Path):
    from moonshine_voice import ModelArch, get_model_for_language

    return get_model_for_language(
        "en",
        ModelArch.MEDIUM_STREAMING,
        cache_root=cache_root,
    )


def setup(cache_root: Path) -> None:
    model_path, model_arch = model(cache_root)
    print(
        json.dumps(
            {
                "schema": "fractal.moonshine_setup.v1",
                "package": "moonshine-voice",
                "package_version": importlib.metadata.version("moonshine-voice"),
                "model": "moonshine-v2-medium-streaming",
                "model_arch": int(model_arch),
                "model_path": str(Path(model_path).resolve()),
            }
        ),
        flush=True,
    )


def transcribe(cache_root: Path) -> None:
    from moonshine_voice import MicTranscriber, TranscriptEventListener

    model_path, model_arch = model(cache_root)

    class Listener(TranscriptEventListener):
        def __init__(self):
            self.completed: list[str] = []
            self.current = ""

        def on_line_text_changed(self, event):
            self.current = event.line.text.strip()
            print(f"\r  {self.current}\033[K", end="", file=sys.stderr, flush=True)

        def on_line_completed(self, event):
            text = event.line.text.strip()
            if text and (not self.completed or self.completed[-1] != text):
                self.completed.append(text)
            self.current = ""
            print(file=sys.stderr, flush=True)

    listener = Listener()
    microphone = MicTranscriber(
        model_path=model_path,
        model_arch=model_arch,
        update_interval=0.25,
    )
    microphone.add_listener(listener)
    microphone.start()
    try:
        sys.stdin.readline()
    except KeyboardInterrupt:
        pass
    finally:
        microphone.stop()
        microphone.close()

    if listener.current and (
        not listener.completed or listener.completed[-1] != listener.current
    ):
        listener.completed.append(listener.current)
    transcript = " ".join(listener.completed).strip()
    print(
        json.dumps(
            {
                "schema": "fractal.moonshine_transcript.v1",
                "engine": "moonshine",
                "model": "moonshine-v2-medium-streaming",
                "transcript": transcript,
            }
        ),
        flush=True,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("setup", "transcribe"))
    parser.add_argument("--cache-root", type=Path, required=True)
    args = parser.parse_args()
    args.cache_root.mkdir(parents=True, exist_ok=True)
    if args.action == "setup":
        setup(args.cache_root)
    else:
        transcribe(args.cache_root)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
