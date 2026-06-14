// Storage layouts, message bodies and address helpers for the per-node stake
// contract (`StakeVault`) and its non-transferable stake-receipt jetton
// (`StakeReceiptWallet`). See BLOCKCHAIN_ECONOMICS §8 (staking/slashing/unbonding)
// and §8.5 (transfer-locked TEP-74 stake-receipt jetton).

import "common"
import "@acton/tolk-stdlib/tvm-dicts"

// ---------------------------------------------------------------------------
// Custom opcodes (distinct 32-bit tags)
// ---------------------------------------------------------------------------

const OP_TOP_UP = 0x534b_4b00
const OP_STAKE_DEPOSIT = 0x534b_4b01
const OP_STAKE_UNBOND = 0x534b_4b02
const OP_STAKE_WITHDRAW = 0x534b_4b03
const OP_STAKE_KEEPER = 0x534b_4b04
const OP_STAKE_SLASH = 0x534b_4b05
const OP_RECEIPT_BURN = 0x534b_4b06
// Mint uses the STANDARD TEP-74 internal_transfer op (0x178d4519), not a custom
// one, so indexers recognize it; see `JettonInternalTransfer` below.

// ---------------------------------------------------------------------------
// Stake-receipt jetton wallet (per-holder, TEP-74 shaped, transfer-LOCKED)
// ---------------------------------------------------------------------------

struct ReceiptWalletStorage {
    status: uint4   // bit 0 = transfers locked (always set: the receipt is non-transferable)
    balance: coins  // receipt units held (1:1 with bonded TON)
    owner: address  // node operator
    master: address // the StakeVault that minted this receipt
}

fun ReceiptWalletStorage.load(): ReceiptWalletStorage {
    return ReceiptWalletStorage.fromCell(contract.getData());
}

fun ReceiptWalletStorage.save(self) {
    contract.setData(self.toCell());
}

// Mint — master-only, via the STANDARD TEP-74 `internal_transfer` opcode so
// indexers/wallets recognize the credit as a real jetton mint (TEP-74 §"Jetton
// wallet"). The receipt is non-transferable, so the ONLY legitimate sender is the
// master vault: there are never sibling-wallet transfers (the outgoing transfer
// op is locked below). Standard body layout = query_id, amount, from,
// response_address, forward_ton_amount, forward_payload.
struct (0x178d_4519) JettonInternalTransfer {
    queryId: uint64
    amount: coins
    from: address
    responseAddress: address
    forwardTonAmount: coins
    forwardPayload: RemainingBitsAndRefs
}

// TEP-74 `transfer_notification` — emitted to the owner on mint when a forward
// amount is attached, so wallet apps surface the incoming receipt. Recognition /
// UX only; it does NOT unlock transfers.
struct (0x7362_d09c) JettonTransferNotification {
    queryId: uint64
    amount: coins
    sender: address
    forwardPayload: RemainingBitsAndRefs
}

// TEP-89 (Jetton Wallet Discovery): a contract asks the master for the jetton
// wallet of `ownerAddress`; the master replies with `TakeWalletAddress`. Indexers
// and explorers rely on this to link holder wallets to this master.
struct (0x2c76_b973) ProvideWalletAddress {
    queryId: uint64
    ownerAddress: address
    includeAddress: bool
}

// TEP-89 reply. `ownerAddress` is a Maybe ^MsgAddress (a `cell?`): present iff the
// request set `includeAddress`.
struct (0xd173_5400) TakeWalletAddress {
    queryId: uint64
    walletAddress: address
    ownerAddress: cell?
}

// TEP-74 outgoing transfer — always rejected for the stake receipt (anti-exit).
struct (0x0f8a7ea5) JettonTransfer {
    queryId: uint64
    amount: coins
    destination: address
    responseDestination: address?
    customPayload: cell?
    forwardTonAmount: coins
    forwardPayload: RemainingBitsAndRefs
}

// Master-driven burn (on withdraw or slash). Only the StakeVault may send it.
struct (0x534b_4b06) ReceiptBurn {
    queryId: uint64
    amount: coins
}

struct (0x534b_4b00) ReceiptTopUp {
    queryId: uint64
}

// Getter shape for `get_wallet_data` (TEP-74).
struct ReceiptWalletData {
    balance: coins
    owner: address
    minter: address
    code: cell
}

// ---------------------------------------------------------------------------
// Stake vault (per node) — also the receipt jetton master
// ---------------------------------------------------------------------------

// Slash split + payout targets, kept in a child cell (bit budget).
struct SlashConfig {
    treasury: address
    redundancyPool: address
    splitChallengerBps: uint16
    splitRedundancyBps: uint16
    splitBurnBps: uint16
    splitTreasuryBps: uint16
}

