//! Tamper detection across folded scales (pipeline P7.4).
//!
//! A modified receipt or root at scale N either fails local chain verification
//! or changes that scale's [`ScaleLedger::head`]. After an upward fold (P7.3),
//! the parent at N+1 still anchors the **pre-tamper** head as a lineage
//! receipt — so [`detect_fold_tamper`] / [`scan_spine_tamper`] report a mismatch
//! detectable at the parent without trusting the child.

use crate::fold::{verify_child_anchored, FoldError, ScaleLevel, ScaleSpine};
use crate::ledger::{verify_blocks, Block, ChainError, ScaleLedger};
use crate::merkle::Hash256;
use crate::receipt::ReceiptKind;

/// How tampering manifests on a parent←child fold edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TamperKind {
    /// The child ledger no longer verifies (receipt/root/signature broken).
    ChildChainInvalid(ChainError),
    /// The parent ledger no longer verifies.
    ParentChainInvalid(ChainError),
    /// Parent has no lineage receipt for this child scale.
    MissingAnchor,
    /// Parent anchors a head that differs from the child's current head.
    HeadMismatch {
        /// Head hash sealed into the parent at fold time (latest matching subject).
        anchored_head: Hash256,
        /// Child's current head after tampering or further appends.
        actual_head: Hash256,
    },
}

/// One detected tamper on a fold edge (child scale N, parent N+1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TamperFinding {
    /// Scale whose history was altered or drifted.
    pub child_scale: ScaleLevel,
    /// Parent scale that still holds the prior anchored head.
    pub parent_scale: ScaleLevel,
    /// Concrete mismatch class.
    pub kind: TamperKind,
}

impl std::fmt::Display for TamperFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tamper at {}→{}: {:?}",
            self.child_scale.as_str(),
            self.parent_scale.as_str(),
            self.kind
        )
    }
}

impl std::error::Error for TamperFinding {}

/// Latest lineage payload the parent recorded for `child_scale`, if any.
#[must_use]
pub fn latest_anchored_head(parent: &ScaleLedger, child_scale: &str) -> Option<Hash256> {
    parent
        .blocks()
        .iter()
        .flat_map(|block| block.receipts.iter())
        .filter(|receipt| receipt.kind == ReceiptKind::Lineage && receipt.subject == child_scale)
        .map(|receipt| receipt.payload_hash)
        .next_back()
}

/// Detect tampering on one fold edge (child scale N vs parent N+1).
///
/// Returns `Ok(())` when the child verifies, the parent verifies, and the
/// parent still anchors the child's current head.
///
/// # Errors
///
/// Returns [`TamperFinding`] describing the first detected mismatch.
pub fn detect_fold_tamper(parent: &ScaleLedger, child: &ScaleLedger) -> Result<(), TamperFinding> {
    let child_scale = ScaleLevel::parse(child.scale()).ok_or(TamperFinding {
        child_scale: ScaleLevel::Node,
        parent_scale: ScaleLevel::Graph,
        kind: TamperKind::MissingAnchor,
    })?;
    let parent_scale = child_scale.parent().ok_or(TamperFinding {
        child_scale,
        parent_scale: ScaleLevel::Society,
        kind: TamperKind::MissingAnchor,
    })?;

    if let Err(error) = child.verify() {
        return Err(TamperFinding {
            child_scale,
            parent_scale,
            kind: TamperKind::ChildChainInvalid(error),
        });
    }
    if let Err(error) = parent.verify() {
        return Err(TamperFinding {
            child_scale,
            parent_scale,
            kind: TamperKind::ParentChainInvalid(error),
        });
    }

    let actual_head = child.head();
    match latest_anchored_head(parent, child.scale()) {
        None => Err(TamperFinding {
            child_scale,
            parent_scale,
            kind: TamperKind::MissingAnchor,
        }),
        Some(anchored_head) if anchored_head == actual_head => Ok(()),
        Some(anchored_head) => Err(TamperFinding {
            child_scale,
            parent_scale,
            kind: TamperKind::HeadMismatch {
                anchored_head,
                actual_head,
            },
        }),
    }
}

/// Scan every fold edge in a spine; return all findings (empty = clean).
#[must_use]
pub fn scan_spine_tamper(spine: &ScaleSpine) -> Vec<TamperFinding> {
    let mut findings = Vec::new();
    for child_level in [
        ScaleLevel::Node,
        ScaleLevel::Graph,
        ScaleLevel::Machine,
        ScaleLevel::Network,
    ] {
        let parent_level = child_level.parent().expect("has parent");
        if let Err(finding) =
            detect_fold_tamper(spine.ledger(parent_level), spine.ledger(child_level))
        {
            findings.push(finding);
        }
    }
    findings
}

/// Prove a block list itself was locally tampered (receipt/root mismatch).
///
/// Used to show that rewriting a receipt at scale N fails closed before the
/// parent edge is even consulted.
///
/// # Errors
///
/// Returns the [`ChainError`] from [`verify_blocks`].
pub fn detect_local_block_tamper(blocks: &[Block]) -> Result<(), ChainError> {
    verify_blocks(blocks)
}

