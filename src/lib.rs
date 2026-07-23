//! # fractal-chain
//!
//! The Fractal Society honesty anchor (P7.1): a per-scale, append-only,
//! hash-linked, ed25519-signed receipt ledger. Each scale (node, graph,
//! machine, network, society) runs one [`ScaleLedger`] that commits the Merkle
//! root of its [`Receipt`]s and the hash of the previous block, signed by the
//! scale's key. History cannot be rewritten (blocks are hash-linked) and facts
//! cannot be faked (only signature-valid, root-recomputable claims verify), so
//! the ledger keeps both the system and the models running on it honest.
//!
//! A parent scale folds a child upward by anchoring the child's [`ScaleLedger::head`]
//! as one of its own receipts (P7.3), so a single global root proves the entire
//! history beneath it. The Merkle math is byte-compatible with FractalChain's
//! `fractal-core::merkle`, so those roots anchor unchanged.
//!
//! P7.2 adds [`anchor`] — typed helpers that commit node-execution, verifier,
//! route, and promotion events as signed receipts on a scale ledger.
//! P7.3 adds [`fold`] — upward Merkle fold (`node→graph→machine→network→society`)
//! so the society's head is a global root over every scale beneath it.
//! P7.4 adds [`tamper`] — detect receipt/root changes at scale N via mismatch
//! with the head anchored at parent scale N+1.
//! P7.5 adds [`honesty`] — accept worker/model claims only when chain-committed
//! under a verifying ed25519 signature over the claimed evidence digest.
//! P7.6 adds [`boundary`] — attest the immutable boundary on-chain; the chain
//! sits inside that boundary and cannot be rewritten by morphogens/evolution.
//! P6.6 adds [`lineage`] — cross-scale developmental lineage: each grow/
//! differentiate/repair step links its motivating outcome to what it produced,
//! anchored on-chain and traversable `node ↔ graph ↔ network`.
//! P5.5 adds [`outcome`] — a replayable evidence root plus a consent-gated,
//! sanitized outcome export (deny-by-default, `Private` fields redacted) bound
//! to that root, for handing a run's result to DataEvol.

mod anchor;
mod boundary;
mod fold;
mod honesty;
mod ledger;
mod lineage;
mod merkle;
mod outcome;
mod receipt;
mod tamper;

pub use anchor::{
    AnchorError, AnchorEvent, anchor_node_execution, anchor_promotion, anchor_route_decision,
    anchor_verifier_verdict, commit_anchors, payload_hash, payload_hash_str,
};
pub use boundary::{
    BOUNDARY_ATTESTATION_SUBJECT, BoundaryAttestationError, IMMUTABLE_BOUNDARY_TARGETS,
    assert_boundary_attested, assert_mutation_outside_boundary, attest_boundary,
    boundary_commitment, chain_sits_inside_boundary, is_immutable_boundary_target,
};
pub use fold::{
    FoldError, ScaleLevel, ScaleSpine, fold_child_into_parent, verify_child_anchored,
};
pub use honesty::{
    AcceptedClaim, Claim, ClaimKind, HonestyRejectReason, HonestyVerdict, accept_claim,
    evaluate_claim,
};
pub use ledger::{Block, ChainError, ScaleLedger, verify_blocks, GENESIS_PREV};
pub use lineage::{
    DevelopmentAudit, DevelopmentalOp, DevelopmentalStep, LineageGraph, anchor_step,
    audit_development, step_is_anchored,
};
pub use merkle::{keccak256, merkle_root, Hash256};
pub use outcome::{
    Consent, EvidenceEntry, ExportError, OutcomeField, SanitizedExport, Sensitivity,
    replayable_evidence_root, sanitized_export, verify_replay,
};
pub use receipt::{Receipt, ReceiptKind};
pub use tamper::{
    TamperFinding, TamperKind, assert_fold_untampered, detect_fold_tamper,
    detect_local_block_tamper, latest_anchored_head, scan_spine_tamper,
};
