#!/usr/bin/env bash
# Group B (remote path) — Query/Dispatch over the live heterogeneous swarm (QUIC).
#
# Asserts both observable RESULTS (correct value streamed back from real worker
# containers) and ERROR strings (NoCandidates / InsufficientWorkers / quorum
# invariant / WalletRequired) surfaced over the wire. The rich QueryOutcome
# metadata (verified / agreement>=2 / winner / receipts) is NOT exposed by the
# extension SQL surface, so those invariants are proven at the library tier
# (cargo test -p p2p-node --test scenarios; see 30_units_scenarios.sh).
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
echo "=================== GROUP B (remote) — QUERY/DISPATCH (swarm) ==================="

CL="$(ensure_client)"
Q="SELECT sum(i) AS s FROM range(1,1001) t(i)"; EXPECTED=500500
HB="$(boot_list $(honest_workers))"
IB="$(boot_list internal-host-1 internal-host-2)"
OB="$(boot_list oom-worker-1 oom-worker-2)"
DEAD="'quic://192.0.2.1:9494'"   # RFC5737 TEST-NET-1: routable syntax, unreachable

# value() — the lone all-digits line is the streamed query result.
value() { printf '%s' "$1" | grep -Ex '[0-9]+' | head -n1; }
# A remote query against a bootstrap set, combined stdout+stderr.
rq() { req_query_all "$CL" "$1" "$2" "${3:-}"; }
# A query with NO p2p_join (no grid targets at all).
nojoin() {
  docker exec -e P2P_BIND_ADDR=0.0.0.0:0 -e "P2P_CONFIG_DIR=/tmp/nj-$RANDOM" "$CL" \
    duckdb -unsigned -list -noheader -c "LOAD '${EXT}'; ${1}" 2>&1
}

# QRY-REMOTE-OK-01 — grid quorum success: correct value streamed from workers.
out="$(rq "$HB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>3, quorum=>2, min_trust=>0.0)")"
assert_eq QRY-REMOTE-OK-01 "$(value "$out")" "$EXPECTED"

# QRY-OV-REPLICAS-01 — per-call replicas/quorum honored (2/2) → correct value.
out="$(rq "$HB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>2, quorum=>2, min_trust=>0.0)")"
assert_eq QRY-OV-REPLICAS-01 "$(value "$out")" "$EXPECTED"

# QRY-REPLICAS-GT-AVAIL-01 — replicas > available truncates to available, still quorums.
out="$(rq "$HB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>50, quorum=>2, min_trust=>0.0)")"
assert_eq QRY-REPLICAS-GT-AVAIL-01 "$(value "$out")" "$EXPECTED"

# QRY-OV-INVALID-01 — quorum > replicas is rejected (cross-field invariant).
out="$(rq "$HB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>2, quorum=>5, min_trust=>0.0)")"
assert_re QRY-OV-INVALID-01 "$out" 'quorum \(5\) must be <= scheduler.replicas \(2\)'

# QRY-MINTRUST-EXCLUDES-ALL-01 — min_trust=0.99 excludes all → have 0.
out="$(rq "$HB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>3, quorum=>2, min_trust=>0.99)")"
assert_have QRY-MINTRUST-EXCLUDES-ALL-01 "$out" "not enough trustworthy workers: have 0, need quorum 2"

# QRY-REQUIRE-STAKED-NOREG-01 — require_staked_hosts fail-closed (no registry) → NoCandidates.
out="$(rq "$HB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>3, quorum=>2, min_trust=>0.0, require_staked_hosts=>true)")"
assert_have QRY-REQUIRE-STAKED-NOREG-01 "$out" "no hosts available to run this query on the grid"

# QRY-DATA-CLASS-ROUTE-MISMATCH-01 — public job to internal-only hosts → they refuse.
out="$(rq "$IB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>2, quorum=>2, min_trust=>0.0)")"
assert_have QRY-DATA-CLASS-ROUTE-MISMATCH-01 "$out" "not enough trustworthy workers: have 0, need quorum 2"

# HST-ADMIT-MEM-01 (live) — tiny-budget hosts reject the per-job lease on admission.
out="$(rq "$OB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>2, quorum=>2, min_trust=>0.0)")"
assert_have HST-ADMIT-MEM-01 "$out" "not enough trustworthy workers: have 0, need quorum 2"

# QRY-REMOTE-FALLBACK-01 — auto + unreachable grid → falls back to local execution.
out="$(rq "$DEAD" "SELECT s FROM p2p_query('$Q', prefer=>'auto', min_trust=>0.0)")"
assert_eq QRY-REMOTE-FALLBACK-01 "$(value "$out")" "$EXPECTED"

# QRY-PREFER-REMOTE-01 / QRY-REMOTE-ONLY-NOCAND-01 — prefer=remote, no grid → NoCandidates.
out="$(nojoin "SELECT s FROM p2p_query('$Q', prefer=>'remote', min_trust=>0.0)")"
assert_have QRY-PREFER-REMOTE-01 "$out" "no hosts available to run this query on the grid"
out="$(nojoin "CALL p2p_planner(prefer=>'remote', local_execution=>false); SELECT s FROM p2p_query('$Q', min_trust=>0.0)")"
assert_have QRY-REMOTE-ONLY-NOCAND-01 "$out" "no hosts available to run this query on the grid"

# QRY-PAYMENT-AUTO-PUBLIC-FREE-01 — payment auto + public → free path → correct value.
out="$(rq "$HB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>3, quorum=>2, min_trust=>0.0, payment=>'auto')")"
assert_eq QRY-PAYMENT-AUTO-PUBLIC-FREE-01 "$(value "$out")" "$EXPECTED"

# QRY-PAYMENT-PAID-NOWALLET-01 — economics on, no wallet/rail + paid → WalletRequired.
out="$(rq "$HB" "SELECT s FROM p2p_query('$Q', prefer=>'remote', replicas=>3, quorum=>2, min_trust=>0.0, payment=>'paid')" "CALL p2p_economics(enabled=>true, settlement=>'noop', default_payment=>'paid');")"
assert_have QRY-PAYMENT-PAID-NOWALLET-01 "$out" "no wallet/settlement configured"

finish "GROUP B remote (Query/Dispatch)"