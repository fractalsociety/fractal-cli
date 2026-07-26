//! Cross-scale developmental lineage (P6.6).
//!
//! Every developmental step — a `grow` / `differentiate` / `repair` applied by
//! morphogenesis at some scale — is recorded with a link to the *outcome that
//! motivated it* and the *outcome it produced*. Because one step's produced
//! outcome can be another step's motivating outcome, and steps live at different
//! scales, the resulting lineage is a single causal chain that is traversable
//! across scales: `node ↔ graph ↔ network ↔ …`.
//!
//! Steps are anchored on a scale's [`ScaleLedger`] as `DevelopmentalStep` +
//! `Lineage` receipts, so the lineage is tamper-evident and folds upward with
//! the rest of the chain — a step cannot be silently rewritten or unlinked from
//! its cause.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::fold::ScaleLevel;
use crate::ledger::{Block, ScaleLedger};
use crate::merkle::{keccak256, Hash256};
use crate::receipt::{Receipt, ReceiptKind};

/// The bounded developmental operations morphogenesis may apply, at any scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevelopmentalOp {
    /// Add a node/sub-graph.
    Grow,
    /// Specialize an existing node/sub-graph.
    Differentiate,
    /// Fix a failed node/sub-graph.
    Repair,
}

impl DevelopmentalOp {
    /// Stable tag used in the step commitment (never renumber).
    #[must_use]
    pub const fn tag(self) -> u8 {
        match self {
            Self::Grow => 1,
            Self::Differentiate => 2,
            Self::Repair => 3,
        }
    }

    /// Stable lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Grow => "grow",
            Self::Differentiate => "differentiate",
            Self::Repair => "repair",
        }
    }
}

/// One developmental step and its causal links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevelopmentalStep {
    /// The scale at which this step was applied.
    pub scale: ScaleLevel,
    /// The node/graph/network id this step reshaped.
    pub subject: String,
    /// Which developmental rule was applied.
    pub operation: DevelopmentalOp,
    /// Unique id of this step (stable across the run).
    pub step_id: String,
    /// Digest of the outcome that *motivated* this step (an evidence root, a
    /// failed verdict, a discovered edge case, …).
    pub motivating_outcome: Hash256,
    /// Digest of the outcome this step *produced* (which may in turn motivate a
    /// step at another scale).
    pub produced_outcome: Hash256,
}

impl DevelopmentalStep {
    /// Deterministic keccak commitment over every field — the step's on-chain
    /// `DevelopmentalStep` leaf. Length-prefixed so no two distinct steps
    /// collide.
    #[must_use]
    pub fn commitment(&self) -> Hash256 {
        let scale = self.scale.as_str().as_bytes();
        let subject = self.subject.as_bytes();
        let step_id = self.step_id.as_bytes();
        let mut pre = Vec::with_capacity(1 + 8 + scale.len() + 8 + subject.len() + 8 + step_id.len() + 64);
        pre.push(self.operation.tag());
        pre.extend_from_slice(&(scale.len() as u64).to_be_bytes());
        pre.extend_from_slice(scale);
        pre.extend_from_slice(&(subject.len() as u64).to_be_bytes());
        pre.extend_from_slice(subject);
        pre.extend_from_slice(&(step_id.len() as u64).to_be_bytes());
        pre.extend_from_slice(step_id);
        pre.extend_from_slice(&self.motivating_outcome);
        pre.extend_from_slice(&self.produced_outcome);
        keccak256(&pre)
    }

    /// The lineage-link commitment binding this step's motivating outcome to the
    /// outcome it produced.
    #[must_use]
    pub fn link_commitment(&self) -> Hash256 {
        let mut pre = [0u8; 64];
        pre[..32].copy_from_slice(&self.motivating_outcome);
        pre[32..].copy_from_slice(&self.produced_outcome);
        keccak256(&pre)
    }
}

/// Anchor a developmental step on its scale's ledger as a `DevelopmentalStep`
/// receipt plus a `Lineage` receipt (motivating → produced), sealed in one
/// block. Reusing the chain makes the lineage tamper-evident and fold-composable.
pub fn anchor_step<'a>(
    ledger: &'a mut ScaleLedger,
    step: &DevelopmentalStep,
    timestamp_ms: u64,
) -> &'a Block {
    let development = Receipt::new(
        ReceiptKind::DevelopmentalStep,
        format!("{}#{}", step.scale.as_str(), step.step_id),
        step.commitment(),
        timestamp_ms,
    );
    let lineage = Receipt::new(
        ReceiptKind::Lineage,
        format!("{}#{}", step.scale.as_str(), step.step_id),
        step.link_commitment(),
        timestamp_ms,
    );
    ledger.append(vec![development, lineage], timestamp_ms)
}

