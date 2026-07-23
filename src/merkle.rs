//! Keccak binary Merkle tree, byte-compatible with FractalChain's
//! `fractal-core::merkle` (same keccak256 pairing rule and duplicate-last
//! promotion). Keeping the algorithm identical is what lets a scale's receipt
//! root fold upward into a FractalChain anchor unchanged (P7.3).

/// A 32-byte keccak256 digest.
pub type Hash256 = [u8; 32];

/// keccak256 over `bytes` — identical to `fractal_crypto::hash::keccak256`.
pub fn keccak256(bytes: &[u8]) -> Hash256 {
    use sha3::{Digest, Keccak256};
    let mut hasher = Keccak256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn hash_pair(left: &Hash256, right: &Hash256) -> Hash256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left);
    buf[32..].copy_from_slice(right);
    keccak256(&buf)
}

/// Merkle root over ordered leaves (empty → zero hash).
///
/// Odd levels promote the last leaf paired with itself, exactly matching the
/// FractalChain / `fractal-consensus` transaction-root rule.
pub fn merkle_root(leaves: &[Hash256]) -> Hash256 {
    if leaves.is_empty() {
        return [0u8; 32];
    }
    let mut level: Vec<Hash256> = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut index = 0;
        while index < level.len() {
            if index + 1 < level.len() {
                next.push(hash_pair(&level[index], &level[index + 1]));
                index += 2;
            } else {
                next.push(hash_pair(&level[index], &level[index]));
                index += 1;
            }
        }
        level = next;
    }
    level[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_root_is_zero() {
        assert_eq!(merkle_root(&[]), [0u8; 32]);
    }

    #[test]
    fn single_leaf_is_itself() {
        let leaf = keccak256(b"one");
        assert_eq!(merkle_root(&[leaf]), leaf);
    }

    #[test]
    fn odd_leaf_count_duplicates_last() {
        let a = keccak256(b"a");
        let b = keccak256(b"b");
        let c = keccak256(b"c");
        // Three leaves: root = hash_pair(hash_pair(a,b), hash_pair(c,c)).
        let expected = hash_pair(&hash_pair(&a, &b), &hash_pair(&c, &c));
        assert_eq!(merkle_root(&[a, b, c]), expected);
    }

    #[test]
    fn order_sensitive() {
        let a = keccak256(b"a");
        let b = keccak256(b"b");
        assert_ne!(merkle_root(&[a, b]), merkle_root(&[b, a]));
    }
}
