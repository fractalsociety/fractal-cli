#!/usr/bin/env python3
"""Fail-closed runner for the trusted corpus-v2 live allowlist.

The live route is deliberately opt-in (``--go``).  ``--plan-only`` and
``--calibrate-only`` are deterministic local operations and never invoke a
model.  A worker receives a seed-file-only copy and a macOS seatbelt profile;
the private checker is staged only after worker termination.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

try:
    from .corpus_v2 import copy_hidden_oracle, fixture_files, fixture_root, run_hidden_oracle, task_manifest
    from .live_v2_policy import canonical_json, offline_env, policy_document, policy_hash, sha256_bytes, sha256_file, write_canonical
    from .live_v2_adapter import (
        LUNA_LEGACY_ROUTE,
        LUNA_POSTHOC_TOKEN_CAP,
        LUNA_STRUCTURED_PATCH_ROUTE,
        MAX_PATCH_FILE_BYTES,
        MAX_PATCH_TOTAL_BYTES,
        run_planner_v2,
        structured_patch_schema_bytes,
        validate_structured_patch_shape,
    )
except ImportError:  # pragma: no cover
    from corpus_v2 import copy_hidden_oracle, fixture_files, fixture_root, run_hidden_oracle, task_manifest
    from live_v2_policy import canonical_json, offline_env, policy_document, policy_hash, sha256_bytes, sha256_file, write_canonical
    from live_v2_adapter import (
        LUNA_LEGACY_ROUTE,
        LUNA_POSTHOC_TOKEN_CAP,
        LUNA_STRUCTURED_PATCH_ROUTE,
        MAX_PATCH_FILE_BYTES,
        MAX_PATCH_TOTAL_BYTES,
        run_planner_v2,
        structured_patch_schema_bytes,
        validate_structured_patch_shape,
    )


ROOT = Path(__file__).resolve().parent
ADAPTER = ROOT / "live_v2_adapter.py"
LIVE_TASK_ALLOWLIST: tuple[str, ...] = (
    "storage-normal", "storage-corrupt", "board-filters", "graph-valid", "policy-retry"
)
# Stable aliases make the contract easy for orchestration/tests to consume;
# both expose the same exact ordered five-task route.
LIVE_ALLOWLIST = LIVE_TASK_ALLOWLIST
ALLOWED_TASKS = frozenset(LIVE_TASK_ALLOWLIST)
DEFAULT_TASKS = LIVE_TASK_ALLOWLIST
PLAN_SCHEMA = "project-graph-context.sol-plan.v1"
RESULT_SCHEMA = "project-graph-context.live-v2-result.v1"
CALIBRATION_SCHEMA = "project-graph-context.live-v2-calibration.v1"
DEFAULT_LUNA_ROUTE = LUNA_STRUCTURED_PATCH_ROUTE


class RunnerError(RuntimeError):
    """A fail-closed configuration or isolation error."""


def validate_live_tasks(task_ids: Iterable[str] | None) -> tuple[str, ...]:
    """Accept exactly the five public v2 live tasks and no holdouts."""
    values = tuple(DEFAULT_TASKS if task_ids is None else task_ids)
    if not values:
        raise RunnerError("at least one task is required")
    if len(values) != len(set(values)):
        raise RunnerError("duplicate task ids are not allowed")
    if any(not isinstance(value, str) or not value.strip() for value in values):
        raise RunnerError("task ids must be non-empty strings")
    unknown = sorted(set(values) - set(LIVE_TASK_ALLOWLIST))
    if unknown:
        raise RunnerError("task(s) refused by exact live allowlist: " + ", ".join(unknown))
    return values


def _write(path: Path, value: Any) -> str:
    return write_canonical(path, value)


def _hash(path: Path) -> str | None:
    return sha256_file(path) if path.is_file() else None


def _safe_relative(relative: str) -> str:
    value = str(relative).replace("\\", "/")
    parts = Path(value).parts
    if value.startswith("/") or ".." in parts or ".git" in parts or "oracles" in parts or "history" in parts or "graph-state" in value:
        raise RunnerError(f"protected fixture path: {relative}")
    return value


def _snapshot(root: Path) -> dict[str, str]:
    rows: dict[str, str] = {}
    if root.exists():
        for path in sorted(root.rglob("*")):
            if path.is_file() and not path.is_symlink():
                rows[str(path.relative_to(root)).replace("\\", "/")] = sha256_file(path)
    return rows


def _snapshot_digest(root: Path) -> str:
    return sha256_bytes(canonical_json(_snapshot(root)))


class StructuredPatchError(RunnerError):
    """A structured Luna patch failed trusted post-exit validation."""

    def __init__(self, code: str, message: str | None = None):
        self.code = str(code)
        super().__init__(message or self.code)


def _route_from_env() -> str:
    route = os.environ.get("FRACTAL_LUNA_ROUTE", DEFAULT_LUNA_ROUTE)
    if "\x00" in route or not route.strip():
        raise RunnerError("Luna route is invalid")
    if route not in {LUNA_STRUCTURED_PATCH_ROUTE, LUNA_LEGACY_ROUTE, "codex-luna", "codex"}:
        raise RunnerError(f"unknown Luna route: {route}")
    return route


def _exact_relative_patch_path(value: Any) -> str:
    """Require a canonical, relative slash path without normalization."""

    if not isinstance(value, str) or not value or "\x00" in value:
        raise StructuredPatchError("structured_patch_path_invalid", "patch path must be a non-empty string")
    if "\\" in value or value.startswith("/") or value.startswith("./") or "//" in value:
        raise StructuredPatchError("structured_patch_path_traversal", "patch path is not an exact relative path")
    parts = value.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        raise StructuredPatchError("structured_patch_path_traversal", "patch path contains traversal")
    if any(part in {".git", "oracles", "history", "hidden-checker", "secrets"} for part in parts) or "graph-state" in value:
        raise StructuredPatchError("structured_patch_path_forbidden", "patch path is protected")
    return value


def _assert_regular_patch_target(root: Path, relative: str) -> Path:
    """Resolve a seed target while refusing symlink components and escapes."""

    target = root / relative
    current = root
    for component in Path(relative).parts:
        current = current / component
        try:
            if current.is_symlink():
                raise StructuredPatchError("structured_patch_symlink", f"patch target traverses a symlink: {relative}")
        except OSError as exc:
            raise StructuredPatchError("structured_patch_target_unreadable", "patch target could not be inspected") from exc
    try:
        resolved = target.resolve(strict=False)
    except OSError as exc:
        raise StructuredPatchError("structured_patch_target_unreadable", "patch target could not be resolved") from exc
    try:
        resolved.relative_to(root)
    except ValueError as exc:
        raise StructuredPatchError("structured_patch_path_traversal", "patch target escapes worktree") from exc
    if not target.exists() or not target.is_file() or target.is_symlink():
        raise StructuredPatchError("structured_patch_target_not_regular", f"patch target is not a seed file: {relative}")
    return target


def validate_structured_patch(
    payload: Mapping[str, Any],
    worktree: Path,
    allowed_paths: Sequence[str],
    forbidden_paths: Sequence[str] = (),
    *,
    max_file_bytes: int = MAX_PATCH_FILE_BYTES,
    max_total_bytes: int = MAX_PATCH_TOTAL_BYTES,
) -> dict[str, Any]:
    """Validate a Luna replacement patch against the exact seed allowlist.

    This function is called by the trusted runner only after the model child
    exits.  It reads current seed bytes but performs no writes.  Any rejected
    patch leaves the worktree byte-for-byte unchanged.
    """

    try:
        shaped = validate_structured_patch_shape(payload)
    except Exception as exc:
        if isinstance(exc, StructuredPatchError):
            raise
        raise StructuredPatchError("structured_patch_schema", str(exc)) from exc
    root = Path(worktree).resolve()
    if not root.is_dir() or root.is_symlink():
        raise StructuredPatchError("structured_patch_worktree_invalid", "patch worktree is not a regular directory")
    allowed: list[str] = []
    for raw in allowed_paths:
        relative = _exact_relative_patch_path(raw)
        if relative in allowed:
            raise StructuredPatchError("structured_patch_allowlist_duplicate", "task allowlist contains duplicate paths")
        allowed.append(relative)
    allowed_set = set(allowed)
    forbidden: list[str] = []
    for raw in forbidden_paths:
        if isinstance(raw, str) and raw:
            candidate = raw.replace("\\", "/").strip("/")
            if candidate:
                forbidden.append(candidate)
    seen: set[str] = set()
    total = 0
    changes: list[dict[str, Any]] = []
    for item in shaped["changes"]:
        relative = _exact_relative_patch_path(item["path"])
        if relative in seen:
            raise StructuredPatchError("structured_patch_duplicate", f"duplicate patch path: {relative}")
        seen.add(relative)
        if relative not in allowed_set:
            raise StructuredPatchError("structured_patch_path_out_of_scope", f"patch path is outside allowlist: {relative}")
        if any(relative == value or relative.startswith(value + "/") for value in forbidden):
            raise StructuredPatchError("structured_patch_path_forbidden", f"patch path is forbidden: {relative}")
        target = _assert_regular_patch_target(root, relative)
        content = item["content"]
        if not isinstance(content, str):
            raise StructuredPatchError("structured_patch_content_invalid", f"patch content is not text: {relative}")
        try:
            encoded = content.encode("utf-8")
        except UnicodeEncodeError as exc:
            raise StructuredPatchError("structured_patch_content_invalid", f"patch content is not UTF-8: {relative}") from exc
        if len(encoded) > max_file_bytes:
            raise StructuredPatchError("structured_patch_oversize", f"patch file exceeds size limit: {relative}")
        total += len(encoded)
        if total > max_total_bytes:
            raise StructuredPatchError("structured_patch_oversize", "patch exceeds total size limit")
        try:
            current = target.read_bytes()
        except OSError as exc:
            raise StructuredPatchError("structured_patch_target_unreadable", f"patch target could not be read: {relative}") from exc
        if current == encoded:
            raise StructuredPatchError("structured_patch_no_change", f"patch is a no-op: {relative}")
        changes.append({"path": relative, "content": content, "content_sha256": sha256_bytes(encoded), "size_bytes": len(encoded)})
    if not changes:
        raise StructuredPatchError("structured_patch_no_change", "patch contains no changes")
    return {
        "changes": changes,
        "summary_sha256": sha256_bytes(str(shaped["summary"]).encode("utf-8")),
        "checks_sha256": sha256_bytes(canonical_json(shaped["checks"])),
        "patch_sha256": sha256_bytes(canonical_json({"changes": [{"path": item["path"], "content": item["content"]} for item in shaped["changes"]], "summary": shaped["summary"], "checks": shaped["checks"]})),
        "total_bytes": total,
    }


def apply_structured_patch(validated: Mapping[str, Any], worktree: Path) -> dict[str, Any]:
    """Atomically replace validated seed files after model process exit.

    All target/content checks happen before the first replacement.  Each file
    is written and fsynced in its own same-directory temporary file, then
    swapped with ``os.replace``.  No model-controlled path is ever opened for
    writing directly.
    """

    root = Path(worktree).resolve()
    changes = validated.get("changes")
    if not isinstance(changes, list) or not changes:
        raise StructuredPatchError("structured_patch_no_change", "validated patch has no changes")
    staged: list[tuple[Path, Path, int]] = []
    try:
        for item in changes:
            if not isinstance(item, Mapping) or not isinstance(item.get("path"), str) or not isinstance(item.get("content"), str):
                raise StructuredPatchError("structured_patch_schema", "validated patch entry is malformed")
            relative = _exact_relative_patch_path(item["path"])
            target = _assert_regular_patch_target(root, relative)
            encoded = item["content"].encode("utf-8")
            if len(encoded) > MAX_PATCH_FILE_BYTES:
                raise StructuredPatchError("structured_patch_oversize", f"patch file exceeds size limit: {relative}")
            # Recheck immediately before staging to close a symlink swap race.
            if target.is_symlink():
                raise StructuredPatchError("structured_patch_symlink", f"patch target became a symlink: {relative}")
            mode = target.stat().st_mode & 0o777
            temporary_name: str | None = None
            try:
                with tempfile.NamedTemporaryFile(prefix=".fractal-patch-", dir=str(target.parent), delete=False) as handle:
                    temporary_name = handle.name
                    handle.write(encoded)
                    handle.flush()
                    os.fsync(handle.fileno())
                temporary = Path(temporary_name)
                os.chmod(temporary, mode)
                staged.append((temporary, target, mode))
            except OSError as exc:
                if temporary_name:
                    try:
                        Path(temporary_name).unlink(missing_ok=True)
                    except OSError:
                        pass
                raise StructuredPatchError("structured_patch_stage_failed", f"could not stage patch: {relative}") from exc
        applied: list[str] = []
        hashes: dict[str, str] = {}
        for temporary, target, _mode in staged:
            if target.is_symlink():
                raise StructuredPatchError("structured_patch_symlink", "patch target became a symlink before apply")
            os.replace(temporary, target)
            applied.append(str(target.relative_to(root)).replace("\\", "/"))
            hashes[applied[-1]] = sha256_file(target)
        return {"changed_paths": sorted(applied), "changed_file_hashes": {key: hashes[key] for key in sorted(hashes)}, "patch_sha256": validated.get("patch_sha256"), "applied": True}
    except StructuredPatchError:
        for temporary, _target, _mode in staged:
            try:
                temporary.unlink(missing_ok=True)
            except OSError:
                pass
        raise
    except OSError as exc:
        for temporary, _target, _mode in staged:
            try:
                temporary.unlink(missing_ok=True)
            except OSError:
                pass
        raise StructuredPatchError("structured_patch_apply_failed", "atomic patch replacement failed") from exc


def _structured_transport(stdout: bytes) -> Mapping[str, Any] | None:
    """Read one ephemeral adapter envelope without retaining raw model text."""

    for line in reversed(stdout.splitlines()):
        try:
            value = json.loads(line.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if isinstance(value, Mapping) and value.get("route") == LUNA_STRUCTURED_PATCH_ROUTE:
            return value
    return None


def materialize_seed_only(task_id: str, destination: Path) -> dict[str, Any]:
    """Copy regular seed files only; no source ``.git`` or oracle tree."""
    validate_live_tasks((task_id,))
    destination = destination.resolve()
    if destination.exists():
        raise RunnerError(f"destination exists: {destination}")
    destination.mkdir(parents=True)
    source_root = fixture_root(task_id).resolve()
    copied: list[str] = []
    for raw in fixture_files(task_id):
        relative = _safe_relative(raw)
        source = (source_root / relative).resolve()
        if source.is_symlink() or not source.is_file() or source_root not in source.parents and source != source_root:
            raise RunnerError(f"fixture is not a regular seed file: {raw}")
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(source.read_bytes())
        copied.append(relative)
    forbidden = [p for p in destination.rglob("*") if p.name in {".git", "history"} or "oracles" in p.parts or "graph-state" in p.name]
    if forbidden:
        raise RunnerError(f"seed copy contains protected path: {forbidden[0]}")
    return {"task_id": task_id, "files": sorted(copied), "seed_sha256": _snapshot_digest(destination), "contains_git": False, "contains_oracle": False}


def _sb_quote(path: Path | str) -> str:
    value = str(Path(path).resolve())
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def sandbox_profile(
    worktree: Path,
    *,
    original_oracles: Path,
    evaluator_staging: Path,
    readonly_roots: Sequence[Path] = (),
    writable_roots: Sequence[Path] = (),
    worktree_writable: bool = True,
) -> str:
    """Return a macOS seatbelt profile with explicit protected-path denies."""
    lines = [
        "(version 1)", "(deny default)", '(import "system.sb")',
        "(allow process-exec)", "(allow process-fork)", "(allow signal (target self))",
        # This outer seatbelt wraps only the trusted Codex controller.  Its
        # service transport is outbound-only; the inner Codex worker sandbox
        # still has network_access=false and web search disabled.
        "(allow network-outbound)",
        # TLS/DNS/account services use the system configuration socket; this
        # is service lookup, not process inspection or inbound socket access.
        "(allow system-socket)",
        '(allow mach-lookup (global-name "com.apple.SystemConfiguration.configd"))',
        '(allow mach-lookup (global-name "com.apple.SecurityServer"))',
        '(allow mach-lookup (global-name "com.apple.CoreServices.coreservicesd"))',
        '(allow mach-lookup (global-name "com.apple.DiskArbitration.diskarbitrationd"))',
        '(allow mach-lookup (global-name "com.apple.FSEvents"))',
        '(allow user-preference-read (preference-domain "com.openai.codex"))',
        '(allow network-outbound (remote tcp "*:443"))', '(allow network-outbound (remote tcp4 "*:443"))', '(allow network-outbound (remote tcp6 "*:443"))', '(allow network-outbound (remote udp "*:53"))',
        f"(allow file-read* (subpath {_sb_quote(worktree)}))",
    ]
    # The interpreter must inspect itself to start, but process enumeration
    # remains denied (the default deny plus the explicit list-pids rule).
    lines.extend(["(allow process-info* (target self))", "(deny process-info-listpids)"])
    # Seatbelt subpath data grants do not grant metadata on ancestors.  Codex
    # must traverse these directories to reach its isolated cwd/home/schema;
    # metadata alone cannot read file contents.  Protected data denies below
    # remain last and therefore still win for oracle/evaluator descendants.
    metadata_targets = [worktree, original_oracles, evaluator_staging, *readonly_roots, *writable_roots]
    metadata_roots: set[Path] = set()
    for target in metadata_targets:
        resolved = Path(target).resolve()
        metadata_roots.update(path for path in resolved.parents if path != Path("/"))
    lines.extend(f"(allow file-read-metadata (subpath {_sb_quote(path)}))" for path in sorted(metadata_roots, key=str))
    user_encoding = Path.home() / ".CFUserTextEncoding"
    if user_encoding.is_file():
        lines.append(f"(allow file-read-data (literal {_sb_quote(user_encoding)}))")
    # Python/Node/Rust executables and their dynamic libraries are system
    # inputs, not worker data.  No write grant is made for these roots.
    lines.extend(f"(allow file-read* (subpath {_sb_quote(path)}))" for path in (Path("/System"), Path("/usr"), Path("/bin"), Path("/sbin"), Path("/Applications"), Path("/Library"), Path("/private/etc"), Path("/private/var/db"), Path("/private/var/run"), Path("/dev")))
    lines.extend(f"(allow file-read* (subpath {_sb_quote(path)}))" for path in readonly_roots)
    lines.extend([
        *([f"(allow file-write* (subpath {_sb_quote(worktree)}))"] if worktree_writable else []),
        '(allow file-read* (literal "/dev/ptmx"))',
        '(allow file-write* (literal "/dev/ptmx"))',
        '(allow file-read* (literal "/dev/tty"))',
        '(allow file-write* (literal "/dev/tty"))',
        "(allow signal (target children))",
        f"(deny file-read* (subpath {_sb_quote(original_oracles)}))",
        f"(deny file-write* (subpath {_sb_quote(original_oracles)}))",
        f"(deny file-read* (subpath {_sb_quote(evaluator_staging)}))",
        f"(deny file-write* (subpath {_sb_quote(evaluator_staging)}))",
        # Keep self process metadata for interpreter startup; process-info for
        # other targets remains denied by the default and list-pids rule.
        # sysctl is needed by Node's allocator; process enumeration remains
        # denied by the explicit list-pids rule above.
    ])
    lines.extend(f"(allow file-write* (subpath {_sb_quote(path)}))" for path in writable_roots)
    return "\n".join(lines) + "\n"


_PROBE = r'''
import json, os, subprocess, sys
from pathlib import Path
oracle, evaluator = Path(sys.argv[1]), Path(sys.argv[2])
checks = {}
def read_denied(label, path):
    try: path.read_bytes()
    except (OSError, PermissionError): checks[label] = "denied"
    else: checks[label] = "visible"
read_denied("relative_oracle_read", Path("fixtures_v2/oracles/checker.py"))
read_denied("absolute_oracle_read", oracle)
read_denied("traversal_oracle_read", Path("..") / ".." / "fixtures_v2" / "oracles" / "checker.py")
read_denied("evaluator_read", evaluator / "checker.py")
try:
    find = subprocess.run(["find", str(oracle.parent.parent), "-name", "checker.py", "-print"], capture_output=True, text=True, timeout=2, check=False)
    text = (find.stdout or "") + (find.stderr or "")
    checks["filesystem_search"] = "denied" if "checker.py" not in text and "oracles" not in text else "visible"
except (OSError, subprocess.SubprocessError): checks["filesystem_search"] = "denied"
env = " ".join(f"{k}={v}" for k,v in os.environ.items())
checks["environment_leakage"] = "denied" if not any(x in env.lower() for x in ("fixtures_v2/oracles", "hidden-checker", "evaluator")) else "visible"
try:
    ps = subprocess.run(["ps", "-axo", "pid,command"], capture_output=True, text=True, timeout=2, check=False)
    checks["process_inspection"] = "denied" if ps.returncode != 0 or not (ps.stdout or "").strip() else "visible"
except (OSError, subprocess.SubprocessError): checks["process_inspection"] = "denied"
passed = all(value == "denied" for value in checks.values())
print(json.dumps({"passed": passed, "checks": checks}, sort_keys=True))
sys.exit(0 if passed else 7)
'''


def run_isolation_probe(worktree: Path, *, original_oracles: Path, evaluator_staging: Path, profile_path: Path, env: Mapping[str, str] | None = None, sandbox_exec: str | None = None) -> dict[str, Any]:
    """Try protected reads, traversal/search, env, and ps under seatbelt."""
    executable = sandbox_exec or os.environ.get("FRACTAL_SANDBOX_EXEC", "/usr/bin/sandbox-exec")
    if not Path(executable).is_file() or not profile_path.is_file():
        return {"passed": False, "failure_code": "sandbox_unavailable", "sandbox_available": False, "checks": {}, "profile_sha256": _hash(profile_path)}
    safe_env = dict(env or offline_env())
    for key in tuple(safe_env):
        lower = f"{key}={safe_env[key]}".lower()
        if any(token in lower for token in ("oracle", "checker", "evaluator", "hidden-checker")):
            safe_env.pop(key, None)
    command = [executable, "-f", str(profile_path), sys.executable, "-I", "-c", _PROBE, str(original_oracles.resolve()), str(evaluator_staging.resolve())]
    try:
        completed = subprocess.run(command, cwd=str(worktree.resolve()), env=safe_env, capture_output=True, timeout=10, check=False, shell=False)
    except (OSError, subprocess.SubprocessError) as exc:
        return {"passed": False, "failure_code": "probe_execution_error", "sandbox_available": True, "checks": {}, "profile_sha256": _hash(profile_path), "output_sha256": sha256_bytes(str(exc).encode())}
    try:
        payload = json.loads((completed.stdout or b"").decode("utf-8", errors="replace").strip() or "{}")
    except (json.JSONDecodeError, UnicodeDecodeError):
        payload = {}
    checks = payload.get("checks") if isinstance(payload, Mapping) and isinstance(payload.get("checks"), Mapping) else {}
    checks = {str(key): str(value) for key, value in sorted(checks.items())}
    passed = bool(payload.get("passed")) and completed.returncode == 0 and all(value == "denied" for value in checks.values()) if isinstance(payload, Mapping) else False
    return {"passed": passed, "failure_code": None if passed else "isolation_probe_failed", "sandbox_available": True, "checks": checks, "profile_sha256": _hash(profile_path), "output_sha256": sha256_bytes((completed.stdout or b"") + (completed.stderr or b""))}


def build_plan(task_id: str) -> dict[str, Any]:
    validate_live_tasks((task_id,))
    intent = task_manifest(task_id)
    return {"schema_version": PLAN_SCHEMA, "task_id": task_id, "objective": str(intent["goal"]), "acceptance_checks": list(intent["acceptance_checks"]), "allowed_paths": list(intent["allowed_paths"]), "steps": list(intent["behavior_steps"])}


def build_context(task_id: str) -> dict[str, Any]:
    validate_live_tasks((task_id,))
    intent = task_manifest(task_id)
    target = str(intent["allowed_paths"][0])
    return {"schema_version": "project-graph-context.c-graph-context.v1", "arm_id": "C", "task_id": task_id, "layers": {"behavior": [{"id": "behavior.goal", "kind": "acceptance", "summary": str(intent["goal"])}], "source": [{"id": "source.target", "kind": "file", "summary": "Edit only the target module.", "path": target, "line": 1}], "execution": [{"id": "execution.check", "kind": "checker", "summary": "Run focused local checks."}]}, "edges": [{"from": "behavior.goal", "to": "source.target", "relation": "implemented_by"}, {"from": "source.target", "to": "execution.check", "relation": "verified_by"}], "retrieval_policy": {"top_k": 8, "include_neighbors": True}}


def _minimal_env() -> dict[str, str]:
    source = offline_env()
    keep = {"PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "SYSTEMROOT", "TERM"}
    result = {key: value for key, value in source.items() if key in keep}
    result.update({"FRACTAL_OFFLINE": "1", "NO_NETWORK": "1", "PIP_NO_INDEX": "1", "GIT_TERMINAL_PROMPT": "0", "GIT_CONFIG_NOSYSTEM": "1", "GIT_OPTIONAL_LOCKS": "0", "NO_PROXY": "*", "PYTHONNOUSERSITE": "1"})
    configured_codex = os.environ.get("FRACTAL_CODEX_BIN")
    if configured_codex:
        result["FRACTAL_CODEX_BIN"] = configured_codex
    return result


def _codex_runtime_roots() -> tuple[Path, ...]:
    """Return only the executable's installation root for seatbelt reads."""
    configured = os.environ.get("FRACTAL_CODEX_BIN") or shutil.which("codex")
    if not configured:
        return ()
    configured_path = Path(configured)
    binary = configured_path.resolve()
    # Homebrew installs the binary under /opt/homebrew/bin; dyld also resolves
    # the /opt prefix itself.  Grant only those narrow install prefixes.
    roots = {configured_path.parent, binary.parent, binary.parent.parent}
    for parent in binary.parents:
        if parent in {Path("/opt"), Path("/usr"), Path("/Applications")}:
            roots.add(parent)
    return tuple(sorted(roots, key=str))