/// A traversable index of developmental steps across every scale.
#[derive(Debug, Default)]
pub struct LineageGraph {
    by_step_id: BTreeMap<String, DevelopmentalStep>,
    /// produced_outcome → step_id that produced it.
    producer_of: BTreeMap<Hash256, String>,
}

impl LineageGraph {
    /// An empty lineage graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a step. Later inserts with the same `step_id` replace the earlier
    /// one; the producer index tracks the most recent producer of an outcome.
    pub fn insert(&mut self, step: DevelopmentalStep) {
        self.producer_of
            .insert(step.produced_outcome, step.step_id.clone());
        self.by_step_id.insert(step.step_id.clone(), step);
    }

    /// Look up a step by id.
    #[must_use]
    pub fn get(&self, step_id: &str) -> Option<&DevelopmentalStep> {
        self.by_step_id.get(step_id)
    }

    /// The step (if any) whose produced outcome is `outcome` — i.e. the step
    /// that *caused* `outcome`.
    #[must_use]
    pub fn producer_of(&self, outcome: &Hash256) -> Option<&DevelopmentalStep> {
        self.producer_of
            .get(outcome)
            .and_then(|step_id| self.by_step_id.get(step_id))
    }

    /// Walk the causal chain from `step_id` toward its root cause: the step,
    /// then the step that produced its motivating outcome, and so on. Stops at a
    /// step whose motivating outcome has no known producer (a root cause) and is
    /// cycle-safe (a repeated step id ends the walk).
    #[must_use]
    pub fn trail(&self, step_id: &str) -> Vec<&DevelopmentalStep> {
        let mut trail = Vec::new();
        let mut seen = BTreeSet::new();
        let mut current = self.by_step_id.get(step_id);
        while let Some(step) = current {
            if !seen.insert(step.step_id.as_str()) {
                break;
            }
            trail.push(step);
            current = self.producer_of(&step.motivating_outcome);
        }
        trail
    }

    /// The set of scales a step's lineage trail touches.
    #[must_use]
    pub fn scales_in_trail(&self, step_id: &str) -> BTreeSet<ScaleLevel> {
        self.trail(step_id)
            .into_iter()
            .map(|step| step.scale)
            .collect()
    }

    /// Whether a step's lineage spans more than one scale (i.e. crosses the
    /// node↔graph↔network boundaries) — the defining property of cross-scale
    /// lineage.
    #[must_use]
    pub fn is_cross_scale(&self, step_id: &str) -> bool {
        self.scales_in_trail(step_id).len() > 1
    }

    /// Every recorded step, in stable id order.
    pub fn steps(&self) -> impl Iterator<Item = &DevelopmentalStep> {
        self.by_step_id.values()
    }
}

/// Whether `step`'s `DevelopmentalStep` and `Lineage` receipts are both
/// committed on `ledger` — i.e. the step is anchored on-chain and therefore
/// tamper-evident. (`anchor_step` seals exactly this receipt pair.)
#[must_use]
pub fn step_is_anchored(ledger: &ScaleLedger, step: &DevelopmentalStep) -> bool {
    let development = step.commitment();
    let link = step.link_commitment();
    let mut has_development = false;
    let mut has_link = false;
    for block in ledger.blocks() {
        for receipt in &block.receipts {
            if receipt.kind == ReceiptKind::DevelopmentalStep && receipt.payload_hash == development
            {
                has_development = true;
            }
            if receipt.kind == ReceiptKind::Lineage && receipt.payload_hash == link {
                has_link = true;
            }
        }
    }
    has_development && has_link
}

/// The result of auditing a run's developmental lineage for P5.4: did the graph
/// grow or repair itself from a real outcome, anchored with traversable lineage?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevelopmentAudit {
    /// At least one `grow` or `repair` developmental step occurred.
    pub grew_or_repaired: bool,
    /// Every grow/repair step is committed on the chain (`DevelopmentalStep` +
    /// `Lineage` receipts) and so is tamper-evident.
    pub anchored: bool,
    /// Every grow/repair step links to a real (non-empty) motivating outcome.
    pub motivated: bool,
}

impl DevelopmentAudit {
    /// P5.4 passes iff the graph developmentally changed itself, that change was
    /// anchored on-chain with lineage, and it was motivated by a real outcome.
    #[must_use]
    pub fn passes(self) -> bool {
        self.grew_or_repaired && self.anchored && self.motivated
    }
}

