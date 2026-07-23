//! Upward Merkle fold across scales (pipeline P7.3).
//!
//! Each scale commits its chain [`ScaleLedger::head`] into the parent scale as a
//! [`ReceiptKind::Lineage`] receipt (subject = child scale name, payload = child
//! head). The tower is `node → graph → machine → network → society`. The
//! society's head is the **global root** — a proof over every scale beneath it.
//! Tampering at scale N changes the child head so it no longer matches the
//! receipt anchored at N+1 (detectable at the parent; expanded in P7.4).

use ed25519_dalek::SigningKey;

use crate::ledger::{Block, ChainError, ScaleLedger};
use crate::merkle::Hash256;
use crate::receipt::{Receipt, ReceiptKind};

/// Ordered Fractal scales that participate in the upward fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScaleLevel {
    /// Single node / work-unit chain.
    Node,
    /// Execution-graph chain.
    Graph,
    /// Machine / host chain.
    Machine,
    /// Multi-machine network chain.
    Network,
    /// Society / global chain.
    Society,
}

impl ScaleLevel {
    /// Stable lowercase tag used as ledger scale names and fold subjects.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Graph => "graph",
            Self::Machine => "machine",
            Self::Network => "network",
            Self::Society => "society",
        }
    }

    /// Parse a scale tag; unknown tags are rejected.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "node" => Some(Self::Node),
            "graph" => Some(Self::Graph),
            "machine" => Some(Self::Machine),
            "network" => Some(Self::Network),
            "society" => Some(Self::Society),
            _ => None,
        }
    }

    /// Immediate parent in the fold tower (`Society` has none).
    #[must_use]
    pub const fn parent(self) -> Option<Self> {
        match self {
            Self::Node => Some(Self::Graph),
            Self::Graph => Some(Self::Machine),
            Self::Machine => Some(Self::Network),
            Self::Network => Some(Self::Society),
            Self::Society => None,
        }
    }

    /// Immediate child in the fold tower (`Node` has none).
    #[must_use]
    pub const fn child(self) -> Option<Self> {
        match self {
            Self::Society => Some(Self::Network),
            Self::Network => Some(Self::Machine),
            Self::Machine => Some(Self::Graph),
            Self::Graph => Some(Self::Node),
            Self::Node => None,
        }
    }

    /// Bottom-up order used when folding the full tower.
    pub const BOTTOM_UP: [Self; 5] = [
        Self::Node,
        Self::Graph,
        Self::Machine,
        Self::Network,
        Self::Society,
    ];
}

/// Failures from upward fold operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FoldError {
    /// Child scale has no parent (cannot fold society upward).
    #[error("scale {scale} has no parent to fold into")]
    NoParent {
        /// Offending scale tag.
        scale: String,
    },
    /// Parent/child scale tags are not adjacent in the tower.
    #[error("cannot fold child scale {child} into parent scale {parent}")]
    ScaleMismatch {
        /// Child scale tag.
        child: String,
        /// Parent scale tag.
        parent: String,
    },
    /// Child ledger failed verification before fold.
    #[error("child ledger invalid: {0}")]
    ChildInvalid(ChainError),
    /// Parent ledger failed verification.
    #[error("parent ledger invalid: {0}")]
    ParentInvalid(ChainError),
    /// Parent does not contain a lineage receipt for the child's current head.
    #[error("parent does not anchor child scale {child} head")]
    ChildNotAnchored {
        /// Child scale tag.
        child: String,
    },
    /// A spine ledger's `scale()` string does not match its slot.
    #[error("spine ledger scale mismatch: expected {expected}, got {actual}")]
    SpineScaleMismatch {
        /// Expected scale tag.
        expected: String,
        /// Actual ledger scale tag.
        actual: String,
    },
}

/// Fold `child.head()` into `parent` as a signed lineage receipt.
///
/// The receipt subject is the child's scale name; the payload is the child's
/// current head hash. Both ledgers must already verify, and `parent.scale()`
/// must be the immediate parent of `child.scale()` in the tower.
///
/// # Errors
///
/// Returns [`FoldError`] on scale mismatch or invalid child/parent chains.
pub fn fold_child_into_parent<'a>(
    parent: &'a mut ScaleLedger,
    child: &ScaleLedger,
    timestamp_ms: u64,
) -> Result<&'a Block, FoldError> {
    let child_level =
        ScaleLevel::parse(child.scale()).ok_or_else(|| FoldError::ScaleMismatch {
            child: child.scale().to_owned(),
            parent: parent.scale().to_owned(),
        })?;
    let expected_parent = child_level.parent().ok_or_else(|| FoldError::NoParent {
        scale: child.scale().to_owned(),
    })?;
    if parent.scale() != expected_parent.as_str() {
        return Err(FoldError::ScaleMismatch {
            child: child.scale().to_owned(),
            parent: parent.scale().to_owned(),
        });
    }
    child.verify().map_err(FoldError::ChildInvalid)?;
    parent.verify().map_err(FoldError::ParentInvalid)?;

    let receipt = Receipt::new(
        ReceiptKind::Lineage,
        child.scale(),
        child.head(),
        timestamp_ms,
    );
    Ok(parent.append(vec![receipt], timestamp_ms))
}

