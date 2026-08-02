#!/usr/bin/env python3
"""Live Sol/Luna adapter for the project-graph-context harness.

This module deliberately keeps the Codex invocation as an argv tuple.  The
``plan`` mode is used exactly once per task to freeze an arm-blind Sol plan;
the worker mode is invoked once by ``runner.py`` for each fresh episode.  The
worker receives only the task intent, frozen plan, and mounted arm context.

Codex's JSONL stream is retained as an event ledger outside the episode
worktree.  Usage receipts are written only when the CLI emits valid numeric
``turn.completed.usage`` fields.  A missing cost field remains ``null``; no
cost or token estimate is inferred from wall time, output bytes, or text.
"""

from __future__ import annotations

import argparse
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

try:
    from .corpus import TASKS, task_manifest
except ImportError:  # pragma: no cover - direct script execution
    from corpus import TASKS, task_manifest


USAGE_SCHEMA_VERSION = "project-graph-context.usage-receipt.v1"
PLAN_SCHEMA_VERSION = "project-graph-context.sol-plan.v1"
ARMS = ("A", "B", "C", "D")
DEFAULT_REASONING = "high"
DEFAULT_WORKER_MAX_TOKENS = 20_000
DEFAULT_CONTEXT_WINDOW = 20_000
DEFAULT_AUTO_COMPACT = 16_000
DEFAULT_TIMEOUT_SECONDS = 115.0
MAX_EVENT_BYTES = 4_000_000


class AdapterError(RuntimeError):
    """A configuration or protocol error that must not be hidden."""


