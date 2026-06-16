// Storage + messages for the per-job escrow / HTLC settlement contract.
// See BLOCKCHAIN_ECONOMICS §4 (earning function), §6.2 (who gets paid), and the
// "HTLC-style escrow release keyed on the quorum result-hash" recommendation
// in §13. Non-custodial: the arbiter can only DIRECT payouts bounded by the
// locked escrow B and gated on the agreed result hash; it can never seize funds,
// and any leftover always refunds to the requester.
//
// Touched by PAID jobs only. Free jobs never open an escrow (§8.2).
//
// TRUST MODEL (B1) — a single compromised arbiter CANNOT redirect funds to
// itself. The requester PRE-COMMITS, at escrow-open time, to the exact set of
// payout-eligible candidates (the N workers it dispatched) via
// `EscrowTerms.candidatesHash`. The arbiter, at settle, only chooses WHICH of
// those requester-authorized candidates wins and the split — it can never name an
// arbitrary recipient (e.g. itself), because every payee (winner + each
// participant) is checked for membership in the committed set, and the arbiter is
// additionally barred from being the winner. So the arbiter supplies the quorum
// VERDICT (result hash + ranking within the committed set), while the fund-owner
// (requester) authorizes the RECIPIENT SET — neither party alone can move funds
// to an outside address.

import "common"

const OP_ESCROW_SETTLE = 0x4553_4302
const OP_ESCROW_REFUND = 0x4553_4303
const OP_ESCROW_TOPUP = 0x4553_4300
const OP_ESCROW_CLAIM = 0x4553_4304  // pull a bounced/queued payout (B2 bounce-safety)
const OP_ESCROW_PAYOUT = 0x4553_4305 // outgoing payout op tag (carried so bounces are identifiable)

struct EscrowTerms {
    // Platform-fee recipient. This is bound to the admin's authoritative
    // `GlobalParams.feeRecipient` AT OPEN (read via `get_fee_recipient`) and is
    // the ONLY address the platform fee can be paid to at settle — the arbiter
    // can never redirect the fee. See the "FEE ENFORCEMENT" trust note below.
    treasury: address     // platform-fee recipient (= admin GlobalParams.feeRecipient)
    expectedHash: uint256 // HTLC lock: settle must present this quorum result hash
    // B1: the requester's pre-committed authorization of the payout-eligible
    // candidate set. Computed off-chain as `hash(candidates dict cell)` over the
    // `map<address, uint1>` of the N dispatched workers' payout wallets
    // (`beginCell().storeDict(candidates).endCell().hash()`), and bound into the
    // escrow's terms (hence its deterministic address) at open. At settle, the
    // arbiter must present a candidate set that hashes to exactly this value, and
    // every payee must be a member — so the arbiter cannot pay an outside address.
    candidatesHash: uint256
    // GlobalParams version/seqno in force when this job was opened. Bound into
    // the escrow's terms (hence its deterministic address) so the host,
    // requester and coordinator all agree which ecosystem params governed the
    // job at settlement / dispute time. Appended last → additive layout.
    paramsVersion: uint32
    // FEE ENFORCEMENT (φ): the admin's authoritative platform-fee rate (basis
    // points) for `paramsVersion`, bound at open from `GlobalParams.get_platform_fee_bps`.
    // At settle the contract REQUIRES `platformFee == winnerAmount * platformFeeBps / BPS_DENOM`
    // (the winner's settled price `winnerAmount` is the fee `base`), paid to
    // `treasury` — so the platform's exact φ-cut is guaranteed on-chain and cannot
    // be shaved by the arbiter. Appended last → additive layout.
    //
    // TRUST MODEL (TON has no synchronous cross-contract storage read, so the
    // escrow cannot itself fetch the live `GlobalParams`): `treasury` +
    // `platformFeeBps` are bound into the terms (hence into the escrow's
    // DETERMINISTIC address) at open. Every honest party independently recomputes
    // the expected escrow address from the authoritative GlobalParams values
    // (`get_fee_recipient` / `get_platform_fee_bps` for the pinned `paramsVersion`)
    // and only funds/settles/anchors THAT address — exactly the established
    // pattern that already binds `candidatesHash` + `paramsVersion`. A
    // self-deployed escrow that points the fee at itself (or sets φ=0) has a
    // DIFFERENT address, so the requester never funds it and the workers never
    // accept its settle. RESIDUAL ASSUMPTION: the binder pinned the correct admin
    // values; a fully trustless variant would route settle through GlobalParams
    // (or carry a GlobalParams-signed (version→feeRecipient,φ) proof) so the
    // escrow could verify the bound values on-chain.
    platformFeeBps: uint16
}

