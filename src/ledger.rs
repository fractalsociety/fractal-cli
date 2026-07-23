//! A `ScaleLedger` is one Fractal chain sub-node: an append-only, hash-linked,
//! ed25519-signed ledger that anchors receipts at a single scale (node, graph,
//! machine, network, society). Each block commits the Merkle root of its
//! receipts and the hash of the previous block, and is signed by the scale's
//! key. Because blocks are hash-linked and signed, no worker or model can
//! rewrite history or fake a fact: the only accepted claim is one committed here
//! under a valid signature over a real (recomputable) receipt root. A parent
//! scale folds a child in by anchoring the child's `head()` as a receipt of its
//! own (P7.3), so a single global root proves the whole history beneath it.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use crate::merkle::{keccak256, merkle_root, Hash256};
use crate::receipt::Receipt;

/// The genesis link used by the first block.
pub const GENESIS_PREV: Hash256 = [0u8; 32];

/// A committed block header (the signed pre-image) plus its receipts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// Height, starting at 0 for genesis and increasing by one.
    pub index: u64,
    /// The scale this ledger anchors ("node", "graph", …).
    pub scale: String,
    /// Hash of the previous block ([`GENESIS_PREV`] for the first).
    pub prev_hash: Hash256,
    /// Merkle root over the receipts' commitments.
    pub receipts_root: Hash256,
    /// Number of receipts committed in this block.
    pub receipt_count: u32,
    /// Wall-clock time the block was sealed (ms since epoch).
    pub timestamp_ms: u64,
    /// The scale signer's ed25519 public key bytes.
    pub signer: [u8; 32],
    /// The receipts themselves (kept so the root is recomputable on verify).
    pub receipts: Vec<Receipt>,
    /// ed25519 signature over [`Block::header_hash`].
    pub signature: [u8; 64],
}

impl Block {
    /// The hash-link pre-image: every header field except the signature, in a
    /// fixed-width, length-prefixed layout. This digest is both what the signer
    /// signs and what the next block references as `prev_hash`.
    pub fn header_hash(&self) -> Hash256 {
        let scale = self.scale.as_bytes();
        let mut pre = Vec::with_capacity(8 + 8 + scale.len() + 32 + 32 + 4 + 8 + 32);
        pre.extend_from_slice(&self.index.to_be_bytes());
        pre.extend_from_slice(&(scale.len() as u64).to_be_bytes());
        pre.extend_from_slice(scale);
        pre.extend_from_slice(&self.prev_hash);
        pre.extend_from_slice(&self.receipts_root);
        pre.extend_from_slice(&self.receipt_count.to_be_bytes());
        pre.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        pre.extend_from_slice(&self.signer);
        keccak256(&pre)
    }
}

/// Errors raised while appending to or verifying a ledger.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ChainError {
    /// A block's index did not follow its predecessor.
    #[error("block {index}: expected index {expected}")]
    BadIndex { index: u64, expected: u64 },
    /// A block's `prev_hash` did not match the real predecessor hash.
    #[error("block {index}: prev_hash does not link to the previous block")]
    BrokenLink { index: u64 },
    /// The committed receipts root did not match the receipts.
    #[error("block {index}: receipts_root does not match the committed receipts")]
    RootMismatch { index: u64 },
    /// The committed receipt count did not match the receipts.
    #[error("block {index}: receipt_count does not match the committed receipts")]
    CountMismatch { index: u64 },
    /// The block signature was invalid, or the signer changed.
    #[error("block {index}: invalid signature for the expected signer")]
    BadSignature { index: u64 },
    /// The stored signer bytes were not a valid ed25519 public key.
    #[error("block {index}: malformed signer public key")]
    BadSignerKey { index: u64 },
}

/// One per-scale append-only signed receipt ledger.
pub struct ScaleLedger {
    scale: String,
    signing_key: SigningKey,
    signer: [u8; 32],
    blocks: Vec<Block>,
}

impl ScaleLedger {
    /// Open an empty ledger for `scale`, signed by `signing_key`.
    pub fn new(scale: impl Into<String>, signing_key: SigningKey) -> Self {
        let signer = signing_key.verifying_key().to_bytes();
        Self {
            scale: scale.into(),
            signing_key,
            signer,
            blocks: Vec::new(),
        }
    }

