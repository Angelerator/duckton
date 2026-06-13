//! BLAKE3 Merkle tree for per-epoch receipt batching (BLOCKCHAIN_ECONOMICS §7.2):
//! batch small signed records, anchor one root, prove inclusion with a ~1 KB proof.

use crate::types::{Hash32, InclusionProof};

/// Hash two child nodes into their parent (domain-separated).
pub fn hash_node(left: &Hash32, right: &Hash32) -> Hash32 {
    let mut h = blake3::Hasher::new();
    h.update(b"duckdb-p2p-merkle-node\x01");
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

/// Build a Merkle tree over `leaves` and return all levels (level 0 = leaves).
/// Odd nodes are promoted by duplication. An empty input yields the zero root.
fn levels(leaves: &[Hash32]) -> Vec<Vec<Hash32>> {
    if leaves.is_empty() {
        return vec![vec![[0u8; 32]]];
    }
    let mut all = vec![leaves.to_vec()];
    while all.last().unwrap().len() > 1 {
        let cur = all.last().unwrap();
        let mut next = Vec::with_capacity(cur.len().div_ceil(2));
        let mut i = 0;
        while i < cur.len() {
            let l = cur[i];
            let r = if i + 1 < cur.len() { cur[i + 1] } else { cur[i] };
            next.push(hash_node(&l, &r));
            i += 2;
        }
        all.push(next);
    }
    all
}

/// Compute the Merkle root of `leaves`.
pub fn merkle_root(leaves: &[Hash32]) -> Hash32 {
    *levels(leaves).last().unwrap().first().unwrap()
}

/// Build an inclusion proof for `index` within `leaves`.
pub fn build_proof(leaves: &[Hash32], index: usize) -> Option<InclusionProof> {
    if index >= leaves.len() {
        return None;
    }
    let all = levels(leaves);
    let mut siblings = Vec::new();
    let mut idx = index;
    for level in &all[..all.len() - 1] {
        let (sib_idx, sib_is_left) = if idx % 2 == 0 {
            // current is the left child; sibling is on the right (duplicate if odd)
            (if idx + 1 < level.len() { idx + 1 } else { idx }, false)
        } else {
            (idx - 1, true)
        };
        siblings.push((sib_is_left, level[sib_idx]));
        idx /= 2;
    }
    Some(InclusionProof { leaf: leaves[index], siblings })
}

/// Verify an inclusion proof against an anchored `root`.
pub fn verify_inclusion(root: &Hash32, proof: &InclusionProof) -> bool {
    let mut acc = proof.leaf;
    for (sib_is_left, sib) in &proof.siblings {
        acc = if *sib_is_left {
            hash_node(sib, &acc)
        } else {
            hash_node(&acc, sib)
        };
    }
    &acc == root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(n: u8) -> Hash32 {
        *blake3::hash(&[n]).as_bytes()
    }

    #[test]
    fn root_and_proof_roundtrip() {
        let leaves: Vec<Hash32> = (0..5).map(leaf).collect();
        let root = merkle_root(&leaves);
        for i in 0..leaves.len() {
            let proof = build_proof(&leaves, i).unwrap();
            assert!(verify_inclusion(&root, &proof), "leaf {i} should verify");
        }
    }

    #[test]
    fn tampered_proof_fails() {
        let leaves: Vec<Hash32> = (0..4).map(leaf).collect();
        let root = merkle_root(&leaves);
        let mut proof = build_proof(&leaves, 1).unwrap();
        proof.leaf = leaf(99); // wrong leaf
        assert!(!verify_inclusion(&root, &proof));
    }

    #[test]
    fn single_leaf_tree() {
        let leaves = vec![leaf(7)];
        let root = merkle_root(&leaves);
        let proof = build_proof(&leaves, 0).unwrap();
        assert!(verify_inclusion(&root, &proof));
        assert_eq!(root, leaf(7));
    }
}