/// Prove that `parent` currently anchors `child.head()` via a lineage receipt.
///
/// # Errors
///
/// Returns [`FoldError`] when either chain is invalid or the anchor is missing.
pub fn verify_child_anchored(parent: &ScaleLedger, child: &ScaleLedger) -> Result<(), FoldError> {
    child.verify().map_err(FoldError::ChildInvalid)?;
    parent.verify().map_err(FoldError::ParentInvalid)?;
    let expected_head = child.head();
    let child_scale = child.scale();
    let anchored = parent
        .blocks()
        .iter()
        .flat_map(|block| block.receipts.iter())
        .any(|receipt| {
            receipt.kind == ReceiptKind::Lineage
                && receipt.subject == child_scale
                && receipt.payload_hash == expected_head
        });
    if anchored {
        Ok(())
    } else {
        Err(FoldError::ChildNotAnchored {
            child: child_scale.to_owned(),
        })
    }
}

/// Five-scale ledger tower with upward fold helpers.
pub struct ScaleSpine {
    node: ScaleLedger,
    graph: ScaleLedger,
    machine: ScaleLedger,
    network: ScaleLedger,
    society: ScaleLedger,
}

impl ScaleSpine {
    /// Open empty ledgers for every scale, each signed by its own key.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError::SpineScaleMismatch`] if a constructed ledger's scale
    /// tag somehow disagrees with its slot (defensive).
    pub fn new(keys: [SigningKey; 5]) -> Result<Self, FoldError> {
        let spine = Self {
            node: ScaleLedger::new(ScaleLevel::Node.as_str(), keys[0].clone()),
            graph: ScaleLedger::new(ScaleLevel::Graph.as_str(), keys[1].clone()),
            machine: ScaleLedger::new(ScaleLevel::Machine.as_str(), keys[2].clone()),
            network: ScaleLedger::new(ScaleLevel::Network.as_str(), keys[3].clone()),
            society: ScaleLedger::new(ScaleLevel::Society.as_str(), keys[4].clone()),
        };
        spine.assert_scale_tags()?;
        Ok(spine)
    }

    /// Borrow the ledger for one scale.
    #[must_use]
    pub fn ledger(&self, level: ScaleLevel) -> &ScaleLedger {
        match level {
            ScaleLevel::Node => &self.node,
            ScaleLevel::Graph => &self.graph,
            ScaleLevel::Machine => &self.machine,
            ScaleLevel::Network => &self.network,
            ScaleLevel::Society => &self.society,
        }
    }

    /// Mutable borrow of the ledger for one scale.
    pub fn ledger_mut(&mut self, level: ScaleLevel) -> &mut ScaleLedger {
        match level {
            ScaleLevel::Node => &mut self.node,
            ScaleLevel::Graph => &mut self.graph,
            ScaleLevel::Machine => &mut self.machine,
            ScaleLevel::Network => &mut self.network,
            ScaleLevel::Society => &mut self.society,
        }
    }

    /// Society chain head — the global root over the folded tower.
    #[must_use]
    pub fn global_root(&self) -> Hash256 {
        self.society.head()
    }

    /// Fold every child into its parent bottom-up; returns the global root.
    ///
    /// # Errors
    ///
    /// Propagates [`FoldError`] from any failed fold step.
    pub fn fold_all_upward(&mut self, timestamp_ms: u64) -> Result<Hash256, FoldError> {
        // Fold node→graph, graph→machine, machine→network, network→society.
        for level in [
            ScaleLevel::Node,
            ScaleLevel::Graph,
            ScaleLevel::Machine,
            ScaleLevel::Network,
        ] {
            self.fold_level_upward(level, timestamp_ms)?;
        }
        Ok(self.global_root())
    }

