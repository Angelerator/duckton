//! A minimal, dependency-free TON cell model just large enough to compute
//! **StateInit-based contract addresses** off-chain (BLOCKCHAIN_ECONOMICS §6.2,
//! §8): a TON contract's address is `(workchain, repr_hash(StateInit))` where the
//! `StateInit` cell carries the contract `code` and initial `data`. Because TON
//! uses deterministic StateInit addressing, the per-node `StakeVault` and per-job
//! `JobEscrow` addresses are known *before* deploy — the off-chain coordinator
//! can resolve exactly which contract a node/job maps to.
//!
//! ## What this implements
//!
//! Only ordinary (non-exotic), level-0 cells are needed here. For such a cell the
//! TON *representation hash* (TON whitepaper §3.1.5) is:
//!
//! ```text
//! repr_hash(c) = sha256( d1 ‖ d2 ‖ data_aug ‖ {depth(ref_i) as u16}* ‖ {repr_hash(ref_i)}* )
//! ```
//!
//! * `d1 = refs_count` (ordinary, level 0)
//! * `d2 = floor(bits/8) + ceil(bits/8)`
//! * `data_aug` is the data byte-padded; if `bits` is not byte-aligned a single
//!   `1` bit is appended followed by `0`s to fill the final byte.
//! * `depth(c) = 0` for a ref-less cell, else `1 + max(depth(ref_i))`.
//!
//! The `StateInit` cell with only `code` + `data` set is `b{00110}` (5 bits:
//! `split_depth=⊥, special=⊥, code=just, data=just, library=⊥`) followed by the
//! `code` and `data` refs — exactly the on-chain `StateInit.calcHashCodeData`
//! (`@acton`/stdlib) and `NEWC b{00110} STSLICECONST STREF STREF HASHBU`.
//!
//! Every encoding here (`coins`/VarUInteger16, `MsgAddressInt` addr_std, fixed
//! uints, refs, repr-hash, StateInit) is cross-checked byte-for-byte against the
//! Acton emulator in the unit tests via reference values from
//! `ton/scripts/_probe_addr.tolk`.

use sha2::{Digest, Sha256};

use crate::types::{Hash32, WalletAddress};

/// The basechain workchain id (TON `BASECHAIN`). Sharded contracts live here.
pub const BASECHAIN: i32 = 0;

/// An ordinary, level-0 TON cell: a bit string plus ordered child references.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cell {
    /// Data bytes; the final byte holds `bit_len % 8` significant high bits.
    data: Vec<u8>,
    bit_len: usize,
    refs: Vec<Cell>,
}

impl Cell {
    pub fn bit_len(&self) -> usize {
        self.bit_len
    }
    pub fn refs(&self) -> &[Cell] {
        &self.refs
    }

    /// Cell tree depth: 0 for a ref-less cell, else `1 + max(child depth)`.
    pub fn depth(&self) -> u16 {
        self.refs.iter().map(|r| r.depth()).max().map_or(0, |m| m + 1)
    }

    /// Two descriptor bytes `(d1, d2)` for an ordinary, level-0 cell.
    fn descriptor(&self) -> [u8; 2] {
        let d1 = self.refs.len() as u8;
        let full = self.bit_len / 8;
        let d2 = if self.bit_len % 8 == 0 { 2 * full } else { 2 * full + 1 } as u8;
        [d1, d2]
    }

    /// Data bytes with TON bit-augmentation applied when not byte-aligned.
    fn augmented_data(&self) -> Vec<u8> {
        let mut bytes = self.data.clone();
        let rem = self.bit_len % 8;
        if rem != 0 {
            // Append a single `1` bit immediately after the last data bit.
            let last = bytes.len() - 1;
            bytes[last] |= 1 << (7 - rem);
        }
        bytes
    }

    /// The TON representation hash (sha256) of this cell tree.
    pub fn repr_hash(&self) -> Hash32 {
        let mut h = Sha256::new();
        h.update(self.descriptor());
        h.update(self.augmented_data());
        for r in &self.refs {
            h.update(r.depth().to_be_bytes());
        }
        for r in &self.refs {
            h.update(r.repr_hash());
        }
        h.finalize().into()
    }
}

/// Builder for an ordinary TON cell (bits stored MSB-first, refs appended in
/// order). Mirrors the subset of `beginCell()…endCell()` we need.
#[derive(Debug, Clone, Default)]
pub struct CellBuilder {
    cell: Cell,
}

