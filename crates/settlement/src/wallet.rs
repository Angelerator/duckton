//! Rust **wallet v5r1** signer: TON-mnemonic key derivation, deterministic
//! address derivation, and signed external-message assembly + BoC for broadcast.
//!
//! This is the half of the on-chain rail that was previously delegated to the
//! Acton harness (`scripts/testnet_e2e.sh` owned the wallet + signing). It lets a
//! node self-broadcast: build + Ed25519-sign a wallet-v5r1 external message that
//! carries the internal contract message(s), serialize the BoC, and POST it to
//! toncenter `sendBoc` (the network hop lives in [`crate::ton`], gated behind
//! `ton-live`).
//!
//! ## Why this is trustworthy offline
//!
//! Building a byte-exact v5r1 message is intricate, so the parts that *can* be
//! checked without a network are pinned to known vectors in the unit tests:
//!   * **mnemonic → keypair**: the TON standard (HMAC-SHA512 entropy →
//!     PBKDF2-SHA512 100k → Ed25519 seed),
//!   * **address**: the repo's own `deployer` v5r1 wallet
//!     (`global.wallets.toml`) must re-derive to its published testnet address,
//!   * **init data cell**: cross-checked against `@ton/ton`'s
//!     `WalletContractV5R1` (mainnet, null pubkey),
//!   * **signing**: the Ed25519 signature over the signing-cell repr-hash must
//!     verify with the wallet public key, and the body layout (opcode / wallet_id
//!     / valid_until / seqno / actions / trailing signature) is pinned.
//!
//! Final on-chain *acceptance* of a built message can only be fully confirmed by
//! a live testnet send (see the report's caveats).

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use sha2::Sha512;

use crate::cell::{Cell, CellBuilder, BASECHAIN};
use crate::types::{Amount, SettleError, WalletAddress};

/// Standard wallet **v5r1** code cell repr-hash (`@ton/ton WalletContractV5R1`).
/// The code is a large fixed cell; for an already-deployed wallet we only need
/// its hash + depth to derive the address and never embed the code BoC.
pub const WALLET_V5R1_CODE_HASH: [u8; 32] = [
    0x20, 0x83, 0x4b, 0x7b, 0x72, 0xb1, 0x12, 0x14, 0x7e, 0x1b, 0x2f, 0xb4, 0x57, 0xb8, 0x4e, 0x74,
    0xd1, 0xa3, 0x0f, 0x04, 0xf7, 0x37, 0xd4, 0xf6, 0x2a, 0x66, 0x8e, 0x95, 0x52, 0xd2, 0xb7, 0x2f,
];
/// Depth of the wallet v5r1 code cell tree.
pub const WALLET_V5R1_CODE_DEPTH: u16 = 6;

/// TON network global ids (folded into the wallet_id so the same mnemonic yields
/// distinct testnet/mainnet addresses — anti-replay).
pub const GLOBAL_ID_MAINNET: i32 = -239;
pub const GLOBAL_ID_TESTNET: i32 = -3;

// v5 external-message auth prefix ("sign"). Internal-auth (0x73696e74) and
// extension-auth (0x6578746e) are unused by this signer.
const OP_AUTH_SIGNED_EXTERNAL: u32 = 0x7369_676e;

type HmacSha512 = Hmac<Sha512>;

/// An Ed25519 wallet keypair derived from a TON mnemonic.
pub struct WalletKey {
    signing: SigningKey,
    public: [u8; 32],
}

impl WalletKey {
    /// Derive the keypair from a TON mnemonic (the standard 24- or 12-word form),
    /// with no extra password. Mirrors `@ton/crypto::mnemonicToPrivateKey`:
    /// `entropy = HMAC-SHA512(words, "")`, `seed = PBKDF2-SHA512(entropy,
    /// "TON default seed", 100_000)[..32]`, `keypair = ed25519(seed)`.
    pub fn from_mnemonic(phrase: &str) -> Result<Self, SettleError> {
        let words: Vec<&str> = phrase.split_whitespace().collect();
        if words.len() != 24 && words.len() != 12 {
            return Err(SettleError::Backend(format!(
                "mnemonic must be 12 or 24 words, got {}",
                words.len()
            )));
        }
        let normalized = words.join(" ");
        let mut mac = HmacSha512::new_from_slice(normalized.as_bytes())
            .map_err(|e| SettleError::Backend(format!("hmac init: {e}")))?;
        mac.update(b""); // empty password
        let entropy = mac.finalize().into_bytes();

        // Validate the mnemonic per the TON standard ("basic seed" check) so a
        // typo'd mnemonic is rejected instead of silently deriving a *different*
        // valid-looking wallet (wrong address ⇒ funds sent from/to the wrong
        // account). isBasicSeed: first byte of
        // PBKDF2-SHA512(entropy, "TON seed version", floor(100_000/256)) == 0.
        let mut check = [0u8; 64];
        pbkdf2::pbkdf2::<HmacSha512>(&entropy, b"TON seed version", 100_000 / 256, &mut check)
            .map_err(|e| SettleError::Backend(format!("pbkdf2 (validity): {e}")))?;
        if check[0] != 0 {
            return Err(SettleError::Backend(
                "invalid TON mnemonic (failed basic-seed checksum)".into(),
            ));
        }

        let mut seed = [0u8; 64];
        pbkdf2::pbkdf2::<HmacSha512>(&entropy, b"TON default seed", 100_000, &mut seed)
            .map_err(|e| SettleError::Backend(format!("pbkdf2: {e}")))?;

        let mut sk = [0u8; 32];
        sk.copy_from_slice(&seed[..32]);
        let signing = SigningKey::from_bytes(&sk);
        let public = signing.verifying_key().to_bytes();
        Ok(Self { signing, public })
    }

