import "@acton/io"
import "@contracts/anchor_types"

// MERKLE v2 reference hash vectors (BLOCKCHAIN_ECONOMICS §7.2 / A7). The Rust side
// (crates/settlement/src/merkle.rs) is pinned to these EXACT values. Printed as
// lowercase hex (pad to 64 nibbles for a uint256 on the consuming side).
fun main() {
    // (a) hashLeaf(0x1111), (b) hashLeaf(0x2222)
    println("hashLeaf(0x1111)=0x{:x}", hashLeaf(0x1111));
    println("hashLeaf(0x2222)=0x{:x}", hashLeaf(0x2222));
    // (c) hashPair(hashLeaf(0x1111), hashLeaf(0x2222))
    println("hashPair(hashLeaf(0x1111),hashLeaf(0x2222))=0x{:x}", hashPair(hashLeaf(0x1111), hashLeaf(0x2222)));
    // (d) 4-leaf ROOT over [0x1111,0x2222,0x3333,0x4444]
    val root = hashPair(hashPair(hashLeaf(0x1111), hashLeaf(0x2222)), hashPair(hashLeaf(0x3333), hashLeaf(0x4444)));
    println("ROOT4[0x1111,0x2222,0x3333,0x4444]=0x{:x}", root);
    // bonus references kept for the unchanged internal-node hash
    println("hashPair(0,0)=0x{:x}", hashPair(0, 0));
    println("hashLeaf(0x3333)=0x{:x}", hashLeaf(0x3333));
    println("hashLeaf(0x4444)=0x{:x}", hashLeaf(0x4444));
}
