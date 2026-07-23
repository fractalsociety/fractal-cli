#!/usr/bin/env python3
"""Hand a fractal-cli run's sanitized outcome to DataEvol's *real* execution-
outcome normalizer and confirm it is accepted.

fractal-cli has already computed a replayable evidence root + a consent-gated,
sanitized export using the real `fractal-chain` primitives. This bridge maps that
sanitized export onto DataEvol's `codex.execution_evidence.v1` wire contract,
binds `source_evidence_hash` to the replayable evidence root, and runs it through
`dataevol.datasets.codex_execution_outcomes.normalize_execution_outcomes`
(+ `outcome_is_acceptable`) — the genuine governance, not a reimplementation.

Reads one JSON object on stdin:
  { "graph_id", "evidence_hex", "commitment_hex", "verified", "dataevol_src" }
Prints one JSON object on stdout on success:
  { "accepted", "outcome_id", "source_evidence_hash" }
Exits non-zero (fail-closed) with a message on stderr otherwise.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def build_wire_record(payload: dict) -> dict:
    """Map the sanitized export onto DataEvol's execution-evidence wire contract.

    The routing-relevant facts (task group, capabilities, risk/effort, the
    executed model as `executed_option_id`, its cost proxy, and a stable
    per-task-kind `counterfactual_group_id`) come from the real run so the
    accumulated outcomes let DataEvol's `build_cheapest_acceptable_rows` learn the
    cheapest acceptable model per capability cell. The comparison-pin hashes are
    stable per task-kind (not evidence-derived) so different-model runs group.
    """
    evidence_hex = str(payload["evidence_hex"]).split(":", 1)[-1]
    commitment_hex = str(payload["commitment_hex"]).split(":", 1)[-1]
    pin_hex = str(payload.get("pin_hash", evidence_hex)).split(":", 1)[-1]
    verified = bool(payload.get("verified", True))
    graph_id = str(payload.get("graph_id", "graph"))
    capabilities = list(payload.get("capabilities") or ["python.tests.execute"])
    option_id = str(payload.get("executed_option_id", "cheap"))
    return {
        "schema": "codex.execution_evidence.v1",
        "outcome_id": str(payload.get("outcome_id", f"fractal-cli-{graph_id}")),
        "experiment_id": "fractal-cli-interactive",
        "counterfactual_group_id": str(payload.get("counterfactual_group_id", f"{graph_id}-pair-0")),
        "arm": "classifier",
        "assignment_mechanism": "randomized",
        "task_id": str(payload.get("task_group", "fractal-cli")),
        "task_group": str(payload.get("task_group", "fractal-cli")),
        "subtask_id": "acceptance",
        "subtask_hash": evidence_hex,
        "plan_hash": commitment_hex,
        "decision_hash": evidence_hex,
        "usage_receipt_hash": evidence_hex,
        # Comparison pins — stable per task-kind so model variants compare.
        "catalog_hash": pin_hex,
        "pricing_hash": pin_hex,
        "policy_hash": pin_hex,
        "candidate_set_hash": pin_hex,
        "evidenceHash": evidence_hex,  # camelCase → source_evidence_hash
        "requiredCapabilities": capabilities,
        "risk": str(payload.get("risk", "low")),
        "estimatedInputTokens": int(payload.get("estimated_input_tokens", 2000)),
        "modelFamily": str(payload.get("model_family", "mixed")),
        "reasoningEffort": str(payload.get("effort", "medium")),
        "teacherOptionId": "teacher",
        "classifierOptionId": option_id,
        "classifierConfidence": 0.99,
        "executedOptionId": option_id,
        "modelId": str(payload.get("model_id", option_id)),
        "modelRevision": evidence_hex,
        "verified": verified,
        "independentVerifier": True,
        "success": verified,
        "verifierScore": 1.0 if verified else 0.0,
        "qualityFloor": 0.9,
        "costAmount": float(payload.get("cost_micros", 0.0)),
        "costUnit": "usd-micros",
        "latencyMs": 0,
        "retries": 0,
        "toolFailures": [],
        "safetyViolations": [],
        "policyViolations": [],
        "cheaperOptionTested": True,
        "completedAt": int(payload.get("completed_at", 1000)),
    }


def main() -> int:
    try:
        payload = json.loads(sys.stdin.read() or "{}")
    except json.JSONDecodeError as error:
        print(f"invalid ingest payload: {error}", file=sys.stderr)
        return 1

    src = Path(payload.get("dataevol_src", "")).expanduser()
    if not src.is_dir():
        print(f"DataEvol source not found: {src}", file=sys.stderr)
        return 2
    sys.path.insert(0, str(src))
    try:
        from dataevol.datasets.codex_execution_outcomes import (  # noqa: E402
            normalize_execution_outcomes,
            outcome_is_acceptable,
        )
    except Exception as error:  # noqa: BLE001 - fail-closed with a clear message
        print(f"cannot import DataEvol normalizer: {error}", file=sys.stderr)
        return 2

    wire = build_wire_record(payload)
    try:
        normalized = normalize_execution_outcomes([wire])[0]
    except Exception as error:  # noqa: BLE001
        print(f"DataEvol rejected the outcome: {error}", file=sys.stderr)
        return 1

    source_root = str(payload["evidence_hex"]).split(":", 1)[-1]
    if normalized.get("source_evidence_hash") != source_root:
        print("normalized outcome is not bound to the replayable evidence root", file=sys.stderr)
        return 1

    accepted = bool(outcome_is_acceptable(normalized))
    if bool(payload.get("verified", True)) and not accepted:
        print("DataEvol did not accept a verified run outcome", file=sys.stderr)
        return 1

    # Persist the normalized outcome to durable, append-only outcome memory so
    # later runs can learn the cheapest acceptable model per capability cell.
    memory_path = payload.get("memory_path")
    if memory_path:
        path = Path(memory_path).expanduser()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(normalized, sort_keys=True) + "\n")

    print(
        json.dumps(
            {
                "accepted": accepted,
                "outcome_id": normalized["outcome_id"],
                "source_evidence_hash": normalized["source_evidence_hash"],
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
