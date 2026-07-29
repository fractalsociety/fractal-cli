//! Honesty gate for worker/model claims (pipeline P7.5).
//!
//! A claim such as "node passed", "graph promoted", or "capability trusted" is
//! accepted **only** when the scale ledger verifies (ed25519 signatures +
//! recomputable receipt roots) **and** it contains a receipt whose kind/subject
//! match the claim and whose `payload_hash` equals the claimed evidence digest.
//! A model cannot fake the record: inventing a payload hash that was never
//! sealed into a signed block fails closed.

use crate::ledger::{ChainError, ScaleLedger};
use crate::merkle::Hash256;
use crate::receipt::ReceiptKind;

/// Classes of claim a worker or model may assert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// "This node passed verification."
    NodePassed,
    /// "This graph / candidate was promoted."
    GraphPromoted,
    /// "This capability cell is trusted for online use."
    CapabilityTrusted,
}

impl ClaimKind {
    /// Receipt kind that must back this claim on-chain.
    #[must_use]
    pub const fn required_receipt_kind(self) -> ReceiptKind {
        match self {
            Self::NodePassed => ReceiptKind::VerifierVerdict,
            Self::GraphPromoted | Self::CapabilityTrusted => ReceiptKind::PolicyDecision,
        }
    }
}

/// A claim presented for honesty gating (never trusted on self-report alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// What is being asserted.
    pub kind: ClaimKind,
    /// Node / graph / capability subject id.
    pub subject: String,
    /// Digest of the real evidence the claim refers to.
    pub evidence_hash: Hash256,
}

impl Claim {
    /// Build a claim over `subject` with an evidence digest.
    #[must_use]
    pub fn new(kind: ClaimKind, subject: impl Into<String>, evidence_hash: Hash256) -> Self {
        Self {
            kind,
            subject: subject.into(),
            evidence_hash,
        }
    }
}

/// Why a claim was rejected by the honesty gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HonestyRejectReason {
    /// Empty subject is refuse-by-default.
    EmptySubject,
    /// The ledger failed local verification (bad signature / root / link).
    LedgerInvalid(ChainError),
    /// No receipt of the required kind exists for this subject.
    NoMatchingReceipt {
        /// Required receipt kind.
        required_kind: ReceiptKind,
        /// Claim subject.
        subject: String,
    },
    /// A receipt exists for the subject/kind, but its payload is not the claimed evidence.
    EvidenceHashMismatch {
        /// Digest asserted by the claim.
        claimed: Hash256,
        /// Digest sealed in the matching receipt (latest for subject+kind).
        committed: Hash256,
    },
}

impl std::fmt::Display for HonestyRejectReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySubject => formatter.write_str("claim subject must be non-empty"),
            Self::LedgerInvalid(error) => write!(formatter, "ledger invalid: {error}"),
            Self::NoMatchingReceipt {
                required_kind,
                subject,
            } => write!(
                formatter,
                "no {required_kind:?} receipt for subject {subject:?}"
            ),
            Self::EvidenceHashMismatch { .. } => formatter.write_str(
                "claimed evidence hash does not match the chain-committed receipt payload",
            ),
        }
    }
}

impl std::error::Error for HonestyRejectReason {}

/// Proof that a claim was accepted against a verified chain commitment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedClaim {
    /// Claim that passed the gate.
    pub claim: Claim,
    /// Block index that sealed the matching receipt.
    pub block_index: u64,
    /// Commitment leaf of the matching receipt.
    pub receipt_commitment: Hash256,
    /// Public key bytes of the scale signer that sealed the block.
    pub signer: [u8; 32],
}

/// Honesty-gate outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HonestyVerdict {
    /// Claim is backed by a signed, verifying chain receipt over the evidence.
    Accepted(AcceptedClaim),
    /// Claim is refused; the model cannot override this.
    Rejected(HonestyRejectReason),
}

impl HonestyVerdict {
    /// True when the claim was accepted.
    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }
}

/// Evaluate a claim against a scale ledger (fail closed).
///
/// Steps: refuse empty subjects → verify the ledger → locate the latest receipt
/// of the required kind for the subject → require `payload_hash == evidence_hash`.
#[must_use]
pub fn evaluate_claim(ledger: &ScaleLedger, claim: &Claim) -> HonestyVerdict {
    if claim.subject.trim().is_empty() {
        return HonestyVerdict::Rejected(HonestyRejectReason::EmptySubject);
    }
    if let Err(error) = ledger.verify() {
        return HonestyVerdict::Rejected(HonestyRejectReason::LedgerInvalid(error));
    }

    let required_kind = claim.kind.required_receipt_kind();
    let mut matched: Option<(u64, Hash256, Hash256, [u8; 32])> = None;
    for block in ledger.blocks() {
        for receipt in &block.receipts {
            if receipt.kind == required_kind && receipt.subject == claim.subject {
                matched = Some((
                    block.index,
                    receipt.commitment(),
                    receipt.payload_hash,
                    block.signer,
                ));
            }
        }
    }

    let Some((block_index, receipt_commitment, committed, signer)) = matched else {
        return HonestyVerdict::Rejected(HonestyRejectReason::NoMatchingReceipt {
            required_kind,
            subject: claim.subject.clone(),
        });
    };

    if committed != claim.evidence_hash {
        return HonestyVerdict::Rejected(HonestyRejectReason::EvidenceHashMismatch {
            claimed: claim.evidence_hash,
            committed,
        });
    }

    HonestyVerdict::Accepted(AcceptedClaim {
        claim: claim.clone(),
        block_index,
        receipt_commitment,
        signer,
    })
}

