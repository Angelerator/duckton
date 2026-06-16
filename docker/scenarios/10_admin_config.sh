#!/usr/bin/env bash
# Group A — Admin/Config (single container; no swarm needed).
#
# Drives the extension's SQL admin surface via the real `duckdb` CLI in one
# container and asserts EXACT rows / values / error substrings. Each assertion
# prints `PASS <id>` / `FAIL <id> …`.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
ensure_solo >/dev/null
echo "=================== GROUP A — ADMIN/CONFIG (solo) ==================="

# ADM-INFO-01 — p2p_info: 6 rows; protocol/version/schema/alpn constants.
out="$(solo_sql "SELECT count(*) AS n FROM p2p_info(); SELECT key||'|'||value AS kv FROM p2p_info();")"
assert_re   ADM-INFO-01a "$out" '^6$'
assert_have ADM-INFO-01b "$out" "protocol_name|duckdb-p2p"
assert_have ADM-INFO-01c "$out" "protocol_version|1.0.0"
assert_have ADM-INFO-01d "$out" "schema_version|1"
assert_have ADM-INFO-01e "$out" "alpn|duckdb-p2p/1"

# ADM-PEERS-01 — p2p_peers shows discovery config rows.
out="$(solo_sql "SELECT kind||'|'||value FROM p2p_peers();")"
assert_have ADM-PEERS-01a "$out" "candidate_sample_size|16"
assert_have ADM-PEERS-01b "$out" "discovery_mode"

# ADM-PEERS-02 — corrupt P2P_CONFIG => single config_error row, LOAD survives.
docker exec "$SOLO" sh -c 'printf "this is not valid toml === {{{\n" > /tmp/corrupt.toml'
out="$(docker exec -e P2P_CONFIG=/tmp/corrupt.toml -e "P2P_CONFIG_DIR=/tmp/c-$RANDOM" "$SOLO" \
        duckdb -unsigned -list -c "LOAD '${EXT}'; SELECT kind FROM p2p_peers();" 2>&1)"
assert_have ADM-PEERS-02 "$out" "config_error"

# ADM-CONFIG-01 — p2p_config grouped + secrets redacted (a storage secret).
out="$(solo_sql "CALL p2p_set('storage.provider_options.s3.secret','topsecret'); SELECT \"group\"||'|'||key||'|'||value FROM p2p_config();")"
assert_have    ADM-CONFIG-01a "$out" "<redacted>"
assert_missing ADM-CONFIG-01b "$out" "topsecret"
assert_re      ADM-CONFIG-01c "$out" '^economics\|'

# ADM-STATUS-01 — default status: testnet / economics off / local+grid.
out="$(solo_sql "SELECT key||'|'||value FROM p2p_status();")"
assert_have ADM-STATUS-01a "$out" "network|testnet"
assert_have ADM-STATUS-01b "$out" "economics_enabled|false"
assert_have ADM-STATUS-01c "$out" "execution_mode|local+grid"

# ADM-ECON-01 — enable economics + settlement 'ton' (maps to onchain) w/ fee.
out="$(solo_sql "CALL p2p_economics(enabled=>true, settlement=>'ton', fee_recipient=>'EQtreasury');")"
assert_have ADM-ECON-01 "$out" "settlement|onchain"

# ADM-ECON-02 — onchain settlement requires fee_recipient (error).
out="$(solo_sql "CALL p2p_economics(enabled=>true, settlement=>'ton');")"
assert_have ADM-ECON-02 "$out" "fee_recipient must be set"

# ADM-ECON-03 — unknown settlement rejected.
out="$(solo_sql "CALL p2p_economics(enabled=>true, settlement=>'dogecoin');")"
assert_have ADM-ECON-03 "$out" "unknown settlement"

# ADM-ECON-04 — mock rail needs no fee_recipient.
out="$(solo_sql "CALL p2p_economics(enabled=>true, settlement=>'mock');")"
assert_have    ADM-ECON-04a "$out" "settlement|mock"
assert_missing ADM-ECON-04b "$out" "fee_recipient must be set"

# ADM-NET-01 — mainnet without confirm is blocked ("REAL TON").
out="$(solo_sql "CALL p2p_economics(network=>'mainnet');")"
assert_re ADM-NET-01 "$out" 'REAL TON|real ton'

# ADM-NET-02 — mainnet + confirm switches and records the opt-in.
out="$(solo_sql "CALL p2p_economics(network=>'mainnet', confirm=>true);")"
assert_have ADM-NET-02a "$out" "network|mainnet"
assert_have ADM-NET-02b "$out" "network_confirmed|true"

# ADM-NET-03 — leaving mainnet resets confirm (mainnet then errors again).
D="/tmp/net3-$RANDOM"
solo_sql_dir "$D" "CALL p2p_economics(network=>'mainnet', confirm=>true); CALL p2p_economics(network=>'testnet');" >/dev/null
out="$(solo_sql_dir "$D" "CALL p2p_economics(network=>'mainnet');")"
assert_re ADM-NET-03 "$out" 'REAL TON|real ton'

