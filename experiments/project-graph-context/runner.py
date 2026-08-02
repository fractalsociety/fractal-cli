#!/usr/bin/env python3
"""Deterministic, offline-oriented episode runner.

The runner is deliberately independent of Fractal's production graph state. It
uses only a frozen git commit, creates a detached worktree for each episode,
mounts context and intent files in a read-only directory outside that
worktree, and executes a command list with ``shell=False``.  A worker may emit
an explicit usage receipt; otherwise token and cost metrics remain unavailable.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

try:  # Direct script execution and module-style imports are both supported.
    from .corpus import copy_hidden_oracle, run_hidden_oracle, task_manifest
except ImportError:  # pragma: no cover - exercised by command-line invocation
    from corpus import copy_hidden_oracle, run_hidden_oracle, task_manifest


SCHEMA_VERSION = "project-graph-context.event-result-ledger.v1"
USAGE_SCHEMA_VERSION = "project-graph-context.usage-receipt.v1"
ARMS = ("A", "B", "C", "D")


class RunnerError(RuntimeError):
    """A configuration or execution error that should be shown to callers."""


def canonical_json(value: Any) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n").encode("utf-8")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _git(repo: Path, args: Sequence[str], *, check: bool = True, timeout: float = 30) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        check=check,
        timeout=timeout,
        env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
    )


def resolve_frozen_commit(source_repo: str | os.PathLike[str], commit: str) -> str:
    """Resolve and validate a commit object without requiring a clean checkout."""

    repo = Path(source_repo).resolve()
    if not (repo / ".git").exists():
        raise RunnerError(f"source repository is not a git worktree: {repo}")
    try:
        resolved = _git(repo, ["rev-parse", "--verify", f"{commit}^{{commit}}"]).stdout.strip()
        object_type = _git(repo, ["cat-file", "-t", resolved]).stdout.strip()
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as exc:
        raise RunnerError(f"frozen commit cannot be resolved: {commit}") from exc
    if object_type != "commit":
        raise RunnerError(f"frozen reference is not a commit: {commit}")
    if len(resolved) != 40 or any(char not in "0123456789abcdef" for char in resolved):
        raise RunnerError("git returned an invalid commit id")
    return resolved


def _readonly_tree(path: Path) -> None:
    """Make a context tree read-only for the child process."""

    for child in sorted(path.rglob("*"), key=lambda p: len(p.parts), reverse=True):
        if child.is_symlink():
            continue
        if child.is_dir():
            child.chmod(0o555)
        else:
            child.chmod(0o444)
    path.chmod(0o555)


def _copy_readonly(source: Path, destination: Path) -> None:
    if source.is_dir():
        shutil.copytree(source, destination)
    elif source.is_file():
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
    else:
        raise RunnerError(f"context source does not exist: {source}")
    _readonly_tree(destination)


def _parse_porcelain_paths(raw: bytes) -> list[str]:
    paths: list[str] = []
    for record in raw.decode("utf-8", errors="surrogateescape").split("\0"):
        if not record:
            continue
        # Porcelain v1 uses two status bytes, one space, then the path. For a
        # rename/copy, -z puts the destination path first in the final record.
        path = record[3:] if len(record) >= 4 else record
        if " -> " in path:
            path = path.rsplit(" -> ", 1)[-1]
        paths.append(path.replace("\\", "/"))
    return sorted(set(paths))


def changed_paths(worktree: Path) -> list[str]:
    completed = subprocess.run(
        ["git", "-C", str(worktree), "status", "--porcelain=v1", "-z", "--untracked-files=all"],
        capture_output=True,
        check=True,
    )
    return _parse_porcelain_paths(completed.stdout)


def _path_matches(path: str, patterns: Iterable[str]) -> bool:
    normalized = path.replace("\\", "/").lstrip("./")
    for pattern in patterns:
        candidate = str(pattern).replace("\\", "/").lstrip("./")
        if normalized == candidate or normalized.startswith(candidate.rstrip("/") + "/"):
            return True
        if fnmatch.fnmatch(normalized, candidate):
            return True
    return False


def score_path_scope(paths: Sequence[str], intent: Mapping[str, Any]) -> Dict[str, Any]:
    allowed = intent.get("allowed_paths", [])
    forbidden = intent.get("forbidden_paths", [])
    violations: list[dict[str, Any]] = []
    for path in sorted(set(paths)):
        unsafe = path.startswith("/") or path == ".." or path.startswith("../") or "../" in path.split("/")
        if unsafe or _path_matches(path, forbidden):
            violations.append({"path": path, "reason": "forbidden_or_traversal", "weight": 2.0})
        elif not _path_matches(path, allowed):
            violations.append({"path": path, "reason": "outside_allowed_scope", "weight": 1.0})
    return {
        "severe": len(violations),
        "weighted": float(sum(float(v["weight"]) for v in violations)),
        "violations": violations,
    }


def _valid_nonnegative_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def validate_usage_receipt(path: Path) -> tuple[Dict[str, Any] | None, str | None]:
    """Accept telemetry only from a structurally valid worker receipt."""

    if not path.is_file():
        return None, "unavailable"
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None, "invalid_receipt"
    if not isinstance(payload, dict) or payload.get("schema_version") != USAGE_SCHEMA_VERSION:
        return None, "invalid_receipt"
    input_tokens = payload.get("input_tokens")
    output_tokens = payload.get("output_tokens")
    total_tokens = payload.get("total_tokens")
    cost_usd = payload.get("cost_usd")
    if not all(_valid_nonnegative_int(v) for v in (input_tokens, output_tokens, total_tokens)):
        return None, "invalid_receipt"
    if total_tokens != input_tokens + output_tokens:
        return None, "invalid_receipt"
    if cost_usd is not None and (isinstance(cost_usd, bool) or not isinstance(cost_usd, (int, float)) or cost_usd < 0):
        return None, "invalid_receipt"
    if payload.get("source") != "worker":
        return None, "invalid_receipt"
    return {
        "available": True,
        "input": input_tokens,
        "output": output_tokens,
        "total": total_tokens,
        "cost_usd": cost_usd,
        "receipt_sha256": sha256_file(path),
    }, None


def _read_trace(path: Path) -> tuple[Dict[str, Any] | None, str | None]:
    if not path.is_file():
        return None, "unavailable"
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None, "invalid_trace"
    if not isinstance(payload, dict):
        return None, "invalid_trace"
    return payload, None


def _trace_metric(trace: Mapping[str, Any] | None, key: str) -> Any:
    if trace is None:
        return None
    return trace.get(key)


@dataclass(frozen=True)
class RunnerConfig:
    timeout_seconds: float = 120.0
    max_output_bytes: int = 1_000_000
    max_repairs: int = 8
    max_tokens: int | None = None
    keep_worktree: bool = False
    dry_run: bool = False


@dataclass(frozen=True)
class EpisodeSpec:
    experiment_id: str
    arm_id: str
    task_id: str
    source_repo: Path
    frozen_commit: str
    worker_command: tuple[str, ...]
    context_source: Path
    intent_source: Path
    output_root: Path
    replicate: int = 0
    config: RunnerConfig = RunnerConfig()


def _validate_command(command: Sequence[str]) -> tuple[str, ...]:
    if isinstance(command, str) or not command:
        raise RunnerError("worker command must be a non-empty argv list, never a shell string")
    normalized = tuple(str(arg) for arg in command)
    if any("\0" in arg for arg in normalized):
        raise RunnerError("worker command contains NUL")
    return normalized


def _bounded_output(raw: bytes, limit: int) -> tuple[bytes, bool]:
    if len(raw) <= limit:
        return raw, False
    return raw[:limit], True


def _write_json(path: Path, value: Any) -> None:
    path.write_bytes(canonical_json(value))


def run_episode(spec: EpisodeSpec) -> Dict[str, Any]:
    """Run one episode and return/write its event-result ledger."""

    if spec.arm_id not in ARMS:
        raise RunnerError(f"unknown arm: {spec.arm_id}")
    command = _validate_command(spec.worker_command)
    if spec.config.timeout_seconds <= 0 or spec.config.max_output_bytes < 1024:
        raise RunnerError("invalid episode budget")
    commit = resolve_frozen_commit(spec.source_repo, spec.frozen_commit)
    intent = json.loads(spec.intent_source.read_text(encoding="utf-8"))
    if intent.get("task_id") != spec.task_id:
        raise RunnerError("intent manifest task id does not match episode")

    episode_id = f"{spec.experiment_id}-{spec.arm_id}-{spec.task_id}-{spec.replicate}"
    output_root = spec.output_root.resolve()
    episode_dir = output_root / episode_id
    episode_dir.mkdir(parents=True, exist_ok=False)
    worktree = episode_dir / "worktree"
    context_dir = episode_dir / "mounted-context"
    hidden_dir = episode_dir / "hidden-checker"
    trace_path = episode_dir / "worker-trace.json"
    event_path = episode_dir / "worker-events.json"
    receipt_path = episode_dir / "usage-receipt.json"
    output_path = episode_dir / "stdout.bin"
    error_path = episode_dir / "stderr.bin"

    events: list[dict[str, Any]] = []
    started = time.monotonic()

    def event(kind: str, **data: Any) -> None:
        events.append({"sequence": len(events), "kind": kind, "elapsed_ms": round((time.monotonic() - started) * 1000.0, 3), "data": data})

    event("runner_started", frozen_commit=commit)
    if spec.config.dry_run:
        plan = {
            "episode_id": episode_id,
            "command": list(command),
            "source_repo": str(spec.source_repo.resolve()),
            "frozen_commit": commit,
            "worktree": str(worktree),
            "context": str(context_dir),
            "offline": True,
        }
        event("dry_run", **plan)
        result = {
            "correctness": {"passed": False, "checker_failure_code": "dry_run"},
            "intent_violations": {"severe": 0, "weighted": 0.0},
            "irrelevant_opens": None,
            "tokens": {"available": False, "input": None, "output": None, "total": None, "cost_usd": None},
            "repair_iterations": None,
            "repeated_failure_codes": None,
            "routing": None,
            "tool_selection": None,
            "changed_paths": [],
            "evidence_hashes": {"plan": sha256_bytes(canonical_json(plan))},
            "timed_out": False,
            "exit_code": None,
            "duration_ms": round((time.monotonic() - started) * 1000.0, 3),
            "path_scope": {"severe": 0, "weighted": 0.0, "violations": []},
        }
        ledger = _ledger(spec, episode_id, events, result)
        _write_json(episode_dir / "ledger.json", ledger)
        return ledger

    try:
        subprocess.run(["git", "-C", str(spec.source_repo.resolve()), "worktree", "add", "--detach", "--quiet", str(worktree), commit], check=True, capture_output=True, timeout=60)
        event("worktree_created", path=str(worktree))
        _copy_readonly(spec.context_source.resolve(), context_dir)
        intent_copy = episode_dir / "intent.json"
        shutil.copy2(spec.intent_source.resolve(), intent_copy)
        _readonly_tree(intent_copy)
        hidden_checker = copy_hidden_oracle(hidden_dir)
        if context_dir.is_file():
            context_hash = sha256_file(context_dir)
        elif (context_dir / "manifest.json").exists():
            context_hash = sha256_file(context_dir / "manifest.json")
        else:
            # A directory context is hashed canonically by relative path and
            # bytes so ordering and platform directory metadata cannot leak.
            context_digest = hashlib.sha256()
            for child in sorted((item for item in context_dir.rglob("*") if item.is_file()), key=lambda item: item.relative_to(context_dir).as_posix()):
                context_digest.update(child.relative_to(context_dir).as_posix().encode("utf-8"))
                context_digest.update(b"\0")
                context_digest.update(child.read_bytes())
                context_digest.update(b"\0")
            context_hash = context_digest.hexdigest()
        event("context_mounted", context=str(context_dir), context_sha256=context_hash)
        env = os.environ.copy()
        env.update(
            {
                "FRACTAL_EXPERIMENT_ID": spec.experiment_id,
                "FRACTAL_EPISODE_ID": episode_id,
                "FRACTAL_ARM_ID": spec.arm_id,
                "FRACTAL_TASK_ID": spec.task_id,
                "FRACTAL_WORKTREE": str(worktree),
                "FRACTAL_CONTEXT_PATH": str(context_dir),
                "FRACTAL_TASK_INTENT_PATH": str(intent_copy),
                "FRACTAL_TRACE_PATH": str(trace_path),
                "FRACTAL_EVENT_PATH": str(event_path),
                "FRACTAL_USAGE_RECEIPT_PATH": str(receipt_path),
                "FRACTAL_OFFLINE": "1",
                "NO_NETWORK": "1",
                "PIP_NO_INDEX": "1",
                "GIT_TERMINAL_PROMPT": "0",
                "HTTP_PROXY": "",
                "HTTPS_PROXY": "",
                "ALL_PROXY": "",
                "http_proxy": "",
                "https_proxy": "",
                "all_proxy": "",
                "NO_PROXY": "*",
                "PYTHONNOUSERSITE": "1",
            }
        )
        event("command_started", argv=list(command), shell=False)
        command_started = time.monotonic()
        try:
            completed = subprocess.run(command, cwd=worktree, env=env, capture_output=True, timeout=spec.config.timeout_seconds, check=False, shell=False)
            timed_out = False
            exit_code: int | None = completed.returncode
            stdout_raw, stdout_truncated = _bounded_output(completed.stdout, spec.config.max_output_bytes)
            stderr_raw, stderr_truncated = _bounded_output(completed.stderr, spec.config.max_output_bytes)
        except subprocess.TimeoutExpired as exc:
            timed_out = True
            exit_code = None
            out = exc.stdout if isinstance(exc.stdout, bytes) else (exc.stdout or "").encode()
            err = exc.stderr if isinstance(exc.stderr, bytes) else (exc.stderr or "").encode()
            stdout_raw, stdout_truncated = _bounded_output(out, spec.config.max_output_bytes)
            stderr_raw, stderr_truncated = _bounded_output(err, spec.config.max_output_bytes)
        command_ms = round((time.monotonic() - command_started) * 1000.0, 3)
        output_path.write_bytes(stdout_raw)
        error_path.write_bytes(stderr_raw)
        event("command_finished", exit_code=exit_code, timed_out=timed_out, duration_ms=command_ms, stdout_truncated=stdout_truncated, stderr_truncated=stderr_truncated)

        paths = changed_paths(worktree)
        changed_file_hashes: dict[str, str] = {}
        for changed_path in paths:
            changed_target = worktree / changed_path
            key = f"file:{changed_path}"
            if changed_target.is_file():
                changed_file_hashes[key] = sha256_file(changed_target)
            else:
                changed_file_hashes[key] = sha256_bytes(b"<deleted-or-nonfile>")
        scope = score_path_scope(paths, intent)
        trace, trace_error = _read_trace(trace_path)
        if trace_error:
            event("trace_unavailable", reason=trace_error)
        else:
            event("trace_read", keys=sorted(trace or {}))
        usage, usage_error = validate_usage_receipt(receipt_path)
        if usage_error:
            event("usage_unavailable", reason=usage_error)
        else:
            event("usage_validated", receipt_sha256=usage["receipt_sha256"] if usage else None)
        checker = run_hidden_oracle(spec.task_id, worktree, hidden_checker)
        event("checker_finished", passed=bool(checker.get("passed")), failure_code=checker.get("failure_code"))
        opens = _trace_metric(trace, "opens")
        irrelevant: int | None
        if isinstance(opens, list) and all(isinstance(item, str) for item in opens):
            relevant = set(str(p).replace("\\", "/") for p in intent.get("allowed_paths", []))
            irrelevant = sum(1 for item in opens if not _path_matches(item, relevant))
        else:
            irrelevant = None
        repairs = _trace_metric(trace, "repair_iterations")
        if not _valid_nonnegative_int(repairs):
            repairs = None
        failures = _trace_metric(trace, "failure_codes")
        if isinstance(failures, list) and all(isinstance(item, str) for item in failures):
            counts: Dict[str, int] = {}
            for code in failures:
                counts[code] = counts.get(code, 0) + 1
            repeated = sum(count - 1 for count in counts.values() if count > 1)
        else:
            repeated = None
        routing = _trace_metric(trace, "routing")
        tool_selection = _trace_metric(trace, "tool_selection")
        if not isinstance(routing, dict):
            routing = None
        if not isinstance(tool_selection, dict):
            tool_selection = None
        if spec.config.max_repairs >= 0 and repairs is not None and repairs > spec.config.max_repairs:
            event("budget_exceeded", budget="max_repairs", observed=repairs, maximum=spec.config.max_repairs)
            timed_out = True
        if spec.config.max_tokens is not None and usage is not None and usage["total"] > spec.config.max_tokens:
            event("budget_exceeded", budget="max_tokens", observed=usage["total"], maximum=spec.config.max_tokens)
            timed_out = True
        result = {
            "correctness": {"passed": bool(checker.get("passed")) and not timed_out, "checker_failure_code": checker.get("failure_code")},
            "intent_violations": {"severe": int(scope["severe"]), "weighted": float(scope["weighted"])},
            "irrelevant_opens": irrelevant,
            "tokens": usage or {"available": False, "input": None, "output": None, "total": None, "cost_usd": None},
            "repair_iterations": repairs,
            "repeated_failure_codes": repeated,
            "routing": routing,
            "tool_selection": tool_selection,
            "changed_paths": paths,
            "evidence_hashes": {
                "stdout": sha256_file(output_path),
                "stderr": sha256_file(error_path),
                "trace": sha256_file(trace_path) if trace_path.exists() else sha256_bytes(b"unavailable"),
                "changed_paths": sha256_bytes(canonical_json(paths)),
                "commit": commit,
                **changed_file_hashes,
            },
            "timed_out": timed_out,
            "exit_code": exit_code,
            "duration_ms": command_ms,
            "path_scope": scope,
        }
    finally:
        if not spec.config.keep_worktree and worktree.exists():
            subprocess.run(["git", "-C", str(spec.source_repo.resolve()), "worktree", "remove", "--force", str(worktree)], check=False, capture_output=True)
    result = locals().get("result")
    if not isinstance(result, dict):
        result = {
            "correctness": {"passed": False, "checker_failure_code": "runner_error"},
            "intent_violations": {"severe": 0, "weighted": 0.0},
            "irrelevant_opens": None,
            "tokens": {"available": False, "input": None, "output": None, "total": None, "cost_usd": None},
            "repair_iterations": None,
            "repeated_failure_codes": None,
            "routing": None,
            "tool_selection": None,
            "changed_paths": [],
            "evidence_hashes": {},
            "timed_out": False,
            "exit_code": None,
            "duration_ms": round((time.monotonic() - started) * 1000.0, 3),
            "path_scope": {"severe": 0, "weighted": 0.0, "violations": []},
        }
    ledger = _ledger(spec, episode_id, events, result)
    _write_json(episode_dir / "ledger.json", ledger)
    return ledger


def _ledger(spec: EpisodeSpec, episode_id: str, events: list[dict[str, Any]], result: Dict[str, Any]) -> Dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "episode_id": episode_id,
        "experiment_id": spec.experiment_id,
        "arm_id": spec.arm_id,
        "task_id": spec.task_id,
        "replicate": spec.replicate,
        "events": events,
        "result": result,
    }


def _cli() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--arm", choices=ARMS, required=True)
    parser.add_argument("--task-id", required=True)
    parser.add_argument("--source-repo", type=Path, required=True)
    parser.add_argument("--frozen-commit", required=True)
    parser.add_argument("--context", type=Path, required=True)
    parser.add_argument("--intent", type=Path, required=True)
    parser.add_argument("--output-root", type=Path, required=True)
    parser.add_argument("--replicate", type=int, default=0)
    parser.add_argument("--timeout-seconds", type=float, default=120)
    parser.add_argument("--max-output-bytes", type=int, default=1_000_000)
    parser.add_argument("--max-repairs", type=int, default=8)
    parser.add_argument("--max-tokens", type=int)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("worker_command", nargs=argparse.REMAINDER, help="argv after --; never parsed as a shell command")
    args = parser.parse_args()
    command = tuple(args.worker_command)
    if command and command[0] == "--":
        command = command[1:]
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    if manifest.get("offline") is not True:
        raise SystemExit("manifest must set offline=true")
    ledger = run_episode(EpisodeSpec(args.manifest.stem, args.arm, args.task_id, args.source_repo, args.frozen_commit, command, args.context, args.intent, args.output_root, args.replicate, RunnerConfig(args.timeout_seconds, args.max_output_bytes, args.max_repairs, args.max_tokens, False, args.dry_run)))
    print(json.dumps(ledger, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