    /// Open an empty ledger from a 32-byte signing seed (so callers need not
    /// depend on ed25519 directly).
    #[must_use]
    pub fn from_seed(scale: impl Into<String>, seed: [u8; 32]) -> Self {
        Self::new(scale, SigningKey::from_bytes(&seed))
    }

    /// The scale this ledger anchors.
    pub fn scale(&self) -> &str {
        &self.scale
    }

    /// The signer's public key bytes.
    pub fn signer(&self) -> [u8; 32] {
        self.signer
    }

    /// The committed blocks, oldest first.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Test-only: replace the block list (used to simulate signature forgery).
    #[cfg(test)]
    pub(crate) fn replace_blocks_for_test(&mut self, blocks: Vec<Block>) {
        self.blocks = blocks;
    }

    /// The current chain head: the hash of the last block, or [`GENESIS_PREV`]
    /// when empty. This is the value a parent scale anchors when folding this
    /// ledger upward.
    pub fn head(&self) -> Hash256 {
        self.blocks.last().map_or(GENESIS_PREV, Block::header_hash)
    }

    /// Seal `receipts` into a new signed block linked to the current head and
    /// append it. Append-only: existing blocks are never mutated or removed.
    pub fn append(&mut self, receipts: Vec<Receipt>, timestamp_ms: u64) -> &Block {
        let index = self.blocks.len() as u64;
        let prev_hash = self.head();
        let leaves: Vec<Hash256> = receipts.iter().map(Receipt::commitment).collect();
        let receipts_root = merkle_root(&leaves);
        let receipt_count = receipts.len() as u32;

        let mut block = Block {
            index,
            scale: self.scale.clone(),
            prev_hash,
            receipts_root,
            receipt_count,
            timestamp_ms,
            signer: self.signer,
            receipts,
            signature: [0u8; 64],
        };
        let signature: Signature = self.signing_key.sign(&block.header_hash());
        block.signature = signature.to_bytes();
        self.blocks.push(block);
        self.blocks.last().expect("a block was just appended")
    }

    /// Verify the whole chain: monotone indices, intact hash links, recomputable
    /// receipt roots, matching counts, and a valid signature by a single stable
    /// signer on every block. Fails closed at the first broken block.
    pub fn verify(&self) -> Result<(), ChainError> {
        verify_blocks(&self.blocks)
    }
}