def codex_smoke(profile: Path, worktree: Path, env: Mapping[str, str] | None = None) -> dict[str, Any]:
    """Run ``codex --version`` under the *same* seatbelt, without a model call."""
    executable = os.environ.get("FRACTAL_CODEX_BIN") or shutil.which("codex")
    sandbox_exec = os.environ.get("FRACTAL_SANDBOX_EXEC", "/usr/bin/sandbox-exec")
    if not executable or not Path(sandbox_exec).is_file() or not profile.is_file():
        return {"passed": False, "failure_code": "codex_smoke_unavailable", "exit_code": None, "output_sha256": sha256_bytes(b"")}
    try:
        completed = subprocess.run([sandbox_exec, "-f", str(profile), executable, "--version"], cwd=str(worktree.resolve()), env=dict(env or _minimal_env()), capture_output=True, timeout=15, check=False, shell=False)
    except (OSError, subprocess.SubprocessError) as exc:
        return {"passed": False, "failure_code": "codex_smoke_error", "exit_code": None, "output_sha256": sha256_bytes(str(exc).encode())}
    stream = (completed.stdout or b"") + (completed.stderr or b"")
    return {"passed": completed.returncode == 0, "failure_code": None if completed.returncode == 0 else "codex_smoke_failed", "exit_code": completed.returncode, "output_sha256": sha256_bytes(stream)}