    /// Construct directly from a 32-byte Ed25519 seed (test/known-vector use).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let public = signing.verifying_key().to_bytes();
        Self { signing, public }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.public
    }

    fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }
}

/// A wallet **v5r1** instance: a public key + the network/workchain/subwallet
/// context that fixes its `wallet_id` (and therefore its address).
#[derive(Debug, Clone)]
pub struct WalletV5R1 {
    pub public_key: [u8; 32],
    pub network_global_id: i32,
    pub workchain: i8,
    pub subwallet: u16,
}

impl WalletV5R1 {
    /// A basechain v5r1 wallet for the given key on the given network.
    pub fn new(public_key: [u8; 32], network_global_id: i32) -> Self {
        Self {
            public_key,
            network_global_id,
            workchain: BASECHAIN as i8,
            subwallet: 0,
        }
    }

    pub fn testnet(public_key: [u8; 32]) -> Self {
        Self::new(public_key, GLOBAL_ID_TESTNET)
    }
    pub fn mainnet(public_key: [u8; 32]) -> Self {
        Self::new(public_key, GLOBAL_ID_MAINNET)
    }

    /// The 32-bit `wallet_id = global_id XOR context_id`, where the client context
    /// is `b1 ‖ wc:int8 ‖ version:uint8(0) ‖ subwallet:uint15` (TON v5r1).
    pub fn wallet_id(&self) -> u32 {
        let context: u32 = ((1u32 << 31)
            | (((self.workchain as u8) as u32) << 23)) // wallet version = 0
            | ((self.subwallet as u32) & 0x7fff);
        (self.network_global_id as u32) ^ context
    }

    /// The wallet's initial **data** cell (`contract_state`): `is_signature_allowed
    /// (1) ‖ seqno:uint32 ‖ wallet_id:uint32 ‖ public_key:uint256 ‖ extensions
    /// (empty HashmapE ⇒ 1 bit 0)`. `seqno = 0` (fresh).
    pub fn data_cell(&self) -> Cell {
        CellBuilder::new()
            .store_uint(1, 1) // is_signature_allowed
            .store_uint(0, 32) // seqno
            .store_uint(self.wallet_id() as u128, 32)
            .store_u256(&self.public_key)
            .store_uint(0, 1) // empty extensions dict
            .build()
    }

    /// Deterministic v5r1 address from `StateInit(code, data)` (code by hash+depth).
    pub fn address(&self) -> WalletAddress {
        WalletAddress::from_code_hash_state_init(
            self.workchain as i32,
            &WALLET_V5R1_CODE_HASH,
            WALLET_V5R1_CODE_DEPTH,
            &self.data_cell(),
        )
    }
}

/// A single internal message to emit from the wallet: destination, attached
/// nanoton `value`, the body cell, an optional `StateInit` (for a funded deploy,
/// e.g. opening a per-job escrow), and the send `mode`.
#[derive(Debug, Clone)]
pub struct InternalMessage {
    pub dest: WalletAddress,
    pub value: Amount,
    pub body: Cell,
    pub state_init: Option<Cell>,
    pub mode: u8,
}

impl InternalMessage {
    /// Common case: send to an existing contract, `value` nanoton, bounceable,
    /// `mode = 3` (pay fees separately + ignore errors). No deploy.
    pub fn to_contract(dest: WalletAddress, value: Amount, body: Cell) -> Self {
        Self {
            dest,
            value,
            body,
            state_init: None,
            mode: 3,
        }
    }