struct EscrowStorage {
    requester: address  // who funded B and receives refunds
    arbiter: address    // quorum oracle / coordinator authorized to settle
    escrowAmount: coins // B — the requester's max bid, locked up front
    deadline: uint32    // refund-on-timeout deadline (unix secs)
    settled: bool
    terms: Cell<EscrowTerms>
    // B2 bounce-safety: a pull-withdrawal ledger. If a pushed payout (winner /
    // fee / participant / refund) BOUNCES because the recipient could not accept
    // it, `onBouncedMessage` credits the returned amount here (keyed by the
    // recipient = the bounced-from address) instead of letting it vanish. The
    // recipient (or anyone, on their behalf) later pulls it with `EscrowClaim`.
    // This converts the fragile fire-and-forget sends into a recoverable
    // push-then-pull pattern. Appended last → additive layout.
    pending: map<address, coins>
}

fun EscrowStorage.load(): EscrowStorage {
    return EscrowStorage.fromCell(contract.getData());
}

fun EscrowStorage.save(self) {
    contract.setData(self.toCell());
}

// Settle after the quorum verdict. `participants` maps each agreeing non-winner
// to its fixed participation commission (§6.2). `winnerAmount` is base + perf
// bonus. Any escrow not paid out refunds to the requester.
//
// B1: `candidates` is the requester-committed payout-eligible set. It MUST hash
// to `terms.candidatesHash`; the `winner` and every `participants` key MUST be a
// member; and `winner` MUST NOT be the arbiter. B4: `participants` is bounded to
// MAX_SETTLE_PARTICIPANTS so all payouts fit one action phase.
struct (0x4553_4302) EscrowSettle {
    queryId: uint64
    resultHash: uint256 // the agreed quorum hash — must equal terms.expectedHash
    winner: address
    // FREE-NODE POLICY: `base` is the QUOTED winning price and is the fee base —
    // the platform fee MUST equal `base * terms.platformFeeBps / BPS_DENOM`,
    // DECOUPLED from what the winner is actually paid. `winnerAmount` is the actual
    // payout to the winner: it is `base` when the winner is a wallet node, or `0`
    // when the winner is a free (walletless) node — in which case the base is left
    // in the escrow and refunded to the requester (the trailing carry-all refund).
    // Either way the platform still collects exactly φ·base. (For a wallet winner
    // `winnerAmount == base`.)
    base: coins
    winnerAmount: coins
    platformFee: coins
    participants: map<address, coins>
    candidates: map<address, uint1> // requester-committed eligible set (hashed-checked)
}

struct (0x4553_4303) EscrowRefund {
    queryId: uint64
}

struct (0x4553_4300) EscrowTopUp {
    queryId: uint64
}

// Pull a queued/bounced payout (B2). Anyone may trigger it; funds always go to
// the `recipient` that the credit is keyed under (so a stuck winner's payout can
// be re-delivered by a keeper without letting anyone redirect it).
struct (0x4553_4304) EscrowClaim {
    queryId: uint64
    recipient: address
}

// Outgoing payout body tag (B2). Carried on every pushed payout so a bounce is
// recognizable; the recipient is recovered from the bounce's sender address, the
// amount from the bounce's value — so the body only needs the op tag + queryId.
struct (0x4553_4305) EscrowPayout {
    queryId: uint64
}

struct EscrowState {
    escrowAmount: coins
    deadline: uint32
    settled: bool
    paramsVersion: uint32
}

// B1 helpers ----------------------------------------------------------------

// Canonical commitment over the requester-authorized candidate set: the repr
// hash of a cell holding the `map<address, uint1>`. TON dicts are key-sorted, so
// this is order-independent and the off-chain Rust builder reproduces it byte for
// byte (`beginCell().storeDict(candidates).endCell().hash()`).
fun candidatesCommitment(candidates: map<address, uint1>): int {
    return beginCell().storeDict(candidates.toLowLevelDict()).endCell().hash();
}
