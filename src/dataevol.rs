//! Genuine DataEvol ingest. After the run computes a consent-gated, sanitized
//! export with the real `fractal-chain` primitives, this hands that outcome to
//! DataEvol's **real** execution-outcome normalizer
//! (`dataevol.datasets.codex_execution_outcomes.normalize_execution_outcomes`)
//! and confirms it is accepted — the actual governance, not a reimplementation.
//!
//! The Python bridge is embedded in the binary so the installed `fractal` needs
//! no side-car files. DataEvol's source tree is discovered from
//! `$FRACTAL_DATAEVOL_SRC` or `~/FractalDataevol/src`; when it is absent, ingest
//! degrades gracefully (the file export still stands) instead of failing the run.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// The embedded DataEvol ingest bridge (run under `python3`).
const INGEST_PY: &str = include_str!("../scripts/dataevol_ingest.py");

/// Outcome of a successful DataEvol ingest.
pub(crate) struct Ingest {
    pub accepted: bool,
    pub outcome_id: String,
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
    let Some(src) = dataevol_src() else {
        return Ok(None);
    };

    // Write the embedded bridge next to a temp file so `python3` can run it.
    let script = std::env::temp_dir().join("fractal_dataevol_ingest.py");
    std::fs::write(&script, INGEST_PY)
        .with_context(|| format!("write ingest bridge {}", script.display()))?;

    // Real routing facts + durable memory path so the outcome accumulates and
    // later runs can learn the cheapest acceptable model per capability cell.
    let payload = json!({
        "graph_id": graph_id,
        "evidence_hex": evidence_hex,
        "commitment_hex": commitment_hex,
        "verified": verified,
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
    })
    .to_string();

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
        // 2 => DataEvol not importable in this env; treat as a graceful skip.
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
            }))
        }
        _ => bail!("DataEvol rejected the run outcome: {}", stderr.trim()),
    }
}