# ADM-NET-04 — per-network address isolation (testnet vs mainnet vaults coexist).
out="$(solo_sql "CALL p2p_contracts(stake_vault=>'kQtestVault'); CALL p2p_economics(network=>'mainnet', confirm=>true); CALL p2p_contracts(stake_vault=>'kQmainVault'); SELECT \"group\"||'|'||key||'|'||value FROM p2p_config();")"
assert_have ADM-NET-04a "$out" "kQtestVault"
assert_have ADM-NET-04b "$out" "kQmainVault"

# ADM-SET-01 — generic p2p_set auto-types (float).
out="$(solo_sql "CALL p2p_set('trust.min_trust','0.85'); SELECT key||'|'||value FROM p2p_config();")"
assert_have ADM-SET-01 "$out" "min_trust|0.85"

# ADM-SET-02 — unknown key rejected with 'unknown field'.
out="$(solo_sql "CALL p2p_set('economics.bogus_key','1');")"
assert_have ADM-SET-02 "$out" "unknown field"

# ADM-SET-03 — invariant violation (quorum>replicas) is NOT persisted.
D="/tmp/set3-$RANDOM"
err="$(solo_sql_dir "$D" "CALL p2p_set('scheduler.quorum','99');")"
out="$(solo_sql_dir "$D" "SELECT key||'|'||value FROM p2p_config() WHERE key='quorum';")"
assert_re   ADM-SET-03a "$err" 'quorum'
assert_have ADM-SET-03b "$out" "quorum|2"

# ADM-RESET-01 — p2p_config_reset restores defaults.
out="$(solo_sql "CALL p2p_set('trust.min_trust','0.95'); CALL p2p_config_reset(); SELECT key||'|'||value FROM p2p_config() WHERE key='min_trust';")"
assert_have ADM-RESET-01a "$out" "restored built-in defaults"
assert_have ADM-RESET-01b "$out" "min_trust|0.7"

# ADM-PLAN-01 — remote-only planner mode.
out="$(solo_sql "CALL p2p_planner(prefer=>'remote', local_execution=>false); SELECT key||'|'||value FROM p2p_status();")"
assert_have ADM-PLAN-01a "$out" "planner_prefer|remote"
assert_have ADM-PLAN-01b "$out" "remote-only (never executes locally)"

# ADM-PLAN-02 — bad prefer rejected.
out="$(solo_sql "CALL p2p_planner(prefer=>'elsewhere');")"
assert_have ADM-PLAN-02 "$out" "prefer"

# ADM-TRUST-01 — min_trust / min_attest.
out="$(solo_sql "CALL p2p_trust(min_trust=>0.8, min_attest=>'L1'); SELECT key||'|'||value FROM p2p_config();")"
assert_have ADM-TRUST-01a "$out" "min_trust|0.8"
assert_have ADM-TRUST-01b "$out" "min_attestation|L1"

# ADM-SEL-01 — replicas / quorum.
out="$(solo_sql "CALL p2p_selection(replicas=>5, quorum=>3); SELECT \"group\"||'|'||key||'|'||value FROM p2p_config();")"
assert_have ADM-SEL-01a "$out" "scheduler|replicas|5"
assert_have ADM-SEL-01b "$out" "scheduler|quorum|3"

# ADM-WALLET-01 — inline mnemonic stored only as a file ref + redacted everywhere.
out="$(solo_sql "CALL p2p_wallet(mnemonic=>'abandon abandon secret words'); SELECT \"group\"||'|'||key||'|'||value FROM p2p_config();")"
assert_have    ADM-WALLET-01a "$out" "mnemonic_file"
assert_missing ADM-WALLET-01b "$out" "abandon"

# ADM-WALLET-02 — mnemonic_file reference stored verbatim.
out="$(solo_sql "CALL p2p_wallet(mnemonic_file=>'/secure/outside/repo/mn.txt'); SELECT key||'|'||value FROM p2p_config();")"
assert_have ADM-WALLET-02 "$out" "/secure/outside/repo/mn.txt"

# ADM-CONTRACTS-01 — contracts registered under the active network (testnet).
out="$(solo_sql "CALL p2p_contracts(stake_vault=>'kQactive'); SELECT \"group\"||'|'||key||'|'||value FROM p2p_config();")"
assert_have ADM-CONTRACTS-01 "$out" "testnet.contracts.stake_vault|kQactive"

# ADM-GROUP-EMPTY-01 — no-arg setter returns current state (no error).
out="$(solo_sql "CALL p2p_economics();")"
assert_have    ADM-GROUP-EMPTY-01a "$out" "network|testnet"
assert_missing ADM-GROUP-EMPTY-01b "$out" "Error"

# ADM-PERSIST-01 — settings survive a "restart" (new process, same dir) + 0600.
D="/tmp/persist-$RANDOM"
solo_sql_dir "$D" "CALL p2p_set('trust.min_trust','0.9');" >/dev/null
out="$(solo_sql_dir "$D" "SELECT key||'|'||value FROM p2p_config() WHERE key='min_trust';")"
perm="$(docker exec "$SOLO" stat -c '%a' "$D/runtime.toml" 2>/dev/null)"
assert_have ADM-PERSIST-01a "$out" "min_trust|0.9"
assert_eq   ADM-PERSIST-01b "$perm" "600"

finish "GROUP A (Admin/Config)"