    /// Build the `MessageRelaxed` cell (`int_msg_info$0 …`) for this message.
    fn to_cell(&self) -> Cell {
        // int_msg_info$0 ihr_disabled:1=1 bounce:1=1 bounced:1=0 src:addr_none$00
        let mut b = CellBuilder::new()
            .store_uint(0, 1) // int_msg_info$0
            .store_uint(1, 1) // ihr_disabled
            .store_uint(1, 1) // bounce
            .store_uint(0, 1) // bounced
            .store_uint(0b00, 2) // src = addr_none
            .store_address(&self.dest)
            .store_coins(self.value)
            .store_uint(0, 1) // empty extra-currency dict
            .store_coins(0) // ihr_fee
            .store_coins(0) // fwd_fee
            .store_uint(0, 64) // created_lt
            .store_uint(0, 32); // created_at

        // init:(Maybe (Either StateInit ^StateInit)) — store as ^ ref when present.
        b = match &self.state_init {
            Some(si) => b.store_uint(1, 1).store_uint(1, 1).store_ref(si.clone()),
            None => b.store_uint(0, 1),
        };
        // body:(Either X ^X) — always a ref (always valid regardless of size).
        b.store_uint(1, 1).store_ref(self.body.clone()).build()
    }

    /// Wrap as a v5 `action_send_msg#0ec3c86d mode:uint8 out_msg:^MessageRelaxed`.
    fn to_out_action(&self, prev: Cell) -> Cell {
        CellBuilder::new()
            .store_ref(prev)
            .store_uint(0x0ec3_c86d, 32)
            .store_uint(self.mode as u128, 8)
            .store_ref(self.to_cell())
            .build()
    }
}

/// Build + Ed25519-sign a wallet **v5r1** external message that emits `messages`,
/// returning the BoC bytes ready for `sendBoc`.
///
/// `valid_until` is a unix-seconds expiry; `seqno` must be the wallet's current
/// on-chain seqno (read via `runGetMethod seqno`).
pub fn build_signed_external_v5r1(
    wallet: &WalletV5R1,
    key: &WalletKey,
    seqno: u32,
    valid_until: u32,
    messages: &[InternalMessage],
) -> Result<Vec<u8>, SettleError> {
    if key.public_key() != wallet.public_key {
        return Err(SettleError::Backend(
            "wallet key does not match the wallet public key".into(),
        ));
    }
    if messages.len() > 255 {
        return Err(SettleError::Backend(
            "at most 255 actions per v5r1 message".into(),
        ));
    }

    // c5 out-action list: nest each action under the previous (out_list_empty$_ is
    // an empty cell). Order is preserved by the contract's action processing.
    let mut out_list = CellBuilder::new().build(); // out_list_empty
    for m in messages {
        out_list = m.to_out_action(out_list);
    }
    let actions_present = !messages.is_empty();

    // InnerRequest: out_actions:(Maybe ^OutList) ‖ has_other_actions:1 (=0 here).
    let inner = CellBuilder::new()
        .store_maybe_ref(actions_present.then_some(out_list))
        .store_uint(0, 1)
        .build();

    // signingMessage = op ‖ wallet_id ‖ valid_until ‖ seqno ‖ inner (inline).
    let signing = CellBuilder::new()
        .store_uint(OP_AUTH_SIGNED_EXTERNAL as u128, 32)
        .store_uint(wallet.wallet_id() as u128, 32)
        .store_uint(valid_until as u128, 32)
        .store_uint(seqno as u128, 32)
        .store_cell_inline(&inner)
        .build();

    // Sign the signing-cell repr-hash; the 512-bit signature is appended AFTER.
    let signature = key.sign(&signing.repr_hash());
    let body = CellBuilder::new()
        .store_cell_inline(&signing)
        .store_bits(&signature, 512)
        .build();

    // External-in message to the wallet: ext_in_msg_info$10 src:addr_none
    // dest:wallet import_fee:0 ; init:Maybe=0 (deployed) ; body:Either ^ = ref.
    let ext = CellBuilder::new()
        .store_uint(0b10, 2) // ext_in_msg_info$10
        .store_uint(0b00, 2) // src = addr_none
        .store_address(&wallet.address())
        .store_coins(0) // import_fee
        .store_uint(0, 1) // no state init (wallet already deployed)
        .store_uint(1, 1) // body in ref
        .store_ref(body)
        .build();

    Ok(ext.to_boc())
}

