import "@acton/io"
import "@contracts/escrow_types"

// Probe: emit a reference `EscrowSettle` body cell (and its participants
// `map<address, coins>` HashmapE) so the off-chain Rust cell model
// (crates/settlement/src/ton.rs) can be pinned byte-for-byte against the real
// Tolk/TON dictionary serialization. Run: `acton script scripts/_probe_dict.tolk`.

const KEY1 = 0x1111111111111111111111111111111111111111111111111111111111111111
const KEY2 = 0x2222222222222222222222222222222222222222222222222222222222222222
const KEY3 = 0x3333333333333333333333333333333333333333333333333333333333333333
const RESULT_HASH: uint256 = 0xABCDEF0123456789
const WINNER = 0x0202020202020202020202020202020202020202020202020202020202020202

fun dumpSettle(label: string, participants: map<address, coins>) {
    val winner = address.fromWorkchainAndHash(0, WINNER);
    // B1: the candidate set the requester committed to. For this byte-pinning
    // probe, include the winner + the participant keys so the body is well-formed.
    var candidates: map<address, uint1> = createEmptyMap();
    candidates.set(winner, 1);
    var pe = participants.findFirst();
    while (pe.isFound) {
        candidates.set(pe.getKey(), 1);
        pe = participants.iterateNext(pe);
    }
    val msg = EscrowSettle {
        queryId: 1,
        resultHash: RESULT_HASH,
        winner: winner,
        base: 60,
        winnerAmount: 60,
        platformFee: 2,
        participants: participants,
        candidates: candidates,
    };
    val c = msg.toCell();
    println("{}_HASH={}", label, c.hash());
    println("{}_TREE={:cell-tree}", label, c);
}

fun main() {
    // (0) empty dict
    var empty: map<address, coins> = createEmptyMap();
    dumpSettle("EMPTY", empty);

    // (1) single entry
    var one: map<address, coins> = createEmptyMap();
    one.set(address.fromWorkchainAndHash(0, KEY1), 1);
    dumpSettle("ONE", one);

    // (2) two entries
    var two: map<address, coins> = createEmptyMap();
    two.set(address.fromWorkchainAndHash(0, KEY1), 1);
    two.set(address.fromWorkchainAndHash(0, KEY2), 256);
    dumpSettle("TWO", two);

    // (3) three entries
    var three: map<address, coins> = createEmptyMap();
    three.set(address.fromWorkchainAndHash(0, KEY1), 1);
    three.set(address.fromWorkchainAndHash(0, KEY2), 256);
    three.set(address.fromWorkchainAndHash(0, KEY3), 0xDEAD);
    dumpSettle("THREE", three);
}