    /// Fold one scale into its immediate parent.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError`] on mismatch or verification failure.
    pub fn fold_level_upward(
        &mut self,
        child_level: ScaleLevel,
        timestamp_ms: u64,
    ) -> Result<(), FoldError> {
        let parent_level = child_level.parent().ok_or_else(|| FoldError::NoParent {
            scale: child_level.as_str().to_owned(),
        })?;
        match (parent_level, child_level) {
            (ScaleLevel::Graph, ScaleLevel::Node) => {
                fold_child_into_parent(&mut self.graph, &self.node, timestamp_ms)?;
            }
            (ScaleLevel::Machine, ScaleLevel::Graph) => {
                fold_child_into_parent(&mut self.machine, &self.graph, timestamp_ms)?;
            }
            (ScaleLevel::Network, ScaleLevel::Machine) => {
                fold_child_into_parent(&mut self.network, &self.machine, timestamp_ms)?;
            }
            (ScaleLevel::Society, ScaleLevel::Network) => {
                fold_child_into_parent(&mut self.society, &self.network, timestamp_ms)?;
            }
            _ => {
                return Err(FoldError::ScaleMismatch {
                    child: child_level.as_str().to_owned(),
                    parent: parent_level.as_str().to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Verify every adjacent fold edge in the tower.
    ///
    /// # Errors
    ///
    /// Returns [`FoldError`] on the first broken edge.
    pub fn verify_all_folds(&self) -> Result<(), FoldError> {
        self.assert_scale_tags()?;
        for level in [
            ScaleLevel::Node,
            ScaleLevel::Graph,
            ScaleLevel::Machine,
            ScaleLevel::Network,
        ] {
            let parent_level = level.parent().expect("level has parent");
            verify_child_anchored(self.ledger(parent_level), self.ledger(level))?;
        }
        Ok(())
    }

    fn assert_scale_tags(&self) -> Result<(), FoldError> {
        for level in ScaleLevel::BOTTOM_UP {
            let actual = self.ledger(level).scale();
            if actual != level.as_str() {
                return Err(FoldError::SpineScaleMismatch {
                    expected: level.as_str().to_owned(),
                    actual: actual.to_owned(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::{commit_anchors, payload_hash_str, AnchorEvent};
    use crate::ledger::GENESIS_PREV;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn keys() -> [SigningKey; 5] {
        [key(1), key(2), key(3), key(4), key(5)]
    }

    fn seed_node(ledger: &mut ScaleLedger) {
        commit_anchors(
            ledger,
            vec![AnchorEvent::NodeExecution {
                subject: "node:patch".into(),
                evidence_root: payload_hash_str("ev"),
            }],
            10,
        )
        .expect("seed node");
    }

    #[test]
    fn scale_parent_child_tower() {
        assert_eq!(ScaleLevel::Node.parent(), Some(ScaleLevel::Graph));
        assert_eq!(ScaleLevel::Society.parent(), None);
        assert_eq!(ScaleLevel::Society.child(), Some(ScaleLevel::Network));
        assert_eq!(ScaleLevel::parse("machine"), Some(ScaleLevel::Machine));
        assert_eq!(ScaleLevel::parse("nope"), None);
    }

    #[test]
    fn fold_child_into_parent_anchors_head() {
        let mut child = ScaleLedger::new("node", key(1));
        seed_node(&mut child);
        let mut parent = ScaleLedger::new("graph", key(2));
        fold_child_into_parent(&mut parent, &child, 100).expect("fold");
        verify_child_anchored(&parent, &child).expect("anchored");
        assert_eq!(parent.blocks()[0].receipts[0].payload_hash, child.head());
        assert_eq!(parent.blocks()[0].receipts[0].subject, "node");
        assert_eq!(
            parent.blocks()[0].receipts[0].kind,
            ReceiptKind::Lineage
        );
    }

    #[test]
    fn rejects_non_adjacent_scales() {
        let child = ScaleLedger::new("node", key(1));
        let mut parent = ScaleLedger::new("machine", key(2));
        let err = fold_child_into_parent(&mut parent, &child, 1).expect_err("mismatch");
        assert!(matches!(err, FoldError::ScaleMismatch { .. }));
    }

    #[test]
    fn spine_fold_all_upward_yields_global_root() {
        let mut spine = ScaleSpine::new(keys()).expect("spine");
        seed_node(spine.ledger_mut(ScaleLevel::Node));
        // Seed intermediate scales so they are non-empty before being folded into.
        commit_anchors(
            spine.ledger_mut(ScaleLevel::Graph),
            vec![AnchorEvent::RouteDecision {
                subject: "graph:1".into(),
                decision_hash: payload_hash_str("route"),
            }],
            20,
        )
        .expect("seed graph");
        commit_anchors(
            spine.ledger_mut(ScaleLevel::Machine),
            vec![AnchorEvent::Promotion {
                subject: "machine:1".into(),
                decision_hash: payload_hash_str("promo"),
            }],
            30,
        )
        .expect("seed machine");
        commit_anchors(
            spine.ledger_mut(ScaleLevel::Network),
            vec![AnchorEvent::VerifierVerdict {
                subject: "network:1".into(),
                verdict_hash: payload_hash_str("ok"),
            }],
            40,
        )
        .expect("seed network");

        let root = spine.fold_all_upward(50).expect("fold all");
        assert_ne!(root, GENESIS_PREV);
        assert_eq!(root, spine.global_root());
        spine.verify_all_folds().expect("all folds hold");

        // Each parent block contains the child's head at fold time.
        assert_eq!(
            spine.ledger(ScaleLevel::Graph).blocks().last().unwrap().receipts[0].payload_hash,
            // Note: after fold_all, node head is unchanged (node not mutated).
            spine.ledger(ScaleLevel::Node).head()
        );
    }

    #[test]
    fn tampered_child_head_is_detectable_at_parent() {
        let mut child = ScaleLedger::new("node", key(1));
        seed_node(&mut child);
        let mut parent = ScaleLedger::new("graph", key(2));
        fold_child_into_parent(&mut parent, &child, 100).expect("fold");
        verify_child_anchored(&parent, &child).expect("ok");

        // Mutate child after fold — parent still holds the old head.
        seed_node(&mut child);
        let err = verify_child_anchored(&parent, &child).expect_err("stale");
        assert!(matches!(err, FoldError::ChildNotAnchored { .. }));
    }
}
