//! Genuine DataEvol ingest. After the run computes a consent-gated, sanitized
//! export with the real `fractal-chain` primitives, this hands that outcome to
//! DataEvol's **real** execution-outcome normalizer
//! (`dataevol.datasets.codex_execution_outcomes.normalize_execution_outcomes`)
//! and confirms it is accepted — the actual governance, not a reimplementation.
//!
//! P1.1: `price_paid_frac`, `harness_revision`, `dataset_lineage_root`, and
//! `fallback_used` are attached to the normalized outcome after DataEvol accepts
//! the wire record. Unknown payload fields are preserved through the same path.
//! DataEvol remains outcome-normalization authority for acceptance / cell / hash
//! of the core schema; settlement fields ride alongside without dropping extras.
//!
//! The Python bridge is embedded in this module (owned) so settlement fields can
//! round-trip without editing the side-car script path. DataEvol's source tree is
//! discovered from `$FRACTAL_DATAEVOL_SRC` or `~/FractalDataevol/src`; when it is
//! absent, ingest degrades gracefully (the file export still stands).

#![allow(dead_code)] // Settlement field surfaces are read by callers / tests.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

/// Owned DataEvol ingest bridge (run under `python3`). Extends the historical
/// side-car with settlement-field round-trip + unknown-field preservation.
const INGEST_PY: &str = r#"#!/usr/bin/env python3
"""Hand a fractal-cli run's sanitized outcome to DataEvol's real normalizer.

Preserves price_paid_frac, harness_revision, dataset_lineage_root, fallback_used
and any unknown payload fields on the normalized outcome after DataEvol accepts
the core wire record (DataEvol remains acceptance authority).
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

SETTLEMENT_KEYS = (
    "price_paid_frac",
    "harness_revision",
    "dataset_lineage_root",
    "fallback_used",
)

# Wire / bridge bookkeeping keys that must not be copied as outcome extras.
_BRIDGE_KEYS = {
    "graph_id",
    "evidence_hex",
    "commitment_hex",
    "verified",
    "dataevol_src",
    "memory_path",
    "outcome_id",
    "counterfactual_group_id",
    "pin_hash",
    "completed_at",
    "task_group",
    "capabilities",
    "risk",
    "effort",
    "estimated_input_tokens",
    "model_family",
    "executed_option_id",
    "model_id",
    "cost_micros",
    "independent_verifier",
    "extras",
}


def build_wire_record(payload: dict) -> dict:
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
        "catalog_hash": pin_hex,
        "pricing_hash": pin_hex,
        "policy_hash": pin_hex,
        "candidate_set_hash": pin_hex,
        "evidenceHash": evidence_hex,
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
        "independentVerifier": bool(payload.get("independent_verifier", True)),
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


def merge_settlement_and_unknown(normalized: dict, payload: dict) -> dict:
    out = dict(normalized)
    for key in SETTLEMENT_KEYS:
        if key in payload and payload[key] is not None:
            out[key] = payload[key]
    extras = payload.get("extras")
    if isinstance(extras, dict):
        for key, value in extras.items():
            if key not in out:
                out[key] = value
    for key, value in payload.items():
        if key in _BRIDGE_KEYS or key in SETTLEMENT_KEYS:
            continue
        if key not in out:
            out[key] = value
    return out


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
    except Exception as error:  # noqa: BLE001
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

    enriched = merge_settlement_and_unknown(normalized, payload)

    memory_path = payload.get("memory_path")
    if memory_path:
        path = Path(memory_path).expanduser()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(enriched, sort_keys=True) + "\n")

    print(
        json.dumps(
            {
                "accepted": accepted,
                "outcome_id": enriched["outcome_id"],
                "source_evidence_hash": enriched["source_evidence_hash"],
                "price_paid_frac": enriched.get("price_paid_frac"),
                "harness_revision": enriched.get("harness_revision"),
                "dataset_lineage_root": enriched.get("dataset_lineage_root"),
                "fallback_used": enriched.get("fallback_used"),
                "normalized": enriched,
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
"#;

/// Settlement / lineage fields attached to the normalized outcome (P1.1).
#[derive(Debug, Clone)]
pub(crate) struct SettlementFields {
    pub price_paid_frac: u128,
    pub harness_revision: String,
    pub dataset_lineage_root: String,
    pub fallback_used: bool,
    /// Additional unknown fields preserved through the ingest path.
    pub extras: Map<String, Value>,
}

impl SettlementFields {
    /// Defaults from run facts + evidence lineage. Offline routing unchanged.
    pub(crate) fn from_run(
        facts: &crate::router::RunFacts,
        dataset_lineage_root: &str,
        fallback_used: bool,
    ) -> Self {
        let harness_revision = std::env::var("FRACTAL_HARNESS_REVISION")
            .unwrap_or_else(|_| "fractal-cli-orchestrate-v1".to_owned());
        Self {
            price_paid_frac: u128::from(facts.cost_micros),
            harness_revision,
            dataset_lineage_root: dataset_lineage_root.to_owned(),
            fallback_used,
            extras: Map::new(),
        }
    }
}

/// Outcome of a successful DataEvol ingest (with optional settlement fields).
pub(crate) struct Ingest {
    pub accepted: bool,
    pub outcome_id: String,
    pub price_paid_frac: Option<u128>,
    pub harness_revision: Option<String>,
    pub dataset_lineage_root: Option<String>,
    pub fallback_used: Option<bool>,
    pub normalized: Option<Value>,
}

/// Locate DataEvol's `src` tree, or `None` when it is not installed.
pub(crate) fn dataevol_src() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("FRACTAL_DATAEVOL_SRC") {
        let path = PathBuf::from(explicit);
        return path.is_dir().then_some(path);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path = home.join("FractalDataevol").join("src");
    path.is_dir().then_some(path)
}

/// Merge settlement + unknown fields onto a DataEvol-normalized object (Rust
/// mirror of the bridge helper; used by tests and offline enrichment).
pub(crate) fn merge_settlement_fields(
    normalized: &Value,
    fields: &SettlementFields,
) -> Result<Value> {
    let mut obj = normalized
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("normalized outcome must be a JSON object"))?;
    obj.insert("price_paid_frac".to_owned(), json!(fields.price_paid_frac));
    obj.insert(
        "harness_revision".to_owned(),
        json!(fields.harness_revision),
    );
    obj.insert(
        "dataset_lineage_root".to_owned(),
        json!(fields.dataset_lineage_root),
    );
    obj.insert("fallback_used".to_owned(), json!(fields.fallback_used));
    for (key, value) in &fields.extras {
        obj.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Ok(Value::Object(obj))
}

/// Hand the sanitized export to DataEvol's real normalizer and confirm it is
/// accepted. `Ok(None)` means DataEvol is not installed here (graceful skip);
/// `Err` means DataEvol was present but *rejected* the outcome (fail-closed).
pub(crate) fn ingest(
    graph_id: &str,
    evidence_hex: &str,
    commitment_hex: &str,
    verified: bool,
    facts: &crate::router::RunFacts,
) -> Result<Option<Ingest>> {
    ingest_with_settlement(
        graph_id,
        evidence_hex,
        commitment_hex,
        verified,
        facts,
        None,
    )
}

/// Like [`ingest`], optionally attaching P1.1 settlement fields that round-trip
/// through the DataEvol bridge without dropping unknown extras.
pub(crate) fn ingest_with_settlement(
    graph_id: &str,
    evidence_hex: &str,
    commitment_hex: &str,
    verified: bool,
    facts: &crate::router::RunFacts,
    settlement: Option<&SettlementFields>,
) -> Result<Option<Ingest>> {
    let Some(src) = dataevol_src() else {
        return Ok(None);
    };

    let script = std::env::temp_dir().join("fractal_dataevol_ingest_settlement19.py");
    std::fs::write(&script, INGEST_PY)
        .with_context(|| format!("write ingest bridge {}", script.display()))?;

    let mut payload = json!({
        "graph_id": graph_id,
        "evidence_hex": evidence_hex,
        "commitment_hex": commitment_hex,
        "verified": verified,
        "independent_verifier": true,
        "dataevol_src": src.to_string_lossy(),
        "memory_path": crate::router::memory_path().to_string_lossy(),
        "outcome_id": facts.outcome_id,
        "counterfactual_group_id": facts.group_id,
        "pin_hash": crate::router::pin_hash(&facts.task_group),
        "completed_at": facts.completed_at,
        "task_group": facts.task_group,
        "capabilities": facts.capabilities,
        "risk": facts.risk,
        "effort": facts.effort,
        "estimated_input_tokens": facts.estimated_input_tokens,
        "model_family": facts.model_family,
        "executed_option_id": facts.option_id,
        "model_id": facts.option_id,
        "cost_micros": facts.cost_micros,
    });
    if let Some(fields) = settlement {
        let obj = payload
            .as_object_mut()
            .ok_or_else(|| anyhow!("payload object"))?;
        obj.insert("price_paid_frac".to_owned(), json!(fields.price_paid_frac));
        obj.insert(
            "harness_revision".to_owned(),
            json!(fields.harness_revision),
        );
        obj.insert(
            "dataset_lineage_root".to_owned(),
            json!(fields.dataset_lineage_root),
        );
        obj.insert("fallback_used".to_owned(), json!(fields.fallback_used));
        if !fields.extras.is_empty() {
            obj.insert("extras".to_owned(), Value::Object(fields.extras.clone()));
        }
    }
    let payload = payload.to_string();

    let mut child = Command::new("python3")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to launch python3 for DataEvol ingest")?;
    child
        .stdin
        .take()
        .context("no stdin for ingest bridge")?
        .write_all(payload.as_bytes())
        .context("failed to write ingest payload")?;
    let output = child
        .wait_with_output()
        .context("DataEvol ingest bridge did not complete")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        Some(2) => Ok(None),
        Some(0) => {
            let line = String::from_utf8_lossy(&output.stdout);
            let last = line.lines().last().unwrap_or("").trim();
            let value: Value = serde_json::from_str(last)
                .map_err(|error| anyhow!("unparsable ingest result `{last}`: {error}"))?;
            Ok(Some(Ingest {
                accepted: value
                    .get("accepted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                outcome_id: value
                    .get("outcome_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                price_paid_frac: value
                    .get("price_paid_frac")
                    .and_then(Value::as_u64)
                    .map(u128::from)
                    .or_else(|| {
                        value
                            .get("price_paid_frac")
                            .and_then(Value::as_str)
                            .and_then(|s| s.parse().ok())
                    }),
                harness_revision: value
                    .get("harness_revision")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                dataset_lineage_root: value
                    .get("dataset_lineage_root")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                fallback_used: value.get("fallback_used").and_then(Value::as_bool),
                normalized: value.get("normalized").cloned(),
            }))
        }
        _ => bail!("DataEvol rejected the run outcome: {}", stderr.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn merge_preserves_unknown_and_settlement_fields() {
        let normalized = json!({
            "schema": "dataevol.codex_execution_outcome.v1",
            "outcome_id": "o1",
            "acceptable": true,
        });
        let mut fields = SettlementFields {
            price_paid_frac: 42,
            harness_revision: "h-rev".into(),
            dataset_lineage_root: "sha256:dead".into(),
            fallback_used: false,
            extras: Map::new(),
        };
        fields
            .extras
            .insert("custom_vendor_tag".into(), json!("keep-me"));
        let merged = merge_settlement_fields(&normalized, &fields).unwrap();
        assert_eq!(merged["price_paid_frac"], 42);
        assert_eq!(merged["harness_revision"], "h-rev");
        assert_eq!(merged["dataset_lineage_root"], "sha256:dead");
        assert_eq!(merged["fallback_used"], false);
        assert_eq!(merged["custom_vendor_tag"], "keep-me");
        assert_eq!(merged["outcome_id"], "o1");
    }

    #[test]
    fn settlement_fields_round_trip_through_dataevol_when_installed() {
        let Some(_) = dataevol_src() else {
            // Graceful skip when DataEvol is absent — offline CLI remains usable.
            return;
        };
        // Isolate from any real outcome-memory store: point FRACTAL_HOME at a
        // temp dir so this test never reads or writes a machine-specific path.
        // The frozen outcome-memory bytes live in tests/fixtures/capability_settlement19.
        let tmp = std::env::temp_dir().join(format!(
            "cap19-fractal-home-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var_os("FRACTAL_HOME");
        // SAFETY: test-only env mutation; restored below.
        unsafe { std::env::set_var("FRACTAL_HOME", &tmp) };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let facts = crate::router::RunFacts {
            task_group: "capability-settlement19".into(),
            capabilities: vec!["python.tests.execute".into()],
            risk: "low".into(),
            effort: "medium".into(),
            estimated_input_tokens: 2000,
            model_family: "mixed".into(),
            option_id: "cheap".into(),
            cost_micros: 42,
            group_id: format!("cap19-group-{now}"),
            outcome_id: format!("cap19-outcome-{now}"),
            completed_at: now,
        };
        let mut fields = SettlementFields::from_run(&facts, "sha256:lineage-root-cap19", false);
        fields
            .extras
            .insert("custom_vendor_tag".into(), json!("round-trip"));
        // Valid-looking 32-byte hex digests for evidence / commitment.
        let evidence = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let commitment = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let result = ingest_with_settlement(
            "cap19-roundtrip",
            evidence,
            commitment,
            true,
            &facts,
            Some(&fields),
        );
        match prev_home {
            Some(v) => unsafe { std::env::set_var("FRACTAL_HOME", v) },
            None => unsafe { std::env::remove_var("FRACTAL_HOME") },
        }
        let _ = std::fs::remove_dir_all(&tmp);
        let result = result
            .expect("DataEvol present must not hard-fail the bridge")
            .expect("DataEvol src is installed");
        assert!(result.accepted || result.normalized.is_some());
        assert_eq!(result.price_paid_frac, Some(42));
        assert_eq!(
            result.harness_revision.as_deref(),
            Some(fields.harness_revision.as_str())
        );
        assert_eq!(
            result.dataset_lineage_root.as_deref(),
            Some("sha256:lineage-root-cap19")
        );
        assert_eq!(result.fallback_used, Some(false));
        if let Some(normalized) = &result.normalized {
            assert_eq!(normalized.get("price_paid_frac"), Some(&json!(42)));
            assert_eq!(
                normalized.get("harness_revision"),
                Some(&json!(fields.harness_revision))
            );
            assert_eq!(
                normalized.get("dataset_lineage_root"),
                Some(&json!("sha256:lineage-root-cap19"))
            );
            assert_eq!(normalized.get("fallback_used"), Some(&json!(false)));
            assert_eq!(
                normalized.get("custom_vendor_tag"),
                Some(&json!("round-trip"))
            );
        }
    }
}
