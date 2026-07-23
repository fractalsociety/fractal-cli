//! End-to-end, self-evolving interactive run. Wraps the multi-agent executor to:
//!   1. lease each node (one owner) and emit a signed chain receipt,
//!   2. anchor a signed receipt for every node execution + verifier verdict,
//!   3. on a *verified failure*, run governed evolution — anchor a repair
//!      developmental step, persist a new child graph with parent lineage, and
//!      re-run (re-enqueuing the nodes),
//!   4. on success, auto-export the sanitized outcome to DataEvol.
//!
//! Everything is committed to a per-run signed [`crate::chain::RunLedger`].

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use fractal_chain::{
    payload_hash_str, sanitized_export, Consent, DevelopmentalOp, DevelopmentalStep, EvidenceEntry,
    Hash256, OutcomeField, ScaleLevel, Sensitivity,
};

use crate::chain::RunLedger;
use crate::{execute, graph_store};

/// Bounded so evolution always terminates.
const MAX_REPAIRS: u32 = 2;

fn hex(hash: &Hash256) -> String {
    let mut s = String::from("sha256:");
    for byte in hash {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn hex_to_hash(hexstr: &str) -> Hash256 {
    let hexstr = hexstr.strip_prefix("sha256:").unwrap_or(hexstr);
    let mut out = [0u8; 32];
    for (index, chunk) in hexstr.as_bytes().chunks(2).take(32).enumerate() {
        out[index] = std::str::from_utf8(chunk)
            .ok()
            .and_then(|s| u8::from_str_radix(s, 16).ok())
            .unwrap_or(0);
    }
    out
}

/// Run a committed graph end-to-end with signed receipts, governed evolution on
/// failure, and a DataEvol export on success. Returns the final outcome.
pub(crate) fn run_end_to_end(
    graph_hash: &str,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
) -> Result<execute::RunOutcome> {
    let mut graph = graph_store::load_graph(graph_hash)?;
    let graph_id = graph
        .get("graph_id")
        .and_then(Value::as_str)
        .unwrap_or("graph")
        .to_owned();
    let ledger = RunLedger::new(&graph_id);

    let mut attempt = 0u32;
    let outcome = loop {
        let outcome = execute::run_multi_agent(&graph, workspace, agents, board)?;

        // (1)+(2) Signed receipts for every lifecycle event: lease, execution,
        // and (for verify nodes) the verdict.
        for run in &outcome.log {
            ledger.lease(&run.node, &run.agent);
            ledger.execution(&run.node, hex_to_hash(&run.evidence_hex));
            if run.is_verify {
                ledger.verdict(&run.node, run.ok);
            }
        }

        // (3) Governed evolution after a verified failure.
        let failed = outcome.verified == Some(false) || outcome.failed_node.is_some();
        if failed && attempt < MAX_REPAIRS {
            let failed_node = outcome
                .failed_node
                .clone()
                .unwrap_or_else(|| "acceptance".to_owned());
            println!(
                "  ⟳ verified failure at `{failed_node}` — governed repair (attempt {})…",
                attempt + 1
            );
            let motivating = outcome
                .log
                .iter()
                .find(|run| run.node == failed_node)
                .map(|run| hex_to_hash(&run.evidence_hex))
                .unwrap_or([0u8; 32]);
            let produced = payload_hash_str(&format!("repair:{graph_id}:{failed_node}:{attempt}"));
            let step = DevelopmentalStep {
                scale: ScaleLevel::Graph,
                subject: format!("{graph_id}#{failed_node}"),
                operation: DevelopmentalOp::Repair,
                step_id: format!("repair-{failed_node}-{attempt}"),
                motivating_outcome: motivating,
                produced_outcome: produced,
            };
            ledger.developmental(&step);

            // (4) Persist a new child graph carrying the repair guidance + parent
            // lineage; (5) re-running it re-enqueues the nodes.
            let (child_hash, child_graph) = evolve_graph(&graph, attempt, &failed_node)?;
            ledger.promotion(
                &format!("{graph_id}->{child_hash}"),
                &format!("lineage:repair:{}", step.step_id),
            );
            println!(
                "  ⟳ persisted child graph {} (repair lineage on-chain)",
                &child_hash[..23.min(child_hash.len())]
            );
            graph = child_graph;
            attempt += 1;
            continue;
        }
        break outcome;
    };

    // (6) Auto-export the sanitized outcome to DataEvol on success.
    if outcome.verified != Some(false) && outcome.failed_node.is_none() {
        if let Err(error) = export_to_dataevol(&graph_id, &outcome, workspace, &ledger) {
            eprintln!("  export note: {error:#}");
        }
    }

    let (blocks, root, ok) = ledger.summary();
    println!(
        "  ⛓  chain: {blocks} signed receipt block(s) · root {} · {}",
        &root[..23.min(root.len())],
        if ok { "verified" } else { "INVALID" }
    );
    Ok(outcome)
}

/// Build a child graph from a failed parent: append repair guidance to the build
/// nodes, record the parent hash for lineage, and commit it (new hash).
fn evolve_graph(parent: &Value, attempt: u32, failed_node: &str) -> Result<(String, Value)> {
    let mut child = parent.clone();
    child["evolution"] = json!(attempt + 1);
    child["parent_graph"] = parent.get("graph_hash").cloned().unwrap_or(Value::Null);
    if let Some(nodes) = child.get_mut("nodes").and_then(Value::as_array_mut) {
        for node in nodes {
            let capability = node
                .get("capability")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if capability.contains("code.generate")
                || capability.ends_with(".edit")
                || capability.contains("code.write")
            {
                let instruction = node
                    .get("instruction")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                node["instruction"] = json!(format!(
                    "{instruction}\n\nREPAIR: a previous attempt FAILED verification at `{failed_node}`. \
                     Read any error output, fix the implementation and tests so the whole suite passes."
                ));
            }
        }
    }
    // Recompute the content hash (canonical over the graph minus graph_hash).
    let mut hash_input = child
        .as_object()
        .cloned()
        .context("child graph must be an object")?;
    hash_input.remove("graph_hash");
    let graph_hash = fractal_contracts::canonical_sha256(&Value::Object(hash_input))
        .map_err(|error| anyhow!("child graph hashing failed: {error}"))?;
    child["graph_hash"] = json!(graph_hash);
    let record = graph_store::commit_graph(&child)?;
    Ok((record.graph_hash, child))
}

/// Build the replayable evidence root + consent-gated sanitized export and write
/// it as the DataEvol handoff.
fn export_to_dataevol(
    graph_id: &str,
    outcome: &execute::RunOutcome,
    workspace: &Path,
    ledger: &RunLedger,
) -> Result<()> {
    let entries: Vec<EvidenceEntry> = outcome
        .log
        .iter()
        .map(|run| EvidenceEntry::new(format!("node:{}", run.node), hex_to_hash(&run.evidence_hex)))
        .collect();
    if entries.is_empty() {
        return Ok(());
    }
    let fields = vec![
        OutcomeField {
            key: "summary".to_owned(),
            value_digest: payload_hash_str(&outcome.detail),
            sensitivity: Sensitivity::Public,
        },
        OutcomeField {
            key: "workspace_path".to_owned(),
            value_digest: payload_hash_str(workspace.to_string_lossy().as_ref()),
            sensitivity: Sensitivity::Private,
        },
    ];
    let consent = Consent {
        granted: true,
        scope: "dataevol:promotion".to_owned(),
    };
    let export = sanitized_export(&entries, &fields, &consent)
        .map_err(|error| anyhow!("consent-gated export denied: {error}"))?;

    let dir = workspace.join(".fractal");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("dataevol-export.json");
    let payload = json!({
        "schema": "fractal.dataevol_export.v1",
        "graph_id": graph_id,
        "evidence_root": hex(&export.evidence_root),
        "public_fields": export.public_fields.iter().map(|(k, d)| json!([k, hex(d)])).collect::<Vec<_>>(),
        "redacted_count": export.redacted_count,
        "consent_scope": export.consent_scope,
        "export_commitment": hex(&export.export_commitment),
    });
    std::fs::write(&path, serde_json::to_string_pretty(&payload)? + "\n")
        .with_context(|| format!("write {}", path.display()))?;
    ledger.promotion(
        graph_id,
        &format!("dataevol:exported:{}", hex(&export.export_commitment)),
    );
    println!(
        "  ⇢ exported sanitized outcome to DataEvol · {} public field(s), {} redacted · {}",
        export.public_fields.len(),
        export.redacted_count,
        path.display()
    );
    Ok(())
}
