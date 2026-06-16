#!/usr/bin/env bash
# Library tier — zero-config local-first metadata invariants (executed_locally /
# verified / quorum=0 / fallback / remote-only / paid-gate) + persistent trust
# survival across restart. These are the QueryOutcome invariants the extension
# SQL surface does not expose.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
LOG="$LOGDIR/units_zeroconfig.log"
LOG2="$LOGDIR/units_persistent_trust.log"
echo "=================== LIBRARY — zero_config + persistent_trust ==================="
echo "==> cargo test -p p2p-node --test zero_config"
run_cargo_suite p2p-node zero_config "$LOG"
grep -E "test result:" "$LOG" | tail -n1 | sed 's/^/    /'

cargo_assert QRY-LOCAL-01            "$LOG" zero_config_query_just_works
cargo_assert QRY-LOCAL-02            "$LOG" per_call_prefer_local_forces_free_local_even_with_seeds
cargo_assert QRY-REMOTE-FALLBACK-01-lib "$LOG" auto_with_unreachable_grid_falls_back_to_local
cargo_assert QRY-REMOTE-ONLY-NOCAND-01-lib "$LOG" remote_only_node_does_not_fall_back_to_local
cargo_assert QRY-PAYMENT-PAID-NOWALLET-01-lib "$LOG" paid_without_wallet_returns_friendly_error
cargo_assert QRY-PAYMENT-PAID-WALLET-01 "$LOG" paid_with_wallet_passes_the_gate

echo "==> cargo test -p p2p-node --test persistent_trust"
run_cargo_suite p2p-node persistent_trust "$LOG2"
grep -E "test result:" "$LOG2" | tail -n1 | sed 's/^/    /'
cargo_assert TRU-PERSIST-01          "$LOG2" persistent_trust_store_survives_restart_via_config_path

finish "LIBRARY zero_config + persistent_trust"