def schema_smoke(profile: Path, worktree: Path, schema_path: Path, env: Mapping[str, str] | None = None) -> dict[str, Any]:
    """Read/hash the exact planner schema under the same seatbelt, no model."""
    sandbox_exec = os.environ.get("FRACTAL_SANDBOX_EXEC", "/usr/bin/sandbox-exec")
    if not Path(sandbox_exec).is_file() or not profile.is_file() or not schema_path.is_file():
        return {"passed": False, "failure_code": "schema_smoke_unavailable", "output_sha256": sha256_bytes(b"")}
    script = "from pathlib import Path; import hashlib,sys; print(hashlib.sha256(Path(sys.argv[1]).read_bytes()).hexdigest())"
    try:
        completed = subprocess.run([sandbox_exec, "-f", str(profile), sys.executable, "-I", "-c", script, str(schema_path.resolve())], cwd=str(worktree.resolve()), env=dict(env or _minimal_env()), capture_output=True, timeout=10, check=False, shell=False)
    except (OSError, subprocess.SubprocessError) as exc:
        return {"passed": False, "failure_code": "schema_smoke_error", "output_sha256": sha256_bytes(str(exc).encode())}
    output = (completed.stdout or b"").decode("ascii", errors="ignore").strip()
    passed = completed.returncode == 0 and len(output) == 64 and all(char in "0123456789abcdef" for char in output)
    return {"passed": passed, "failure_code": None if passed else "schema_smoke_failed", "schema_sha256": output if passed else None, "output_sha256": sha256_bytes((completed.stdout or b"") + (completed.stderr or b""))}


