//! Immutable-boundary attestation (pipeline P7.6).
//!
//! The immutable boundary is itself attested on-chain: a verifying
//! [`ScaleLedger`] seals a [`ReceiptKind::PolicyDecision`] whose payload is the
//! canonical commitment over the protected target set. The Fractal chain sits
//! **inside** that boundary (`fractal-chain` is an immutable target), so no
//! morphogen, worker, or evolution step may rewrite the ledger or the boundary
//! attestation itself.

use crate::ledger::{Block, ScaleLedger};
use crate::merkle::{keccak256, Hash256};
use crate::receipt::{Receipt, ReceiptKind};

/// Subject id used for the on-chain boundary attestation receipt.
pub const BOUNDARY_ATTESTATION_SUBJECT: &str = "immutable-boundary";

/// Protected targets that morphogens / workers / evolution must never rewrite.
///
/// Mirrors `fractal_evolution::IMMUTABLE_TARGETS` and adds chain-local entries
/// so the ledger and its attestation sit inside the boundary.
pub const IMMUTABLE_BOUNDARY_TARGETS: &[&str] = &[
    "trust-policy",
    "signatures",
    "redaction",
    "hidden-verifiers",
    "lease-validation",
    "evidence-hashing",
    "runtime-binaries",
    "policy-engine",
    // Chain spine — inside the boundary (P7.6).
    "fractal-chain",
    "chain-attestation",
    "immutable-boundary",
];

/// Failures from boundary attestation / rewrite refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BoundaryAttestationError {
    /// Proposed mutation names a protected target.
    #[error("mutation targets immutable boundary {target:?}")]
    ImmutableTarget {
        /// Protected target name.
        target: String,
    },
    /// Proposed mutation names an unclassified target (deny-by-default).
    #[error("mutation target {target:?} is not explicitly mutable")]
    UnclassifiedTarget {
        /// Unknown target name.
        target: String,
    },
    /// Ledger failed verification before attestation checks.
    #[error("ledger invalid: {0}")]
    LedgerInvalid(crate::ledger::ChainError),
    /// No boundary attestation receipt is present (or payload mismatches).
    #[error("immutable boundary is not attested on-chain")]
    NotAttested,
    /// Attestation payload does not match the canonical boundary commitment.
    #[error("on-chain boundary attestation does not match canonical commitment")]
    AttestationMismatch {
        /// Expected canonical commitment.
        expected: Hash256,
        /// Payload sealed on-chain.
        actual: Hash256,
    },
}

/// Explicitly mutable targets (parity with `fractal_evolution::MUTABLE_TARGETS`).
const MUTABLE_TARGETS: &[&str] = &[
    "harness-topology",
    "route-candidates",
    "retry-policy",
    "memory-packaging",
    "resource-estimates",
    "capability-requirements",
    "model-candidates",
    "warm-cache-policy",
    "verifier-ordering",
];

/// Canonical keccak commitment over the sorted immutable boundary target set.
#[must_use]
pub fn boundary_commitment() -> Hash256 {
    let mut targets: Vec<&str> = IMMUTABLE_BOUNDARY_TARGETS.to_vec();
    targets.sort_unstable();
    let mut preimage = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        if index > 0 {
            preimage.push(b'|');
        }
        preimage.extend_from_slice(target.as_bytes());
    }
    keccak256(&preimage)
}

/// True when `target` is listed in the immutable boundary (incl. chain spine).
#[must_use]
pub fn is_immutable_boundary_target(target: &str) -> bool {
    let normalized = target.trim();
    if IMMUTABLE_BOUNDARY_TARGETS.contains(&normalized) {
        return true;
    }
    // Family prefixes — specific spellings cannot evade the boundary.
    IMMUTABLE_BOUNDARY_TARGETS.iter().any(|protected| {
        normalized.starts_with(&format!("{protected}-"))
            || normalized.starts_with(&format!("{protected}/"))
    })
}

/// Refuse morphogen/worker/evolution mutations that touch the boundary or chain.
///
/// # Errors
///
/// Returns [`BoundaryAttestationError`] for immutable or unclassified targets.
pub fn assert_mutation_outside_boundary(
    targets: &[String],
) -> Result<(), BoundaryAttestationError> {
    for target in targets {
        if is_immutable_boundary_target(target) {
            return Err(BoundaryAttestationError::ImmutableTarget {
                target: target.clone(),
            });
        }
        if !MUTABLE_TARGETS.contains(&target.as_str()) {
            return Err(BoundaryAttestationError::UnclassifiedTarget {
                target: target.clone(),
            });
        }
    }
    Ok(())
}