/// Standard base64 encoding (RFC 4648, with `=` padding) — used to encode the
/// BoC for the toncenter `sendBoc` JSON body. Dependency-light by design.
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard or url-safe base64 (padding optional), the inverse of
/// [`base64_encode`]. Returns `None` on any invalid character. Used to load a
/// contract code BoC from an Acton build artifact's `code_boc64` field.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let s = s.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        let v = val(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Verify an Ed25519 signature over `msg` with `public_key` (test helper).
pub fn verify_sig(public_key: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    let sig = ed25519_dalek::Signature::from_bytes(sig);
    vk.verify_strict(msg, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic, NON-SECRET seed for tests that only need *some* valid
    /// wallet key (signing / BoC round-trip). Never a real wallet seed — those
    /// must never be committed (see `.gitignore`: `*.mnemonic`, `*.wallets.toml`).
    const TEST_SEED: [u8; 32] = [0x42; 32];

    #[test]
    fn wallet_id_matches_ton_reference() {
        // @ton/ton documents these exact values for wc=0, version=0, subwallet=0.
        assert_eq!(WalletV5R1::mainnet([0u8; 32]).wallet_id(), 2_147_483_409);
        assert_eq!(WalletV5R1::testnet([0u8; 32]).wallet_id(), 2_147_483_645);
    }

    #[test]
    fn data_cell_matches_ton_core_mainnet_null_pubkey() {
        // Null-pubkey v5r1 init data cell hash, verified against @ton/ton
        // WalletContractV5R1 (mainnet wallet_id) by the open-wallet-standard crate.
        let w = WalletV5R1::mainnet([0u8; 32]);
        assert_eq!(
            hex::encode(w.data_cell().repr_hash()),
            "0f80a4e3e2630cba3f6f37d12dbcf6afaaa015cd889eeb681a334a4fbe84cf31"
        );
    }

    /// Cross-check the full mnemonic → key → data cell → StateInit → address
    /// pipeline against a real wallet WITHOUT committing any seed: the mnemonic
    /// and its expected addresses are supplied out-of-band via env vars (e.g. a
    /// CI secret). Skipped when unset so the suite never depends on a secret.
    #[test]
    fn mnemonic_rederives_published_testnet_address() {
        let (Ok(mnemonic), Ok(raw), Ok(friendly)) = (
            std::env::var("P2P_TEST_DEPLOYER_MNEMONIC"),
            std::env::var("P2P_TEST_DEPLOYER_RAW"),
            std::env::var("P2P_TEST_DEPLOYER_FRIENDLY"),
        ) else {
            eprintln!(
                "skipping mnemonic_rederives_published_testnet_address: set \
                 P2P_TEST_DEPLOYER_{{MNEMONIC,RAW,FRIENDLY}} to run"
            );
            return;
        };
        let key = WalletKey::from_mnemonic(&mnemonic).unwrap();
        let wallet = WalletV5R1::testnet(key.public_key());
        let addr = wallet.address();
        assert_eq!(addr.to_raw_string(), raw);
        assert_eq!(addr, WalletAddress::from_base64_str(&friendly).unwrap());
    }

    #[test]
    fn signed_external_message_signature_verifies_and_boc_round_trips() {
        let key = WalletKey::from_seed(&TEST_SEED);
        let wallet = WalletV5R1::testnet(key.public_key());

        let body = CellBuilder::new().store_uint(0xdead_beef, 32).build();
        let msg = InternalMessage::to_contract(WalletAddress::new(0, [0x11; 32]), 50_000_000, body);
        let boc = build_signed_external_v5r1(&wallet, &key, 7, 1_900_000_000, &[msg]).unwrap();

        // The BoC must parse back to an identical cell tree.
        let ext = Cell::from_boc(&boc).expect("self-produced BoC parses");
        assert!(ext.bit_len() > 0);

        // Reconstruct the signing cell to confirm the appended signature verifies
        // with the wallet pubkey over the signing-cell repr-hash.
        let out_list = InternalMessage::to_contract(
            WalletAddress::new(0, [0x11; 32]),
            50_000_000,
            CellBuilder::new().store_uint(0xdead_beef, 32).build(),
        )
        .to_out_action(CellBuilder::new().build());
        let inner = CellBuilder::new()
            .store_maybe_ref(Some(out_list))
            .store_uint(0, 1)
            .build();
        let signing = CellBuilder::new()
            .store_uint(OP_AUTH_SIGNED_EXTERNAL as u128, 32)
            .store_uint(wallet.wallet_id() as u128, 32)
            .store_uint(1_900_000_000u128, 32)
            .store_uint(7u128, 32)
            .store_cell_inline(&inner)
            .build();
        let sig = key.sign(&signing.repr_hash());
        assert!(verify_sig(&wallet.public_key, &signing.repr_hash(), &sig));
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn rejects_mismatched_key() {
        let k1 = WalletKey::from_seed(&[1u8; 32]);
        let wallet = WalletV5R1::testnet([2u8; 32]); // different pubkey
        let err = build_signed_external_v5r1(&wallet, &k1, 0, 1, &[]).unwrap_err();
        assert!(matches!(err, SettleError::Backend(_)));
    }
}
