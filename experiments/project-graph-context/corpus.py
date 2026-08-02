#!/usr/bin/env python3
"""Deterministic task corpus for the project-graph-context experiment.

The corpus intentionally contains tiny, related task pairs.  Seed repositories
are materialised on demand so an episode can always be based on a fresh,
detached commit.  Oracles live under ``fixtures/oracles`` and are copied to a
private directory by the runner; they are never mounted in the worker
worktree.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any, Dict, Iterable, Mapping


ROOT = Path(__file__).resolve().parent
FIXTURES = ROOT / "fixtures"
ORACLE_SOURCE = FIXTURES / "oracles" / "checker.py"


TASKS: Dict[str, Dict[str, Any]] = {
    "calculator-add": {
        "task_id": "calculator-add",
        "pair_id": "calculator",
        "title": "Implement calculator addition",
        "goal": "Implement add(a, b) in src/calculator.py.",
        "allowed_paths": ["src/calculator.py"],
        "forbidden_paths": [".git", "tests/secret_expected.py"],
        "acceptance_checks": ["add(2, 3) == 5", "add(-2, 2) == 0"],
        "dependencies": [],
    },
    "calculator-subtract": {
        "task_id": "calculator-subtract",
        "pair_id": "calculator",
        "title": "Implement calculator subtraction",
        "goal": "Implement subtract(a, b) in src/calculator.py.",
        "allowed_paths": ["src/calculator.py"],
        "forbidden_paths": [".git", "tests/secret_expected.py"],
        "acceptance_checks": ["subtract(8, 3) == 5", "subtract(-2, 2) == -4"],
        "dependencies": [],
    },
    "text-slugify": {
        "task_id": "text-slugify",
        "pair_id": "text",
        "title": "Add basic slugification",
        "goal": "Implement slugify(text) in src/text_utils.py.",
        "allowed_paths": ["src/text_utils.py"],
        "forbidden_paths": [".git", "tests/secret_expected.py"],
        "acceptance_checks": ["slugify('Hello, World!') == 'hello-world'"],
        "dependencies": [],
    },
    "text-slugify-edge": {
        "task_id": "text-slugify-edge",
        "pair_id": "text",
        "title": "Handle accented slugification",
        "goal": "Implement slugify(text) with punctuation, whitespace, and accents handled in src/text_utils.py.",
        "allowed_paths": ["src/text_utils.py"],
        "forbidden_paths": [".git", "tests/secret_expected.py"],
        "acceptance_checks": ["slugify('  Café au lait  ') == 'cafe-au-lait'"],
        "dependencies": [],
    },
}


SEEDS: Dict[str, Dict[str, str]] = {
    "calculator": {
        "src/calculator.py": '''"""Small calculator fixture; the task intentionally starts incomplete."""\n\ndef add(a, b):\n    raise NotImplementedError("task pending")\n\n\ndef subtract(a, b):\n    raise NotImplementedError("task pending")\n''',
        "README.md": "# Calculator fixture\n",
    },
    "text": {
        "src/text_utils.py": '''"""Small text fixture; the task intentionally starts incomplete."""\n\ndef slugify(text):\n    raise NotImplementedError("task pending")\n''',
        "README.md": "# Text fixture\n",
    },
}


def task_manifest(task_id: str) -> Dict[str, Any]:
    """Return a defensive copy of one task's intent/scope manifest."""

    if task_id not in TASKS:
        raise KeyError(f"unknown task: {task_id}")
    manifest = json.loads(json.dumps(TASKS[task_id], sort_keys=True))
    manifest["schema_version"] = "project-graph-context.task-intent-scope-manifest.v1"
    return manifest


def task_ids() -> list[str]:
    return sorted(TASKS)


def related_pairs() -> list[list[str]]:
    groups: Dict[str, list[str]] = {}
    for task in TASKS.values():
        groups.setdefault(str(task["pair_id"]), []).append(str(task["task_id"]))
    return [sorted(ids) for _, ids in sorted(groups.items()) if len(ids) >= 2]