impl CellBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn push_bit(&mut self, bit: bool) {
        let pos = self.cell.bit_len % 8;
        if pos == 0 {
            self.cell.data.push(0);
        }
        if bit {
            let last = self.cell.data.len() - 1;
            self.cell.data[last] |= 1 << (7 - pos);
        }
        self.cell.bit_len += 1;
    }

    /// Store the low `nbits` of `value`, most-significant bit first.
    pub fn store_uint(mut self, value: u128, nbits: u32) -> Self {
        debug_assert!(nbits <= 128);
        for i in (0..nbits).rev() {
            self.push_bit((value >> i) & 1 == 1);
        }
        self
    }

    /// Store a signed integer in `nbits` two's-complement, MSB first.
    pub fn store_int(self, value: i64, nbits: u32) -> Self {
        let mask: u128 = if nbits >= 128 { u128::MAX } else { (1u128 << nbits) - 1 };
        self.store_uint((value as i128 as u128) & mask, nbits)
    }

    /// Store the first `nbits` of `bytes` (MSB-first within each byte).
    pub fn store_bits(mut self, bytes: &[u8], nbits: usize) -> Self {
        for i in 0..nbits {
            let byte = bytes[i / 8];
            let bit = (byte >> (7 - (i % 8))) & 1 == 1;
            self.push_bit(bit);
        }
        self
    }

    /// Store a 256-bit unsigned integer given as 32 big-endian bytes.
    pub fn store_u256(self, h: &Hash32) -> Self {
        self.store_bits(h, 256)
    }

    /// Store a TL-B `coins` (VarUInteger 16): a 4-bit byte-length prefix `L`
    /// followed by the `L` big-endian value bytes (`0` ⇒ just the 4-bit `0`).
    pub fn store_coins(mut self, value: u128) -> Self {
        let used = if value == 0 { 0 } else { ((128 - value.leading_zeros()) as usize).div_ceil(8) };
        self = self.store_uint(used as u128, 4);
        for i in (0..used).rev() {
            self = self.store_uint(((value >> (i * 8)) & 0xff) as u128, 8);
        }
        self
    }

    /// Store a standard internal address (`addr_std$10`, no anycast): the 3-bit
    /// prefix `100`, an 8-bit workchain, then the 256-bit account id (267 bits).
    pub fn store_address(self, addr: &WalletAddress) -> Self {
        self.store_uint(0b100, 3)
            .store_int(addr.workchain as i64, 8)
            .store_u256(&addr.hash)
    }

    /// Append a child reference.
    pub fn store_ref(mut self, cell: Cell) -> Self {
        self.cell.refs.push(cell);
        self
    }

    pub fn build(self) -> Cell {
        self.cell
    }
}

/// Compute a `StateInit` cell carrying only `code` + `data` (the common case),
/// matching the on-chain `StateInit.calcHashCodeData` layout (`b{00110}` then the
/// `code` and `data` refs).
pub fn state_init_cell(code: Cell, data: Cell) -> Cell {
    CellBuilder::new()
        .store_uint(0b00110, 5)
        .store_ref(code)
        .store_ref(data)
        .build()
}

impl WalletAddress {
    /// Deterministic TON contract address from its `StateInit` (code + data):
    /// `(workchain, repr_hash(StateInit))`. This is how the per-node `StakeVault`
    /// and per-job `JobEscrow` addresses are derived (BLOCKCHAIN_ECONOMICS §6.2,
    /// §8) — known before the contract is even deployed.
    pub fn from_state_init(workchain: i32, code: &Cell, data: &Cell) -> Self {
        let si = state_init_cell(code.clone(), data.clone());
        WalletAddress::new(workchain, si.repr_hash())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// repr_hash of a ref-less cell with exactly 512 byte-aligned bits reduces to
    /// `sha256(00 80 ‖ data)` — the same reduction the Merkle node hash relies on.
    #[test]
    fn repr_hash_of_512_bit_cell_matches_merkle_pair() {
        let cell = CellBuilder::new()
            .store_u256(&[0u8; 32])
            .store_u256(&[0u8; 32])
            .build();
        assert_eq!(cell.descriptor(), [0x00, 0x80]);
        assert_eq!(
            hex::encode(cell.repr_hash()),
            // hashPair(0, 0) reference from ton/scripts/_probe_hash.tolk
            "ac165244115ace66658b50e85fb073fb3c02e37f9d6349ed4c6c2b0cc5564c2d"
        );
    }

    #[test]
    fn coins_var_uint16_lengths() {
        // value 0 => 4-bit length 0, no value bytes (4 bits total).
        assert_eq!(CellBuilder::new().store_coins(0).build().bit_len(), 4);
        // value 1 => length 1, one byte (4 + 8 bits).
        assert_eq!(CellBuilder::new().store_coins(1).build().bit_len(), 12);
        // value 256 => length 2, two bytes (4 + 16 bits).
        assert_eq!(CellBuilder::new().store_coins(256).build().bit_len(), 20);
    }

    #[test]
    fn address_is_267_bits() {
        let a = WalletAddress::new(0, [0u8; 32]);
        assert_eq!(CellBuilder::new().store_address(&a).build().bit_len(), 267);
    }

    #[test]
    fn depth_is_tree_height() {
        let leaf = CellBuilder::new().store_uint(1, 8).build();
        assert_eq!(leaf.depth(), 0);
        let parent = CellBuilder::new().store_ref(leaf).build();
        assert_eq!(parent.depth(), 1);
        let grand = CellBuilder::new().store_ref(parent).build();
        assert_eq!(grand.depth(), 2);
    }
}