def canonical_json(value: Any) -> bytes:
    return (json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")) + "\n").encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(canonical_json(value))


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
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise AdapterError(f"{label} is not valid JSON: {path}") from exc
    if not isinstance(payload, Mapping):
        raise AdapterError(f"{label} must be a JSON object: {path}")
    return payload


def _validate_task(task_id: str) -> Mapping[str, Any]:
    if task_id not in TASKS:
        raise AdapterError(f"unknown task: {task_id}")
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
        if not isinstance(plan.get(key), list) or not all(isinstance(item, (str, Mapping)) for item in plan[key]):
            raise AdapterError(f"frozen Sol plan field {key} must be a list of strings/objects")
    forbidden = {"arm", "context", "prior", "outcome", "oracle", "checker"}
    leaked = {str(key).lower() for key in plan} & forbidden
    if leaked:
        raise AdapterError(f"arm-blind Sol plan contains forbidden context fields: {sorted(leaked)}")


def _extract_json_object(text: str) -> Mapping[str, Any]:
    candidate = text.strip()
    if candidate.startswith("```"):
        candidate = re.sub(r"^```(?:json)?\s*", "", candidate, flags=re.IGNORECASE)
        candidate = re.sub(r"\s*```$", "", candidate)
    try:
        value = json.loads(candidate)
    except json.JSONDecodeError:
        starts = [index for index, char in enumerate(candidate) if char == "{"]
        if not starts:
            raise AdapterError("Sol final response did not contain a JSON object")
        end = candidate.rfind("}")
        if end <= starts[0]:
            raise AdapterError("Sol final response contained an incomplete JSON object")
        try:
            value = json.loads(candidate[starts[0] : end + 1])
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


def _base_codex_argv(*, model: str, sandbox: str, cwd: Path, output_schema: Path | None = None) -> list[str]:
    """Return explicit argv; this function must never return a shell string."""

    argv = [
        _codex_binary(),
        "--ask-for-approval",
        "never",
        "--sandbox",
        sandbox,
        "--model",
        model,
        "-c",
        f'model_reasoning_effort="{DEFAULT_REASONING}"',
        "-c",
        'web_search="disabled"',
        "-c",
        f"model_context_window={DEFAULT_CONTEXT_WINDOW}",
        "-c",
        f"model_auto_compact_token_limit={DEFAULT_AUTO_COMPACT}",
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


def _offline_env(base: Mapping[str, str] | None = None) -> dict[str, str]:
    env = dict(base or os.environ)
    env.update(
        {
            "FRACTAL_OFFLINE": "1",
            "NO_NETWORK": "1",
            "PIP_NO_INDEX": "1",
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_OPTIONAL_LOCKS": "0",
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
    return env


def _usage_rows(events: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    rows: list[Mapping[str, Any]] = []
    for event in events:
        if event.get("type") != "turn.completed":
            continue
        usage = event.get("usage")
        if isinstance(usage, Mapping):
            rows.append(usage)
    return rows


def _actual_usage_receipt(events: Sequence[Mapping[str, Any]], receipt_path: Path) -> tuple[dict[str, Any] | None, str]:
    """Write a receipt only from numeric CLI usage fields.

    The CLI currently emits input/output fields but not total or cost.  Total
    is the arithmetic sum of those two emitted fields (never an estimate),
    while cost remains unavailable unless a numeric CLI cost field is present.
    """

    rows = _usage_rows(events)
    if not rows:
        return None, "unavailable"
    numeric_rows: list[tuple[int, int]] = []
    costs: list[float] = []
    for row in rows:
        input_tokens = row.get("input_tokens")
        output_tokens = row.get("output_tokens")
        if (
            isinstance(input_tokens, bool)
            or not isinstance(input_tokens, int)
            or input_tokens < 0
            or isinstance(output_tokens, bool)
            or not isinstance(output_tokens, int)
            or output_tokens < 0
        ):
            return None, "invalid_usage"
        numeric_rows.append((input_tokens, output_tokens))
        cost = row.get("cost_usd")
        if cost is not None:
            if isinstance(cost, bool) or not isinstance(cost, (int, float)) or cost < 0:
                return None, "invalid_usage"
            costs.append(float(cost))
    input_total = sum(pair[0] for pair in numeric_rows)
    output_total = sum(pair[1] for pair in numeric_rows)
    cost: float | None = sum(costs) if len(costs) == len(numeric_rows) else None
    receipt = {
        "schema_version": USAGE_SCHEMA_VERSION,
        "source": "worker",
        "input_tokens": input_total,
        "output_tokens": output_total,
        "total_tokens": input_total + output_total,
        "cost_usd": cost,
        "cli_usage_fields": [dict(row) for row in rows],
    }
    _write_json(receipt_path, receipt)
    return receipt, "valid"


def _event_item(event: Mapping[str, Any]) -> Mapping[str, Any] | None:
    item = event.get("item")
    return item if isinstance(item, Mapping) else None


def _command_text(item: Mapping[str, Any]) -> str | None:
    if item.get("type") not in {"command_execution", "command", "shell_command"}:
        return None
    for key in ("command", "cmd", "argv"):
        value = item.get(key)
        if isinstance(value, str):
            return value
        if isinstance(value, list) and all(isinstance(arg, str) for arg in value):
            return " ".join(value)
    return None


def _trace_from_events(events: Sequence[Mapping[str, Any]], allowed_paths: Iterable[str]) -> dict[str, Any]:
    """Normalize only observations present in the Codex JSONL stream."""

    allowed = {str(path).replace("\\", "/") for path in allowed_paths}
    command_rows: list[str] = []
    opens: list[str] = []
    failures: list[str] = []
    for event in events:
        item = _event_item(event)
        if item is None:
            continue
        command = _command_text(item)
        if command is None:
            continue
        command_rows.append(command)
        try:
            tokens = shlex.split(command, posix=True)
        except ValueError:
            tokens = command.split()
        # Read commands are an explicit, conservative open trace.  We retain
        # only relative repository-looking paths, never absolute host paths.
        executable = Path(tokens[0]).name if tokens else ""
        if executable in {"cat", "sed", "head", "tail", "less", "more", "rg", "grep", "find", "ls", "git"}:
            for token in tokens[1:]:
                clean = token.strip("'\"").replace("\\", "/")
                if not clean or clean.startswith("-") or clean.startswith("/") or clean.startswith("$"):
                    continue
                if "/" in clean or clean.endswith((".py", ".md", ".toml", ".json")):
                    opens.append(clean)
        exit_code = item.get("exit_code")
        if isinstance(exit_code, int) and exit_code != 0:
            failures.append("command_exit_nonzero")
    trace: dict[str, Any] = {
        "schema_version": "project-graph-context.worker-trace.v1",
        "source": "codex-jsonl",
        "command_events": len(command_rows),
    }
    if command_rows:
        trace["opens"] = sorted(set(opens))
        trace["failure_codes"] = failures
    # Repairs, routing, and tool selection require explicit worker telemetry;
    # do not infer them from turn count or command count.
    if allowed:
        trace["allowed_paths_sha256"] = sha256_bytes(canonical_json(sorted(allowed)))
    return trace


def _snapshot_git(cwd: Path) -> tuple[str | None, tuple[str, ...]]:
    def run_git(args: Sequence[str]) -> str | None:
        try:
            completed = subprocess.run(["git", "-C", str(cwd), *args], capture_output=True, text=True, check=True, timeout=10, env=_offline_env())
        except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
            return None
        return completed.stdout.strip()

    head = run_git(["rev-parse", "HEAD"])
    refs_raw = run_git(["for-each-ref", "--format=%(refname)=%(objectname)"])
    refs = tuple(sorted(refs_raw.splitlines())) if refs_raw is not None else tuple()
    return head, refs


def _kill_process_group(process: subprocess.Popen[bytes]) -> None:
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except (OSError, ProcessLookupError):
        try:
            process.terminate()
        except OSError:
            pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (OSError, ProcessLookupError):
            try:
                process.kill()
            except OSError:
                pass


def _run_codex(argv: Sequence[str], prompt: str, *, env: Mapping[str, str], timeout_seconds: float) -> tuple[int | None, bool, list[Mapping[str, Any]], bytes]:
    if isinstance(argv, str) or not argv:
        raise AdapterError("Codex invocation must be a non-empty argv list")
    if any("\0" in str(arg) for arg in argv):
        raise AdapterError("Codex argv contains NUL")
    process = subprocess.Popen(
        [str(arg) for arg in argv],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=dict(env),
        cwd=str(env.get("FRACTAL_WORKTREE") or env.get("FRACTAL_ADAPTER_CWD") or Path.cwd()),
        shell=False,
        start_new_session=True,
    )
    try:
        stdout, _stderr = process.communicate(prompt.encode("utf-8"), timeout=timeout_seconds)
        timed_out = False
    except subprocess.TimeoutExpired as exc:
        _kill_process_group(process)
        stdout = exc.output or b""
        timed_out = True
    events: list[Mapping[str, Any]] = []
    for line in stdout.splitlines():
        try:
            payload = json.loads(line.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if isinstance(payload, Mapping):
            events.append(payload)
    return process.returncode if not timed_out else None, timed_out, events, stdout


def _write_jsonl(path: Path, rows: Sequence[Mapping[str, Any]], *, prefix: Mapping[str, Any] | None = None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    written = 0
    with path.open("wb") as handle:
        all_rows: list[Mapping[str, Any]] = []
        if prefix is not None:
            all_rows.append(prefix)
        all_rows.extend(rows)
        for row in all_rows:
            data = canonical_json(row)
            if written + len(data) > MAX_EVENT_BYTES:
                marker = canonical_json({"type": "adapter.events_truncated", "max_bytes": MAX_EVENT_BYTES})
                if written + len(marker) <= MAX_EVENT_BYTES:
                    handle.write(marker)
                break
            handle.write(data)
            written += len(data)


def _planner_prompt(task_id: str, intent: Mapping[str, Any]) -> str:
    payload = {
        "task_id": task_id,
        "title": intent.get("title"),
        "goal": intent.get("goal"),
        "allowed_paths": intent.get("allowed_paths", []),
        "acceptance_checks": intent.get("acceptance_checks", []),
    }
    return (
        "You are the frozen Sol-high planner for an offline coding benchmark. "
        "Generate one arm-blind orchestration/acceptance skeleton. You are not given and must not request any arm, graph, prior, outcome, oracle, or hidden-checker context. "
        "Return JSON only with exactly these useful fields: schema_version, task_id, objective, acceptance_checks, allowed_paths, steps. "
        "steps must be a short ordered list of concrete implementation/check commands in plain language. "
        "Do not include secrets, absolute paths, shell pipelines, network actions, or claims about an observed outcome. "
        f"Task manifest:\n{json.dumps(payload, sort_keys=True)}"
    )


def run_planner(task_id: str, output_dir: Path) -> dict[str, Any]:
    """Run exactly one Sol call and freeze its validated plan."""

    intent = _validate_task(task_id)
    output_dir = output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    plan_path = output_dir / f"{task_id}.json"
    if plan_path.exists():
        raise AdapterError(f"refusing to overwrite frozen plan: {plan_path}")
    with tempfile.TemporaryDirectory(prefix="pgc-sol-", dir=str(output_dir)) as temporary:
        cwd = Path(temporary)
        schema_path = Path(__file__).resolve().parent / "schemas" / "live-plan.v1.schema.json"
        argv = _base_codex_argv(model="gpt-5.6-sol", sandbox="read-only", cwd=cwd, output_schema=schema_path)
        env = _offline_env()
        env["FRACTAL_ADAPTER_CWD"] = str(cwd)
        started = time.monotonic()
        exit_code, timed_out, events, raw_stdout = _run_codex(argv, _planner_prompt(task_id, intent), env=env, timeout_seconds=float(os.environ.get("FRACTAL_SOL_TIMEOUT_SECONDS", "120")))
        if timed_out:
            raise AdapterError(f"Sol planner timed out for {task_id}")
        final_texts = [
            str(event["item"].get("text"))
            for event in events
            if event.get("type") == "item.completed"
            and isinstance(event.get("item"), Mapping)
            and event["item"].get("type") == "agent_message"
            and isinstance(event["item"].get("text"), str)
        ]
        if not final_texts:
            raise AdapterError(f"Sol planner emitted no final agent message for {task_id} (exit={exit_code})")
        plan_raw = dict(_extract_json_object(final_texts[-1]))
        # Preserve the requested schema as an adapter-owned envelope, while
        # retaining only fields the planner was allowed to see.
        plan = {
            "schema_version": PLAN_SCHEMA_VERSION,
            "task_id": task_id,
            "objective": plan_raw.get("objective"),
            "acceptance_checks": plan_raw.get("acceptance_checks", intent.get("acceptance_checks", [])),
            "allowed_paths": plan_raw.get("allowed_paths", intent.get("allowed_paths", [])),
            "steps": plan_raw.get("steps"),
        }
        _validate_plan(plan, task_id)
        plan_digest = sha256_bytes(canonical_json(plan))
        _write_json(plan_path, plan)
        usage_rows = _usage_rows(events)
        usage = _summarize_usage(usage_rows)
        metadata = {
            "task_id": task_id,
            "model": "gpt-5.6-sol",
            "reasoning_effort": DEFAULT_REASONING,
            "plan_sha256": plan_digest,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "duration_ms": round((time.monotonic() - started) * 1000.0, 3),
            "usage": usage,
            "raw_event_sha256": sha256_bytes(raw_stdout),
        }
        _write_json(output_dir / f"{task_id}.metadata.json", metadata)
        return metadata


def _summarize_usage(rows: Sequence[Mapping[str, Any]]) -> dict[str, Any] | None:
    if not rows:
        return None
    input_tokens = [row.get("input_tokens") for row in rows]
    output_tokens = [row.get("output_tokens") for row in rows]
    if not all(isinstance(value, int) and not isinstance(value, bool) and value >= 0 for value in (*input_tokens, *output_tokens)):
        return None
    return {
        "input_tokens": sum(input_tokens),
        "output_tokens": sum(output_tokens),
        "total_tokens": sum(input_tokens) + sum(output_tokens),
        "cli_fields": [dict(row) for row in rows],
    }


def _worker_prompt(intent: Mapping[str, Any], plan: Mapping[str, Any], context: Mapping[str, Any], arm_id: str) -> str:
    task_payload = {
        "task_id": intent.get("task_id"),
        "title": intent.get("title"),
        "goal": intent.get("goal"),
        "allowed_paths": intent.get("allowed_paths", []),
        "forbidden_paths": intent.get("forbidden_paths", []),
        "acceptance_checks": intent.get("acceptance_checks", []),
    }
    return (
        "You are the Luna implementation worker in an offline, isolated benchmark episode. "
        "Implement the task in the current worktree. Use only the task prompt, frozen Sol plan, and the arm context below. "
        "Do not seek web/external services, do not read hidden checkers, do not commit/push, and do not modify files outside the allowed target path. "
        "Use repository-local commands only (for example, inspect the target file and run a focused local Python check). "
        "The mounted context is read-only and may be incomplete; never attempt to write it. "
        f"Arm={arm_id}.\nTASK PROMPT:\n{json.dumps(task_payload, sort_keys=True)}\n"
        f"FROZEN SOL PLAN (arm-blind):\n{json.dumps(plan, sort_keys=True)}\n"
        f"ARM CONTEXT (the only curated context for this arm):\n{json.dumps(context, sort_keys=True)}"
    )


def run_worker() -> int:
    """Run one Luna episode using only FRACTAL_* paths supplied by runner."""

    task_id = _safe_env("FRACTAL_TASK_ID")
    arm_id = _safe_env("FRACTAL_ARM_ID")
    assert task_id is not None and arm_id is not None
    if arm_id not in ARMS:
        raise AdapterError(f"unknown arm: {arm_id}")
    intent_path = _resolve_env_path("FRACTAL_TASK_INTENT_PATH")
    context_path = _resolve_env_path("FRACTAL_CONTEXT_PATH")
    worktree = _resolve_env_path("FRACTAL_WORKTREE")
    trace_path = _resolve_env_path("FRACTAL_TRACE_PATH")
    event_path = _resolve_env_path("FRACTAL_EVENT_PATH")
    receipt_path = _resolve_env_path("FRACTAL_USAGE_RECEIPT_PATH")
    plan_dir = _resolve_env_path("FRACTAL_SOL_PLAN_DIR")
    assert intent_path and context_path and worktree and trace_path and event_path and receipt_path and plan_dir
    if not worktree.is_dir() or not intent_path.is_file() or not context_path.exists() or not plan_dir.is_dir():
        raise AdapterError("runner paths are missing or not the expected kind")
    intent = _read_json(intent_path, "task intent")
    if intent.get("task_id") != task_id:
        raise AdapterError("task intent id does not match FRACTAL_TASK_ID")
    context = _read_json(context_path, "arm context")
    if context.get("arm_id") not in {None, arm_id}:
        raise AdapterError("context arm id does not match FRACTAL_ARM_ID")
    plan_path = plan_dir / f"{task_id}.json"
    try:
        plan_path.relative_to(plan_dir)
    except ValueError as exc:
        raise AdapterError("plan path escaped plan directory") from exc
    plan = _read_json(plan_path, "frozen Sol plan")
    _validate_plan(plan, task_id)
    if not context_path.is_file():
        raise AdapterError("worker context must be a single mounted JSON file")
    try:
        context_mode = context_path.stat().st_mode & 0o222
    except OSError as exc:
        raise AdapterError("unable to inspect mounted context permissions") from exc
    if context_mode:
        raise AdapterError("mounted context is writable; refusing to run")

    trace_path.parent.mkdir(parents=True, exist_ok=True)
    pre_head, pre_refs = _snapshot_git(worktree)
    prefix = {
        "type": "adapter.started",
        "task_id": task_id,
        "arm_id": arm_id,
        "model": "gpt-5.6-luna",
        "reasoning_effort": DEFAULT_REASONING,
        "sandbox": "workspace-write",
        "network_disabled": True,
        "plan_sha256": sha256_file(plan_path),
        "context_sha256": sha256_file(context_path) if context_path.is_file() else None,
    }
    argv = _base_codex_argv(model="gpt-5.6-luna", sandbox="workspace-write", cwd=worktree)
    env = _offline_env()
    env.update(
        {
            "FRACTAL_WORKTREE": str(worktree),
            "FRACTAL_ADAPTER_CWD": str(worktree),
            "FRACTAL_CONTEXT_PATH": str(context_path),
            "FRACTAL_TASK_INTENT_PATH": str(intent_path),
            "FRACTAL_ARM_ID": arm_id,
            "FRACTAL_TASK_ID": task_id,
        }
    )
    started = time.monotonic()
    exit_code, timed_out, events, raw_stdout = _run_codex(
        argv,
        _worker_prompt(intent, plan, context, arm_id),
        env=env,
        timeout_seconds=float(os.environ.get("FRACTAL_LIVE_TIMEOUT_SECONDS", str(DEFAULT_TIMEOUT_SECONDS))),
    )
    _write_jsonl(event_path, events, prefix=prefix)
    trace = _trace_from_events(events, intent.get("allowed_paths", []))
    trace["timed_out"] = timed_out
    trace["exit_code"] = exit_code
    trace["duration_ms"] = round((time.monotonic() - started) * 1000.0, 3)
    usage, usage_state = _actual_usage_receipt(events, receipt_path)
    trace["usage_state"] = usage_state
    if usage is not None:
        trace["usage"] = {key: usage[key] for key in ("input_tokens", "output_tokens", "total_tokens", "cost_usd")}
    post_head, post_refs = _snapshot_git(worktree)
    mutation = pre_head != post_head or pre_refs != post_refs
    trace["git_mutation_detected"] = mutation
    if mutation:
        trace["git_mutation_detail"] = {"head_changed": pre_head != post_head, "refs_changed": pre_refs != post_refs}
    _write_json(trace_path, trace)
    summary = {
        "adapter": "codex-live",
        "model": "gpt-5.6-luna",
        "exit_code": exit_code,
        "timed_out": timed_out,
        "usage_state": usage_state,
        "usage": trace.get("usage"),
        "git_mutation_detected": mutation,
        "event_sha256": sha256_bytes(raw_stdout),
    }
    print(json.dumps(summary, sort_keys=True))
    if mutation:
        return 70
    if timed_out:
        return 124
    return int(exit_code or 0)


def _planner_cli(args: argparse.Namespace) -> int:
    tasks = args.task_id or sorted(TASKS)
    if len(tasks) != len(set(tasks)):
        raise SystemExit("duplicate task ids would cause multiple Sol calls")
    metadata = [run_planner(task_id, args.output_dir) for task_id in tasks]
    print(json.dumps({"plans": metadata, "count": len(metadata)}, indent=2, sort_keys=True))
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    planner = sub.add_parser("plan", help="run one frozen Sol plan call per task")
    planner.add_argument("--task-id", action="append", required=True)
    planner.add_argument("--output-dir", type=Path, required=True)
    planner.set_defaults(func=_planner_cli)
    worker = sub.add_parser("worker", help="run one Luna worker from FRACTAL_* paths")
    worker.set_defaults(func=lambda _args: run_worker())
    args = parser.parse_args(argv)
    try:
        return int(args.func(args))
    except AdapterError as exc:
        print(json.dumps({"adapter_error": str(exc)}, sort_keys=True), file=sys.stderr)
        return 78


if __name__ == "__main__":
    raise SystemExit(main())
