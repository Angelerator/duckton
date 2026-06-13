import "@acton/io"
import "@contracts/anchor_types"

fun main() {
    println("HP_0x1111_0x2222={}", hashPair(0x1111, 0x2222));
    println("HP_0x3333_0x4444={}", hashPair(0x3333, 0x4444));
    println("ROOT={}", hashPair(hashPair(0x1111, 0x2222), hashPair(0x3333, 0x4444)));
    println("HP_0_0={}", hashPair(0, 0));
}
