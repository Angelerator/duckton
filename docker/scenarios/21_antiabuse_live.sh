#!/usr/bin/env bash
# Group H (live) — Anti-abuse deny-list management surface (single container).
#
# The block/unblock/list surface + kind inference (b3:->NodeId, else Wallet) is
# fully observable via SQL and asserted here against an isolated blocklist.toml.
#
# The SELECTION-time effects (ABU-CAND-EXCLUDE / WORKER-REFUSE / AUTOBLOCK /
# RATELIMIT / COSTGATE / SIGNAL / FAULTATTR) are NOT live-observable through the
# extension: StaticDiscovery candidates from bootstrap URLs carry no node_id
# (TOFU), so a node-id block cannot match a bootstrap candidate at the requester.
# Those are proven at the library tier (cargo test -p p2p-node --test antiabuse;
# see 31_units_antiabuse.sh) where candidates have real node_ids.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
ensure_solo >/dev/null
echo "=================== GROUP H (live) — ANTI-ABUSE DENY-LIST ==================="

# ABU-BLOCK-01 — block a node id (b3: prefix => kind node_id).
D="/tmp/abu-$RANDOM"
out="$(solo_sql_dir "$D" "CALL p2p_block(id=>'b3:deadbeef', reason=>'cheating');")"
assert_have ABU-BLOCK-01a "$out" "result|blocked|b3:deadbeef"
assert_have ABU-BLOCK-01b "$out" "b3:deadbeef|node_id|cheating"

# ABU-BLOCK-WALLET-01 — a non-b3 id is inferred as a wallet.
D="/tmp/abu-$RANDOM"
out="$(solo_sql_dir "$D" "CALL p2p_block(id=>'EQwalletABC', reason=>'fraud');")"
assert_have ABU-BLOCK-WALLET-01 "$out" "EQwalletABC|wallet|fraud"

# ABU-BLOCK-EMPTY-01 — empty id is rejected.
out="$(solo_sql "CALL p2p_block(id=>'');")"
assert_have ABU-BLOCK-EMPTY-01 "$out" "an \`id\` (node_id or wallet) is required"

# ABU-UNBLOCK-01 — unblock removes (unblocked) and reports not_found otherwise.
D="/tmp/abu-$RANDOM"
out="$(solo_sql_dir "$D" "CALL p2p_block(id=>'b3:abc'); CALL p2p_unblock(id=>'b3:abc'); CALL p2p_unblock(id=>'b3:nope');")"
assert_have ABU-UNBLOCK-01a "$out" "result|unblocked|b3:abc"
assert_have ABU-UNBLOCK-01b "$out" "result|not_found|b3:nope"

# ABU-BLOCKLIST-VIEW-01 — the blocklist view lists current entries.
D="/tmp/abu-$RANDOM"
out="$(solo_sql_dir "$D" "CALL p2p_block(id=>'b3:v1', reason=>'r1'); CALL p2p_block(id=>'EQv2', reason=>'r2'); SELECT id||'|'||kind FROM p2p_blocklist();")"
assert_have ABU-BLOCKLIST-VIEW-01a "$out" "b3:v1|node_id"
assert_have ABU-BLOCKLIST-VIEW-01b "$out" "EQv2|wallet"

finish "GROUP H (Anti-abuse deny-list)"