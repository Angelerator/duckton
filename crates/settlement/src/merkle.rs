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
//! ### Leaf domain separation (second-preimage resistance)
//!
//! Leaves are **domain-separated** from internal nodes: a leaf is hashed as a TON
//! cell holding exactly one 256-bit integer (descriptor `00 40`), while a node is
//! a cell holding two (descriptor `00 80`). The contract's verifier therefore
//! starts the fold with `acc = hashLeaf(leaf)`:
//!
//! ```tolk
//! hashLeaf(x) = beginCell().storeUint(x, 256).endCell().hash()  // = sha256(00 40 ‖ x)
//! hashPair(l, r) = beginCell().storeUint(l,256).storeUint(r,256).endCell().hash()
//! ```
//!
//! Without this, a 32-byte internal-node value could be presented as a *leaf*
//! (the classic Merkle second-preimage), letting an attacker fabricate inclusion
//! proofs for nodes that were never real leaves. With `hashLeaf` a node value can
//! never equal a leaf value.
//!
//! ### Odd levels: promotion, not duplication
//!
//! An odd node is **promoted unchanged** to the next level (NOT hashed with a
//! copy of itself). Self-duplication is the RFC-6962 anti-pattern that lets trees
//! of N and N+1 leaves collide; promotion avoids it. The concatenation order
//! (`left ‖ right`) matches `dir` handling in the contract (`dir = 0` ⇒ sibling
//! on the right).

use sha2::{Digest, Sha256};

use crate::types::{Hash32, InclusionProof};

/// Cell descriptor for an ordinary, ref-less cell holding exactly 512 data bits
/// (two `uint256`): `d1 = 0`, `d2 = 0x80`. Prepended before `left ‖ right`.
const PAIR_CELL_DESCRIPTOR: [u8; 2] = [0x00, 0x80];

/// Cell descriptor for an ordinary, ref-less cell holding exactly 256 data bits
/// (one `uint256`): `d1 = 0`, `d2 = 0x40`. Prepended before the leaf value. This
/// is what domain-separates a leaf from an internal node.
const LEAF_CELL_DESCRIPTOR: [u8; 2] = [0x00, 0x40];

/// Hash a raw leaf value (a 32-byte `uint256` commitment) into its
/// domain-separated leaf-layer hash, matching the on-chain `hashLeaf`.
pub(crate) fn hash_leaf(value: &Hash32) -> Hash32 {
    let mut h = Sha256::new();
    h.update(LEAF_CELL_DESCRIPTOR);
    h.update(value);
    h.finalize().into()
}