def _run_worker(task_id: str, worktree: Path, staging: Path, profile: Path, episode_id: str, plan: Mapping[str, Any] | None = None, codex_home_root: Path | None = None) -> dict[str, Any]:
    """Invoke the companion Luna adapter; this is the sole model-call path."""
    intent, context, plan = task_manifest(task_id), build_context(task_id), (dict(plan) if plan is not None else build_plan(task_id))
    policy = policy_document(intent["allowed_paths"])
    route = _route_from_env()
    paths = {"intent": staging / "intent.json", "context": staging / "context.json", "plan": staging / "plan.json", "policy": staging / "policy.json", "schema": staging / "structured-patch.schema.json", "trace": staging / "trace.json", "events": staging / "events.json", "usage": staging / "usage.json", "report": staging / "enforcement.json"}
    for key, payload in (("intent", intent), ("context", context), ("plan", plan), ("policy", policy)):
        _write(paths[key], payload)
        paths[key].chmod(0o444)
    if route == LUNA_STRUCTURED_PATCH_ROUTE:
        paths["schema"].write_bytes(structured_patch_schema_bytes())
        paths["schema"].chmod(0o444)
    env = _minimal_env()
    env.update({"FRACTAL_TASK_ID": task_id, "FRACTAL_EPISODE_ID": episode_id, "FRACTAL_WORKTREE": str(worktree.resolve()), "FRACTAL_TASK_INTENT_PATH": str(paths["intent"].resolve()), "FRACTAL_CONTEXT_PATH": str(paths["context"].resolve()), "FRACTAL_SOL_PLAN_PATH": str(paths["plan"].resolve()), "FRACTAL_TRACE_PATH": str(paths["trace"].resolve()), "FRACTAL_EVENT_PATH": str(paths["events"].resolve()), "FRACTAL_USAGE_RECEIPT_PATH": str(paths["usage"].resolve()), "FRACTAL_POLICY_PATH": str(paths["policy"].resolve()), "FRACTAL_ENFORCEMENT_REPORT_PATH": str(paths["report"].resolve()), "FRACTAL_SANDBOX_PROFILE": str(profile.resolve()), "FRACTAL_SANDBOX_EXEC": os.environ.get("FRACTAL_SANDBOX_EXEC", "/usr/bin/sandbox-exec"), "FRACTAL_LUNA_ROUTE": route})
    if route == LUNA_STRUCTURED_PATCH_ROUTE:
        env["FRACTAL_PATCH_SCHEMA_PATH"] = str(paths["schema"].resolve())
    if codex_home_root is not None:
        env["FRACTAL_CODEX_HOME_ROOT"] = str(codex_home_root.resolve())
    started = time.monotonic()
    try:
        completed = subprocess.run([sys.executable, str(ADAPTER), "worker"], cwd=str(worktree.resolve()), env=env, capture_output=True, timeout=float(os.environ.get("FRACTAL_LUNA_TIMEOUT_SECONDS", "240")), check=False, shell=False)
        exit_code, timed_out, stdout, stderr = completed.returncode, False, completed.stdout or b"", completed.stderr or b""
    except subprocess.TimeoutExpired as exc:
        exit_code, timed_out = None, True
        stdout = exc.stdout if isinstance(exc.stdout, bytes) else str(exc.stdout or "").encode()
        stderr = exc.stderr if isinstance(exc.stderr, bytes) else str(exc.stderr or "").encode()
    result = {"route": route, "exit_code": exit_code, "timed_out": timed_out, "duration_ms": round((time.monotonic() - started) * 1000.0, 3), "stdout_sha256": sha256_bytes(stdout), "stderr_sha256": sha256_bytes(stderr), "trace_sha256": _hash(paths["trace"]), "events_sha256": _hash(paths["events"]), "usage_sha256": _hash(paths["usage"]), "enforcement_sha256": _hash(paths["report"]), "policy_sha256": policy_hash(policy), "usage_state": "present" if paths["usage"].is_file() else "unavailable", "outer_transport": "trusted-codex-controller-outbound-only", "inner_network": "denied"}
    if route == LUNA_STRUCTURED_PATCH_ROUTE:
        transport = _structured_transport(stdout)
        result.update({"patch_status": "missing", "patch_sha256": None, "preloaded_file_open_count": None, "applied": False})
        if isinstance(transport, Mapping):
            opened = transport.get("preloaded_file_open_count")
            if isinstance(opened, int) and not isinstance(opened, bool) and opened >= 0:
                result["preloaded_file_open_count"] = opened
            patch_digest = transport.get("patch_sha256")
            if patch_digest is None or isinstance(patch_digest, str):
                result["patch_sha256"] = patch_digest
            result["patch_status"] = str(transport.get("patch_status") or "rejected")
            transport_error = transport.get("patch_error")
            if isinstance(transport_error, str) and transport_error:
                result["patch_error_sha256"] = sha256_bytes(transport_error.encode("utf-8", errors="replace"))
            patch = transport.get("patch")
            if patch is not None and exit_code in (0, None) and not timed_out:
                try:
                    validated = validate_structured_patch(patch, worktree, intent["allowed_paths"], intent.get("forbidden_paths", []))
                    applied = apply_structured_patch(validated, worktree)
                    result.update({"patch_status": "applied", "applied": True, "patch_sha256": applied.get("patch_sha256"), "applied_paths_sha256": sha256_bytes(canonical_json(applied.get("changed_paths", [])))})
                except StructuredPatchError as exc:
                    result["patch_status"] = "rejected"
                    result["patch_error_code"] = exc.code
                    result["patch_error_sha256"] = sha256_bytes(str(exc).encode("utf-8", errors="replace"))
            del patch
        del transport
    if paths["trace"].is_file():
        try:
            trace_payload = json.loads(paths["trace"].read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            trace_payload = {}
        if isinstance(trace_payload, Mapping):
            attempts = trace_payload.get("leakage_attempts")
            if isinstance(attempts, list) and attempts:
                result["leakage_attempts"] = sorted({str(value)[:128] for value in attempts})
                result["safety_violation"] = "protected_auth_or_codex_home_access"
            trace_summary: dict[str, Any] = {}
            events_count = trace_payload.get("command_events")
            if isinstance(events_count, int) and not isinstance(events_count, bool) and events_count >= 0:
                trace_summary["command_events"] = events_count
            for key in ("opens", "failure_codes", "network_attempts", "external_side_effect_attempts", "process_inspection_attempts"):
                values = trace_payload.get(key)
                if isinstance(values, list):
                    trace_summary[key] = sorted({str(value)[:128] for value in values if isinstance(value, str)})[:128]
            if trace_summary:
                result["trace_summary"] = trace_summary
                if any(trace_summary.get(key) for key in ("network_attempts", "external_side_effect_attempts", "process_inspection_attempts")):
                    result["safety_violation"] = result.get("safety_violation") or "forbidden_worker_attempt"
    if paths["usage"].is_file():
        try:
            usage_payload = json.loads(paths["usage"].read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            usage_payload = {}
        if isinstance(usage_payload, Mapping):
            numeric = {key: usage_payload.get(key) for key in ("input_tokens", "output_tokens", "total_tokens", "cost_usd")}
            if isinstance(numeric.get("input_tokens"), int) and isinstance(numeric.get("output_tokens"), int) and isinstance(numeric.get("total_tokens"), int):
                result["usage"] = {key: value for key, value in numeric.items() if value is not None}
    if paths["events"].is_file():
        try:
            events_payload = json.loads(paths["events"].read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            events_payload = {}
        if isinstance(events_payload, Mapping):
            event_count = events_payload.get("event_count")
            event_types = events_payload.get("event_types")
            if isinstance(event_count, int) and isinstance(event_types, Mapping):
                event_summary: dict[str, Any] = {"event_count": event_count, "event_types": {str(key): int(value) for key, value in event_types.items() if isinstance(value, int) and not isinstance(value, bool) and value >= 0}}
                item_types = events_payload.get("item_types")
                if isinstance(item_types, Mapping):
                    event_summary["item_types"] = {str(key): int(value) for key, value in item_types.items() if isinstance(value, int) and not isinstance(value, bool) and value >= 0}
                for key in ("final_category", "final_length", "final_sha256"):
                    value = events_payload.get(key)
                    if key == "final_length" and isinstance(value, int) and not isinstance(value, bool) and value >= 0:
                        event_summary[key] = value
                    elif key != "final_length" and (value is None or isinstance(value, str)):
                        event_summary[key] = value
                result["event_summary"] = event_summary
    return result


def _run_planner(task_id: str, planner_dir: Path, profile: Path, codex_home_root: Path | None = None) -> tuple[dict[str, Any], dict[str, Any]]:
    """Run one arm-blind Sol-high planner call under the same seatbelt."""
    planner_dir.mkdir(parents=True, exist_ok=True)
    env = _minimal_env()
    env.update({"FRACTAL_SANDBOX_PROFILE": str(profile.resolve()), "FRACTAL_SANDBOX_EXEC": os.environ.get("FRACTAL_SANDBOX_EXEC", "/usr/bin/sandbox-exec")})
    if codex_home_root is not None:
        env["FRACTAL_CODEX_HOME_ROOT"] = str(codex_home_root.resolve())
    command = [sys.executable, str(ADAPTER), "plan", "--task-id", task_id, "--output-dir", str(planner_dir.resolve())]
    try:
        completed = subprocess.run(command, cwd=str(planner_dir.resolve()), env=env, capture_output=True, timeout=300, check=False, shell=False)
    except (OSError, subprocess.SubprocessError) as exc:
        error = RunnerError("Sol planner launch failed")
        error.details = {"stage": "sol_planner", "failure_code": "sol_planner_launch_error"}
        raise error from exc
    if completed.returncode != 0:
        error = RunnerError("Sol planner failed before freezing a plan")
        details: dict[str, Any] = {"stage": "sol_planner", "failure_code": "sol_planner_exit_nonzero", "exit_code": completed.returncode, "stdout_sha256": sha256_bytes(completed.stdout or b""), "stderr_sha256": sha256_bytes(completed.stderr or b"")}
        try:
            for line in (completed.stderr or b"").decode("utf-8", errors="replace").splitlines():
                payload = json.loads(line)
                message = payload.get("adapter_error") if isinstance(payload, Mapping) else None
                if not isinstance(message, str) or ":" not in message:
                    continue
                _, encoded = message.split(":", 1)
                summary = json.loads(encoded)
                if isinstance(summary, Mapping):
                    details["adapter_failure"] = {"event_type_counts": summary.get("event_type_counts", {}), "error_events": summary.get("error_events", [])[-8:], "stderr_category": summary.get("stderr_category", "unknown")}
                break
        except (UnicodeDecodeError, json.JSONDecodeError, TypeError):
            pass
        error.details = details
        raise error
    path = planner_dir / f"{task_id}.json"
    try:
        plan = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        error = RunnerError("Sol planner did not produce a valid frozen plan")
        error.details = {"stage": "sol_planner", "failure_code": "sol_planner_invalid_plan"}
        raise error from exc
    if not isinstance(plan, Mapping):
        error = RunnerError("Sol planner plan is not an object")
        error.details = {"stage": "sol_planner", "failure_code": "sol_planner_invalid_plan"}
        raise error
    metadata = {"plan_sha256": _hash(path), "metadata_sha256": _hash(planner_dir / f"{task_id}.metadata.json"), "stdout_sha256": sha256_bytes(completed.stdout or b""), "stderr_sha256": sha256_bytes(completed.stderr or b""), "model": "gpt-5.6-sol"}
    metadata_path = planner_dir / f"{task_id}.metadata.json"
    try:
        planner_metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        planner_metadata = {}
    usage = planner_metadata.get("usage") if isinstance(planner_metadata, Mapping) else None
    if isinstance(usage, Mapping):
        inputs, outputs = usage.get("input_tokens"), usage.get("output_tokens")
        if isinstance(inputs, int) and isinstance(outputs, int) and not isinstance(inputs, bool) and not isinstance(outputs, bool) and inputs >= 0 and outputs >= 0:
            metadata["usage"] = {"input_tokens": inputs, "output_tokens": outputs, "total_tokens": inputs + outputs}
    return dict(plan), metadata


def run_task(task_id: str, output: Path, *, go: bool = False) -> dict[str, Any]:
    validate_live_tasks((task_id,))
    if not go:
        raise RunnerError("model calls require explicit --go")
    route = _route_from_env()
    output = output.resolve(); output.mkdir(parents=True, exist_ok=True)
    result_path, episode_id, started = output / "results" / f"{task_id}.json", f"pgc-v2-{task_id}-0", time.monotonic()
    worker: dict[str, Any] = {}; checker: dict[str, Any] = {}; planner: dict[str, Any] = {}
    try:
        with tempfile.TemporaryDirectory(prefix=f"pgc-v2-{task_id}-", dir=str(output)) as temporary:
            root = Path(temporary); worktree = root / "worktree"; materialize_seed_only(task_id, worktree); baseline = _snapshot(worktree)
            staging, evaluator, profile, codex_home_root = root / "staging", root / "evaluator-staging", root / "worker.sb", root / "codex-home"; staging.mkdir(); codex_home_root.mkdir()
            profile.write_text(sandbox_profile(worktree, original_oracles=ROOT / "fixtures_v2" / "oracles", evaluator_staging=evaluator, readonly_roots=(root, staging, ROOT / "schemas", Path(tempfile.gettempdir()), *_codex_runtime_roots()), writable_roots=(codex_home_root,), worktree_writable=route != LUNA_STRUCTURED_PATCH_ROUTE), encoding="utf-8"); profile.chmod(0o400)
            # The adversarial probe is a hard gate.  No Sol or Luna call may
            # occur if the worker seatbelt cannot prove protected-path denial.
            probe = run_isolation_probe(worktree, original_oracles=ROOT / "fixtures_v2" / "oracles", evaluator_staging=evaluator, profile_path=profile, env=_minimal_env())
            if not probe.get("passed"):
                error = RunnerError("isolation probe failed before model call")
                error.details = {"stage": "isolation_probe", "failure_code": str(probe.get("failure_code") or "isolation_probe_failed"), "probe_sha256": str(probe.get("output_sha256") or probe.get("profile_sha256") or "")}
                raise error
            schema_check = schema_smoke(profile, worktree, ROOT / "schemas" / "live-plan.v1.schema.json", _minimal_env())
            if not schema_check.get("passed"):
                error = RunnerError("planner schema sandbox smoke failed before model call")
                error.details = {"stage": "schema_smoke", "failure_code": str(schema_check.get("failure_code") or "schema_smoke_failed"), "output_sha256": str(schema_check.get("output_sha256") or "")}
                raise error
            smoke = codex_smoke(profile, worktree, _minimal_env())
            if not smoke.get("passed"):
                error = RunnerError("Codex sandbox smoke failed before model call")
                error.details = {"stage": "codex_smoke", "failure_code": str(smoke.get("failure_code") or "codex_smoke_failed"), "output_sha256": str(smoke.get("output_sha256") or "")}
                raise error
            planner_dir = root / "plans"
            frozen_plan, planner_meta = _run_planner(task_id, planner_dir, profile, codex_home_root)
            planner = {**planner_meta, "schema_smoke": schema_check}
            worker = _run_worker(task_id, worktree, staging, profile, episode_id, frozen_plan, codex_home_root)
            worker["codex_smoke"] = smoke
            worker["planner"] = planner
            after = _snapshot(worktree); changed = sorted(path for path in set(baseline) | set(after) if baseline.get(path) != after.get(path))
            worker["changed_paths_sha256"] = sha256_bytes(canonical_json(changed)); worker["changed_file_hashes"] = {path: after[path] for path in changed if path in after}
            policy_failures: list[str] = []
            usage = worker.get("usage")
            if isinstance(usage, Mapping) and isinstance(usage.get("total_tokens"), int) and usage["total_tokens"] > LUNA_POSTHOC_TOKEN_CAP:
                policy_failures.append("budget_exceeded")
            if route == LUNA_STRUCTURED_PATCH_ROUTE and worker.get("patch_status") != "applied":
                policy_failures.append("structured_patch_rejected")
            if not changed:
                policy_failures.append("worker_no_changes")
            if policy_failures:
                worker["policy_failure_codes"] = policy_failures
            # Private checker staging happens strictly after adapter exit.
            evaluator.mkdir(); checker_path = copy_hidden_oracle(evaluator); raw = run_hidden_oracle(task_id, worktree, checker_path)
            checker_exit = raw.get("checker_exit_code") if isinstance(raw.get("checker_exit_code"), int) else None
            failure_code = raw.get("failure_code") if isinstance(raw.get("failure_code"), str) else None
            if failure_code is None and not bool(raw.get("passed")):
                failure_code = "checker_exit_nonzero" if checker_exit not in (None, 0) else "checker_assertion_failed"
            safe = {"passed": bool(raw.get("passed")), "failure_code": failure_code, "checker_exit_code": checker_exit}
            checker = {**safe, "checker_sha256": _hash(checker_path), "sanitized_sha256": sha256_bytes(canonical_json(safe)), "staged_after_worker": True}
            result = {"schema_version": RESULT_SCHEMA, "task_id": task_id, "episode_id": episode_id, "model": "gpt-5.6-luna", "route": route, "worker": worker, "checker": checker, "outcome": "pass" if safe["passed"] and not worker["timed_out"] and worker["exit_code"] in (0, None) and not worker.get("safety_violation") and not worker.get("policy_failure_codes") else "fail", "duration_ms": round((time.monotonic() - started) * 1000.0, 3), "limitations": ["Raw streams and temporary worktrees are not persisted."]}
            _write(result_path, result); return result
    except Exception as exc:
        details = getattr(exc, "details", None)
        if isinstance(details, Mapping):
            planner = dict(details)
        failure_code = str(planner.get("failure_code") or "runner_error")
        stage = str(planner.get("stage") or "runner")
        result = {"schema_version": RESULT_SCHEMA, "task_id": task_id, "episode_id": episode_id, "model": "gpt-5.6-luna", "route": route, "planner": planner, "worker": worker, "checker": checker, "outcome": "fail", "failure_code": failure_code, "failure_stage": stage, "error_sha256": sha256_bytes(str(exc).encode("utf-8", errors="replace")), "duration_ms": round((time.monotonic() - started) * 1000.0, 3), "limitations": ["Raw streams and temporary worktrees are not persisted."]}
        _write(result_path, result); return result


def plan_only(task_ids: Sequence[str], output: Path) -> dict[str, Any]:
    tasks = validate_live_tasks(task_ids); output = output.resolve(); rows = []
    for task_id in tasks:
        rows.append({"task_id": task_id, "plan_sha256": _write(output / "plans" / f"{task_id}.json", build_plan(task_id))})
    summary = {"schema_version": CALIBRATION_SCHEMA, "mode": "plan-only", "tasks": list(tasks), "model_calls": 0, "plans": rows}; _write(output / "plan-summary.json", summary); return summary


def calibrate_only(task_ids: Sequence[str], output: Path) -> dict[str, Any]:
    tasks = validate_live_tasks(task_ids); output = output.resolve(); output.mkdir(parents=True, exist_ok=True); rows = []; oracle = ROOT / "fixtures_v2" / "oracles"
    with tempfile.TemporaryDirectory(prefix="pgc-v2-calibrate-", dir=str(output)) as temporary:
        root = Path(temporary)
        for task_id in tasks:
            task_root, worktree = root / task_id, root / task_id / "worktree"; materialize_seed_only(task_id, worktree); staging, evaluator, profile = task_root / "staging", task_root / "evaluator-staging", task_root / "worker.sb"; staging.mkdir(parents=True)
            profile.write_text(sandbox_profile(worktree, original_oracles=oracle, evaluator_staging=evaluator, readonly_roots=(staging,)), encoding="utf-8"); profile.chmod(0o400)
            rows.append({"task_id": task_id, "seed_sha256": _snapshot_digest(worktree), "profile_sha256": _hash(profile), "isolation": run_isolation_probe(worktree, original_oracles=oracle, evaluator_staging=evaluator, profile_path=profile, env=_minimal_env())})
    passed = all(row["isolation"].get("passed") for row in rows); summary = {"schema_version": CALIBRATION_SCHEMA, "mode": "calibrate-only", "tasks": list(tasks), "model_calls": 0, "passed": bool(passed), "failure_code": None if passed else "isolation_calibration_failed", "tasks_detail": rows, "limitations": ["No model call and no private checker staging in calibration."]}; _write(output / "calibration.json", summary); return summary


def _cli(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__); parser.add_argument("--output", type=Path, required=True); parser.add_argument("--task-id", action="append", dest="task_ids"); parser.add_argument("--plan-only", action="store_true"); parser.add_argument("--calibrate-only", action="store_true"); parser.add_argument("--go", action="store_true"); parser.add_argument("--jobs", type=int, default=4, help="parallel live tasks (1-4)")
    args = parser.parse_args(argv)
    try:
        if args.plan_only and args.calibrate_only: raise RunnerError("--plan-only and --calibrate-only are mutually exclusive")
        tasks = validate_live_tasks(args.task_ids)
        if args.plan_only: summary = plan_only(tasks, args.output)
        elif args.calibrate_only: summary = calibrate_only(tasks, args.output)
        else:
            if not args.go: raise RunnerError("model calls require explicit --go")
            if args.jobs < 1 or args.jobs > 4: raise RunnerError("--jobs must be between 1 and 4")
            with concurrent.futures.ThreadPoolExecutor(max_workers=min(args.jobs, len(tasks))) as pool:
                futures = {pool.submit(run_task, task_id, args.output, go=True): task_id for task_id in tasks}
                by_task = {task_id: future.result() for future, task_id in ((future, futures[future]) for future in futures)}
            results = [by_task[task_id] for task_id in tasks]; summary = {"schema_version": RESULT_SCHEMA, "mode": "live", "tasks": list(tasks), "model_calls": len(results), "jobs": args.jobs, "results": results}; _write(args.output.resolve() / "summary.json", summary)
        print(json.dumps(summary, sort_keys=True)); return 0 if summary.get("passed", True) else 2
    except RunnerError as exc:
        print(json.dumps({"runner_error": str(exc)}, sort_keys=True), file=sys.stderr); return 78


if __name__ == "__main__":
    raise SystemExit(_cli())
