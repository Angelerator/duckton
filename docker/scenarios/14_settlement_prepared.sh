#!/usr/bin/env bash
# Group E — Settlement-from-extension (single container; prepared, no chain).
#
# Without `--features ton-live` the provider stake/unstake/admin actions are
# gated + "prepared" (never broadcast). We assert every gate's exact error and
# the prepared status. The PAID grid settlement (mock rail), the broken-commitment
# fine, and the GlobalParams overlay (SET-PAID-*, SET-FINE-*, SET-PARAMS-SYNC) are
# proven over real QUIC + the in-memory mock rail by cargo test -p p2p-node --test
# settlement_integration (see 06_settlement_units.sh + 05_economics_modes.sh).
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
ensure_solo >/dev/null
echo "=================== GROUP E — SETTLEMENT (prepared, solo) ==================="

ON="CALL p2p_economics(enabled=>true, settlement=>'ton', fee_recipient=>'EQtreasury');"
V="CALL p2p_contracts(stake_vault=>'kQvault');"
GP="CALL p2p_contracts(global_params=>'kQgp');"
W="CALL p2p_wallet(mnemonic_file=>'/secure/outside/repo/mn.txt');"

# SET-STAKE-AMT-01 — non-positive amount rejected (checked first).
out="$(solo_sql "CALL p2p_stake(amount=>0);")"
assert_have SET-STAKE-AMT-01 "$out" "amount must be a positive whole-TON value"

# SET-STAKE-NOECON-01 — staking requires on-chain settlement.
out="$(solo_sql "CALL p2p_stake(amount=>100);")"
assert_have SET-STAKE-NOECON-01 "$out" "requires on-chain settlement"

# SET-STAKE-NOVAULT-01 — no stake_vault registered.
out="$(solo_sql "$ON CALL p2p_stake(amount=>100);")"
assert_have SET-STAKE-NOVAULT-01 "$out" "no stake_vault contract registered"

# SET-STAKE-NOWALLET-01 — no wallet configured.
out="$(solo_sql "$ON $V CALL p2p_stake(amount=>100);")"
assert_have SET-STAKE-NOWALLET-01 "$out" "no wallet configured"

# SET-STAKE-01 — fully configured: prepared status (no broadcast).
out="$(solo_sql "$ON $V $W CALL p2p_stake(amount=>100);")"
assert_have SET-STAKE-01 "$out" "stake|status|prepared"

# SET-UNSTAKE-01 — unstake prepared status.
out="$(solo_sql "$ON $V $W CALL p2p_unstake(amount=>100);")"
assert_have SET-UNSTAKE-01 "$out" "stake|status|prepared"

# SET-ADMIN-NOGP-01 — admin params needs a global_params contract.
out="$(solo_sql "$ON $W CALL p2p_admin_params();")"
assert_have SET-ADMIN-NOGP-01 "$out" "no global_params contract registered"

# SET-ADMIN-01 — admin params prepared status.
out="$(solo_sql "$ON $GP $W CALL p2p_admin_params();")"
assert_have SET-ADMIN-01 "$out" "admin_params|status|prepared"

# SET-SECRET-REDACT-01 — an inline secret is never echoed by a stake action.
WSEC="CALL p2p_wallet(mnemonic=>'abandon abandon settlement secret');"
out="$(solo_sql "$ON $V $WSEC CALL p2p_stake(amount=>100);")"
assert_have    SET-SECRET-REDACT-01a "$out" "stake|status|prepared"
assert_missing SET-SECRET-REDACT-01b "$out" "abandon"

# SET-STAKE-MAINNET-01 — NOTE: the mainnet guard is enforced at config-set time
# (the network switch itself is blocked without confirm), so an unconfirmed-mainnet
# stake state is unreachable via SQL; the guard is unit-proven in config tests.

finish "GROUP E (Settlement prepared)"