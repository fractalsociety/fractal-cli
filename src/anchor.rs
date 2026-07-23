//! Receipt anchoring for pipeline events (pipeline P7.2).
//!
//! Every node execution, verifier verdict, promotion, and route/policy decision
//! commits a signed [`Receipt`] into a [`ScaleLedger`] block. Callers supply
//! content-addressed payload digests (never raw private bodies); the ledger
//! seals them under the scale's ed25519 key so later honesty gates can require
//! a chain-committed fact.

use crate::ledger::{Block, ScaleLedger};
use crate::merkle::{keccak256, Hash256};
use crate::receipt::{Receipt, ReceiptKind};

/// A typed fact awaiting commit to the scale chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorEvent {
    /// Node (or graph) execution completed with a replayable evidence root.
    NodeExecution {
        /// Node / work subject id.
        subject: String,
        /// Digest of the evidence root artifact.
        evidence_root: Hash256,
    },
    /// Independent verifier verdict for a subject.
    VerifierVerdict {
        /// Node / work subject id.
        subject: String,
        /// Digest of the verdict artifact.
        verdict_hash: Hash256,
    },
    /// Router provider / expansion decision.
    RouteDecision {
        /// Node / work subject id.
        subject: String,
        /// Digest of the route decision artifact.
        decision_hash: Hash256,
    },
    /// Promotion-authority decision (eligible / rejected / inconclusive).
    Promotion {
        /// Candidate / graph subject id.
        subject: String,
        /// Digest of the promotion decision artifact.
        decision_hash: Hash256,
    },
    /// Lineage link (artifact derives from motivating outcome).
    Lineage {
        /// Candidate / step subject id.
        subject: String,
        /// Digest of the lineage record.
        lineage_hash: Hash256,
    },
    /// Morphogenesis developmental step.
    DevelopmentalStep {
        /// Morphogen / graph subject id.
        subject: String,
        /// Digest of the developmental step record.
        step_hash: Hash256,
    },
}

impl AnchorEvent {
    fn into_receipt(self, timestamp_ms: u64) -> Receipt {
        match self {
            Self::NodeExecution {
                subject,
                evidence_root,
            } => Receipt::new(
                ReceiptKind::EvidenceRoot,
                subject,
                evidence_root,
                timestamp_ms,
            ),
            Self::VerifierVerdict {
                subject,
                verdict_hash,
            } => Receipt::new(
                ReceiptKind::VerifierVerdict,
                subject,
                verdict_hash,
                timestamp_ms,
            ),
            Self::RouteDecision {
                subject,
                decision_hash,
            } => Receipt::new(
                ReceiptKind::RouteDecision,
                subject,
                decision_hash,
                timestamp_ms,
            ),
            Self::Promotion {
                subject,
                decision_hash,
            } => Receipt::new(
                ReceiptKind::PolicyDecision,
                subject,
                decision_hash,
                timestamp_ms,
            ),
            Self::Lineage {
                subject,
                lineage_hash,
            } => Receipt::new(ReceiptKind::Lineage, subject, lineage_hash, timestamp_ms),
            Self::DevelopmentalStep { subject, step_hash } => Receipt::new(
                ReceiptKind::DevelopmentalStep,
                subject,
                step_hash,
                timestamp_ms,
            ),
        }
    }
}

/// Failures from the anchoring helpers.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AnchorError {
    /// No events were supplied for the block.
    #[error("anchor commit requires at least one event")]
    EmptyBatch,
    /// A subject id was empty.
    #[error("anchor subject must be non-empty")]
    EmptySubject,
}

/// keccak256 digest of arbitrary bytes (payload pre-image for a receipt).
#[must_use]
pub fn payload_hash(bytes: &[u8]) -> Hash256 {
    keccak256(bytes)
}

/// keccak256 digest of a UTF-8 string payload.
#[must_use]
pub fn payload_hash_str(value: &str) -> Hash256 {
    payload_hash(value.as_bytes())
}

/// Commit one or more pipeline events as a single signed ledger block.
///
/// # Errors
///
/// Returns [`AnchorError::EmptyBatch`] when `events` is empty, or
/// [`AnchorError::EmptySubject`] when any event has an empty subject.
pub fn commit_anchors(
    ledger: &mut ScaleLedger,
    events: Vec<AnchorEvent>,
    timestamp_ms: u64,
) -> Result<&Block, AnchorError> {
    if events.is_empty() {
        return Err(AnchorError::EmptyBatch);
    }
    for event in &events {
        let subject = match event {
            AnchorEvent::NodeExecution { subject, .. }
            | AnchorEvent::VerifierVerdict { subject, .. }
            | AnchorEvent::RouteDecision { subject, .. }
            | AnchorEvent::Promotion { subject, .. }
            | AnchorEvent::Lineage { subject, .. }
            | AnchorEvent::DevelopmentalStep { subject, .. } => subject,
        };
        if subject.trim().is_empty() {
            return Err(AnchorError::EmptySubject);
        }
    }
    let receipts: Vec<Receipt> = events
        .into_iter()
        .map(|event| event.into_receipt(timestamp_ms))
        .collect();
    Ok(ledger.append(receipts, timestamp_ms))
}