/// Convenience: fold-edge check that mirrors [`verify_child_anchored`] but
/// returns structured [`TamperFinding`] values.
///
/// # Errors
///
/// Returns [`TamperFinding`] when the edge is broken.
pub fn assert_fold_untampered(
    parent: &ScaleLedger,
    child: &ScaleLedger,
) -> Result<(), TamperFinding> {
    match verify_child_anchored(parent, child) {
        Ok(()) => Ok(()),
        Err(FoldError::ChildInvalid(error)) => {
            let child_scale = ScaleLevel::parse(child.scale()).unwrap_or(ScaleLevel::Node);
            let parent_scale = child_scale.parent().unwrap_or(ScaleLevel::Graph);
            Err(TamperFinding {
                child_scale,
                parent_scale,
                kind: TamperKind::ChildChainInvalid(error),
            })
        }
        Err(FoldError::ParentInvalid(error)) => {
            let child_scale = ScaleLevel::parse(child.scale()).unwrap_or(ScaleLevel::Node);
            let parent_scale = child_scale.parent().unwrap_or(ScaleLevel::Graph);
            Err(TamperFinding {
                child_scale,
                parent_scale,
                kind: TamperKind::ParentChainInvalid(error),
            })
        }
        Err(FoldError::ChildNotAnchored { .. }) => detect_fold_tamper(parent, child),
        Err(_) => detect_fold_tamper(parent, child),
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::anchor::{commit_anchors, payload_hash_str, AnchorEvent};
    use crate::fold::{fold_child_into_parent, ScaleSpine};

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn keys() -> [SigningKey; 5] {
        [key(1), key(2), key(3), key(4), key(5)]
    }

    fn seed(ledger: &mut ScaleLedger, subject: &str, payload: &str, ts: u64) {
        commit_anchors(
            ledger,
            vec![AnchorEvent::NodeExecution {
                subject: subject.into(),
                evidence_root: payload_hash_str(payload),
            }],
            ts,
        )
        .expect("seed");
    }

    #[test]
    fn clean_fold_edge_has_no_tamper() {
        let mut child = ScaleLedger::new("node", key(1));
        seed(&mut child, "node:a", "ev1", 10);
        let mut parent = ScaleLedger::new("graph", key(2));
        fold_child_into_parent(&mut parent, &child, 20).expect("fold");
        detect_fold_tamper(&parent, &child).expect("clean");
        assert_fold_untampered(&parent, &child).expect("untampered");
    }

    #[test]
    fn modified_child_receipt_fails_local_verify() {
        let mut child = ScaleLedger::new("node", key(1));
        seed(&mut child, "node:a", "ev1", 10);
        let mut blocks = child.blocks().to_vec();
        blocks[0].receipts[0].payload_hash[0] ^= 0xff;
        let err = detect_local_block_tamper(&blocks).expect_err("tampered");
        assert!(matches!(err, ChainError::RootMismatch { .. }));
    }

    #[test]
    fn child_head_change_after_fold_is_detectable_at_parent() {
        let mut child = ScaleLedger::new("node", key(1));
        seed(&mut child, "node:a", "ev1", 10);
        let mut parent = ScaleLedger::new("graph", key(2));
        fold_child_into_parent(&mut parent, &child, 20).expect("fold");
        let anchored = latest_anchored_head(&parent, "node").expect("anchor");
        assert_eq!(anchored, child.head());

        // Append more history at the child — head moves; parent still holds old head.
        seed(&mut child, "node:a", "ev2-tamper", 30);
        let finding = detect_fold_tamper(&parent, &child).expect_err("detect");
        assert_eq!(finding.child_scale, ScaleLevel::Node);
        assert_eq!(finding.parent_scale, ScaleLevel::Graph);
        match finding.kind {
            TamperKind::HeadMismatch {
                anchored_head,
                actual_head,
            } => {
                assert_eq!(anchored_head, anchored);
                assert_eq!(actual_head, child.head());
                assert_ne!(anchored_head, actual_head);
            }
            other => panic!("expected HeadMismatch, got {other:?}"),
        }
    }

    #[test]
    fn spine_scan_reports_tampered_edge() {
        let mut spine = ScaleSpine::new(keys()).expect("spine");
        seed(spine.ledger_mut(ScaleLevel::Node), "node:a", "ev", 1);
        seed(spine.ledger_mut(ScaleLevel::Graph), "graph:a", "g", 2);
        seed(spine.ledger_mut(ScaleLevel::Machine), "machine:a", "m", 3);
        seed(spine.ledger_mut(ScaleLevel::Network), "network:a", "n", 4);
        spine.fold_all_upward(10).expect("fold");
        assert!(scan_spine_tamper(&spine).is_empty());

        // Drift the node scale after fold.
        seed(spine.ledger_mut(ScaleLevel::Node), "node:a", "drift", 99);
        let findings = scan_spine_tamper(&spine);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].child_scale, ScaleLevel::Node);
        assert!(matches!(findings[0].kind, TamperKind::HeadMismatch { .. }));
    }
}
