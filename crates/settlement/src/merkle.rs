//! Merkle tree for per-epoch receipt batching (BLOCKCHAIN_ECONOMICS §7.2):
//! batch small signed records, anchor one root, prove inclusion with a ~1 KB proof.
//!
//! ## Hash scheme: TON cell-representation hashing (on-chain aligned)
//!
//! The internal-node hash MUST be identical off-chain (here) and on-chain (the
//! `RecordAnchor` Tolk verifier, `ton/contracts/anchor_types.tolk::hashPair`), or
//! a multi-leaf inclusion proof built off-chain will not reproduce the anchored
//! root inside the contract. The contract computes a parent as:
//!
//! ```tolk
//! beginCell().storeUint(left, 256).storeUint(right, 256).endCell().hash()
//! ```
//!
//! i.e. the TON *cell representation hash* of an ordinary cell with no refs and
//! exactly 512 data bits (`left ‖ right`, each a 256-bit big-endian integer).
//! For such a cell that representation hash reduces to:
//!
//! ```text
//! sha256( d1 ‖ d2 ‖ data ) = sha256( 0x00 ‖ 0x80 ‖ left_be32 ‖ right_be32 )
//! ```
//!
//! where `d1 = 0` (ordinary cell, level 0, 0 refs) and `d2 = 0x80` (512 bits is
//! byte-aligned: `floor(512/8) + ceil(512/8) = 64 + 64 = 128`). The two-byte
//! prefix `00 80` is the cell descriptor; no bit-augmentation is needed because
//! the data is byte-aligned. Reference values for this reduction are pinned in
//! the unit tests against the contract's own `hashPair` output.
//!
//! Leaves are used raw (the contract's verifier folds from the bare `leaf`
//! integer; see `computeRootFromProof`), so a leaf is just its 32-byte
//! [`JobRecord::leaf`](crate::types::JobRecord::leaf) commitment interpreted as a
//! 256-bit big-endian integer — there is no separate leaf-hashing step on either
//! side. Domain separation between the leaf layer and the node layer is provided
//! naturally by the differing hash constructions (BLAKE3 record leaves vs SHA-256
//! cell-hash nodes); the concatenation order (`left ‖ right`) matches `dir`
//! handling in the contract (`dir = 0` ⇒ sibling on the right).

use sha2::{Digest, Sha256};

use crate::types::{Hash32, InclusionProof};

/// Cell descriptor for an ordinary, ref-less cell holding exactly 512 data bits:
/// `d1 = 0`, `d2 = 0x80`. Prepended before `left ‖ right` to reproduce the TON
/// cell representation hash (see module docs).
const PAIR_CELL_DESCRIPTOR: [u8; 2] = [0x00, 0x80];

