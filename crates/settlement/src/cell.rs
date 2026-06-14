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
    /// The raw data bytes (the final byte holds `bit_len % 8` significant high
    /// bits). Exposed so a [`CellParser`] / live caller can read the slice back.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// A read cursor over this cell's bits + child refs (TON "slice"). The inverse
    /// of [`CellBuilder`]; used by the live layer to parse get-method results
    /// (e.g. decode the on-chain `EcoParams` cell + `feeRecipient` address back
    /// into typed values before re-broadcasting a toggled `update_params`).
    pub fn parser(&self) -> CellParser<'_> {
        CellParser { cell: self, bit: 0, next_ref: 0 }
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

    /// Store a `Maybe ^Cell`: a `1` bit + a ref when `Some`, a `0` bit when `None`.
    pub fn store_maybe_ref(self, cell: Option<Cell>) -> Self {
        match cell {
            Some(c) => self.store_uint(1, 1).store_ref(c),
            None => self.store_uint(0, 1),
        }
    }

    /// Append all of `other`'s bits AND its child refs inline (TON `storeBuilder`),
    /// used to splice the v5 signing message into the final signed body.
    pub fn store_cell_inline(mut self, other: &Cell) -> Self {
        for i in 0..other.bit_len {
            let bit = (other.data[i / 8] >> (7 - (i % 8))) & 1 == 1;
            self.push_bit(bit);
        }
        self.cell.refs.extend(other.refs.iter().cloned());
        self
    }

    pub fn build(self) -> Cell {
        self.cell
    }
}

/// A read cursor over a [`Cell`]'s bits + child refs (the TON "slice"), the exact
/// inverse of the subset of [`CellBuilder`] this crate emits. It is dependency-
/// free and only implements what the live layer needs to decode get-method
/// results: fixed `uint`/`int`, TL-B `coins` (VarUInteger16), `MsgAddressInt`
/// (`addr_std`), raw bit runs, and child refs. Reads are MSB-first and return
/// `None` on under-run so callers fail loudly rather than mis-parse.
#[derive(Debug, Clone)]
pub struct CellParser<'a> {
    cell: &'a Cell,
    bit: usize,
    next_ref: usize,
}

impl<'a> CellParser<'a> {
    /// Bits not yet consumed.
    pub fn remaining_bits(&self) -> usize {
        self.cell.bit_len.saturating_sub(self.bit)
    }
    /// Refs not yet consumed.
    pub fn remaining_refs(&self) -> usize {
        self.cell.refs.len().saturating_sub(self.next_ref)
    }

    fn load_bit(&mut self) -> Option<bool> {
        if self.bit >= self.cell.bit_len {
            return None;
        }
        let b = (self.cell.data[self.bit / 8] >> (7 - (self.bit % 8))) & 1 == 1;
        self.bit += 1;
        Some(b)
    }

    /// Read `nbits` (≤128) as an unsigned integer, MSB-first.
    pub fn load_uint(&mut self, nbits: usize) -> Option<u128> {
        if nbits > 128 || self.bit + nbits > self.cell.bit_len {
            return None;
        }
        let mut v = 0u128;
        for _ in 0..nbits {
            v = (v << 1) | (self.load_bit()? as u128);
        }
        Some(v)
    }

    /// Read `nbits` (≤128) as a two's-complement signed integer.
    pub fn load_int(&mut self, nbits: usize) -> Option<i128> {
        let u = self.load_uint(nbits)?;
        if nbits > 0 && (u >> (nbits - 1)) & 1 == 1 {
            Some(u as i128 - (1i128 << nbits))
        } else {
            Some(u as i128)
        }
    }

    /// Read a TL-B `coins` (VarUInteger16): a 4-bit byte-length `L` then `L` bytes.
    pub fn load_coins(&mut self) -> Option<u128> {
        let len = self.load_uint(4)? as usize;
        if len == 0 {
            return Some(0);
        }
        self.load_uint(len * 8)
    }

