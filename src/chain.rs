//! Per-run signed receipt ledger. Every lifecycle event in an interactive run —
//! a node lease, a node execution, a verifier verdict, a developmental (repair/
//! grow) step, and the final promotion/export — is committed as an ed25519-signed
//! receipt on a graph-scale [`fractal_chain::ScaleLedger`]. The whole run is then
//! auditable and tamper-evident, and folds into the wider chain.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use fractal_chain::{
    anchor_node_execution, anchor_promotion, anchor_step, anchor_verifier_verdict, commit_anchors,
    payload_hash_str, AnchorEvent, DevelopmentalStep, Hash256, ScaleLedger,
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
}