/// Hash two child nodes into their parent, matching the on-chain `RecordAnchor`
/// verifier's TON cell-representation hash (`hashPair`). See module docs.
pub fn hash_node(left: &Hash32, right: &Hash32) -> Hash32 {
    let mut h = Sha256::new();
    h.update(PAIR_CELL_DESCRIPTOR);
    h.update(left);
    h.update(right);
    h.finalize().into()
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

    /// A 32-byte big-endian leaf from a small integer (matches how the contract
    /// folds a `uint256` leaf).
    fn leaf_u32(n: u32) -> Hash32 {
        let mut h = [0u8; 32];
        h[28..].copy_from_slice(&n.to_be_bytes());
        h
    }

    /// Cross-check the off-chain node hash against the on-chain `hashPair` output
    /// (reference values captured from the `RecordAnchor` contract via
    /// `ton/scripts/_probe_hash.tolk`). If these drift, off-chain proofs will no
    /// longer verify on-chain.
    #[test]
    fn hash_node_matches_onchain_hashpair_reference() {
        // hashPair(0, 0)
        let zero = hash_node(&[0u8; 32], &[0u8; 32]);
        assert_eq!(
            hex::encode(zero),
            "ac165244115ace66658b50e85fb073fb3c02e37f9d6349ed4c6c2b0cc5564c2d"
        );
        // hashPair(0x1111, 0x2222)
        let hp = hash_node(&leaf_u32(0x1111), &leaf_u32(0x2222));
        assert_eq!(
            hex::encode(hp),
            "df96613f475a40cceebaf4bfe1e15dc7ff2cedd5a4c148c600cde200896c5732"
        );
        // ROOT = hashPair(hashPair(0x1111,0x2222), hashPair(0x3333,0x4444))
        let root = merkle_root(&[leaf_u32(0x1111), leaf_u32(0x2222), leaf_u32(0x3333), leaf_u32(0x4444)]);
        assert_eq!(
            hex::encode(root),
            "5995dd67a4a2cf3d48e49b16734f6f94edaf5202e1f6ba8907e439b8086e4b72"
        );
    }

    /// The off-chain proof for a balanced multi-leaf tree must fold to the same
    /// root the on-chain verifier produces. This pins the exact sibling order
    /// (`dir`) the contract walks: for the 4-leaf tree the proof for leaf L0 is
    /// `[L1 (right), hashPair(L2,L3) (right)]`, matching `anchor.test.tolk`.
    #[test]
    fn four_leaf_proof_matches_onchain_layout() {
        let leaves = [leaf_u32(0x1111), leaf_u32(0x2222), leaf_u32(0x3333), leaf_u32(0x4444)];
        let root = merkle_root(&leaves);
        let proof = build_proof(&leaves, 0).unwrap();
        // Both siblings are to the RIGHT (dir = 0 on-chain).
        assert_eq!(proof.siblings.len(), 2);
        assert!(!proof.siblings[0].0, "L1 is the right sibling");
        assert_eq!(proof.siblings[0].1, leaf_u32(0x2222));
        assert!(!proof.siblings[1].0, "hashPair(L2,L3) is the right sibling");
        assert_eq!(proof.siblings[1].1, hash_node(&leaf_u32(0x3333), &leaf_u32(0x4444)));
        assert!(verify_inclusion(&root, &proof));
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

    /// Multi-leaf epoch tree over real `JobRecord` leaves (the case the testnet
    /// e2e never exercised — it only used single-leaf trees where root == leaf).
    /// Every leaf must produce a proof that folds to the anchored root, and a
    /// tampered leaf OR a tampered sibling must be rejected. Covers both even and
    /// odd leaf counts (odd promotes the last node by self-duplication).
    #[test]
    fn multi_leaf_jobrecord_inclusion_and_tamper() {
        use crate::types::JobRecord;

        let record = |i: usize| JobRecord {
            job_id: format!("job-{i}"),
            query_hash: format!("q{i}"),
            requester_wallet: "0:1111111111111111111111111111111111111111111111111111111111111111".into(),
            max_bid: 1_000 + i as u128,
            result_hash: format!("r{i}"),
            epoch: 7,
            prev_root: [9u8; 32],
        };

        for count in [4usize, 5, 6, 7, 8] {
            let leaves: Vec<Hash32> = (0..count).map(|i| record(i).leaf()).collect();
            let root = merkle_root(&leaves);

            for i in 0..count {
                let proof = build_proof(&leaves, i).unwrap();
                assert!(verify_inclusion(&root, &proof), "count={count} leaf {i} must verify");

                // Tamper the leaf: rejected.
                let mut bad_leaf = proof.clone();
                bad_leaf.leaf[0] ^= 0xff;
                assert!(!verify_inclusion(&root, &bad_leaf), "count={count} tampered leaf {i} must fail");

                // Tamper a sibling (if any): rejected.
                if let Some(first) = proof.siblings.first().copied() {
                    let mut bad_sib = proof.clone();
                    bad_sib.siblings[0].1[0] ^= 0xff;
                    assert_ne!(bad_sib.siblings[0].1, first.1);
                    assert!(!verify_inclusion(&root, &bad_sib), "count={count} tampered sibling {i} must fail");
                }
            }
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