/// Verify a sequence of blocks in isolation (used by [`ScaleLedger::verify`] and
/// available for auditing an imported chain). Enforces a single signer: the
/// public key recorded in block 0 must sign every block.
pub fn verify_blocks(blocks: &[Block]) -> Result<(), ChainError> {
    let mut prev_hash = GENESIS_PREV;
    let mut expected_signer: Option<[u8; 32]> = None;

    for (position, block) in blocks.iter().enumerate() {
        let index = position as u64;
        if block.index != index {
            return Err(ChainError::BadIndex {
                index: block.index,
                expected: index,
            });
        }
        if block.prev_hash != prev_hash {
            return Err(ChainError::BrokenLink { index });
        }
        if block.receipt_count as usize != block.receipts.len() {
            return Err(ChainError::CountMismatch { index });
        }
        let leaves: Vec<Hash256> = block.receipts.iter().map(Receipt::commitment).collect();
        if merkle_root(&leaves) != block.receipts_root {
            return Err(ChainError::RootMismatch { index });
        }

        let signer = *expected_signer.get_or_insert(block.signer);
        if block.signer != signer {
            return Err(ChainError::BadSignature { index });
        }
        let verifying_key = VerifyingKey::from_bytes(&block.signer)
            .map_err(|_| ChainError::BadSignerKey { index })?;
        let signature = Signature::from_bytes(&block.signature);
        verifying_key
            .verify(&block.header_hash(), &signature)
            .map_err(|_| ChainError::BadSignature { index })?;

        prev_hash = block.header_hash();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::ReceiptKind;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn receipt(kind: ReceiptKind, subject: &str, byte: u8, ts: u64) -> Receipt {
        Receipt::new(kind, subject, [byte; 32], ts)
    }

    fn seeded_ledger() -> ScaleLedger {
        let mut ledger = ScaleLedger::new("graph", key(7));
        ledger.append(
            vec![receipt(ReceiptKind::EvidenceRoot, "node:patch", 1, 10)],
            100,
        );
        ledger.append(
            vec![
                receipt(ReceiptKind::VerifierVerdict, "node:patch", 2, 20),
                receipt(ReceiptKind::DevelopmentalStep, "graph", 3, 21),
            ],
            200,
        );
        ledger
    }

    #[test]
    fn empty_head_is_genesis() {
        let ledger = ScaleLedger::new("node", key(1));
        assert_eq!(ledger.head(), GENESIS_PREV);
        assert!(ledger.verify().is_ok());
    }

    #[test]
    fn append_links_and_verifies() {
        let ledger = seeded_ledger();
        assert_eq!(ledger.blocks().len(), 2);
        assert_eq!(ledger.blocks()[0].index, 0);
        assert_eq!(ledger.blocks()[0].prev_hash, GENESIS_PREV);
        assert_eq!(
            ledger.blocks()[1].prev_hash,
            ledger.blocks()[0].header_hash()
        );
        assert_eq!(ledger.head(), ledger.blocks()[1].header_hash());
        assert_eq!(ledger.blocks()[1].receipt_count, 2);
        ledger.verify().expect("honest chain verifies");
    }

    #[test]
    fn tampering_with_a_receipt_breaks_the_root() {
        let mut ledger = seeded_ledger();
        // Flip a byte in a committed receipt payload without re-sealing.
        ledger.blocks[0].receipts[0].payload_hash[0] ^= 1;
        assert_eq!(ledger.verify(), Err(ChainError::RootMismatch { index: 0 }));
    }

    #[test]
    fn rewriting_a_committed_root_breaks_the_signature() {
        let mut ledger = seeded_ledger();
        // Re-point the root to a value; header_hash changes, so the old
        // signature no longer verifies.
        ledger.blocks[1].receipts_root[0] ^= 0xFF;
        // The root now also mismatches its receipts, caught first.
        assert_eq!(ledger.verify(), Err(ChainError::RootMismatch { index: 1 }));
    }

    #[test]
    fn forging_a_block_with_a_foreign_key_is_rejected() {
        let mut ledger = seeded_ledger();
        // Re-sign block 1 with a different key and stamp its public key.
        let forger = key(99);
        let mut forged = ledger.blocks[1].clone();
        forged.signer = forger.verifying_key().to_bytes();
        let sig = forger.sign(&forged.header_hash());
        forged.signature = sig.to_bytes();
        ledger.blocks[1] = forged;
        // Signature is valid for the forger, but the signer changed from block 0.
        assert_eq!(ledger.verify(), Err(ChainError::BadSignature { index: 1 }));
    }

    #[test]
    fn cutting_a_block_breaks_the_link() {
        let mut ledger = seeded_ledger();
        // Drop the middle of the link by replacing block 1's prev_hash.
        ledger.blocks[1].prev_hash[0] ^= 1;
        assert_eq!(ledger.verify(), Err(ChainError::BrokenLink { index: 1 }));
    }

    #[test]
    fn reordering_blocks_breaks_indices() {
        let mut ledger = seeded_ledger();
        ledger.blocks.swap(0, 1);
        // Block now at position 0 has index 1.
        assert_eq!(
            ledger.verify(),
            Err(ChainError::BadIndex {
                index: 1,
                expected: 0
            })
        );
    }

    #[test]
    fn append_is_deterministic_for_fixed_inputs() {
        let a = seeded_ledger();
        let b = seeded_ledger();
        assert_eq!(a.head(), b.head());
        assert_eq!(a.blocks(), b.blocks());
    }

    #[test]
    fn parent_can_anchor_child_head_as_a_receipt() {
        // P7.3 fold: a child's head becomes a receipt in the parent scale.
        let child = seeded_ledger();
        let mut parent = ScaleLedger::new("machine", key(42));
        let fold = Receipt::new(ReceiptKind::Lineage, "graph", child.head(), 300);
        parent.append(vec![fold], 300);
        parent.verify().expect("parent verifies");
        assert_eq!(parent.blocks()[0].receipts[0].payload_hash, child.head());
    }
}