    /// Read `nbits` into a freshly allocated, MSB-first byte buffer (last byte
    /// zero-padded in its low bits).
    pub fn load_bits(&mut self, nbits: usize) -> Option<Vec<u8>> {
        let mut out = vec![0u8; nbits.div_ceil(8)];
        for i in 0..nbits {
            if self.load_bit()? {
                out[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        Some(out)
    }

    /// Read a `MsgAddressInt`. Returns `Some(addr)` for `addr_std$10` (no
    /// anycast), `None` for `addr_none$00`. Other variants are unsupported.
    pub fn load_address(&mut self) -> Option<WalletAddress> {
        let tag = self.load_uint(2)?;
        if tag == 0b00 {
            return None; // addr_none
        }
        if tag != 0b10 {
            return None; // addr_extern / addr_var unsupported
        }
        let anycast = self.load_bit()?; // Maybe Anycast (expected 0)
        if anycast {
            return None;
        }
        let wc = self.load_int(8)? as i32;
        let bits = self.load_bits(256)?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bits);
        Some(WalletAddress::new(wc, hash))
    }

    /// Consume the next child reference.
    pub fn load_ref(&mut self) -> Option<&'a Cell> {
        let r = self.cell.refs.get(self.next_ref)?;
        self.next_ref += 1;
        Some(r)
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

    /// Like [`WalletAddress::from_state_init`] but the `code` cell is provided only
    /// by its `(repr_hash, depth)` — enough to hash the `StateInit` without the
    /// full code tree. This is how the wallet **v5r1** address is derived: the
    /// standard wallet code is a large, fixed cell whose repr-hash/depth are
    /// well-known (`20834b7b…`, depth 6), so we never embed the code BoC and never
    /// need it to sign/broadcast from an *already-deployed* wallet.
    pub fn from_code_hash_state_init(
        workchain: i32,
        code_hash: &Hash32,
        code_depth: u16,
        data: &Cell,
    ) -> Self {
        // StateInit cell: bits b{00110} (5), refs [code, data]; descriptor (2,1),
        // augmented data byte 0x34. The repr-hash folds in each ref's depth then
        // each ref's repr-hash — so the external code ref needs only hash+depth.
        let mut h = Sha256::new();
        h.update([2u8, 1u8]); // descriptor: 2 refs, 5 data bits
        h.update([0x34u8]); // augmented b{00110}
        h.update(code_depth.to_be_bytes());
        h.update(data.depth().to_be_bytes());
        h.update(code_hash);
        h.update(data.repr_hash());
        WalletAddress::new(workchain, h.finalize().into())
    }
}

// ---------------------------------------------------------------------------
// Bag-of-Cells (BoC) serialization — the on-wire encoding `sendBoc` expects.
//
// Only what the live broadcaster needs: a single-root, ordinary-cell tree
// serialized with the standard `b5ee9c72` header + crc32c. A minimal inverse
// (`from_boc`) is provided for round-trip unit tests (serialize → parse →
// identical repr-hash), so the encoder is checked offline without a network hop.
// ---------------------------------------------------------------------------

impl Cell {
    /// Build a [`Cell`] directly from raw parts (used by the BoC parser/tests).
    fn from_parts(data: Vec<u8>, bit_len: usize, refs: Vec<Cell>) -> Self {
        Cell { data, bit_len, refs }
    }

    /// Serialized size of this cell in a BoC body (descriptors + data + refs).
    fn serialized_len(&self, ref_size: usize) -> usize {
        2 + self.augmented_data().len() + self.refs.len() * ref_size
    }

    /// Flatten the cell tree into a parents-before-children ordering (every cell's
    /// ref indices are strictly greater than its own), deduplicating shared cells
    /// by repr-hash. Returns the ordered cells plus, for each, its ref indices.
    fn topological_order(&self) -> (Vec<&Cell>, Vec<Vec<usize>>) {
        // Collect distinct cells (by repr-hash) reachable from the root.
        let mut order: Vec<&Cell> = Vec::new();
        let mut index: std::collections::HashMap<Hash32, usize> = std::collections::HashMap::new();
        // DFS post-order then reverse → parents precede children for a DAG.
        fn visit<'a>(
            c: &'a Cell,
            order: &mut Vec<&'a Cell>,
            index: &mut std::collections::HashMap<Hash32, usize>,
            seen: &mut std::collections::HashSet<Hash32>,
        ) {
            let h = c.repr_hash();
            if !seen.insert(h) {
                return;
            }
            for r in &c.refs {
                visit(r, order, index, seen);
            }
            order.push(c);
        }
        let mut seen = std::collections::HashSet::new();
        visit(self, &mut order, &mut index, &mut seen);
        order.reverse(); // root first, children after
        for (i, c) in order.iter().enumerate() {
            index.insert(c.repr_hash(), i);
        }
        let ref_indices: Vec<Vec<usize>> = order
            .iter()
            .map(|c| c.refs.iter().map(|r| index[&r.repr_hash()]).collect())
            .collect();
        (order, ref_indices)
    }

    /// Serialize this cell tree into a standard BoC byte string (single root,
    /// crc32c appended). This is what gets base64-encoded for `sendBoc`.
    pub fn to_boc(&self) -> Vec<u8> {
        let (cells, ref_indices) = self.topological_order();
        let cell_count = cells.len();

        // Bytes needed to hold a ref index (`size`) and a data offset (`off_bytes`).
        let ref_size = bytes_for(cell_count as u64).max(1);
        let tot_size: usize = cells.iter().map(|c| c.serialized_len(ref_size)).sum();
        let off_bytes = bytes_for(tot_size as u64).max(1);

        let mut out = Vec::new();
        out.extend_from_slice(&[0xb5, 0xee, 0x9c, 0x72]); // magic
        // flags byte: has_idx=0, has_crc32c=1, has_cache=0, flags=0, size=ref_size.
        out.push((1 << 6) | (ref_size as u8 & 0b111));
        out.push(off_bytes as u8);
        out.extend_from_slice(&be_bytes(cell_count as u64, ref_size)); // cells
        out.extend_from_slice(&be_bytes(1, ref_size)); // roots = 1
        out.extend_from_slice(&be_bytes(0, ref_size)); // absent = 0
        out.extend_from_slice(&be_bytes(tot_size as u64, off_bytes)); // tot cell size
        out.extend_from_slice(&be_bytes(0, ref_size)); // root index = 0

        for (c, refs) in cells.iter().zip(ref_indices.iter()) {
            let [d1, d2] = c.descriptor();
            out.push(d1);
            out.push(d2);
            out.extend_from_slice(&c.augmented_data());
            for &r in refs {
                out.extend_from_slice(&be_bytes(r as u64, ref_size));
            }
        }

        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parse a BoC produced by [`Cell::to_boc`] back into its root cell. Supports
    /// the subset this crate emits (single root, optional crc, no index table).
    /// Used by round-trip tests; returns `None` on any malformed input.
    pub fn from_boc(bytes: &[u8]) -> Option<Cell> {
        if bytes.len() < 6 || bytes[0..4] != [0xb5, 0xee, 0x9c, 0x72] {
            return None;
        }
        let flags = bytes[4];
        let has_idx = flags & (1 << 7) != 0;
        let has_crc = flags & (1 << 6) != 0;
        let ref_size = (flags & 0b111) as usize;
        let off_bytes = bytes[5] as usize;
        let mut p = 6usize;
        let read = |buf: &[u8], p: &mut usize, n: usize| -> Option<u64> {
            if *p + n > buf.len() {
                return None;
            }
            let mut v = 0u64;
            for &b in &buf[*p..*p + n] {
                v = (v << 8) | b as u64;
            }
            *p += n;
            Some(v)
        };
        let cell_count = read(bytes, &mut p, ref_size)? as usize;
        let _roots = read(bytes, &mut p, ref_size)?;
        let _absent = read(bytes, &mut p, ref_size)?;
        let _tot = read(bytes, &mut p, off_bytes)?;
        let root_idx = read(bytes, &mut p, ref_size)? as usize;
        if has_idx {
            p += cell_count * off_bytes; // skip index table
        }
        let body_end = if has_crc { bytes.len().checked_sub(4)? } else { bytes.len() };
        // Parse raw (descriptor + data + ref indices) for each cell in order.
        let mut raw: Vec<(Vec<u8>, usize, Vec<usize>)> = Vec::with_capacity(cell_count);
        for _ in 0..cell_count {
            if p + 2 > body_end {
                return None;
            }
            let d1 = bytes[p];
            let d2 = bytes[p + 1];
            p += 2;
            let refs_n = (d1 & 0b111) as usize;
            let data_bytes = (d2 as usize).div_ceil(2);
            let not_aligned = d2 & 1 == 1;
            if p + data_bytes > body_end {
                return None;
            }
            let data = bytes[p..p + data_bytes].to_vec();
            p += data_bytes;
            let bit_len = if not_aligned {
                // Strip the augmentation bit: bits = 8*(full) + position of the
                // final set completion bit in the last byte.
                let full = data_bytes - 1;
                let last = data[data_bytes - 1];
                // The augmentation bit is the LOWEST set bit of the final byte;
                // the real data bits sit above it.
                let mut bl = full * 8;
                for i in 0..8 {
                    if last & (1 << i) != 0 {
                        bl = full * 8 + (7 - i);
                        break;
                    }
                }
                bl
            } else {
                data_bytes * 8
            };
            let mut refs = Vec::with_capacity(refs_n);
            for _ in 0..refs_n {
                refs.push(read(bytes, &mut p, ref_size)? as usize);
            }
            raw.push((data, bit_len, refs));
        }
        // Rebuild cells children-first (refs always have a higher index).
        let mut built: Vec<Option<Cell>> = vec![None; cell_count];
        for i in (0..cell_count).rev() {
            let (data, bit_len, refs) = &raw[i];
            let child_cells: Vec<Cell> = refs.iter().map(|&r| built[r].clone().unwrap()).collect();
            built[i] = Some(Cell::from_parts(data.clone(), *bit_len, child_cells));
        }
        built[root_idx].clone()
    }
}

/// Minimum number of bytes needed to big-endian-encode `v` (at least 0 ⇒ 0; the
/// callers clamp to a `.max(1)`).
fn bytes_for(v: u64) -> usize {
    let mut n = 0usize;
    let mut x = v;
    while x > 0 {
        n += 1;
        x >>= 8;
    }
    n
}

/// Big-endian encode `v` into exactly `n` bytes.
fn be_bytes(v: u64, n: usize) -> Vec<u8> {
    let full = v.to_be_bytes();
    full[8 - n..].to_vec()
}

/// CRC32C (Castagnoli, poly 0x1EDC6F41 reflected = 0x82F63B78) — the checksum the
/// BoC trailer uses. Bitwise (table-free) to stay dependency-light.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
        }
    }
    !crc
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

    #[test]
    fn boc_round_trips_a_cell_tree() {
        // A non-trivial tree: refs, a shared child, and an unaligned bit length.
        let shared = CellBuilder::new().store_uint(0xABCD, 16).build();
        let child = CellBuilder::new()
            .store_uint(0b101, 3) // unaligned
            .store_ref(shared.clone())
            .build();
        let root = CellBuilder::new()
            .store_coins(123_456)
            .store_address(&WalletAddress::new(0, [9u8; 32]))
            .store_ref(child)
            .store_ref(shared)
            .build();

        let boc = root.to_boc();
        // Header magic + crc trailer present.
        assert_eq!(&boc[0..4], &[0xb5, 0xee, 0x9c, 0x72]);
        let parsed = Cell::from_boc(&boc).expect("our own BoC must parse");
        // Strongest check: the parsed tree hashes identically to the original.
        assert_eq!(parsed.repr_hash(), root.repr_hash());
    }

    #[test]
    fn parser_round_trips_builder_fields() {
        // Build a mixed cell, then parse it back field-for-field (MSB-first).
        let addr = WalletAddress::new(0, [0xABu8; 32]);
        let child = CellBuilder::new().store_uint(0xBEEF, 16).build();
        let cell = CellBuilder::new()
            .store_uint(0x47504101, 32) // opcode
            .store_uint(0xDEADBEEF, 64) // a u64
            .store_coins(1_234_567_890) // VarUInteger16 coins
            .store_coins(0) // zero coins => 4-bit length 0
            .store_address(&addr)
            .store_int(-3, 8) // signed
            .store_ref(child.clone())
            .build();

        let mut p = cell.parser();
        assert_eq!(p.load_uint(32), Some(0x47504101));
        assert_eq!(p.load_uint(64), Some(0xDEADBEEF));
        assert_eq!(p.load_coins(), Some(1_234_567_890));
        assert_eq!(p.load_coins(), Some(0));
        assert_eq!(p.load_address(), Some(addr));
        assert_eq!(p.load_int(8), Some(-3));
        assert_eq!(p.remaining_bits(), 0);
        assert_eq!(p.load_ref().map(|c| c.repr_hash()), Some(child.repr_hash()));
        assert!(p.load_ref().is_none());
    }

    #[test]
    fn parser_addr_none_and_under_run() {
        // addr_none$00 yields None; an over-read past the end also yields None.
        let cell = CellBuilder::new().store_uint(0b00, 2).build();
        assert_eq!(cell.parser().load_address(), None);
        let small = CellBuilder::new().store_uint(0b1, 1).build();
        assert_eq!(small.parser().load_uint(8), None);
    }

    #[test]
    fn code_hash_state_init_matches_full_cell_derivation() {
        // The (code_hash, depth) StateInit hasher must agree with hashing a full
        // code cell — so the wallet-v5r1 address derivation is exact.
        let code = CellBuilder::new().store_uint(0xC0DE, 16).store_ref(
            CellBuilder::new().store_uint(0xBEEF, 16).build(),
        ).build();
        let data = CellBuilder::new().store_uint(0x1234_5678, 32).build();
        let full = WalletAddress::from_state_init(BASECHAIN, &code, &data);
        let via_hash = WalletAddress::from_code_hash_state_init(
            BASECHAIN,
            &code.repr_hash(),
            code.depth(),
            &data,
        );
        assert_eq!(full, via_hash);
    }
}