/// Seal the canonical immutable-boundary commitment into `ledger`.
///
/// # Errors
///
/// Currently infallible given a live ledger; reserved for future checks.
pub fn attest_boundary(
    ledger: &mut ScaleLedger,
    timestamp_ms: u64,
) -> Result<&Block, BoundaryAttestationError> {
    let receipt = Receipt::new(
        ReceiptKind::PolicyDecision,
        BOUNDARY_ATTESTATION_SUBJECT,
        boundary_commitment(),
        timestamp_ms,
    );
    Ok(ledger.append(vec![receipt], timestamp_ms))
}

/// Prove the ledger verifies and carries a matching boundary attestation.
///
/// # Errors
///
/// Returns [`BoundaryAttestationError`] when the ledger is invalid, the
/// attestation is missing, or its payload does not match [`boundary_commitment`].
pub fn assert_boundary_attested(ledger: &ScaleLedger) -> Result<(), BoundaryAttestationError> {
    ledger
        .verify()
        .map_err(BoundaryAttestationError::LedgerInvalid)?;
    let expected = boundary_commitment();
    let mut found = None;
    for block in ledger.blocks() {
        for receipt in &block.receipts {
            if receipt.kind == ReceiptKind::PolicyDecision
                && receipt.subject == BOUNDARY_ATTESTATION_SUBJECT
            {
                found = Some(receipt.payload_hash);
            }
        }
    }
    match found {
        None => Err(BoundaryAttestationError::NotAttested),
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(BoundaryAttestationError::AttestationMismatch { expected, actual }),
    }
}

/// True when the Fractal chain identity is listed inside the immutable boundary.
#[must_use]
pub fn chain_sits_inside_boundary() -> bool {
    is_immutable_boundary_target("fractal-chain")
        && is_immutable_boundary_target("chain-attestation")
        && is_immutable_boundary_target("immutable-boundary")
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::anchor::{anchor_promotion, payload_hash_str};

    fn ledger() -> ScaleLedger {
        ScaleLedger::new("society", SigningKey::from_bytes(&[17u8; 32]))
    }

    #[test]
    fn boundary_commitment_is_stable_and_order_independent() {
        let first = boundary_commitment();
        let second = boundary_commitment();
        assert_eq!(first, second);
        assert_ne!(first, [0u8; 32]);
    }

    #[test]
    fn attest_and_assert_boundary_on_chain() {
        let mut chain = ledger();
        assert!(matches!(
            assert_boundary_attested(&chain),
            Err(BoundaryAttestationError::NotAttested)
        ));
        attest_boundary(&mut chain, 100).expect("attest");
        assert_boundary_attested(&chain).expect("attested");
        assert!(chain_sits_inside_boundary());
    }

    #[test]
    fn refuses_chain_and_boundary_rewrites() {
        for target in [
            "fractal-chain",
            "chain-attestation",
            "immutable-boundary",
            "signatures",
            "evidence-hashing",
            "trust-policy-extra",
        ] {
            let err = assert_mutation_outside_boundary(&[target.to_owned()]).expect_err(target);
            assert!(matches!(
                err,
                BoundaryAttestationError::ImmutableTarget { .. }
            ));
        }
        assert_mutation_outside_boundary(&[
            "route-candidates".to_owned(),
            "retry-policy".to_owned(),
        ])
        .expect("mutable ok");
        let err = assert_mutation_outside_boundary(&["mystery-knob".to_owned()]).expect_err("unk");
        assert!(matches!(
            err,
            BoundaryAttestationError::UnclassifiedTarget { .. }
        ));
    }

    #[test]
    fn mismatched_attestation_payload_is_detected() {
        let mut chain = ledger();
        // Seal a wrong policy decision under the boundary subject.
        anchor_promotion(
            &mut chain,
            BOUNDARY_ATTESTATION_SUBJECT,
            payload_hash_str("not-the-boundary"),
            50,
        )
        .expect("wrong promo");
        let err = assert_boundary_attested(&chain).expect_err("mismatch");
        assert!(matches!(
            err,
            BoundaryAttestationError::AttestationMismatch { .. }
        ));
    }
}
