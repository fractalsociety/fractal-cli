//! Receipts are the anchored facts of the Fractal Society: the small,
//! hash-committed records that later fold upward into the chain. Each receipt
//! binds a *kind* (what happened) to a *subject* (which node/graph it concerns)
//! and a *payload hash* (the keccak/sha of the real artifact — an evidence root,
//! a verifier verdict, a route/policy decision, a lineage or developmental step).
//!
//! A receipt never contains the artifact itself, only its digest, so the ledger
//! stays small while remaining a faithful, tamper-evident index of the truth.

use crate::merkle::{keccak256, Hash256};

/// The class of fact a receipt anchors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptKind {
    /// A node/graph's replayable evidence root.
    EvidenceRoot,
    /// A `fractal-verify` verdict (pass/fail + floor).
    VerifierVerdict,
    /// A `fractal-route` provider/expansion decision.
    RouteDecision,
    /// A policy/capability decision hash.
    PolicyDecision,
    /// A lineage link (this artifact derives from that outcome).
    Lineage,
    /// A morphogenesis step (grow / differentiate / repair).
    DevelopmentalStep,
}

impl ReceiptKind {
    /// Stable on-wire tag used in the receipt commitment. Never reorder or
    /// renumber — the tag is part of the hashed pre-image.
    pub fn tag(self) -> u8 {
        match self {
            Self::EvidenceRoot => 1,
            Self::VerifierVerdict => 2,
            Self::RouteDecision => 3,
            Self::PolicyDecision => 4,
            Self::Lineage => 5,
            Self::DevelopmentalStep => 6,
        }
    }
}

/// One anchored fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// What kind of fact this is.
    pub kind: ReceiptKind,
    /// The node/graph/work identifier this fact concerns.
    pub subject: String,
    /// keccak/sha digest of the real artifact this receipt anchors.
    pub payload_hash: Hash256,
    /// Wall-clock time the fact was recorded (ms since epoch).
    pub timestamp_ms: u64,
}

impl Receipt {
    /// Build a receipt anchoring `payload_hash` for `subject`.
    pub fn new(
        kind: ReceiptKind,
        subject: impl Into<String>,
        payload_hash: Hash256,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            kind,
            subject: subject.into(),
            payload_hash,
            timestamp_ms,
        }
    }

    /// Deterministic keccak commitment used as this receipt's Merkle leaf.
    ///
    /// The pre-image is a length-prefixed, fixed-width encoding so two receipts
    /// commit equal iff every field is equal — no ambiguous concatenation.
    pub fn commitment(&self) -> Hash256 {
        let subject = self.subject.as_bytes();
        let mut pre = Vec::with_capacity(1 + 8 + subject.len() + 32 + 8);
        pre.push(self.kind.tag());
        pre.extend_from_slice(&(subject.len() as u64).to_be_bytes());
        pre.extend_from_slice(subject);
        pre.extend_from_slice(&self.payload_hash);
        pre.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        keccak256(&pre)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Receipt {
        Receipt::new(
            ReceiptKind::EvidenceRoot,
            "graph:abc#node:patch",
            [9u8; 32],
            1_700,
        )
    }

    #[test]
    fn commitment_is_deterministic() {
        assert_eq!(sample().commitment(), sample().commitment());
    }

    #[test]
    fn commitment_changes_with_every_field() {
        let base = sample();
        let mut kind = base.clone();
        kind.kind = ReceiptKind::VerifierVerdict;
        let mut subject = base.clone();
        subject.subject.push('!');
        let mut payload = base.clone();
        payload.payload_hash[0] ^= 1;
        let mut ts = base.clone();
        ts.timestamp_ms += 1;
        for other in [kind, subject, payload, ts] {
            assert_ne!(base.commitment(), other.commitment());
        }
    }

    #[test]
    fn length_prefix_prevents_field_boundary_collisions() {
        // "ab"+payload vs "a"+shifted must not collide by concatenation.
        let left = Receipt::new(ReceiptKind::Lineage, "ab", [0u8; 32], 0);
        let right = Receipt::new(ReceiptKind::Lineage, "a", [0u8; 32], 0);
        assert_ne!(left.commitment(), right.commitment());
    }
}