/// Convenience: commit a single node-execution evidence root.
///
/// # Errors
///
/// Propagates [`AnchorError`] from [`commit_anchors`].
pub fn anchor_node_execution(
    ledger: &mut ScaleLedger,
    subject: impl Into<String>,
    evidence_root: Hash256,
    timestamp_ms: u64,
) -> Result<&Block, AnchorError> {
    commit_anchors(
        ledger,
        vec![AnchorEvent::NodeExecution {
            subject: subject.into(),
            evidence_root,
        }],
        timestamp_ms,
    )
}

/// Convenience: commit a verifier verdict.
///
/// # Errors
///
/// Propagates [`AnchorError`] from [`commit_anchors`].
pub fn anchor_verifier_verdict(
    ledger: &mut ScaleLedger,
    subject: impl Into<String>,
    verdict_hash: Hash256,
    timestamp_ms: u64,
) -> Result<&Block, AnchorError> {
    commit_anchors(
        ledger,
        vec![AnchorEvent::VerifierVerdict {
            subject: subject.into(),
            verdict_hash,
        }],
        timestamp_ms,
    )
}

/// Convenience: commit a route decision.
///
/// # Errors
///
/// Propagates [`AnchorError`] from [`commit_anchors`].
pub fn anchor_route_decision(
    ledger: &mut ScaleLedger,
    subject: impl Into<String>,
    decision_hash: Hash256,
    timestamp_ms: u64,
) -> Result<&Block, AnchorError> {
    commit_anchors(
        ledger,
        vec![AnchorEvent::RouteDecision {
            subject: subject.into(),
            decision_hash,
        }],
        timestamp_ms,
    )
}

/// Convenience: commit a promotion-authority decision.
///
/// # Errors
///
/// Propagates [`AnchorError`] from [`commit_anchors`].
pub fn anchor_promotion(
    ledger: &mut ScaleLedger,
    subject: impl Into<String>,
    decision_hash: Hash256,
    timestamp_ms: u64,
) -> Result<&Block, AnchorError> {
    commit_anchors(
        ledger,
        vec![AnchorEvent::Promotion {
            subject: subject.into(),
            decision_hash,
        }],
        timestamp_ms,
    )
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::receipt::ReceiptKind;

    fn ledger() -> ScaleLedger {
        ScaleLedger::new("graph", SigningKey::from_bytes(&[9u8; 32]))
    }

    #[test]
    fn commits_execution_verdict_route_and_promotion() {
        let mut ledger = ledger();
        let block = commit_anchors(
            &mut ledger,
            vec![
                AnchorEvent::NodeExecution {
                    subject: "node:patch".into(),
                    evidence_root: payload_hash_str("evidence-root-v1"),
                },
                AnchorEvent::VerifierVerdict {
                    subject: "node:patch".into(),
                    verdict_hash: payload_hash_str("verdict-pass"),
                },
                AnchorEvent::RouteDecision {
                    subject: "node:patch".into(),
                    decision_hash: payload_hash_str("route:cursor"),
                },
                AnchorEvent::Promotion {
                    subject: "candidate:grow.1".into(),
                    decision_hash: payload_hash_str("ELIGIBLE"),
                },
            ],
            1_000,
        )
        .expect("commit");

        assert_eq!(block.receipt_count, 4);
        assert_eq!(block.receipts[0].kind, ReceiptKind::EvidenceRoot);
        assert_eq!(block.receipts[1].kind, ReceiptKind::VerifierVerdict);
        assert_eq!(block.receipts[2].kind, ReceiptKind::RouteDecision);
        assert_eq!(block.receipts[3].kind, ReceiptKind::PolicyDecision);
        ledger.verify().expect("chain verifies");
    }

    #[test]
    fn convenience_helpers_append_signed_blocks() {
        let mut ledger = ledger();
        anchor_node_execution(
            &mut ledger,
            "node:a",
            payload_hash(b"ev"),
            10,
        )
        .expect("exec");
        anchor_verifier_verdict(&mut ledger, "node:a", payload_hash(b"ok"), 11).expect("verdict");
        anchor_route_decision(&mut ledger, "node:a", payload_hash(b"route"), 12).expect("route");
        anchor_promotion(
            &mut ledger,
            "cand:1",
            payload_hash(b"ELIGIBLE"),
            13,
        )
        .expect("promo");
        assert_eq!(ledger.blocks().len(), 4);
        ledger.verify().expect("verify");
        // Heads advance (hash-linked).
        assert_ne!(ledger.head(), crate::ledger::GENESIS_PREV);
    }

    #[test]
    fn rejects_empty_batch_and_empty_subject() {
        let mut ledger = ledger();
        assert_eq!(
            commit_anchors(&mut ledger, vec![], 1).unwrap_err(),
            AnchorError::EmptyBatch
        );
        assert_eq!(
            anchor_node_execution(&mut ledger, "  ", payload_hash(b"x"), 1).unwrap_err(),
            AnchorError::EmptySubject
        );
    }

    #[test]
    fn tampered_receipt_fails_verify() {
        let mut ledger = ledger();
        commit_anchors(
            &mut ledger,
            vec![AnchorEvent::DevelopmentalStep {
                subject: "morphogen:grow".into(),
                step_hash: payload_hash_str("step"),
            }],
            50,
        )
        .expect("commit");
        let mut blocks = ledger.blocks().to_vec();
        blocks[0].receipts[0].payload_hash[0] ^= 0xff;
        assert!(matches!(
            crate::ledger::verify_blocks(&blocks),
            Err(crate::ledger::ChainError::RootMismatch { .. })
        ));
    }
}