def _write_seed(task_id: str, destination: Path) -> None:
    pair_id = str(TASKS[task_id]["pair_id"])
    files = SEEDS[pair_id]
    destination.mkdir(parents=True, exist_ok=False)
    for relative, content in files.items():
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")


def materialize_task_repo(task_id: str, destination: str | os.PathLike[str]) -> Path:
    """Create a clean git repository for ``task_id`` and return its commit.

    The commit metadata is fixed, making the seed tree and commit reproducible
    across machines.  No network or user git configuration is consulted.
    """

    if task_id not in TASKS:
        raise KeyError(f"unknown task: {task_id}")
    repo = Path(destination).resolve()
    _write_seed(task_id, repo)
    env = os.environ.copy()
    env.update(
        {
            "GIT_AUTHOR_NAME": "project-graph-context-corpus",
            "GIT_AUTHOR_EMAIL": "corpus@example.invalid",
            "GIT_COMMITTER_NAME": "project-graph-context-corpus",
            "GIT_COMMITTER_EMAIL": "corpus@example.invalid",
            "GIT_AUTHOR_DATE": "2000-01-01T00:00:00Z",
            "GIT_COMMITTER_DATE": "2000-01-01T00:00:00Z",
        }
    )
    subprocess.run(["git", "-C", str(repo), "init", "--quiet"], check=True, env=env)
    subprocess.run(["git", "-C", str(repo), "add", "--all"], check=True, env=env)
    subprocess.run(
        ["git", "-C", str(repo), "commit", "--quiet", "--no-gpg-sign", "-m", f"seed {task_id}"],
        check=True,
        env=env,
    )
    commit = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], env=env, text=True).strip()
    (repo / ".frozen-commit").write_text(commit + "\n", encoding="ascii")
    # The marker must not become part of the seed commit.  It is useful to
    # callers, but the runner always resolves the commit before this marker.
    return repo


def seed_digest(task_id: str) -> str:
    """Hash the canonical seed tree without creating a repository."""

    pair_id = str(TASKS[task_id]["pair_id"])
    digest = hashlib.sha256()
    for relative, content in sorted(SEEDS[pair_id].items()):
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(content.encode("utf-8"))
        digest.update(b"\0")
    return digest.hexdigest()


def copy_hidden_oracle(destination: str | os.PathLike[str]) -> Path:
    """Copy the checker into a private path outside a worker worktree."""

    target = Path(destination).resolve()
    target.mkdir(parents=True, exist_ok=True)
    checker = target / "checker.py"
    shutil.copy2(ORACLE_SOURCE, checker)
    checker.chmod(0o500)
    return checker


def run_hidden_oracle(task_id: str, worktree: str | os.PathLike[str], checker: str | os.PathLike[str]) -> Dict[str, Any]:
    """Run the copied checker without shell interpolation."""

    command = [
        os.environ.get("PYTHON", "python3"),
        str(checker),
        "--task-id",
        task_id,
        "--worktree",
        str(Path(worktree).resolve()),
    ]
    completed = subprocess.run(command, capture_output=True, text=True, timeout=20, check=False)
    try:
        payload = json.loads(completed.stdout.strip() or "{}")
    except json.JSONDecodeError:
        payload = {"passed": False, "failure_code": "oracle_invalid_json", "detail": completed.stdout[-500:]}
    if not isinstance(payload, dict):
        payload = {"passed": False, "failure_code": "oracle_invalid_payload"}
    payload["checker_exit_code"] = completed.returncode
    if completed.stderr:
        payload["checker_stderr_sha256"] = hashlib.sha256(completed.stderr.encode("utf-8")).hexdigest()
    return payload


def write_all_manifests(destination: str | os.PathLike[str]) -> None:
    """Write task intent/scope examples used by calibration and reviewers."""

    out = Path(destination)
    out.mkdir(parents=True, exist_ok=True)
    for task_id in task_ids():
        (out / f"{task_id}.json").write_text(
            json.dumps(task_manifest(task_id), indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
