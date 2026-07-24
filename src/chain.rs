//! Per-run signed receipt ledger. Every lifecycle event in an interactive run —
//! a node lease, a node execution, a verifier verdict, a developmental (repair/
//! grow) step, and the final promotion/export — is committed as an ed25519-signed
//! receipt on a graph-scale [`fractal_chain::ScaleLedger`]. The whole run is then
//! auditable and tamper-evident, and folds into the wider chain.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use fractal_chain::{
    anchor_node_execution, anchor_promotion, anchor_step, anchor_verifier_verdict, commit_anchors,
    fold_child_into_parent, payload_hash_str, verify_child_anchored, AnchorEvent,
    DevelopmentalStep, Hash256, Receipt, ReceiptKind, ScaleLedger,
};

/// A thread-safe signed ledger for one interactive run.
pub(crate) struct RunLedger {
    ledger: Mutex<ScaleLedger>,
    graph_id: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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

fn chain_dir() -> PathBuf {
    let root = match std::env::var_os("FRACTAL_HOME") {
        Some(home) => PathBuf::from(home),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".fractal"),
            None => PathBuf::from(".fractal"),
        },
    };
    root.join("chain")
}

/// The durable, append-only fold log for the per-home machine-scale chain.
fn fold_log_path() -> PathBuf {
    chain_dir().join("machine-fold-log.jsonl")
}

/// Stable per-home signing seed for the machine-scale chain.
fn machine_seed() -> [u8; 32] {
    payload_hash_str(&format!("fractal.machine.v1:{}", chain_dir().display()))
}

/// Reconstruct the durable machine-scale ledger by replaying the fold log. Each
/// log entry replays the exact `Lineage` receipt `fold_child_into_parent` would
/// append (`child.scale`, `child.head`, timestamp), so the ledger is rebuilt
/// deterministically and signs identically across runs.
fn machine_ledger() -> ScaleLedger {
    let mut machine = ScaleLedger::from_seed("machine", machine_seed());
    if let Ok(text) = std::fs::read_to_string(fold_log_path()) {
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let scale = record
                .get("child_scale")
                .and_then(|value| value.as_str())
                .unwrap_or("graph");
            let root = record
                .get("child_root")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let timestamp = record
                .get("timestamp_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let receipt = Receipt::new(ReceiptKind::Lineage, scale, hex_to_hash(root), timestamp);
            machine.append(vec![receipt], timestamp);
        }
    }
    machine
}

/// Summary of the durable machine-scale chain after a fold.
pub(crate) struct FoldSummary {
    /// Runs anchored into the machine chain (one block per run).
    pub runs: usize,
    /// Global machine-chain root hash (hex).
    pub machine_root: String,
    /// Whether the machine chain currently verifies.
    pub verified: bool,
}

/// Reconstruct and verify the durable machine-scale chain without folding.
/// `(runs_anchored, machine_root_hex, verified)`.
pub(crate) fn machine_summary() -> (usize, String, bool) {
    let machine = machine_ledger();
    let verified = machine.verify().is_ok();
    (machine.blocks().len(), hex(&machine.head()), verified)
}

impl RunLedger {
    /// A ledger for `graph_id`, keyed by a seed derived from the graph id.
    pub(crate) fn new(graph_id: &str) -> Self {
        let mut seed = [0u8; 32];
        for (index, byte) in graph_id.as_bytes().iter().take(32).enumerate() {
            seed[index] = *byte;
        }
        Self {
            ledger: Mutex::new(ScaleLedger::from_seed("graph", seed)),
            graph_id: graph_id.to_owned(),
        }
    }

    fn subject(&self, node: &str) -> String {
        format!("{}#{node}", self.graph_id)
    }

    /// A node was leased to an agent (one owner).
    pub(crate) fn lease(&self, node: &str, agent: &str) {
        if let Ok(mut ledger) = self.ledger.lock() {
            let _ = commit_anchors(
                &mut ledger,
                vec![AnchorEvent::RouteDecision {
                    subject: self.subject(node),
                    decision_hash: payload_hash_str(&format!("lease:{node}:{agent}")),
                }],
                now_ms(),
            );
        }
    }

    /// A node finished executing, with an evidence digest.
    pub(crate) fn execution(&self, node: &str, evidence: Hash256) {
        if let Ok(mut ledger) = self.ledger.lock() {
            let _ = anchor_node_execution(&mut ledger, self.subject(node), evidence, now_ms());
        }
    }

    /// A verifier verdict for a node.
    pub(crate) fn verdict(&self, node: &str, passed: bool) {
        if let Ok(mut ledger) = self.ledger.lock() {
            let _ = anchor_verifier_verdict(
                &mut ledger,
                self.subject(node),
                payload_hash_str(&format!(
                    "verdict:{node}:{}",
                    if passed { "pass" } else { "fail" }
                )),
                now_ms(),
            );
        }
    }

    /// A developmental (grow/repair) step, anchored with lineage.
    pub(crate) fn developmental(&self, step: &DevelopmentalStep) {
        if let Ok(mut ledger) = self.ledger.lock() {
            anchor_step(&mut ledger, step, now_ms());
        }
    }

    /// A promotion / export decision for the run outcome.
    pub(crate) fn promotion(&self, subject: &str, decision: &str) {
        if let Ok(mut ledger) = self.ledger.lock() {
            let _ = anchor_promotion(
                &mut ledger,
                subject.to_owned(),
                payload_hash_str(decision),
                now_ms(),
            );
        }
    }

    /// `(receipt_blocks, global_root_hex, verified)`.
    pub(crate) fn summary(&self) -> (usize, String, bool) {
        match self.ledger.lock() {
            Ok(ledger) => (
                ledger.blocks().len(),
                hex(&ledger.head()),
                ledger.verify().is_ok(),
            ),
            Err(_) => (0, hex(&[0u8; 32]), false),
        }
    }

    /// Fold this run's signed graph-scale head into the durable, per-home
    /// machine-scale chain and persist the fold so the chain accumulates across
    /// runs. The machine ledger is reconstructed by replay, this run's head is
    /// folded in as a signed lineage receipt, both chains are re-verified, and
    /// the fold is appended to the log. Returns `None` on any error.
    pub(crate) fn fold_into_machine(&self, verified: bool) -> Option<FoldSummary> {
        let run = self.ledger.lock().ok()?;
        let mut machine = machine_ledger();
        let timestamp = now_ms();

        // Fold graph→machine (verifies both chains + scale-tower adjacency).
        fold_child_into_parent(&mut machine, &run, timestamp).ok()?;
        machine.verify().ok()?;
        verify_child_anchored(&machine, &run).ok()?;

        // Persist the fold for deterministic replay on the next run.
        let record = serde_json::json!({
            "child_scale": run.scale(),
            "child_root": hex(&run.head()),
            "timestamp_ms": timestamp,
            "graph_id": self.graph_id,
            "verified": verified,
            "receipts": run.blocks().len(),
        });
        std::fs::create_dir_all(chain_dir()).ok();
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(fold_log_path())
        {
            let _ = writeln!(file, "{record}");
        }

        Some(FoldSummary {
            runs: machine.blocks().len(),
            machine_root: hex(&machine.head()),
            verified: machine.verify().is_ok(),
        })
    }
}