/// Accept a claim or return the rejection reason.
///
/// # Errors
///
/// Returns [`HonestyRejectReason`] when the claim is not chain-backed.
pub fn accept_claim(
    ledger: &ScaleLedger,
    claim: &Claim,
) -> Result<AcceptedClaim, HonestyRejectReason> {
    match evaluate_claim(ledger, claim) {
        HonestyVerdict::Accepted(accepted) => Ok(accepted),
        HonestyVerdict::Rejected(reason) => Err(reason),
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::anchor::{
        anchor_node_execution, anchor_promotion, anchor_verifier_verdict, payload_hash_str,
    };

    fn ledger() -> ScaleLedger {
        ScaleLedger::new("graph", SigningKey::from_bytes(&[11u8; 32]))
    }

    #[test]
    fn accepts_node_passed_when_verdict_receipt_matches() {
        let mut ledger = ledger();
        let evidence = payload_hash_str("verdict:pass");
        anchor_verifier_verdict(&mut ledger, "node:patch", evidence, 10).expect("anchor");

        let claim = Claim::new(ClaimKind::NodePassed, "node:patch", evidence);
        let accepted = accept_claim(&ledger, &claim).expect("accept");
        assert_eq!(accepted.block_index, 0);
        assert_eq!(accepted.signer, ledger.signer());
        assert!(evaluate_claim(&ledger, &claim).is_accepted());
    }

    #[test]
    fn accepts_promotion_and_capability_claims_on_policy_receipts() {
        let mut ledger = ledger();
        let promo = payload_hash_str("ELIGIBLE");
        let trust = payload_hash_str("capability:trusted");
        anchor_promotion(&mut ledger, "candidate:grow.1", promo, 20).expect("promo");
        anchor_promotion(&mut ledger, "capability:retrieve", trust, 21).expect("trust");

        accept_claim(
            &ledger,
            &Claim::new(ClaimKind::GraphPromoted, "candidate:grow.1", promo),
        )
        .expect("promoted");
        accept_claim(
            &ledger,
            &Claim::new(ClaimKind::CapabilityTrusted, "capability:retrieve", trust),
        )
        .expect("trusted");
    }

    #[test]
    fn rejects_uncommitted_or_mismatched_evidence() {
        let mut chain = ledger();
        let real = payload_hash_str("verdict:pass");
        anchor_verifier_verdict(&mut chain, "node:patch", real, 10).expect("anchor");

        // Model invents a different evidence digest.
        let fake = Claim::new(
            ClaimKind::NodePassed,
            "node:patch",
            payload_hash_str("verdict:i-swear"),
        );
        let err = accept_claim(&chain, &fake).expect_err("mismatch");
        assert!(matches!(
            err,
            HonestyRejectReason::EvidenceHashMismatch { .. }
        ));

        // No receipt for this subject.
        let missing = Claim::new(ClaimKind::NodePassed, "node:other", real);
        let err = accept_claim(&chain, &missing).expect_err("missing");
        assert!(matches!(err, HonestyRejectReason::NoMatchingReceipt { .. }));

        // Evidence-only commitment is not enough for NodePassed (needs verdict).
        let mut bare = ledger();
        anchor_node_execution(&mut bare, "node:x", payload_hash_str("ev"), 1).expect("ev");
        let err = accept_claim(
            &bare,
            &Claim::new(ClaimKind::NodePassed, "node:x", payload_hash_str("ev")),
        )
        .expect_err("wrong kind");
        assert!(matches!(err, HonestyRejectReason::NoMatchingReceipt { .. }));
    }

    #[test]
    fn rejects_when_ledger_signature_is_forged() {
        let mut ledger = ledger();
        let evidence = payload_hash_str("verdict:pass");
        anchor_verifier_verdict(&mut ledger, "node:patch", evidence, 10).expect("anchor");

        let mut blocks = ledger.blocks().to_vec();
        blocks[0].signature[0] ^= 0xff;
        ledger.replace_blocks_for_test(blocks);

        let err = accept_claim(
            &ledger,
            &Claim::new(ClaimKind::NodePassed, "node:patch", evidence),
        )
        .expect_err("forged");
        assert!(matches!(err, HonestyRejectReason::LedgerInvalid(_)));

        let err = accept_claim(
            &ScaleLedger::new("graph", SigningKey::from_bytes(&[11u8; 32])),
            &Claim::new(ClaimKind::NodePassed, "  ", evidence),
        )
        .expect_err("empty");
        assert_eq!(err, HonestyRejectReason::EmptySubject);
    }
}
