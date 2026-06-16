#!/usr/bin/env bash
# Group B (local path) — Query/Dispatch on the free in-process HostEngine.
#
# Single container: with no bootstrap + local execution enabled, p2p_query runs
# locally and streams VARCHAR rows back. The extension exposes only the result
# ROWS (not the QueryOutcome metadata: executed_locally / verified / quorum=0),
# so those metadata invariants are asserted at the library tier
# (cargo test -p p2p-node --test zero_config); here we assert the observable rows.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
ensure_solo >/dev/null
echo "=================== GROUP B (local) — QUERY/DISPATCH ==================="

# QRY-LOCAL-01 — zero-config local-first returns the correct value.
out="$(solo_sql "SELECT x FROM p2p_query('SELECT 42 AS x');")"
assert_have QRY-LOCAL-01 "$out" "42"

# QRY-LOCAL-02 — prefer => 'local' returns the correct value.
out="$(solo_sql "SELECT s FROM p2p_query('SELECT sum(i) AS s FROM range(1,1001) t(i)', prefer=>'local');")"
assert_have QRY-LOCAL-02 "$out" "500500"

# QRY-NULLS-01 — heterogeneous types (NULL/int/decimal/bool) all render as VARCHAR.
out="$(solo_sql "FROM p2p_query('SELECT NULL AS a, 1 AS b, 1.5 AS c, true AS d');")"
assert_have QRY-NULLS-01a "$out" "true"
assert_have QRY-NULLS-01b "$out" "1.5"
assert_have QRY-NULLS-01c "$out" "NULL"

# QRY-PAGINATION-01 — >2048 rows are chunked across multiple output vectors and
# fully reassembled (count proves no rows dropped at the VECTOR_SIZE boundary).
out="$(solo_sql "SELECT count(*) FROM p2p_query('SELECT i FROM range(3000) t(i)');")"
assert_have QRY-PAGINATION-01 "$out" "3000"

# QRY-EMPTYCOLS-01 — NOTE: not live-triggerable. The synthesized "result" column
# branch fires only when the engine returns zero columns; the host DuckDB
# (duckdb-rs) always reports >=1 column (e.g. "Count"/"Success"), so this is a
# structural code path covered by the column-synthesis logic, not asserted here.

finish "GROUP B local (Query/Dispatch)"