/// Audit a run's developmental steps (from `lineage`) against their anchoring
/// `ledger`. Only `grow`/`repair` steps count as the graph reshaping itself
/// (`differentiate` specializes but is not the P5.4 signal).
#[must_use]
pub fn audit_development(lineage: &LineageGraph, ledger: &ScaleLedger) -> DevelopmentAudit {
    let reshaping: Vec<&DevelopmentalStep> = lineage
        .steps()
        .filter(|step| {
            matches!(
                step.operation,
                DevelopmentalOp::Grow | DevelopmentalOp::Repair
            )
        })
        .collect();
    DevelopmentAudit {
        grew_or_repaired: !reshaping.is_empty(),
        anchored: !reshaping.is_empty()
            && reshaping
                .iter()
                .all(|step| step_is_anchored(ledger, step)),
        motivated: !reshaping.is_empty()
            && reshaping
                .iter()
                .all(|step| step.motivating_outcome != [0u8; 32]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::ScaleLedger;
    use ed25519_dalek::SigningKey;

    fn outcome(byte: u8) -> Hash256 {
        [byte; 32]
    }

    fn step(
        scale: ScaleLevel,
        id: &str,
        op: DevelopmentalOp,
        motivating: Hash256,
        produced: Hash256,
    ) -> DevelopmentalStep {
        DevelopmentalStep {
            scale,
            subject: format!("{}:subject", scale.as_str()),
            operation: op,
            step_id: id.to_owned(),
            motivating_outcome: motivating,
            produced_outcome: produced,
        }
    }

    /// A causal chain that crosses three scales:
    /// a network `differentiate` produced O1, which motivated a graph `grow`
    /// producing O2, which motivated a node `repair` producing O3.
    fn cross_scale_graph() -> (LineageGraph, &'static str) {
        let net = step(
            ScaleLevel::Network,
            "net-diff",
            DevelopmentalOp::Differentiate,
            outcome(9),
            outcome(1),
        );
        let graph = step(
            ScaleLevel::Graph,
            "graph-grow",
            DevelopmentalOp::Grow,
            outcome(1),
            outcome(2),
        );
        let node = step(
            ScaleLevel::Node,
            "node-repair",
            DevelopmentalOp::Repair,
            outcome(2),
            outcome(3),
        );
        let mut lineage = LineageGraph::new();
        lineage.insert(net);
        lineage.insert(graph);
        lineage.insert(node);
        (lineage, "node-repair")
    }

    #[test]
    fn commitment_and_link_are_deterministic_and_field_sensitive() {
        let base = step(ScaleLevel::Node, "s1", DevelopmentalOp::Grow, outcome(1), outcome(2));
        assert_eq!(base.commitment(), base.commitment());
        let mut other = base.clone();
        other.operation = DevelopmentalOp::Repair;
        assert_ne!(base.commitment(), other.commitment());
        let mut swapped = base.clone();
        std::mem::swap(&mut swapped.motivating_outcome, &mut swapped.produced_outcome);
        assert_ne!(base.link_commitment(), swapped.link_commitment());
    }

    #[test]
    fn trail_walks_the_causal_chain_across_scales() {
        let (lineage, start) = cross_scale_graph();
        let trail = lineage.trail(start);
        let ids: Vec<&str> = trail.iter().map(|step| step.step_id.as_str()).collect();
        assert_eq!(ids, ["node-repair", "graph-grow", "net-diff"]);
        assert!(lineage.is_cross_scale(start));
        assert_eq!(
            lineage.scales_in_trail(start),
            BTreeSet::from([ScaleLevel::Node, ScaleLevel::Graph, ScaleLevel::Network])
        );
    }

    #[test]
    fn producer_lookup_links_outcome_to_its_cause() {
        let (lineage, _) = cross_scale_graph();
        assert_eq!(
            lineage.producer_of(&outcome(2)).map(|s| s.step_id.as_str()),
            Some("graph-grow")
        );
        assert!(lineage.producer_of(&outcome(42)).is_none());
    }

    #[test]
    fn single_scale_lineage_is_not_cross_scale() {
        let mut lineage = LineageGraph::new();
        lineage.insert(step(ScaleLevel::Node, "a", DevelopmentalOp::Grow, outcome(5), outcome(6)));
        lineage.insert(step(ScaleLevel::Node, "b", DevelopmentalOp::Repair, outcome(6), outcome(7)));
        assert!(!lineage.is_cross_scale("b"));
        assert_eq!(lineage.trail("b").len(), 2);
    }

    #[test]
    fn trail_is_cycle_safe() {
        // Two steps that motivate each other must not loop forever.
        let mut lineage = LineageGraph::new();
        lineage.insert(step(ScaleLevel::Node, "x", DevelopmentalOp::Grow, outcome(1), outcome(2)));
        lineage.insert(step(ScaleLevel::Graph, "y", DevelopmentalOp::Grow, outcome(2), outcome(1)));
        let ids: Vec<&str> = lineage.trail("x").iter().map(|s| s.step_id.as_str()).collect();
        // x → producer(1)=y → producer(2)=x already seen → stop.
        assert_eq!(ids, ["x", "y"]);
    }

    #[test]
    fn audit_passes_for_an_anchored_motivated_repair() {
        let mut ledger = ScaleLedger::new("graph", SigningKey::from_bytes(&[4u8; 32]));
        let grow = step(ScaleLevel::Graph, "g1", DevelopmentalOp::Grow, outcome(1), outcome(2));
        anchor_step(&mut ledger, &grow, 10);
        let mut lineage = LineageGraph::new();
        lineage.insert(grow);

        let audit = audit_development(&lineage, &ledger);
        assert!(audit.grew_or_repaired);
        assert!(audit.anchored);
        assert!(audit.motivated);
        assert!(audit.passes());
    }

    #[test]
    fn audit_fails_when_step_is_not_anchored() {
        let ledger = ScaleLedger::new("graph", SigningKey::from_bytes(&[4u8; 32]));
        let mut lineage = LineageGraph::new();
        lineage.insert(step(ScaleLevel::Graph, "g1", DevelopmentalOp::Repair, outcome(1), outcome(2)));
        let audit = audit_development(&lineage, &ledger); // nothing anchored
        assert!(audit.grew_or_repaired);
        assert!(!audit.anchored);
        assert!(!audit.passes());
    }

    #[test]
    fn audit_fails_without_a_real_motivating_outcome() {
        let mut ledger = ScaleLedger::new("graph", SigningKey::from_bytes(&[4u8; 32]));
        // motivating_outcome is the empty digest → not a real outcome.
        let grow = step(ScaleLevel::Graph, "g1", DevelopmentalOp::Grow, [0u8; 32], outcome(2));
        anchor_step(&mut ledger, &grow, 10);
        let mut lineage = LineageGraph::new();
        lineage.insert(grow);
        assert!(!audit_development(&lineage, &ledger).motivated);
    }

    #[test]
    fn differentiate_alone_is_not_a_reshaping_signal() {
        let mut ledger = ScaleLedger::new("graph", SigningKey::from_bytes(&[4u8; 32]));
        let diff = step(ScaleLevel::Graph, "d1", DevelopmentalOp::Differentiate, outcome(1), outcome(2));
        anchor_step(&mut ledger, &diff, 10);
        let mut lineage = LineageGraph::new();
        lineage.insert(diff);
        assert!(!audit_development(&lineage, &ledger).grew_or_repaired);
    }

    #[test]
    fn step_is_anchored_detects_a_tampered_commitment() {
        let mut ledger = ScaleLedger::new("node", SigningKey::from_bytes(&[3u8; 32]));
        let s = step(ScaleLevel::Node, "s", DevelopmentalOp::Repair, outcome(2), outcome(3));
        anchor_step(&mut ledger, &s, 1_000);
        assert!(step_is_anchored(&ledger, &s));
        // A step with any changed field has a different commitment → not anchored.
        let mut other = s.clone();
        other.produced_outcome = outcome(9);
        assert!(!step_is_anchored(&ledger, &other));
    }

    #[test]
    fn anchored_steps_are_tamper_evident_on_the_chain() {
        let mut ledger = ScaleLedger::new("node", SigningKey::from_bytes(&[3u8; 32]));
        let s = step(ScaleLevel::Node, "node-repair", DevelopmentalOp::Repair, outcome(2), outcome(3));
        anchor_step(&mut ledger, &s, 1_000);
        ledger.verify().expect("anchored lineage verifies");
        assert_eq!(ledger.blocks()[0].receipts.len(), 2);
        assert_eq!(
            ledger.blocks()[0].receipts[0].kind,
            ReceiptKind::DevelopmentalStep
        );
        assert_eq!(ledger.blocks()[0].receipts[1].kind, ReceiptKind::Lineage);
    }
}
