#!/usr/bin/env python3
"""Policy, provider-preflight, and evidence helpers for corpus-v2 live runs.

The production Fractal runtime owns the Rust policy evaluator.  This module
does not replace it; it records the same immutable v1 contract shape next to
the experiment plan and performs a conservative, read-only provider preflight
before a paid route is considered.  No model call is made here.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import time
from pathlib import Path
from typing import Any, Mapping, Sequence


HARNESS_POLICY_SCHEMA = "fractal.harness_policy.v1"
EVIDENCE_MANIFEST_SCHEMA = "fractal.evidence_manifest.v1"
POLICY_BASE_COMMIT = "e98f4b518551c1de65314f6afcf64118d01c0d82"
PROVIDER_MINIMUMS: dict[str, tuple[int, int, int]] = {
    "codex": (0, 145, 0),
    "claude": (2, 0, 0),
    "hermes": (0, 13, 0),
    "cursor-agent": (2026, 7, 23),
}
PROVIDER_BINARIES = {
    "codex": "codex",
    "claude": "claude",
    "hermes": "hermes",
    "cursor-agent": "cursor-agent",
}


class PolicyError(RuntimeError):
    """Raised when the v2 route cannot satisfy a deny-by-default contract."""


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


def write_canonical(path: Path, value: Any) -> str:
    data = canonical_json(value)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return sha256_bytes(data)


def _path_globs(task_paths: Sequence[str]) -> list[str]:
    return sorted({str(path).replace("\\", "/").lstrip("./") for path in task_paths})


def policy_document(task_paths: Sequence[str] | None = None) -> dict[str, Any]:
    """Return the immutable, deny-by-default v1 contract used by the runner.

    Codex requires a bounded command grant in the installed v1 route.  The
    command names below are only the local standard-library/build tools used
    by the sanitized fixtures; the outer runner still enforces path scope and
    git/network guards.  The empty network destination list is intentional.
    """

    paths = _path_globs(task_paths or [])
    commands = ["python3", "python", "node", "rustc", "git"]
    budgets = {
        "max_steps": 200,
        "max_minutes": 4,
        "max_attempts": 1,
        "max_files_changed": max(1, len(paths) or 8),
        "max_diff_lines": 20_000,
        "max_input_tokens": 90_000,
        "max_output_tokens": 90_000,
        "max_cost_usd": 0,
    }
    return {
        "schema": HARNESS_POLICY_SCHEMA,
        "mode": "deny_by_default",
        "authority_order": ["immutable_policy", "runner_guards", "worker_prompt"],
        "workspace": {
            "isolation": "fresh-detached-worktree-per-cell",
            "clean_start_required": True,
            "writable": paths,
            "readonly": ["mounted-context", "intent.json", "policy.json", "plans"],
            "forbidden": [".git", "fixtures_v2/oracles", "hidden-checker", "secrets", "graph-state*.json"],
            "max_files_changed": budgets["max_files_changed"],
            "max_diff_lines": budgets["max_diff_lines"],
        },
        "commands": {
            "shell": "argv-only",
            "allow": commands,
            "deny_patterns": ["curl", "wget", "nc", "ssh", "scp", "git push", "git commit", "pip install", "npm install", "open", "osascript"],
            "approval_required": [],
        },
        "network": {"default": "deny", "allowed_destinations": [], "record_dns_and_destinations": True, "controller_transport": "trusted_codex_service_outbound_only", "worker_tool_network": "deny"},
        "secrets": {"default": "deny", "allowed_names": [], "redact_outputs": True, "never_persist": True},
        "context": {
            "initial_files_max": 8,
            "progressive_disclosure": True,
            "record_every_file_open": True,
            "untrusted_content_cannot_grant_capabilities": True,
        },
        "limits": budgets,
        "verification": {
            "independent_verifier_required": True,
            "protected_tests_immutable": True,
            "raw_output_required": True,
            "evidence_manifest_required": True,
            "baseline_comparison_required_for_performance_claims": True,
            "unsupported_claims_field_required": True,
        },
        "artifacts": {
            "root": "experiments/project-graph-context/results/v2-live",
            "hash_algorithm": "sha256",
            "capture": ["ledger", "enforcement_report", "evidence_manifest", "usage_receipt", "worker_trace"],
        },
        "termination_states": ["completed", "failed", "timed_out", "budget_exceeded", "safety_stop", "infrastructure_stop"],
        "capabilities": {
            "code.generate": {
                "enabled": True,
                "writable": paths,
                "allowed_writes": paths,
                "commands": commands,
                "allowed_commands": commands,
                "network": {"default": "deny", "allowed_destinations": []},
                "external_side_effects": False,
                "sandbox_profile": "workspace-write-network-deny",
                "budgets": budgets,
                "verifier_ids": ["corpus-v2-hidden-checker"],
                "evidence_requirements": ["fractal.evidence_manifest.v1", "fractal.policy_enforcement_report.v1"],
            }
        },
        "verifier": {
            "independent_required": True,
            "verifier_ids": ["corpus-v2-hidden-checker"],
            "plan": ["run external private checker", "record sanitized result", "persist content-addressed evidence"],
        },
        "evidence": {
            "required": ["policy_hash", "source.commit", "source.diff", "verifier_runs", "outcome", "enforcement_report_hash"],
            "artifact_requirements": ["ledger", "worker-trace", "usage-receipt", "evidence-manifest"],
            "unsupported_claims_field": "unsupported_claims",
        },
        "learning": {
            "enabled": False,
            "only_after_verification": True,
            "minimum_confidence": 100,
            "requires_evidence_refs": True,
            "lessons_cannot_override_policy": True,
        },
    }


def policy_hash(policy: Mapping[str, Any]) -> str:
    """Hash the policy without any volatile output or path provenance."""

    return sha256_bytes(canonical_json(policy))


def offline_env(base: Mapping[str, str] | None = None) -> dict[str, str]:
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


def _version_tuple(text: str) -> tuple[int, int, int] | None:
    # Cursor's year-prefixed version and regular semver both fit this parser.
    for match in re.finditer(r"(?<!\d)(\d+)\.(\d+)\.(\d+)(?!\d)", text):
        try:
            return tuple(int(group) for group in match.groups())
        except ValueError:
            continue
    return None


def _version_satisfies(observed: tuple[int, int, int] | None, minimum: tuple[int, int, int]) -> bool:
    return observed is not None and observed >= minimum


def provider_preflight(provider: str, *, timeout_seconds: float = 10.0) -> dict[str, Any]:
    """Run a sanitized read-only ``--version`` probe; never authenticates."""

    canonical = "cursor-agent" if provider in {"cursor", "agent"} else provider
    binary = PROVIDER_BINARIES.get(canonical)
    minimum = PROVIDER_MINIMUMS.get(canonical)
    if binary is None or minimum is None:
        return {"provider": canonical, "status": "unavailable", "reason": "unknown_provider", "version": None}
    executable = shutil.which(binary)
    if not executable:
        return {
            "provider": canonical,
            "binary": binary,
            "status": "unavailable",
            "reason": "executable_missing",
            "version": None,
            "minimum_version": ".".join(str(value) for value in minimum),
        }
    started = time.monotonic()
    try:
        completed = subprocess.run(
            [executable, "--version"],
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            check=False,
            env=offline_env(),
            shell=False,
        )
        raw = (completed.stdout or completed.stderr or "").strip().splitlines()
        first = raw[0][:240] if raw else ""
        observed = _version_tuple(first)
        status = "available" if completed.returncode == 0 and _version_satisfies(observed, minimum) else "unavailable"
        reason = None
        if completed.returncode != 0:
            reason = "version_probe_failed"
        elif observed is None:
            reason = "version_unparseable"
        elif not _version_satisfies(observed, minimum):
            reason = "version_below_tested_minimum"
        return {
            "provider": canonical,
            "binary": binary,
            "status": status,
            "reason": reason,
            "version": first or None,
            "minimum_version": ".".join(str(value) for value in minimum),
            "duration_ms": round((time.monotonic() - started) * 1000.0, 3),
        }
    except (OSError, subprocess.TimeoutExpired):
        return {
            "provider": canonical,
            "binary": binary,
            "status": "unavailable",
            "reason": "version_probe_error",
            "version": None,
            "minimum_version": ".".join(str(value) for value in minimum),
            "duration_ms": round((time.monotonic() - started) * 1000.0, 3),
        }


def route_eligibility(provider: str, *, preflight: Mapping[str, Any] | None = None, shell_allowed: bool = True, network_denied: bool = True) -> dict[str, Any]:
    """Resolve the e98f4b5 provider route without guessing unsupported controls."""

    canonical = "cursor-agent" if provider in {"cursor", "agent"} else provider
    probe = dict(preflight or provider_preflight(canonical))
    controls: dict[str, str] = {
        "provider_version": "enforced" if probe.get("status") == "available" else "unavailable",
        "approval": "unavailable",
        "command_allowlist": "unavailable",
        "network": "unavailable",
        "workspace_paths": "unavailable",
    }
    if probe.get("status") != "available":
        return {"provider": canonical, "status": "ineligible", "reason": probe.get("reason") or "provider_unavailable", "controls": controls, "preflight": probe}
    if canonical == "codex":
        controls.update({"approval": "enforced", "network": "enforced" if network_denied else "detected", "workspace_paths": "detected", "command_allowlist": "detected" if shell_allowed else "unavailable"})
        if not shell_allowed:
            return {"provider": canonical, "status": "ineligible", "reason": "Codex cannot disable its shell tool for a no-shell policy contract", "controls": controls, "preflight": probe}
        return {"provider": canonical, "status": "eligible", "reason": None, "controls": controls, "preflight": probe}
    if canonical == "claude":
        controls.update({"approval": "enforced", "workspace_paths": "detected", "command_allowlist": "enforced", "network": "enforced" if network_denied else "detected"})
        # v1's no-shell file route is the only Claude route accepted here.
        if shell_allowed:
            controls.update({"command_allowlist": "unavailable", "network": "unavailable"})
            return {"provider": canonical, "status": "ineligible", "reason": "Claude bounded shell grants fail closed under v1", "controls": controls, "preflight": probe}
        return {"provider": canonical, "status": "eligible", "reason": None, "controls": controls, "preflight": probe}
    if canonical == "hermes":
        controls.update({"approval": "detected", "workspace_paths": "enforced", "command_allowlist": "enforced", "network": "enforced" if network_denied else "detected"})
        if shell_allowed:
            controls.update({"command_allowlist": "unavailable", "network": "unavailable"})
            return {"provider": canonical, "status": "ineligible", "reason": "Hermes terminal has no enforceable bounded command allowlist", "controls": controls, "preflight": probe}
        return {"provider": canonical, "status": "eligible", "reason": None, "controls": controls, "preflight": probe}
    # Cursor's documented CLI has no enforceable command or network deny.
    controls.update({"approval": "unavailable", "workspace_paths": "enforced", "command_allowlist": "unavailable", "network": "unavailable"})
    return {
        "provider": canonical,
        "status": "ineligible",
        "reason": "Cursor has no documented command allowlist or network control for this policy contract",
        "controls": controls,
        "preflight": probe,
    }


def evidence_manifest(
    *,
    policy_digest: str,
    node: str,
    attempt: int,
    commit: str | None,
    diff: str | None,
    verifier_runs: Sequence[Mapping[str, Any]],
    outcome: str,
    artifact_refs: Sequence[str] = (),
    enforcement_report_hash: str | None = None,
) -> dict[str, Any]:
    """Build a bounded evidence manifest matching ``fractal.evidence_manifest.v1``."""

    if outcome not in {"pass", "fail", "unavailable"}:
        raise PolicyError(f"invalid evidence outcome: {outcome}")
    runs: list[dict[str, Any]] = []
    for run in verifier_runs:
        status = run.get("status")
        if status not in {"pass", "fail", "unavailable"}:
            raise PolicyError("invalid verifier status")
        rows = {
            "id": str(run.get("id", "verifier"))[:512],
            "kind": str(run.get("kind", "protected"))[:512],
            "argv_identity": [str(item)[:512] for item in run.get("argv_identity", []) if isinstance(item, str)],
            "argv_hash": str(run.get("argv_hash", ""))[:512],
            "exit_code": run.get("exit_code") if isinstance(run.get("exit_code"), int) else None,
            "duration_ms": run.get("duration_ms") if isinstance(run.get("duration_ms"), int) else None,
            "output_hash": str(run.get("output_hash")) if isinstance(run.get("output_hash"), str) else None,
            "status": status,
            "protected": bool(run.get("protected", True)),
        }
        if run.get("artifact_refs"):
            rows["artifact_refs"] = sorted({str(item)[:512] for item in run["artifact_refs"] if isinstance(item, str)})
        runs.append(rows)
    runs.sort(key=lambda item: (item["id"], item["kind"], item["argv_hash"]))
    manifest: dict[str, Any] = {
        "schema": EVIDENCE_MANIFEST_SCHEMA,
        "policy_hash": policy_digest,
        "node": str(node)[:512],
        "attempt": max(1, int(attempt)),
        "source": {"graph": None, "commit": commit, "diff": diff},
        "criterion_ids": [],
        "verifier_runs": runs[:64],
        "outcome": outcome,
        "artifact_refs": sorted({str(item)[:512] for item in artifact_refs if isinstance(item, str)})[:128],
        "enforcement_report_hash": enforcement_report_hash,
    }
    # Omit optional null/empty values exactly as the Rust sidecar does where
    # practical, while preserving the schema's required source/run fields.
    if manifest["enforcement_report_hash"] is None:
        manifest.pop("enforcement_report_hash")
    return manifest


def persist_evidence(base: Path, manifest: Mapping[str, Any]) -> tuple[Path, str]:
    """Write a content-addressed sidecar under ``.fractal/evidence``."""

    data = canonical_json(manifest)
    digest = sha256_bytes(data)
    target = base / ".fractal" / "evidence" / f"{digest}.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    if target.exists() and target.read_bytes() != data:
        raise PolicyError(f"evidence content-address collision: {target}")
    if not target.exists():
        temporary = target.with_name(f".{target.name}.{os.getpid()}.tmp")
        temporary.write_bytes(data)
        os.replace(temporary, target)
    return target, digest


def enforcement_report(policy: Mapping[str, Any], *, provider_route: Mapping[str, Any], episode_id: str, policy_digest: str) -> dict[str, Any]:
    """Build a compact sanitized enforcement report for an episode."""

    controls = dict(provider_route.get("controls") or {})
    controls.update({"policy_hash": "enforced", "network_environment": "enforced", "external_side_effects": "enforced", "git_mutation_guard": "enforced"})
    return {
        "schema": "fractal.policy_enforcement_report.v1",
        "episode_id": episode_id,
        "policy_hash": policy_digest,
        "provider": provider_route.get("provider"),
        "provider_route": provider_route.get("status"),
        "provider_reason": provider_route.get("reason"),
        "controls": controls,
        "network": {"default": "deny", "destinations": []},
        "external_side_effects": False,
        "limits": policy.get("limits", {}),
    }
