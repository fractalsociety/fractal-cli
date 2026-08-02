#!/usr/bin/env python3
"""Sanitized, deterministic corpus v2 for project-graph-context.

The v2 corpus is intentionally independent of ``corpus.py``.  Its fixtures are
small local seed trees; no source prompts, answer patches, evaluator output,
learning records, or legacy graph-state files are read.  Hidden checkers are
copied into an external private directory before an episode and emit only
sanitized pass/failure summaries.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any, Iterable, Mapping


ROOT = Path(__file__).resolve().parent
FIXTURES = ROOT / "fixtures_v2"
ORACLE_SOURCE = FIXTURES / "oracles" / "checker.py"
CORPUS_SCHEMA = "project-graph-context.corpus-v2.v1"
TASK_SCHEMA = "project-graph-context.task-intent-scope-manifest.v2"
QUALITY_SCHEMA = "project-graph-context.task-quality.v1"


TELEMETRY_REQUIREMENTS: dict[str, Any] = {
    "schema_version": "project-graph-context.telemetry-requirements.v1",
    "required": [
        "command_argv",
        "exit_code",
        "duration_ms",
        "open_read_write_attempts",
        "network_attempts",
        "repairs",
        "routes",
        "tools",
        "usage_receipt",
    ],
    "missing_value": None,
    "capture": {
        "command_argv": "argv array only; never a shell string",
        "exit_code": "process exit code, null when unavailable",
        "duration_ms": "monotonic wall duration, null when unavailable",
        "open_read_write_attempts": "optional audit trace, null when unavailable",
        "network_attempts": "count or null when unavailable",
        "repairs": "structured repair records or null",
        "routes": "routing records or null",
        "tools": "tool-selection records or null",
        "usage_receipt": "validated usage receipt or null; absence is not zero",
    },
}


def _intent(
    task_id: str,
    pair_id: str,
    title: str,
    goal: str,
    allowed: list[str],
    symbols: list[str],
    forbidden_symbols: list[str],
    acceptance: list[str],
    steps: list[str],
    shape: str,
    behavior: str,
    split: str,
) -> dict[str, Any]:
    return {
        "schema_version": TASK_SCHEMA,
        "task_id": task_id,
        "pair_id": pair_id,
        "title": title,
        "goal": goal,
        "allowed_paths": allowed,
        "required_paths": allowed,
        "required_symbols": symbols,
        "forbidden_paths": [".git", "fixtures_v2/oracles", "tests/secret", "__pycache__"],
        "forbidden_symbols": forbidden_symbols,
        "acceptance_checks": acceptance,
        "behavior_steps": steps,
        "dependencies": [],
        "dependency_shape": shape,
        "behavior_fingerprint": behavior,
        "split": split,
        "intent_weights": {"forbidden_path": 3.0, "out_of_scope_path": 2.0, "forbidden_symbol": 3.0},
        "safety_policy": {
            "network": "deny",
            "external_side_effects": "deny",
            "subprocess": "deny",
            "package_install": "deny",
            "git_mutation": "deny",
            "filesystem": "fixture_paths_only",
        },
        "telemetry_requirements": TELEMETRY_REQUIREMENTS,
    }


# Public tasks are suitable for calibration.  Holdout tasks are structurally
# paired but their checker details are never included in context manifests.
TASKS_V2: dict[str, dict[str, Any]] = {
    "storage-normal": _intent(
        "storage-normal", "storage", "Durable state and inclusive expiry", "Implement durable StateStore persistence, reload, and TTL behavior.",
        ["app/storage.py", "app/codec.py", "app/clock.py"],
        ["StateStore.__init__", "StateStore.put", "StateStore.get", "StateStore.save", "StateStore.load", "StateStore._purge_expired"],
        ["socket", "requests", "subprocess", "store_helpers"],
        ["put/get round trip", "save then load preserves values", "missing key returns default", "TTL remains visible before deadline", "TTL expires at exact deadline", "TTL stays absent after deadline", "save creates no stray file", "JSON keys are deterministic"],
        ["construct store with manual clock", "write values and optional TTL", "save atomically", "reload from JSON", "observe boundary expiry"],
        "python-package:storage->codec,clock", "storage:reload+ttl-boundary", "public",
    ),
    "storage-corrupt": _intent(
        "storage-corrupt", "storage", "Corrupt durable state recovery", "Implement defensive StateStore loading for malformed and boundary documents.",
        ["app/storage.py", "app/codec.py", "app/clock.py"],
        ["StateStore.load", "StateStore.get", "StateStore.put", "StateStore.save", "StateStore._purge_expired"],
        ["socket", "requests", "subprocess", "store_helpers"],
        ["missing file gives empty store", "malformed JSON gives empty store", "non-object JSON gives empty store", "wrong version gives empty store", "invalid entries gives empty store", "valid entries reload", "expiry at exact deadline", "save removes temporary sibling"],
        ["construct malformed inputs", "load without raising", "reload valid document", "apply inclusive deadline", "write repaired state"],
        "python-package:storage->codec,clock", "storage:corrupt+reload-boundary", "holdout",
    ),
    "board-filters": _intent(
        "board-filters", "board", "Task-board filters and keyboard order", "Implement deterministic task-board filtering and keyboard traversal.",
        ["lib/board.js", "lib/filter.js", "lib/keyboard.js"],
        ["TaskBoard", "TaskBoard.prototype.visible", "TaskBoard.prototype.focusOrder"],
        ["http", "https", "fetch", "XMLHttpRequest", "server_stub"],
        ["all status preserves order", "status filter", "case-insensitive query", "trimmed query", "assignee filter", "unknown status is harmless", "focused keyboard order", "wrap-around order", "missing focus starts first"],
        ["construct immutable task snapshot", "normalize filters", "select visible rows", "derive keyboard order", "handle malformed filter"],
        "node-modules:board->filter,keyboard", "board:filter+keyboard-stable", "public",
    ),
    "board-rollback": _intent(
        "board-rollback", "board", "Optimistic board update recovery", "Implement optimistic updates with exact server settlement and rollback.",
        ["lib/board.js", "lib/filter.js", "lib/keyboard.js"],
        ["TaskBoard", "TaskBoard.prototype.applyOptimistic", "TaskBoard.prototype.settle", "TaskBoard.prototype.rollback"],
        ["http", "https", "fetch", "XMLHttpRequest", "server_stub"],
        ["optimistic update is immediate", "one snapshot per update", "rollback restores fields", "rollback restores order", "rollback is idempotent", "unknown id is no-op", "settle accepts server order", "settle clears history", "filters still work"],
        ["snapshot current rows", "apply shallow patch", "settle complete snapshot", "restore exact snapshot", "repeat failure safely"],
        "node-modules:board->filter,keyboard", "board:optimistic+rollback", "holdout",
    ),
    "graph-valid": _intent(
        "graph-valid", "graph", "Valid JSON relation resolution", "Resolve a unique graph label from a stdlib-only JSON relation graph.",
        ["Cargo.toml", "src/main.rs", "src/model.rs", "src/resolve.rs"],
        ["parse_graph", "resolve_label", "run_json", "main"],
        ["reqwest", "serde_json", "curl", "Command::new", "graph_utils"],
        ["valid JSON parses", "unique trimmed label resolves", "id preserves case", "unknown query is unresolved", "duplicate label is ambiguous", "empty nodes is unresolved", "malformed JSON is malformed_graph", "edges do not reorder nodes"],
        ["read JSON stdin", "parse nodes/edges", "validate relation", "resolve unique label", "emit compact result"],
        "rust-bin:main->resolve->model", "graph:json+unique-resolution", "public",
    ),
    "graph-diagnostics": _intent(
        "graph-diagnostics", "graph", "Graph relation diagnostics", "Resolve relations and report malformed, unresolved, ambiguous, and cyclic graphs deterministically.",
        ["Cargo.toml", "src/main.rs", "src/model.rs", "src/resolve.rs"],
        ["parse_graph", "resolve_label", "run_json", "detect_cycle", "main"],
        ["reqwest", "serde_json", "curl", "Command::new", "graph_utils"],
        ["malformed JSON diagnostic", "missing fields diagnostic", "unresolved label diagnostic", "ambiguous candidates sorted", "cycle diagnostic", "self-cycle diagnostic", "valid acyclic resolution", "no panic on boundary input"],
        ["parse defensive JSON", "classify lookup", "detect directed cycle", "sort diagnostic candidates", "emit sanitized result"],
        "rust-bin:main->resolve->model", "graph:diagnostics+cycle", "holdout",
    ),
    "policy-retry": _intent(
        "policy-retry", "policy", "Transient retry and checkpoint recovery", "Implement bounded transient retry with durable checkpoints.",
        ["policy/retry.py", "policy/checkpoint.py", "policy/decisions.py"],
        ["run_plan", "save_checkpoint", "load_checkpoint", "classify"],
        ["socket", "requests", "subprocess", "legacy_policy.should_retry"],
        ["all ok completes", "transient retries", "retry bound", "checkpoint after success", "resume skips completed", "ordered completion", "budget counts retries", "denied is terminal"],
        ["load checkpoint", "iterate plan", "classify outcome", "retry transient within bound", "persist after success"],
        "python-package:retry->checkpoint,decisions", "policy:transient+checkpoint", "public",
    ),
    "policy-terminal": _intent(
        "policy-terminal", "policy", "Terminal denial and hard-budget stop", "Implement policy recovery that never retries denial and stops before a hard budget is exceeded.",
        ["policy/retry.py", "policy/checkpoint.py", "policy/decisions.py"],
        ["run_plan", "save_checkpoint", "load_checkpoint", "classify"],
        ["socket", "requests", "subprocess", "legacy_policy.should_retry"],
        ["denied stops immediately", "unknown outcome is terminal", "budget exhaustion before attempt", "no checkpoint on failure", "resume valid checkpoint", "transient bound", "ordered attempts", "no external effects"],
        ["load checkpoint", "enforce budget", "classify terminal", "record attempts", "return policy decision"],
        "python-package:retry->checkpoint,decisions", "policy:denial+hard-budget", "holdout",
    ),
}

# Public alias intentionally excludes the old four tasks.  v2 consumers should
# never silently mix corpus versions.
TASKS = TASKS_V2


def task_ids(*, split: str | None = None) -> list[str]:
    ids = sorted(TASKS_V2)
    if split is not None:
        if split not in {"public", "holdout"}:
            raise ValueError("split must be public or holdout")
        ids = [task_id for task_id in ids if TASKS_V2[task_id]["split"] == split]
    return ids


def related_pairs() -> list[list[str]]:
    groups: dict[str, list[str]] = {}
    for task in TASKS_V2.values():
        groups.setdefault(str(task["pair_id"]), []).append(str(task["task_id"]))
    return [sorted(value) for _, value in sorted(groups.items())]


def deduplication_key(task_id: str) -> tuple[str, str]:
    """Return the structural dedupe key; titles are intentionally excluded."""

    manifest = TASKS_V2[task_id]
    return (str(manifest["dependency_shape"]), str(manifest["behavior_fingerprint"]))


def dedupe_task_ids(task_ids_input: Iterable[str]) -> tuple[list[str], list[str]]:
    """Dedupe by dependency shape and behavior fingerprint, stably by id.

    The second return value contains later colliding IDs so callers can
    quarantine them rather than silently selecting by title.
    """

    kept: list[str] = []
    duplicates: list[str] = []
    seen: set[tuple[str, str]] = set()
    for task_id in sorted(set(task_ids_input)):
        key = deduplication_key(task_id)
        if key in seen:
            duplicates.append(task_id)
        else:
            seen.add(key); kept.append(task_id)
    return kept, duplicates


def task_manifest(task_id: str, *, include_holdout: bool = True) -> dict[str, Any]:
    if task_id not in TASKS_V2:
        raise KeyError(f"unknown v2 task: {task_id}")
    value = json.loads(json.dumps(TASKS_V2[task_id], sort_keys=True))
    if value["split"] == "holdout" and not include_holdout:
        # Contexts may carry intent/safety, but never checker implementation or
        # answer details.  Keep only behavior and scope metadata.
        value.pop("split", None)
    return value


def fixture_root(task_id: str) -> Path:
    if task_id not in TASKS_V2:
        raise KeyError(f"unknown v2 task: {task_id}")
    return FIXTURES / task_id


def fixture_files(task_id: str) -> list[str]:
    root = fixture_root(task_id)
    return sorted(str(path.relative_to(root)) for path in root.rglob("*") if path.is_file())


def seed_digest(task_id: str) -> str:
    digest = hashlib.sha256()
    root = fixture_root(task_id)
    for relative in fixture_files(task_id):
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update((root / relative).read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def _git_env() -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "GIT_AUTHOR_NAME": "project-graph-context-corpus-v2",
            "GIT_AUTHOR_EMAIL": "corpus-v2@example.invalid",
            "GIT_COMMITTER_NAME": "project-graph-context-corpus-v2",
            "GIT_COMMITTER_EMAIL": "corpus-v2@example.invalid",
            "GIT_AUTHOR_DATE": "2001-01-01T00:00:00Z",
            "GIT_COMMITTER_DATE": "2001-01-01T00:00:00Z",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
    )
    return env


def materialize_task_repo_v2(task_id: str, destination: str | os.PathLike[str]) -> Path:
    """Materialize a clean, deterministic git seed repository for one task."""

    root = fixture_root(task_id)
    repo = Path(destination).resolve()
    if repo.exists():
        raise FileExistsError(repo)
    for relative in fixture_files(task_id):
        source = root / relative
        target = repo / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(source.read_bytes())
    env = _git_env()
    subprocess.run(["git", "-C", str(repo), "init", "--quiet"], check=True, env=env, stdout=subprocess.DEVNULL)
    subprocess.run(["git", "-C", str(repo), "add", "--all"], check=True, env=env, stdout=subprocess.DEVNULL)
    subprocess.run(["git", "-C", str(repo), "commit", "--quiet", "--no-gpg-sign", "-m", f"seed {task_id}"], check=True, env=env, stdout=subprocess.DEVNULL)
    commit = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], env=env, text=True).strip()
    (repo / ".frozen-commit").write_text(commit + "\n", encoding="ascii")
    return repo


# Consistent names make it easy for callers to swap v1/v2 corpus adapters.
materialize_task_repo = materialize_task_repo_v2


def copy_hidden_oracle(destination: str | os.PathLike[str]) -> Path:
    target = Path(destination).resolve()
    target.mkdir(parents=True, exist_ok=True)
    checker = target / "checker.py"
    shutil.copy2(ORACLE_SOURCE, checker)
    checker.chmod(0o500)
    return checker


def _sanitized_payload(payload: Any, returncode: int, stderr: str = "") -> dict[str, Any]:
    if not isinstance(payload, Mapping):
        return {"passed": False, "failure_code": "oracle_invalid_payload", "checker_exit_code": returncode}
    result = {
        "passed": bool(payload.get("passed", False)),
        "failure_code": payload.get("failure_code") if isinstance(payload.get("failure_code"), str) else None,
        "clauses_passed": int(payload.get("clauses_passed", 0)) if isinstance(payload.get("clauses_passed"), int) else 0,
        "clauses_total": int(payload.get("clauses_total", 0)) if isinstance(payload.get("clauses_total"), int) else 0,
        "checker_exit_code": returncode,
    }
    if stderr:
        result["checker_stderr_sha256"] = hashlib.sha256(stderr.encode("utf-8")).hexdigest()
    return result


def run_hidden_oracle(task_id: str, worktree: str | os.PathLike[str], checker: str | os.PathLike[str] | None = None) -> dict[str, Any]:
    """Run an external checker and return only sanitized behavioral metrics."""

    if task_id not in TASKS_V2:
        raise KeyError(f"unknown v2 task: {task_id}")
    checker_path = Path(checker).resolve() if checker else ORACLE_SOURCE
    command = [os.environ.get("PYTHON", "python3"), str(checker_path), "--task-id", task_id, "--worktree", str(Path(worktree).resolve())]
    env = _git_env()
    env["PGC_CORPUS_ROOT"] = str(ROOT)
    completed = subprocess.run(command, capture_output=True, text=True, timeout=20, check=False, env=env)
    try:
        payload = json.loads(completed.stdout.strip() or "{}")
    except json.JSONDecodeError:
        payload = {}
    return _sanitized_payload(payload, completed.returncode, completed.stderr)


def split_metadata() -> dict[str, Any]:
    splits: dict[str, list[dict[str, Any]]] = {"public": [], "holdout": []}
    for task_id in task_ids():
        manifest = TASKS_V2[task_id]
        splits[str(manifest["split"])].append(
            {
                "task_id": task_id,
                "pair_id": manifest["pair_id"],
                "seed_sha256": seed_digest(task_id),
                "dependency_shape": manifest["dependency_shape"],
                "behavior_fingerprint": manifest["behavior_fingerprint"],
            }
        )
    for entries in splits.values():
        entries.sort(key=lambda value: value["task_id"])
    return {
        "schema_version": CORPUS_SCHEMA,
        "splits": splits,
        "split_hashes": {name: hashlib.sha256(json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()).hexdigest() for name, entries in splits.items()},
        "corpus_hash": corpus_hash(),
        "holdout_checker_contents": "sealed-external",
        "dedupe_basis": ["dependency_shape", "behavior_fingerprint"],
    }


def corpus_hash() -> str:
    values = []
    for task_id in task_ids():
        values.append({"task_id": task_id, "manifest": task_manifest(task_id), "seed_sha256": seed_digest(task_id)})
    return hashlib.sha256(json.dumps(values, sort_keys=True, separators=(",", ":")).encode("utf-8")).hexdigest()


def telemetry_manifest() -> dict[str, Any]:
    return json.loads(json.dumps(TELEMETRY_REQUIREMENTS, sort_keys=True))


def write_manifests(destination: str | os.PathLike[str], *, split: str | None = None) -> list[Path]:
    out = Path(destination)
    out.mkdir(parents=True, exist_ok=True)
    paths = []
    for task_id in task_ids(split=split):
        path = out / f"{task_id}.json"
        path.write_text(json.dumps(task_manifest(task_id), indent=2, sort_keys=True) + "\n", encoding="utf-8")
        paths.append(path)
    return paths


def _cli() -> int:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--split", choices=("public", "holdout"))
    args = parser.parse_args()
    payload = split_metadata()
    if args.split:
        payload = {**payload, "selected_split": args.split, "tasks": payload["splits"][args.split]}
    print(json.dumps(payload, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(_cli())
