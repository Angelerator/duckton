#!/usr/bin/env bash
# Scenario 2b — resilience guarantees against in-memory rails.
#
# The extension's live Node wires plain StaticDiscovery + free settlement, so the
# phi-accrual/SWIM dead-node exclusion and the paid FailedCommitment fine are
# proven deterministically by the library's resilience suite (real loopback QUIC,
# mock engine, in-memory stake registry — NO live TON, no per-node gas).
#
# Output is written to /tmp; only the summary is printed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LOG=/tmp/p2pgrid/resilience_test.log
mkdir -p /tmp/p2pgrid

echo "==> cargo test -p p2p-node --test resilience (in-memory rails; phi/SWIM + FailedCommitment fine)"
( cd "$ROOT" && cargo test -p p2p-node --test resilience -- --nocapture ) >"$LOG" 2>&1 || true

echo "==> result line:"
grep -E "test result:" "$LOG" | tail -n 1 | sed 's/^/    /'
echo "==> key cases:"
grep -E "^test (paid_broken_commitment_is_fined|phi_convicted_node_is_excluded_from_selection|host_job_timeout_abandons_and_redispatches|all_silent_redispatches_to_a_fresh_set|progress_stall_detected_redispatches) " "$LOG" \
  | sed 's/^/    /' || true