/// Hash two child nodes into their parent, matching the on-chain `RecordAnchor`
/// verifier's TON cell-representation hash (`hashPair`). See module docs.
pub fn hash_node(left: &Hash32, right: &Hash32) -> Hash32 {
    let mut h = Sha256::new();
    h.update(PAIR_CELL_DESCRIPTOR);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Build a Merkle tree over the raw leaf `values` and return all levels (level 0
/// = the **domain-separated** leaf hashes). Odd nodes are promoted unchanged.
/// Returns `None` for an empty input (there is no tree / root for an empty epoch).
fn levels(values: &[Hash32]) -> Option<Vec<Vec<Hash32>>> {
    if values.is_empty() {
        return None;
    }
    let level0: Vec<Hash32> = values.iter().map(hash_leaf).collect();
    let mut all = vec![level0];
    while all.last().unwrap().len() > 1 {
        let cur = all.last().unwrap();
        let mut next = Vec::with_capacity(cur.len().div_ceil(2));
        let mut i = 0;
        while i < cur.len() {
            if i + 1 < cur.len() {
                next.push(hash_node(&cur[i], &cur[i + 1]));
            } else {
                // Odd lone node: promote unchanged (no self-duplication).
                next.push(cur[i]);
            }
            i += 2;
        }
        all.push(next);
    }
    Some(all)
}

/// Compute the Merkle root of the raw leaf `values`.
///
/// Returns the all-zero hash for an empty input as a *query* convenience (so the
/// `epoch_root` accessor has a value). Callers that **submit** a root on-chain
/// MUST use [`try_merkle_root`] and refuse an empty epoch — anchoring the zero
/// root would collide with the genesis `lastRoot == 0` and bypass the chained
/// prev-root check.
pub fn merkle_root(values: &[Hash32]) -> Hash32 {
    try_merkle_root(values).unwrap_or([0u8; 32])
}

/// Compute the Merkle root, or `None` for an empty epoch (nothing to anchor).
pub fn try_merkle_root(values: &[Hash32]) -> Option<Hash32> {
    Some(*levels(values)?.last().unwrap().first().unwrap())
}

/// Build an inclusion proof for `index` within the raw leaf `values`. The
/// returned `proof.leaf` is the **raw** value; the verifier re-applies
/// `hashLeaf` (so a node value can't masquerade as a leaf).
pub fn build_proof(values: &[Hash32], index: usize) -> Option<InclusionProof> {
    if index >= values.len() {
        return None;
    }
    let all = levels(values)?;
    let mut siblings = Vec::new();
    let mut idx = index;
    for level in &all[..all.len() - 1] {
        if idx.is_multiple_of(2) {
            // Left child: sibling on the right — unless this node was promoted
            // (no right neighbor at this level), in which case there is no step.
            if idx + 1 < level.len() {
                siblings.push((false, level[idx + 1]));
            }
        } else {
            // Right child: sibling on the left.
            siblings.push((true, level[idx - 1]));
        }
        idx /= 2;
    }
    Some(InclusionProof {
        leaf: values[index],
        siblings,
    })
}

/// Verify an inclusion proof against an anchored `root`. Mirrors the on-chain
/// `computeRootFromProof`: start from `hashLeaf(leaf)`, then fold siblings.
pub fn verify_inclusion(root: &Hash32, proof: &InclusionProof) -> bool {
    let mut acc = hash_leaf(&proof.leaf);
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
        // hashPair(0x1111, 0x2222) — internal-node hashing is unchanged by v2.
        let hp = hash_node(&leaf_u32(0x1111), &leaf_u32(0x2222));
        assert_eq!(
            hex::encode(hp),
            "df96613f475a40cceebaf4bfe1e15dc7ff2cedd5a4c148c600cde200896c5732"
        );
        // v2 leaf domain separation: hashLeaf(x) = sha256(00 40 ‖ x_be32). These
        // MUST equal the on-chain `hashLeaf` (= beginCell().storeUint(x,256)
        // .endCell().hash()) — cross-checked against the RecordAnchor emulator.
        assert_eq!(
            hex::encode(hash_leaf(&leaf_u32(0x1111))),
            "d8553b5eff29dfc8598e44d8a601afee996cfed09795dd92dc0bb3086a7b0f81"
        );
        assert_eq!(
            hex::encode(hash_leaf(&leaf_u32(0x2222))),
            "99baad8064aebe6fbbf291ef23e395a0fa374c8c677e4d73e019ab230bec039b"
        );
        // ROOT4 over the v2 tree (leaves are hashLeaf'd first):
        // hashPair(hashPair(hashLeaf(L0),hashLeaf(L1)), hashPair(hashLeaf(L2),hashLeaf(L3))).
        let root = merkle_root(&[
            leaf_u32(0x1111),
            leaf_u32(0x2222),
            leaf_u32(0x3333),
            leaf_u32(0x4444),
        ]);
        assert_eq!(
            hex::encode(root),
            "d71883509985078d4b68f441a4647296db9a1c514c913a1ceacfe35458918824"
        );
    }

    /// The off-chain proof for a balanced multi-leaf tree must fold to the same
    /// root the on-chain verifier produces. This pins the exact sibling order
    /// (`dir`) the contract walks: for the 4-leaf tree the proof for leaf L0 is
    /// `[L1 (right), hashPair(L2,L3) (right)]`, matching `anchor.test.tolk`.
    #[test]
    fn four_leaf_proof_matches_onchain_layout() {
        let leaves = [
            leaf_u32(0x1111),
            leaf_u32(0x2222),
            leaf_u32(0x3333),
            leaf_u32(0x4444),
        ];
        let root = merkle_root(&leaves);
        let proof = build_proof(&leaves, 0).unwrap();
        // Both siblings are to the RIGHT (dir = 0 on-chain). Level-0 siblings are
        // the domain-separated leaf hashes (not the raw leaf values).
        assert_eq!(proof.siblings.len(), 2);
        assert!(!proof.siblings[0].0, "L1 is the right sibling");
        assert_eq!(proof.siblings[0].1, hash_leaf(&leaf_u32(0x2222)));
        assert!(
            !proof.siblings[1].0,
            "hashPair(hashLeaf(L2),hashLeaf(L3)) is the right sibling"
        );
        assert_eq!(
            proof.siblings[1].1,
            hash_node(&hash_leaf(&leaf_u32(0x3333)), &hash_leaf(&leaf_u32(0x4444)))
        );
        assert!(verify_inclusion(&root, &proof));
    }

    #[test]
    fn empty_tree_has_no_root() {
        assert_eq!(
            try_merkle_root(&[]),
            None,
            "an empty epoch has no anchorable root"
        );
        assert_eq!(merkle_root(&[]), [0u8; 32], "query convenience only");
        assert!(build_proof(&[], 0).is_none());
    }

    #[test]
    fn node_value_cannot_masquerade_as_leaf() {
        // Second-preimage: an internal-node hash presented as a leaf must NOT
        // verify, because the verifier re-applies hashLeaf to proof.leaf.
        let leaves = [leaf_u32(1), leaf_u32(2)];
        let root = merkle_root(&leaves);
        let node = hash_node(&hash_leaf(&leaf_u32(1)), &hash_leaf(&leaf_u32(2))); // == root
        assert_eq!(node, root);
        // Forge a "proof" claiming the node value is itself a leaf with no siblings.
        let forged = InclusionProof {
            leaf: node,
            siblings: vec![],
        };
        assert!(
            !verify_inclusion(&root, &forged),
            "node-as-leaf must be rejected"
        );
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
    /// odd leaf counts (odd promotes the last node unchanged).
    #[test]
    fn multi_leaf_jobrecord_inclusion_and_tamper() {
        use crate::types::JobRecord;

        let record = |i: usize| JobRecord {
            job_id: format!("job-{i}"),
            query_hash: format!("q{i}"),
            requester_wallet: "0:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            max_bid: 1_000 + i as u128,
            result_hash: format!("r{i}"),
            epoch: 7,
            prev_root: [9u8; 32],
            params_version: 0,
        };

        for count in [4usize, 5, 6, 7, 8] {
            let leaves: Vec<Hash32> = (0..count).map(|i| record(i).leaf()).collect();
            let root = merkle_root(&leaves);

            for i in 0..count {
                let proof = build_proof(&leaves, i).unwrap();
                assert!(
                    verify_inclusion(&root, &proof),
                    "count={count} leaf {i} must verify"
                );

                // Tamper the leaf: rejected.
                let mut bad_leaf = proof.clone();
                bad_leaf.leaf[0] ^= 0xff;
                assert!(
                    !verify_inclusion(&root, &bad_leaf),
                    "count={count} tampered leaf {i} must fail"
                );

                // Tamper a sibling (if any): rejected.
                if let Some(first) = proof.siblings.first().copied() {
                    let mut bad_sib = proof.clone();
                    bad_sib.siblings[0].1[0] ^= 0xff;
                    assert_ne!(bad_sib.siblings[0].1, first.1);
                    assert!(
                        !verify_inclusion(&root, &bad_sib),
                        "count={count} tampered sibling {i} must fail"
                    );
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
        // Single-leaf root is the domain-separated leaf hash, NOT the raw leaf.
        assert_eq!(root, hash_leaf(&leaf(7)));
    }
}
