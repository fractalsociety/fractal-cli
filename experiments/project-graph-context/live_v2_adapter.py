#!/usr/bin/env python3
"""Codex Sol/Luna adapter for the sanitized corpus-v2 live runner.

The adapter has two deliberately separate entry points:

* ``plan`` performs one arm-blind Sol-high call for each requested task and
  freezes the validated JSON skeleton; and
* ``worker`` performs exactly one Luna call inside the runner-created fresh
  worktree.

The runner is the authority for process/worktree/policy isolation.  This file
never mounts the private checker, never uses a shell string, and never invents
usage values when the Codex JSONL stream omits them.  The CLI entry point is
safe to import and test without invoking a model.
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

try:  # module and direct-script invocation are both supported
    from .corpus_v2 import TASKS_V2, task_manifest
    from .live_v2_policy import (
        canonical_json,
        enforcement_report,
        offline_env,
        policy_hash,
        provider_preflight,
        route_eligibility,
        sha256_bytes,
        sha256_file,
        write_canonical,
    )
except ImportError:  # pragma: no cover
    from corpus_v2 import TASKS_V2, task_manifest
    from live_v2_policy import canonical_json, enforcement_report, offline_env, policy_hash, provider_preflight, route_eligibility, sha256_bytes, sha256_file, write_canonical


USAGE_SCHEMA_VERSION = "project-graph-context.usage-receipt.v1"
PLAN_SCHEMA_VERSION = "project-graph-context.sol-plan.v1"
ARMS = ("C",)
DEFAULT_REASONING = "high"
DEFAULT_SOL_TIMEOUT_SECONDS = 240.0
DEFAULT_LUNA_TIMEOUT_SECONDS = 240.0
MAX_EVENT_BYTES = 4_000_000
LUNA_STRUCTURED_PATCH_ROUTE = "codex-luna-structured-patch-v1"
LUNA_LEGACY_ROUTE = "codex-luna-workspace-v1"
STRUCTURED_PATCH_SCHEMA_VERSION = "project-graph-context.luna-structured-patch.v1"
# A patch is deliberately much smaller than the model/context budget.  The
# runner enforces the same limits after the worker exits; these local limits
# keep malformed output from being transported through the adapter.
MAX_PATCH_FILE_BYTES = 1_000_000
MAX_PATCH_TOTAL_BYTES = 4_000_000
MAX_PRELOADED_FILE_BYTES = 1_000_000
LUNA_POSTHOC_TOKEN_CAP = 90_000


class AdapterError(RuntimeError):
    """A configuration/protocol error that must fail closed."""


def _planner_failure_summary(events: Sequence[Mapping[str, Any]], stderr: bytes) -> dict[str, Any]:
    """Return event/error categories without persisting model text or paths."""
    counts: dict[str, int] = {}
    error_rows: list[dict[str, Any]] = []
    for event in events:
        kind = event.get("type")
        if isinstance(kind, str):
            counts[kind] = counts.get(kind, 0) + 1
        if isinstance(kind, str) and (kind in {"error", "turn.failed", "turn.aborted", "response.failed", "stream_error"} or "error" in kind.lower()):
            row: dict[str, Any] = {"type": kind[:96]}
            for key in ("code", "status", "error_type", "failure_stage"):
                value = event.get(key)
                if isinstance(value, (str, int, float)) and not isinstance(value, bool):
                    row[key] = str(value)[:96]
            item = event.get("item")
            if isinstance(item, Mapping):
                item_type = item.get("type")
                if isinstance(item_type, str):
                    row["item_type"] = item_type[:96]
                for key in ("code", "status", "error_type", "failure_stage"):
                    value = item.get(key)
                    if isinstance(value, (str, int, float)) and not isinstance(value, bool):
                        row[key] = str(value)[:96]
            nested = event.get("error")
            if isinstance(nested, Mapping):
                for key in ("code", "status", "error_type", "failure_stage", "type"):
                    value = nested.get(key)
                    if isinstance(value, (str, int, float)) and not isinstance(value, bool):
                        row[key] = str(value)[:96]
            error_rows.append(row)
    lowered = stderr.decode("utf-8", errors="replace").lower()
    patterns = (
        ("auth", ("auth", "login", "token", "unauthorized")),
        ("model", ("model_not_found", "model not found", "unsupported model", "model unavailable")),
        ("network", ("connection", "websocket", "stream", "dns", "http", "socket")),
        ("rate_limit", ("rate limit", "quota", "too many requests", "429")),
        ("schema", ("schema", "json schema", "output-schema")),
        ("sandbox", ("sandbox", "operation not permitted", "permission denied")),
    )
    category = "unknown"
    for candidate, needles in patterns:
        if any(needle in lowered for needle in needles):
            category = candidate
            break
    return {
        "event_type_counts": dict(sorted(counts.items())),
        "error_events": error_rows[-8:],
        "stderr_category": category,
    }


def _safe_env(name: str, *, required: bool = True) -> str | None:
    value = os.environ.get(name)
    if required and not value:
        raise AdapterError(f"missing required environment variable {name}")
    if value is not None and "\0" in value:
        raise AdapterError(f"environment variable {name} contains NUL")
    return value


def _resolve_env_path(name: str, *, required: bool = True) -> Path | None:
    value = _safe_env(name, required=required)
    return Path(value).resolve() if value else None


def _read_json(path: Path, label: str) -> Mapping[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise AdapterError(f"{label} is not valid JSON: {path}") from exc
    if not isinstance(value, Mapping):
        raise AdapterError(f"{label} must be a JSON object: {path}")
    return value


def _validate_task(task_id: str) -> Mapping[str, Any]:
    if task_id not in TASKS_V2:
        raise AdapterError(f"unknown corpus-v2 task: {task_id}")
    return task_manifest(task_id)


def _validate_plan(plan: Mapping[str, Any], task_id: str) -> None:
    required = ("schema_version", "task_id", "objective", "acceptance_checks", "allowed_paths", "steps")
    missing = [key for key in required if key not in plan]
    if missing:
        raise AdapterError(f"frozen Sol plan is missing fields: {', '.join(missing)}")
    if plan.get("schema_version") != PLAN_SCHEMA_VERSION or plan.get("task_id") != task_id:
        raise AdapterError("frozen Sol plan schema/task mismatch")
    if not isinstance(plan.get("objective"), str) or not plan["objective"].strip():
        raise AdapterError("frozen Sol plan objective must be a non-empty string")
    for key in ("acceptance_checks", "allowed_paths", "steps"):
        if not isinstance(plan.get(key), list) or not all(isinstance(item, str) for item in plan[key]):
            raise AdapterError(f"frozen Sol plan field {key} must be a list of strings")
    if not plan["steps"]:
        raise AdapterError("frozen Sol plan must contain at least one step")
    forbidden = {"arm", "context", "prior", "outcome", "oracle", "checker", "lesson"}
    leaked = {str(key).lower() for key in plan} & forbidden
    if leaked:
        raise AdapterError(f"arm-blind Sol plan contains forbidden context fields: {sorted(leaked)}")


def structured_patch_schema() -> dict[str, Any]:
    """Return the strict JSON schema used by the recorded Luna fallback.

    The schema is intentionally independent of the worktree.  A worker emits
    a complete replacement for each allowed file instead of receiving command
    tools or a writable checkout.  The trusted runner performs the final path,
    symlink, size, and no-op checks before applying anything.
    """

    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": STRUCTURED_PATCH_SCHEMA_VERSION,
        "title": "Fractal Luna structured patch",
        "type": "object",
        "additionalProperties": False,
        "required": ["changes", "summary", "checks"],
        "properties": {
            "changes": {
                "type": "array",
                "minItems": 1,
                "maxItems": 8,
                "items": {
                    "type": "object",
                    "additionalProperties": False,
                    "required": ["path", "content"],
                    "properties": {
                        "path": {"type": "string", "minLength": 1, "maxLength": 512},
                        "content": {"type": "string", "maxLength": MAX_PATCH_FILE_BYTES},
                    },
                },
            },
            "summary": {"type": "string", "maxLength": 4_000},
            "checks": {
                "type": "array",
                "maxItems": 128,
                "items": {"type": "string", "maxLength": 512},
            },
        },
    }


def structured_patch_schema_bytes() -> bytes:
    """Canonical schema bytes, useful when staging a read-only schema file."""

    return canonical_json(structured_patch_schema())


def _canonical_relative_path(value: str) -> str:
    """Normalize only separators; reject paths that are not exact relatives.

    Exact path validation belongs to the trusted runner, but rejecting obvious
    traversal here prevents the model transport from carrying dangerous names
    at all.  In particular, ``./foo`` and backslash variants are not silently
    converted into an allowed path.
    """

    if not isinstance(value, str) or not value or "\x00" in value:
        raise AdapterError("structured patch path must be a non-empty string")
    if "\\" in value:
        raise AdapterError("structured patch path uses a non-canonical separator")
    path = Path(value)
    if path.is_absolute() or value.startswith("/") or any(part in {"", ".", ".."} for part in path.parts):
        raise AdapterError("structured patch path is not a safe relative path")
    if any(part in {".git", "oracles", "history"} for part in path.parts) or "graph-state" in value:
        raise AdapterError("structured patch path is protected")
    return value


def validate_structured_patch_shape(payload: Mapping[str, Any]) -> Mapping[str, Any]:
    """Validate the non-worktree portion of the structured output schema.

    Full validation (exact allowlist and whether a change is a no-op) happens
    in ``live_v2_runner`` after the model process has exited.  This shape check
    deliberately does not persist or log any model-provided text.
    """

    if not isinstance(payload, Mapping):
        raise AdapterError("structured patch output must be an object")
    if set(payload) != {"changes", "summary", "checks"}:
        raise AdapterError("structured patch output must contain exactly changes, summary, and checks")
    changes = payload.get("changes")
    summary = payload.get("summary")
    checks = payload.get("checks")
    if not isinstance(changes, list) or not changes:
        raise AdapterError("structured patch changes must be a non-empty list")
    if len(changes) > 8:
        raise AdapterError("structured patch has too many changes")
    if not isinstance(summary, str) or len(summary) > 4_000:
        raise AdapterError("structured patch summary must be a bounded string")
    if not isinstance(checks, list) or len(checks) > 128 or not all(isinstance(item, str) and len(item) <= 512 for item in checks):
        raise AdapterError("structured patch checks must be a bounded list of strings")
    total = 0
    normalized: list[dict[str, str]] = []
    for item in changes:
        if not isinstance(item, Mapping) or set(item) != {"path", "content"}:
            raise AdapterError("each structured patch change must contain only path and content")
        path = item.get("path")
        if not isinstance(path, str) or not path or len(path) > 512 or "\x00" in path:
            raise AdapterError("structured patch path must be a bounded string")
        content = item.get("content")
        if not isinstance(content, str):
            raise AdapterError("structured patch content must be a string")
        try:
            encoded = content.encode("utf-8")
        except UnicodeEncodeError as exc:
            raise AdapterError("structured patch content is not valid UTF-8") from exc
        if len(encoded) > MAX_PATCH_FILE_BYTES:
            raise AdapterError("structured patch file exceeds size limit")
        total += len(encoded)
        if total > MAX_PATCH_TOTAL_BYTES:
            raise AdapterError("structured patch exceeds total size limit")
        normalized.append({"path": path, "content": content})
    return {"changes": normalized, "summary": summary, "checks": list(checks)}


def preload_seed_files(worktree: Path, allowed_paths: Sequence[str]) -> tuple[dict[str, str], int, str]:
    """Read exactly the allowed regular seed files once for prompt mounting.

    The model receives this map in its prompt and has no command tools or
    writable checkout.  The open count is returned as first-class telemetry;
    callers must not infer it from the number of files after a failed read.
    """

    root = Path(worktree).resolve()
    files: dict[str, str] = {}
    opened = 0
    for raw in allowed_paths:
        relative = _canonical_relative_path(str(raw))
        target = root / relative
        # Reject symlinked files and parent directories before opening.  This
        # check is repeated by the trusted runner before apply.
        current = root
        for part in Path(relative).parts:
            current = current / part
            if current.is_symlink():
                raise AdapterError("allowed seed path resolves through a symlink")
        if not target.is_file() or target.is_symlink():
            raise AdapterError(f"allowed seed path is not a regular file: {relative}")
        try:
            size = target.stat().st_size
        except OSError as exc:
            raise AdapterError("allowed seed path could not be stat'ed") from exc
        if size > MAX_PRELOADED_FILE_BYTES:
            raise AdapterError("allowed seed file exceeds prompt size limit")
        try:
            with target.open("rb") as handle:
                data = handle.read()
            opened += 1
            text_value = data.decode("utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            raise AdapterError(f"allowed seed path could not be read: {relative}") from exc
        files[relative] = text_value
    digest = sha256_bytes(canonical_json(files))
    return files, opened, digest


def _extract_json_object(text: str) -> Mapping[str, Any]:
    candidate = text.strip()
    if candidate.startswith("```"):
        candidate = re.sub(r"^```(?:json)?\s*", "", candidate, flags=re.IGNORECASE)
        candidate = re.sub(r"\s*```$", "", candidate)
    try:
        value = json.loads(candidate)
    except json.JSONDecodeError:
        start = candidate.find("{")
        end = candidate.rfind("}")
        if start < 0 or end <= start:
            raise AdapterError("Sol final response did not contain a JSON object")
        try:
            value = json.loads(candidate[start : end + 1])
        except json.JSONDecodeError as exc:
            raise AdapterError("Sol final response JSON could not be parsed") from exc
    if not isinstance(value, Mapping):
        raise AdapterError("Sol final response must be a JSON object")
    return value


def _codex_binary() -> str:
    configured = os.environ.get("FRACTAL_CODEX_BIN")
    binary = configured or shutil.which("codex")
    if not binary:
        raise AdapterError("codex CLI is unavailable on PATH")
    if "\0" in binary:
        raise AdapterError("codex binary path contains NUL")
    return binary


def codex_argv(*, model: str, sandbox: str, cwd: Path, output_schema: Path | None = None) -> list[str]:
    """Build an explicit safe argv list for the installed Codex CLI."""

    argv = [
        _codex_binary(),
        "--ask-for-approval",
        "never",
        "--sandbox",
        sandbox,
        "--model",
        model,
        "-c",
        'model_reasoning_effort="high"',
        "-c",
        'web_search="disabled"',
        "-c",
        'approval_policy="never"',
        "-c",
        "sandbox_workspace_write.network_access=false",
        "exec",
        "--json",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        "--skip-git-repo-check",
        "--cd",
        str(cwd),
    ]
    if output_schema is not None:
        argv.extend(["--output-schema", str(output_schema)])
    return argv


@contextmanager
def fresh_codex_home(env: Mapping[str, str]):
    """Yield a per-process CODEX_HOME containing auth only."""

    source_home = Path(os.environ.get("CODEX_HOME") or (Path.home() / ".codex")).resolve()
    auth_source = source_home / "auth.json"
    if not auth_source.is_file():
        raise AdapterError("Codex auth.json is unavailable; refusing a non-authenticated live call")
    parent = env.get("FRACTAL_CODEX_HOME_ROOT")
    if parent:
        parent_path = Path(parent).resolve()
        parent_path.mkdir(parents=True, exist_ok=True)
        temporary_context = tempfile.TemporaryDirectory(prefix="pgc-v2-codex-home-", dir=str(parent_path))
    else:
        temporary_context = tempfile.TemporaryDirectory(prefix="pgc-v2-codex-home-")
    with temporary_context as temporary:
        home = Path(temporary)
        shutil.copy2(auth_source, home / "auth.json")
        (home / "auth.json").chmod(0o600)
        isolated = dict(env)
        isolated["CODEX_HOME"] = str(home)
        isolated["HOME"] = str(home)
        isolated["XDG_CONFIG_HOME"] = str(home / "config")
        isolated["XDG_CACHE_HOME"] = str(home / "cache")
        yield isolated


def _run_codex(argv: Sequence[str], prompt: str, *, env: Mapping[str, str], cwd: Path, timeout_seconds: float) -> tuple[int | None, bool, list[Mapping[str, Any]], bytes, bytes]:
    if isinstance(argv, str) or not argv or any("\0" in str(item) for item in argv):
        raise AdapterError("Codex invocation must be a non-empty NUL-free argv list")
    command = [str(item) for item in argv]
    # The trusted adapter stays outside the seatbelt so it can stage the
    # ephemeral auth file.  Only the Codex/model process is placed inside the
    # per-cell profile supplied by the runner.  A missing profile is accepted
    # only for unit tests/plan generation; a real worker always supplies one.
    profile = os.environ.get("FRACTAL_SANDBOX_PROFILE")
    sandbox_exec = os.environ.get("FRACTAL_SANDBOX_EXEC", "/usr/bin/sandbox-exec")
    if profile:
        if not Path(sandbox_exec).is_file() or not Path(profile).is_file():
            raise AdapterError("sandbox profile/exec is unavailable; refusing an unsandboxed model process")
        command = [sandbox_exec, "-f", profile, *command]
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=str(cwd),
        env=dict(env),
        shell=False,
        start_new_session=True,
    )
    timed_out = False
    try:
        stdout, stderr = process.communicate(prompt.encode("utf-8"), timeout=timeout_seconds)
    except subprocess.TimeoutExpired as exc:
        timed_out = True
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except (OSError, ProcessLookupError):
            process.terminate()
        try:
            stdout, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except (OSError, ProcessLookupError):
                process.kill()
            stdout, stderr = process.communicate()
        stdout = (exc.output or b"") + (stdout or b"")
        stderr = (exc.stderr or b"") + (stderr or b"")
    events: list[Mapping[str, Any]] = []
    for line in stdout.splitlines():
        try:
            value = json.loads(line.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if isinstance(value, Mapping):
            events.append(value)
    return (None if timed_out else process.returncode), timed_out, events, stdout, stderr


def _usage_rows(events: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    rows: list[Mapping[str, Any]] = []
    for event in events:
        if event.get("type") != "turn.completed":
            continue
        usage = event.get("usage")
        if isinstance(usage, Mapping):
            rows.append(usage)
    return rows


def usage_receipt(events: Sequence[Mapping[str, Any]], destination: Path) -> tuple[dict[str, Any] | None, str]:
    """Persist only numeric CLI usage rows; unavailable stays unavailable."""

    rows = _usage_rows(events)
    if not rows:
        return None, "unavailable"
    input_total = 0
    output_total = 0
    costs: list[float] = []
    for row in rows:
        input_tokens = row.get("input_tokens")
        output_tokens = row.get("output_tokens")
        if any(isinstance(value, bool) or not isinstance(value, int) or value < 0 for value in (input_tokens, output_tokens)):
            return None, "invalid_usage"
        input_total += int(input_tokens)
        output_total += int(output_tokens)
        cost = row.get("cost_usd")
        if cost is not None:
            if isinstance(cost, bool) or not isinstance(cost, (int, float)) or cost < 0:
                return None, "invalid_usage"
            costs.append(float(cost))
    receipt = {
        "schema_version": USAGE_SCHEMA_VERSION,
        "source": "worker",
        "input_tokens": input_total,
        "output_tokens": output_total,
        "total_tokens": input_total + output_total,
        "cost_usd": sum(costs) if len(costs) == len(rows) else None,
        "cli_usage_fields": [dict(row) for row in rows],
    }
    write_canonical(destination, receipt)
    return receipt, "valid"


def _final_agent_text(events: Sequence[Mapping[str, Any]]) -> str | None:
    texts = [
        str(event["item"].get("text"))
        for event in events
        if event.get("type") == "item.completed"
        and isinstance(event.get("item"), Mapping)
        and event["item"].get("type") in {"agent_message", "message"}
        and isinstance(event["item"].get("text"), str)
    ]
    return texts[-1] if texts else None


def _structured_transport_summary(
    *,
    task_id: str,
    episode_id: str,
    exit_code: int | None,
    timed_out: bool,
    events: Sequence[Mapping[str, Any]],
    raw_stdout: bytes,
    raw_stderr: bytes,
    usage_state: str,
    usage: Mapping[str, Any] | None,
    patch: Mapping[str, Any] | None,
    patch_error: str | None,
    policy_digest: str,
    plan_path: Path,
    context_path: Path,
    opened_files: int,
    seed_digest: str,
    started: float,
) -> dict[str, Any]:
    """Build the ephemeral adapter-to-runner envelope.

    ``patch`` is intentionally present only on the subprocess stdout channel;
    the runner consumes it and keeps only hashes/status in durable results.
    """

    payload: dict[str, Any] = {
        "adapter": "codex-live-v2",
        "route": LUNA_STRUCTURED_PATCH_ROUTE,
        "task_id": task_id,
        "episode_id": episode_id,
        "model": "gpt-5.6-luna",
        "exit_code": exit_code,
        "timed_out": timed_out,
        "usage_state": usage_state,
        "usage": {key: usage.get(key) for key in ("input_tokens", "output_tokens", "total_tokens", "cost_usd")} if usage else None,
        "policy_hash": policy_digest,
        "plan_sha256": sha256_file(plan_path),
        "context_sha256": sha256_file(context_path),
        "seed_files_sha256": seed_digest,
        "preloaded_file_open_count": opened_files,
        "event_sha256": sha256_bytes(raw_stdout),
        "stderr_sha256": sha256_bytes(raw_stderr),
        "patch_sha256": sha256_bytes(canonical_json(patch)) if patch is not None else None,
        "patch_status": "ready" if patch is not None and patch_error is None else "rejected",
        "patch_error": patch_error,
        "duration_ms": round((time.monotonic() - started) * 1000.0, 3),
    }
    # The raw replacement content is a transient transport detail.  The
    # runner removes it from its result object after applying/validating.
    if patch is not None and patch_error is None:
        payload["patch"] = patch
    return payload


def run_structured_patch_worker_v2() -> int:
    """Run one Luna call that emits a replacement patch, never edits files.

    This is the recorded ``codex-luna-structured-patch-v1`` fallback.  The
    runner gives the model a read-only/empty worktree and consumes the
    ephemeral envelope only after the child exits.
    """

    task_id = _safe_env("FRACTAL_TASK_ID")
    episode_id = _safe_env("FRACTAL_EPISODE_ID")
    worktree = _resolve_env_path("FRACTAL_WORKTREE")
    intent_path = _resolve_env_path("FRACTAL_TASK_INTENT_PATH")
    context_path = _resolve_env_path("FRACTAL_CONTEXT_PATH")
    plan_path = _resolve_env_path("FRACTAL_SOL_PLAN_PATH")
    trace_path = _resolve_env_path("FRACTAL_TRACE_PATH")
    event_path = _resolve_env_path("FRACTAL_EVENT_PATH")
    receipt_path = _resolve_env_path("FRACTAL_USAGE_RECEIPT_PATH")
    policy_path = _resolve_env_path("FRACTAL_POLICY_PATH")
    report_path = _resolve_env_path("FRACTAL_ENFORCEMENT_REPORT_PATH")
    schema_path = _resolve_env_path("FRACTAL_PATCH_SCHEMA_PATH")
    assert task_id and episode_id and worktree and intent_path and context_path and plan_path and trace_path and event_path and receipt_path and policy_path and report_path and schema_path
    if task_id not in TASKS_V2 or not worktree.is_dir() or not intent_path.is_file() or not context_path.is_file() or not plan_path.is_file() or not policy_path.is_file() or not schema_path.is_file():
        raise AdapterError("runner paths are missing or wrong kind")
    intent = _read_json(intent_path, "task intent")
    context = _read_json(context_path, "arm C context")
    plan = _read_json(plan_path, "frozen Sol plan")
    if intent.get("task_id") != task_id or context.get("task_id") != task_id:
        raise AdapterError("task/context id mismatch")
    _validate_plan(plan, task_id)
    if context.get("schema_version") != "project-graph-context.c-graph-context.v1":
        raise AdapterError("worker requires context condition C")
    if "prior" in context or "lesson" in json.dumps(context, sort_keys=True).lower():
        raise AdapterError("context C must not contain prior lessons")
    if context_path.stat().st_mode & 0o222:
        raise AdapterError("mounted context is writable")
    if schema_path.stat().st_mode & 0o222:
        raise AdapterError("structured output schema is writable")
    policy = _read_json(policy_path, "harness policy")
    digest = policy_hash(policy)
    # The installed Codex v1 contract cannot disable its shell tool at the
    # provider layer.  Keep the policy route eligible, then enforce the
    # structured no-write boundary with a read-only seatbelt/worktree and a
    # prompt that carries only preloaded seed contents.
    route = route_eligibility("codex", shell_allowed=True, network_denied=True)
    report = enforcement_report(policy, provider_route=route, episode_id=episode_id, policy_digest=digest)
    write_canonical(report_path, report)
    if route.get("status") != "eligible":
        raise AdapterError(f"Codex route ineligible: {route.get('reason')}")
    allowed = intent.get("allowed_paths", [])
    if not isinstance(allowed, list) or not all(isinstance(item, str) for item in allowed):
        raise AdapterError("task allowed_paths must be a list of strings")
    seed_files, opened_files, seed_digest = preload_seed_files(worktree, allowed)
    prefix = {
        "type": "adapter.started",
        "task_id": task_id,
        "episode_id": episode_id,
        "model": "gpt-5.6-luna",
        "route": LUNA_STRUCTURED_PATCH_ROUTE,
        "reasoning_effort": DEFAULT_REASONING,
        "sandbox": "read-only",
        "network_disabled": True,
        "policy_hash": digest,
        "plan_sha256": sha256_file(plan_path),
        "context_sha256": sha256_file(context_path),
        "seed_files_sha256": seed_digest,
        "preloaded_file_open_count": opened_files,
    }
    argv = codex_argv(model="gpt-5.6-luna", sandbox="read-only", cwd=worktree, output_schema=schema_path)
    env = offline_env()
    env.update(
        {
            "FRACTAL_WORKTREE": str(worktree),
            "FRACTAL_ADAPTER_CWD": str(worktree),
            "FRACTAL_TASK_ID": task_id,
            "FRACTAL_EPISODE_ID": episode_id,
            "FRACTAL_CONTEXT_PATH": str(context_path),
            "FRACTAL_TASK_INTENT_PATH": str(intent_path),
            "FRACTAL_SOL_PLAN_PATH": str(plan_path),
            "FRACTAL_TRACE_PATH": str(trace_path),
            "FRACTAL_EVENT_PATH": str(event_path),
            "FRACTAL_USAGE_RECEIPT_PATH": str(receipt_path),
            "FRACTAL_POLICY_PATH": str(policy_path),
            "FRACTAL_ENFORCEMENT_REPORT_PATH": str(report_path),
            "FRACTAL_PATCH_SCHEMA_PATH": str(schema_path),
        }
    )
    started = time.monotonic()
    with fresh_codex_home(env) as isolated:
        exit_code, timed_out, events, raw_stdout, raw_stderr = _run_codex(
            argv,
            structured_patch_prompt(intent, plan, context, seed_files, task_id=task_id),
            env=isolated,
            cwd=worktree,
            timeout_seconds=float(os.environ.get("FRACTAL_LUNA_TIMEOUT_SECONDS", str(DEFAULT_LUNA_TIMEOUT_SECONDS))),
        )
    _write_sanitized_events(event_path, events, prefix=prefix, raw_stdout=raw_stdout)
    trace = trace_from_events(events, allowed)
    trace.update({"route": LUNA_STRUCTURED_PATCH_ROUTE, "timed_out": timed_out, "exit_code": exit_code, "duration_ms": round((time.monotonic() - started) * 1000.0, 3), "preloaded_file_open_count": opened_files, "seed_files_sha256": seed_digest})
    usage, usage_state = usage_receipt(events, receipt_path)
    trace["usage_state"] = usage_state
    if usage is not None:
        trace["usage"] = {key: usage.get(key) for key in ("input_tokens", "output_tokens", "total_tokens", "cost_usd")}
    write_canonical(trace_path, trace)
    patch: Mapping[str, Any] | None = None
    patch_error: str | None = None
    final_text = _final_agent_text(events)
    if timed_out:
        patch_error = "worker_timeout"
    elif exit_code not in (0, None):
        patch_error = "worker_exit_nonzero"
    elif not final_text:
        patch_error = "worker_no_structured_output"
    else:
        try:
            patch = validate_structured_patch_shape(_extract_json_object(final_text))
        except AdapterError as exc:
            patch_error = str(exc)
    envelope = _structured_transport_summary(
        task_id=task_id,
        episode_id=episode_id,
        exit_code=exit_code,
        timed_out=timed_out,
        events=events,
        raw_stdout=raw_stdout,
        raw_stderr=raw_stderr,
        usage_state=usage_state,
        usage=usage,
        patch=patch,
        patch_error=patch_error,
        policy_digest=digest,
        plan_path=plan_path,
        context_path=context_path,
        opened_files=opened_files,
        seed_digest=seed_digest,
        started=started,
    )
    print(json.dumps(envelope, ensure_ascii=False, sort_keys=True))
    if timed_out:
        return 124
    return int(exit_code or 0) if patch_error is None else 78


def trace_from_events(events: Sequence[Mapping[str, Any]], allowed_paths: Iterable[str]) -> dict[str, Any]:
    """Retain only sanitized command/open observations from Codex JSONL."""

    allowed = sorted({str(value).replace("\\", "/") for value in allowed_paths})
    command_hashes: list[str] = []
    opens: list[str] = []
    failures: list[str] = []
    network_attempts: list[str] = []
    leakage_attempts: list[str] = []
    external_side_effect_attempts: list[str] = []
    process_inspection_attempts: list[str] = []
    denied_commands = {"curl", "wget", "nc", "netcat", "ssh", "scp", "telnet", "ftp", "pip", "npm", "brew"}
    leakage_tokens = ("fixtures_v2/oracles", "hidden-checker", "checker.py", "reference", "oracle", "secret", "graph-state", ".codex", "auth.json", "codex_home", "codex-home")
    external_tokens = ("git push", "git commit", "osascript", "open ", "defaults write", "launchctl")
    process_tokens = ("ps ", "pgrep", "pkill", "lsof", "docker", "/proc", "sysctl")
    for event in events:
        item = event.get("item")
        if not isinstance(item, Mapping) or item.get("type") not in {"command_execution", "command", "shell_command"}:
            continue
        command = item.get("command") or item.get("cmd")
        if isinstance(command, list) and all(isinstance(arg, str) for arg in command):
            command = " ".join(command)
        if not isinstance(command, str):
            continue
        command_hashes.append(sha256_bytes(command.encode("utf-8", errors="replace")))
        lowered = command.lower()
        if any(token in lowered for token in leakage_tokens):
            leakage_attempts.append("protected_path_or_content")
        if any(token in lowered for token in external_tokens):
            external_side_effect_attempts.append("external_side_effect_command")
        if any(token in lowered for token in process_tokens):
            process_inspection_attempts.append("process_or_socket_inspection")
        try:
            tokens = shlex.split(command, posix=True)
        except ValueError:
            tokens = command.split()
        if tokens and Path(tokens[0]).name in denied_commands:
            network_attempts.append(Path(tokens[0]).name)
        executable = Path(tokens[0]).name if tokens else ""
        if executable in {"cat", "sed", "head", "tail", "less", "more", "rg", "grep", "find", "ls", "git"}:
            for token in tokens[1:]:
                clean = token.strip("'\"").replace("\\", "/")
                if not clean or clean.startswith(("-", "/", "$")):
                    continue
                if "/" in clean or clean.endswith((".py", ".md", ".toml", ".json", ".js", ".rs")):
                    opens.append(clean[:512])
        exit_code = item.get("exit_code")
        if isinstance(exit_code, int) and exit_code != 0:
            failures.append("command_exit_nonzero")
    return {
        "schema_version": "project-graph-context.worker-trace.v2",
        "source": "codex-jsonl",
        "command_events": len(command_hashes),
        "command_sha256": command_hashes,
        "opens": sorted(set(opens)),
        "failure_codes": failures,
        "network_attempts": sorted(set(network_attempts)),
        "leakage_attempts": sorted(set(leakage_attempts)),
        "external_side_effect_attempts": sorted(set(external_side_effect_attempts)),
        "process_inspection_attempts": sorted(set(process_inspection_attempts)),
        "allowed_paths_sha256": sha256_bytes(canonical_json(allowed)),
    }


def _write_sanitized_events(path: Path, events: Sequence[Mapping[str, Any]], *, prefix: Mapping[str, Any], raw_stdout: bytes) -> None:
    """Persist event/item type counts and hashed final category only."""

    types: dict[str, int] = {}
    item_types: dict[str, int] = {}
    final_text = ""
    for event in events:
        kind = event.get("type")
        if isinstance(kind, str):
            types[kind] = types.get(kind, 0) + 1
        item = event.get("item")
        if isinstance(item, Mapping):
            item_kind = item.get("type")
            if isinstance(item_kind, str):
                item_types[item_kind] = item_types.get(item_kind, 0) + 1
            if item_kind in {"agent_message", "message"} and isinstance(item.get("text"), str):
                final_text = str(item["text"])
    lowered = final_text.lower()
    if not final_text:
        final_category = "no_text"
    elif any(token in lowered for token in ("implemented", "changed", "fixed", "edited")):
        final_category = "implemented"
    elif any(token in lowered for token in ("blocked", "cannot", "unable", "timed out")):
        final_category = "blocked"
    elif any(token in lowered for token in ("refuse", "refused", "decline")):
        final_category = "refusal"
    elif any(token in lowered for token in ("explain", "summary", "here is", "completed")):
        final_category = "explanation"
    else:
        final_category = "unknown"
    payload = {
        "schema_version": "project-graph-context.worker-events-summary.v1",
        "prefix": {key: value for key, value in prefix.items() if key not in {"context_path", "policy_path"}},
        "event_count": len(events),
        "event_types": dict(sorted(types.items())),
        "item_types": dict(sorted(item_types.items())),
        "final_category": final_category,
        "final_length": len(final_text),
        "final_sha256": sha256_bytes(final_text.encode("utf-8")) if final_text else None,
        "raw_stream_sha256": sha256_bytes(raw_stdout),
    }
    write_canonical(path, payload)


def planner_prompt(task_id: str, intent: Mapping[str, Any]) -> str:
    payload = {
        "task_id": task_id,
        "title": intent.get("title"),
        "goal": intent.get("goal"),
        "allowed_paths": intent.get("allowed_paths", []),
        "acceptance_checks": intent.get("acceptance_checks", []),
        "behavior_steps": intent.get("behavior_steps", []),
    }
    return (
        "You are the arm-blind Sol-high planner for a sanitized offline coding benchmark. "
        "Produce one small orchestration skeleton. You are not given and must not request any arm, graph, prior, outcome, oracle, checker, or lesson context. "
        "Return JSON only with exactly these fields: schema_version, task_id, objective, acceptance_checks, allowed_paths, steps. "
        "Do not include secrets, absolute paths, shell pipelines, network actions, or claims about observed outcomes. "
        f"Task manifest:\n{json.dumps(payload, sort_keys=True)}"
    )


def run_planner_v2(task_id: str, output_dir: Path, *, max_tokens: int = 25_000, timeout_seconds: float = DEFAULT_SOL_TIMEOUT_SECONDS) -> dict[str, Any]:
    """Run and freeze exactly one Sol-high plan for one v2 task."""

    intent = _validate_task(task_id)
    output_dir = output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    plan_path = output_dir / f"{task_id}.json"
    metadata_path = output_dir / f"{task_id}.metadata.json"
    if plan_path.exists() or metadata_path.exists():
        raise AdapterError(f"refusing to overwrite frozen Sol plan: {plan_path}")
    with tempfile.TemporaryDirectory(prefix="pgc-v2-sol-", dir=str(output_dir)) as temporary:
        cwd = Path(temporary)
        schema_path = Path(__file__).resolve().parent / "schemas" / "live-plan.v1.schema.json"
        argv = codex_argv(model="gpt-5.6-sol", sandbox="read-only", cwd=cwd, output_schema=schema_path)
        env = offline_env()
        env["FRACTAL_ADAPTER_CWD"] = str(cwd)
        started = time.monotonic()
        with fresh_codex_home(env) as isolated:
            exit_code, timed_out, events, raw_stdout, raw_stderr = _run_codex(argv, planner_prompt(task_id, intent), env=isolated, cwd=cwd, timeout_seconds=timeout_seconds)
        if timed_out:
            raise AdapterError("planner_timeout:" + json.dumps(_planner_failure_summary(events, raw_stderr), sort_keys=True))
        final_texts = [
            str(event["item"].get("text"))
            for event in events
            if event.get("type") == "item.completed"
            and isinstance(event.get("item"), Mapping)
            and event["item"].get("type") == "agent_message"
            and isinstance(event["item"].get("text"), str)
        ]
        if not final_texts:
            raise AdapterError("planner_no_final:" + json.dumps(_planner_failure_summary(events, raw_stderr), sort_keys=True))
        raw = _extract_json_object(final_texts[-1])
        plan = {
            "schema_version": PLAN_SCHEMA_VERSION,
            "task_id": task_id,
            "objective": raw.get("objective"),
            "acceptance_checks": raw.get("acceptance_checks", intent.get("acceptance_checks", [])),
            "allowed_paths": raw.get("allowed_paths", intent.get("allowed_paths", [])),
            "steps": raw.get("steps"),
        }
        _validate_plan(plan, task_id)
        usage_rows = _usage_rows(events)
        usage: dict[str, Any] | None = None
        if usage_rows:
            inputs = [row.get("input_tokens") for row in usage_rows]
            outputs = [row.get("output_tokens") for row in usage_rows]
            if all(isinstance(value, int) and not isinstance(value, bool) and value >= 0 for value in [*inputs, *outputs]):
                usage = {"input_tokens": sum(inputs), "output_tokens": sum(outputs), "total_tokens": sum(inputs) + sum(outputs), "cli_fields": [dict(row) for row in usage_rows]}
        if usage and usage["total_tokens"] > max_tokens:
            raise AdapterError(f"Sol planner exceeded token cap for {task_id}: {usage['total_tokens']} > {max_tokens}")
        plan_digest = write_canonical(plan_path, plan)
        metadata = {
            "task_id": task_id,
            "model": "gpt-5.6-sol",
            "reasoning_effort": DEFAULT_REASONING,
            "plan_sha256": plan_digest,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "duration_ms": round((time.monotonic() - started) * 1000.0, 3),
            "usage": usage,
            "usage_state": "valid" if usage is not None else "unavailable",
            "raw_event_sha256": sha256_bytes(raw_stdout),
            "stderr_sha256": sha256_bytes(raw_stderr),
            "token_cap": max_tokens,
        }
        write_canonical(metadata_path, metadata)
        return metadata


def worker_prompt(intent: Mapping[str, Any], plan: Mapping[str, Any], context: Mapping[str, Any], task_id: str) -> str:
    task_payload = {
        "task_id": task_id,
        "title": intent.get("title"),
        "goal": intent.get("goal"),
        "allowed_paths": intent.get("allowed_paths", []),
        "forbidden_paths": intent.get("forbidden_paths", []),
        "acceptance_checks": intent.get("acceptance_checks", []),
        "behavior_steps": intent.get("behavior_steps", []),
    }
    return (
        "You are the Luna implementation worker in one fresh, isolated offline benchmark worktree. "
        "Implement only the task prompt using the frozen arm-blind Sol plan and the C context below. "
        "Do not seek web or external services, do not read hidden checkers or reference patches, do not inspect or copy .codex/auth.json/CODEX_HOME, do not commit/push, and do not modify files outside allowed paths. "
        "Use repository-local checks only. You must inspect and edit the allowed source files with tools, run a focused local verification, and never report success without a real allowed-path change. If blocked, say so explicitly. The C context is read-only and contains behavior/source/execution graph nodes with no prior lessons. "
        f"TASK PROMPT:\n{json.dumps(task_payload, sort_keys=True)}\n"
        f"FROZEN SOL PLAN (same task only):\n{json.dumps(plan, sort_keys=True)}\n"
        f"CONTEXT CONDITION C:\n{json.dumps(context, sort_keys=True)}"
    )


def structured_patch_prompt(
    intent: Mapping[str, Any],
    plan: Mapping[str, Any],
    context: Mapping[str, Any],
    seed_files: Mapping[str, str],
    *,
    task_id: str | None = None,
) -> str:
    """Build the fallback prompt from the four explicitly allowed payloads.

    No mounted-context path, event stream, prior attempt, hidden-checker
    detail, or environment value is interpolated.  ``seed_files`` is already
    read and audited by ``preload_seed_files``; this function only serializes
    it in deterministic order.
    """

    payload = {
        "task_manifest": json.loads(json.dumps(intent, ensure_ascii=False, sort_keys=True)),
        "context_condition_c": json.loads(json.dumps(context, ensure_ascii=False, sort_keys=True)),
        "frozen_sol_plan": json.loads(json.dumps(plan, ensure_ascii=False, sort_keys=True)),
        "allowed_seed_files": {str(path): str(seed_files[path]) for path in sorted(seed_files)},
    }
    return (
        "You are Luna, an offline implementation worker. Use only the task "
        "manifest, condition-C graph, frozen Sol plan, and allowed seed-file "
        "contents below. Do not request tools, open other files, inspect the "
        "environment, use a network, or invent context. Return JSON only that "
        "matches the supplied structured-patch schema: changes is a list of "
        "complete file replacements with exact relative path and UTF-8 content; "
        "summary is a short description; checks is a list of local checks you "
        "would run. Include at least one real changed allowed file. Do not put "
        "markdown fences around the JSON. The trusted runner validates and "
        "applies changes after this process exits.\n"
        f"TASK MANIFEST, C GRAPH, FROZEN SOL PLAN, AND SEED FILES:\n"
        f"{json.dumps(payload, ensure_ascii=False, sort_keys=True)}"
    )


def run_worker_v2() -> int:
    """Run one Luna call using paths supplied by ``live_v2_runner``."""

    route = os.environ.get("FRACTAL_LUNA_ROUTE", LUNA_STRUCTURED_PATCH_ROUTE)
    if route == LUNA_STRUCTURED_PATCH_ROUTE:
        return run_structured_patch_worker_v2()
    if route not in {LUNA_LEGACY_ROUTE, "codex-luna", "codex"}:
        raise AdapterError(f"unknown Luna route: {route}")

    task_id = _safe_env("FRACTAL_TASK_ID")
    episode_id = _safe_env("FRACTAL_EPISODE_ID")
    worktree = _resolve_env_path("FRACTAL_WORKTREE")
    intent_path = _resolve_env_path("FRACTAL_TASK_INTENT_PATH")
    context_path = _resolve_env_path("FRACTAL_CONTEXT_PATH")
    plan_path = _resolve_env_path("FRACTAL_SOL_PLAN_PATH")
    trace_path = _resolve_env_path("FRACTAL_TRACE_PATH")
    event_path = _resolve_env_path("FRACTAL_EVENT_PATH")
    receipt_path = _resolve_env_path("FRACTAL_USAGE_RECEIPT_PATH")
    policy_path = _resolve_env_path("FRACTAL_POLICY_PATH")
    report_path = _resolve_env_path("FRACTAL_ENFORCEMENT_REPORT_PATH")
    assert task_id and episode_id and worktree and intent_path and context_path and plan_path and trace_path and event_path and receipt_path and policy_path and report_path
    if task_id not in TASKS_V2 or not worktree.is_dir() or not intent_path.is_file() or not context_path.is_file() or not plan_path.is_file() or not policy_path.is_file():
        raise AdapterError("runner paths are missing or wrong kind")
    intent = _read_json(intent_path, "task intent")
    context = _read_json(context_path, "arm C context")
    plan = _read_json(plan_path, "frozen Sol plan")
    if intent.get("task_id") != task_id or context.get("task_id") != task_id:
        raise AdapterError("task/context id mismatch")
    _validate_plan(plan, task_id)
    if context.get("schema_version") != "project-graph-context.c-graph-context.v1":
        raise AdapterError("worker requires context condition C")
    if "prior" in context or "lesson" in json.dumps(context, sort_keys=True).lower():
        raise AdapterError("context C must not contain prior lessons")
    if context_path.stat().st_mode & 0o222:
        raise AdapterError("mounted context is writable")
    policy = _read_json(policy_path, "harness policy")
    digest = policy_hash(policy)
    route = route_eligibility("codex", shell_allowed=True, network_denied=True)
    report = enforcement_report(policy, provider_route=route, episode_id=episode_id, policy_digest=digest)
    write_canonical(report_path, report)
    if route.get("status") != "eligible":
        raise AdapterError(f"Codex route ineligible: {route.get('reason')}")
    prefix = {
        "type": "adapter.started",
        "task_id": task_id,
        "episode_id": episode_id,
        "model": "gpt-5.6-luna",
        "route": route,
        "reasoning_effort": DEFAULT_REASONING,
        "sandbox": "workspace-write",
        "network_disabled": True,
        "policy_hash": digest,
        "plan_sha256": sha256_file(plan_path),
        "context_sha256": sha256_file(context_path),
    }
    argv = codex_argv(model="gpt-5.6-luna", sandbox="workspace-write", cwd=worktree)
    env = offline_env()
    env.update(
        {
            "FRACTAL_WORKTREE": str(worktree),
            "FRACTAL_ADAPTER_CWD": str(worktree),
            "FRACTAL_TASK_ID": task_id,
            "FRACTAL_EPISODE_ID": episode_id,
            "FRACTAL_CONTEXT_PATH": str(context_path),
            "FRACTAL_TASK_INTENT_PATH": str(intent_path),
            "FRACTAL_SOL_PLAN_PATH": str(plan_path),
            "FRACTAL_TRACE_PATH": str(trace_path),
            "FRACTAL_EVENT_PATH": str(event_path),
            "FRACTAL_USAGE_RECEIPT_PATH": str(receipt_path),
            "FRACTAL_POLICY_PATH": str(policy_path),
            "FRACTAL_ENFORCEMENT_REPORT_PATH": str(report_path),
        }
    )
    started = time.monotonic()
    with fresh_codex_home(env) as isolated:
        exit_code, timed_out, events, raw_stdout, raw_stderr = _run_codex(argv, worker_prompt(intent, plan, context, task_id), env=isolated, cwd=worktree, timeout_seconds=float(os.environ.get("FRACTAL_LUNA_TIMEOUT_SECONDS", str(DEFAULT_LUNA_TIMEOUT_SECONDS))))
    _write_sanitized_events(event_path, events, prefix=prefix, raw_stdout=raw_stdout)
    trace = trace_from_events(events, intent.get("allowed_paths", []))
    trace.update({"timed_out": timed_out, "exit_code": exit_code, "duration_ms": round((time.monotonic() - started) * 1000.0, 3)})
    usage, usage_state = usage_receipt(events, receipt_path)
    trace["usage_state"] = usage_state
    if usage is not None:
        trace["usage"] = {key: usage.get(key) for key in ("input_tokens", "output_tokens", "total_tokens", "cost_usd")}
    write_canonical(trace_path, trace)
    # The adapter itself never returns a success claim for a git mutation or a
    # timeout.  The runner performs the authoritative snapshot/checker step.
    summary = {
        "adapter": "codex-live-v2",
        "route": route,
        "task_id": task_id,
        "episode_id": episode_id,
        "model": "gpt-5.6-luna",
        "exit_code": exit_code,
        "timed_out": timed_out,
        "usage_state": usage_state,
        "usage": trace.get("usage"),
        "policy_hash": digest,
        "event_sha256": sha256_bytes(raw_stdout),
        "stderr_sha256": sha256_bytes(raw_stderr),
    }
    print(json.dumps(summary, sort_keys=True))
    if timed_out:
        return 124
    return int(exit_code or 0)


def _planner_cli(args: argparse.Namespace) -> int:
    tasks = args.task_id
    if len(tasks) != len(set(tasks)):
        raise SystemExit("duplicate task ids would cause multiple Sol calls")
    metadata = [run_planner_v2(task_id, args.output_dir, max_tokens=args.max_tokens, timeout_seconds=args.timeout_seconds) for task_id in tasks]
    print(json.dumps({"plans": metadata, "count": len(metadata)}, indent=2, sort_keys=True))
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    planner = sub.add_parser("plan", help="run one frozen Sol-high plan call per task (paid; use only after root approval)")
    planner.add_argument("--task-id", action="append", required=True)
    planner.add_argument("--output-dir", type=Path, required=True)
    planner.add_argument("--max-tokens", type=int, default=25_000)
    planner.add_argument("--timeout-seconds", type=float, default=DEFAULT_SOL_TIMEOUT_SECONDS)
    planner.set_defaults(func=_planner_cli)
    worker = sub.add_parser("worker", help="run one Luna worker from FRACTAL_* paths")
    worker.set_defaults(func=lambda _args: run_worker_v2())
    args = parser.parse_args(argv)
    try:
        return int(args.func(args))
    except AdapterError as exc:
        print(json.dumps({"adapter_error": str(exc)}, sort_keys=True), file=sys.stderr)
        return 78


if __name__ == "__main__":
    raise SystemExit(main())
