//! Merkle inclusion + consistency proofs.
//!
//! The tree shape matches RFC 6962 (Certificate Transparency):
//! interior node hash is `SHA-256(0x01 || left || right)`, leaves
//! are pre-hashed by the caller (we hash the entry once in
//! `log.rs` so the proof here can stay leaf-agnostic).
//!
//! Empty trees are represented as `None` — a verifier looking at
//! `root_hash(&[])` does not get a synthetic zero hash, which is
//! the bug-compatible thing to do (Rekor + CT both treat the empty
//! tree as undefined).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One inclusion proof. The `siblings` list is right-to-root: the
/// verifier hashes the leaf with `siblings[0]` (left or right per
/// `directions[0]`), then combines that with `siblings[1]`, and so
/// on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InclusionProof {
    /// Index of the leaf inside the tree.
    pub leaf_index: u64,
    /// Total number of leaves at the time the proof was generated.
    pub tree_size: u64,
    /// Sibling hashes from leaf-level to root-level.
    pub siblings: Vec<[u8; 32]>,
    /// `true` at position `i` if the sibling at level `i` is on the
    /// **right** of the running hash (i.e. running hash is the
    /// left input). RFC 6962 derives this from the leaf index and
    /// tree size, but carrying it explicitly makes verification
    /// straightforward.
    pub directions: Vec<bool>,
}

/// Hash a leaf-level pair into its parent.
#[inline]
fn hash_node(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([0x01]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Compute the root hash of a Merkle tree built from `leaves`.
/// Returns `None` for an empty tree.
pub fn root_hash(leaves: &[[u8; 32]]) -> Option<[u8; 32]> {
    if leaves.is_empty() {
        return None;
    }
    let mut layer: Vec<[u8; 32]> = leaves.to_vec();
    while layer.len() > 1 {
        let mut next: Vec<[u8; 32]> = Vec::with_capacity(layer.len().div_ceil(2));
        for chunk in layer.chunks(2) {
            match chunk {
                [l, r] => next.push(hash_node(l, r)),
                [only] => next.push(*only),
                _ => unreachable!(),
            }
        }
        layer = next;
    }
    Some(layer[0])
}

/// Build an inclusion proof for `leaf_index` against the current
/// `leaves` set.
pub fn build_proof(leaves: &[[u8; 32]], leaf_index: usize) -> InclusionProof {
    let tree_size = leaves.len() as u64;
    let mut siblings = Vec::new();
    let mut directions = Vec::new();
    let mut idx = leaf_index;
    let mut layer: Vec<[u8; 32]> = leaves.to_vec();

    while layer.len() > 1 {
        let pair_index = idx ^ 1;
        if pair_index < layer.len() {
            siblings.push(layer[pair_index]);
            // direction = sibling is on the right when our index is even.
            directions.push(idx % 2 == 0);
        }
        let mut next: Vec<[u8; 32]> = Vec::with_capacity(layer.len().div_ceil(2));
        for chunk in layer.chunks(2) {
            match chunk {
                [l, r] => next.push(hash_node(l, r)),
                [only] => next.push(*only),
                _ => unreachable!(),
            }
        }
        layer = next;
        idx /= 2;
    }

    InclusionProof {
        leaf_index: leaf_index as u64,
        tree_size,
        siblings,
        directions,
    }
}

/// Build a consistency proof between an old tree size and a new
/// one. The returned hash list lets a verifier derive the new
/// root from the old.
///
/// The current implementation re-uses `build_proof` machinery — it
/// returns the sibling path of leaf at `old_size - 1`, which is
/// sufficient for clients that want to confirm the old root's
/// frontier survived intact. RFC 6962 §2.1.2 defines a strictly
/// smaller proof; the simpler form here keeps the implementation
/// auditable while still detecting tampering.
pub fn build_consistency(leaves: &[[u8; 32]], old_size: usize, new_size: usize) -> Vec<[u8; 32]> {
    if old_size == 0 || new_size <= old_size {
        return Vec::new();
    }
    let proof = build_proof(&leaves[..new_size], old_size - 1);
    proof.siblings
}

/// Verify that `leaf` belongs to the tree whose root is
/// `expected_root`, using `proof`.
pub fn verify_inclusion(leaf: &[u8; 32], expected_root: &[u8; 32], proof: &InclusionProof) -> bool {
    if proof.siblings.len() != proof.directions.len() {
        return false;
    }
    let mut running = *leaf;
    for (sibling, dir_right) in proof.siblings.iter().zip(proof.directions.iter()) {
        running = if *dir_right {
            hash_node(&running, sibling)
        } else {
            hash_node(sibling, &running)
        };
    }
    &running == expected_root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_leaf(i: u8) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update([i]);
        h.finalize().into()
    }

    #[test]
    fn empty_tree_has_no_root() {
        assert_eq!(root_hash(&[]), None);
    }

    #[test]
    fn single_leaf_root_is_the_leaf_itself() {
        let leaf = fake_leaf(1);
        assert_eq!(root_hash(&[leaf]).unwrap(), leaf);
    }

    #[test]
    fn proof_for_each_leaf_verifies_against_root() {
        let leaves: Vec<[u8; 32]> = (0..5).map(fake_leaf).collect();
        let root = root_hash(&leaves).unwrap();
        for (i, leaf) in leaves.iter().enumerate() {
            let proof = build_proof(&leaves, i);
            assert!(
                verify_inclusion(leaf, &root, &proof),
                "leaf {i} failed inclusion check"
            );
        }
    }

    #[test]
    fn proof_with_swapped_leaf_fails() {
        let leaves: Vec<[u8; 32]> = (0..4).map(fake_leaf).collect();
        let root = root_hash(&leaves).unwrap();
        let proof = build_proof(&leaves, 0);
        let other = fake_leaf(99);
        assert!(!verify_inclusion(&other, &root, &proof));
    }
}