struct VaultConfig {
    minStake: coins         // eligibility threshold (0 = free tier, public only)
    unbondingPeriod: uint32 // cooldown secs; MUST be >= challengeWindow (validated off-chain)
    challengeWindow: uint32 // optimistic-settlement window secs
    keeperGrace: uint32     // extra secs after cooldown before a keeper may act
    keeperBountyBps: uint16 // keeper reward (bps of the withdrawn amount)
    slasher: address        // authorized slasher (the dispute contract)
    receiptWalletCode: cell // code of StakeReceiptWallet (for deterministic mint)
    slash: Cell<SlashConfig>
}

struct VaultStorage {
    owner: address         // node operator wallet
    staked: coins          // currently bonded
    unbondingAmount: coins // in cooldown (still slashable)
    unbondingAt: uint32    // unix ts the cooldown started (0 = none)
    totalSupply: coins     // receipts outstanding (== staked + unbondingAmount)
    bindingHash: uint256   // wallet<->node identity binding hash (§3.2)
    config: Cell<VaultConfig>
}

fun VaultStorage.load(): VaultStorage {
    return VaultStorage.fromCell(contract.getData());
}

fun VaultStorage.save(self) {
    contract.setData(self.toCell());
}

// Vault messages
struct (0x534b_4b01) StakeDeposit {
    queryId: uint64
    amount: coins // how much of the attached value to bond + mint as receipt
}

struct (0x534b_4b02) StakeRequestUnbond {
    queryId: uint64
    amount: coins
}

struct (0x534b_4b03) StakeWithdraw {
    queryId: uint64
}

struct (0x534b_4b04) StakeKeeperWithdraw {
    queryId: uint64
}

struct (0x534b_4b05) StakeSlash {
    queryId: uint64
    amount: coins
    reason: uint8       // SlashReason discriminant (mirrors §8.3)
    challenger: address // receives the challenger bounty share
}

// Getter shapes
struct VaultState {
    staked: coins
    unbondingAmount: coins
    unbondingAt: uint32
    totalSupply: coins
    eligible: bool // staked >= minStake AND no unbonding in progress
}

// TEP-74 `get_jetton_data` shape for the stake-receipt jetton (the vault is the
// master). `mintable` is true (the vault mints 1:1 on deposit); `adminAddress`
// is the vault itself (master == admin); `jettonContent` is the TEP-64 on-chain
// metadata (Duckton); `jettonWalletCode` is the receipt wallet code.
struct ReceiptJettonData {
    totalSupply: coins
    mintable: bool
    adminAddress: address
    jettonContent: cell
    jettonWalletCode: cell
}

// ---------------------------------------------------------------------------
// TEP-64 on-chain metadata for the "Duckton" stake-receipt jetton (§8.5).
//
// Gives the (non-transferable) receipt human-readable on-chain metadata so
// wallets/explorers show it: name "Duckton", symbol "DUCKTON", decimals 9, a
// short description. The transfer lock is unchanged — this is metadata only.
// Layout: on-chain content cell = `0x00` prefix + dict(sha256(key) -> ^snake),
// each value a snake-format cell (`0x00` prefix + utf-8 bytes), per TEP-64.
// ---------------------------------------------------------------------------

const RECEIPT_JETTON_DECIMALS = 9

// Wrap a UTF-8 value into a TEP-64 snake-format content cell.
fun snakeContentCell(value: slice): cell {
    return beginCell().storeUint(0, 8).storeSlice(value).endCell();
}

// Build the TEP-64 on-chain content cell for the Duckton receipt jetton.
fun buildReceiptJettonContent(): cell {
    var content: dict = createEmptyDict();
    content.uDictSetRef(256, "name".sha256(), snakeContentCell("Duckton".literalSlice()));
    content.uDictSetRef(256, "symbol".sha256(), snakeContentCell("DUCKTON".literalSlice()));
    content.uDictSetRef(256, "decimals".sha256(), snakeContentCell("9".literalSlice()));
    content.uDictSetRef(
        256,
        "description".sha256(),
        snakeContentCell(
            "Non-transferable 1:1 stake-receipt for a bonded duckdb-p2p node (slashable accountability bond)."
                .literalSlice(),
        ),
    );
    return beginCell().storeUint(0, 8).storeDict(content).endCell();
}

// ---------------------------------------------------------------------------
// Deterministic receipt-wallet address (mirrors the TEP-74 jetton pattern)
// ---------------------------------------------------------------------------

fun calcDeployedReceiptWallet(
    owner: address,
    master: address,
    walletCode: cell,
): AutoDeployAddress {
    val empty: ReceiptWalletStorage = {
        status: RECEIPT_STATUS_LOCKED,
        balance: 0,
        owner: owner,
        master: master,
    };
    return { workchain: BASECHAIN, stateInit: { code: walletCode, data: empty.toCell() } };
}

fun calcReceiptWalletAddress(owner: address, master: address, walletCode: cell): address {
    return calcDeployedReceiptWallet(owner, master, walletCode).calculateAddress();
}
