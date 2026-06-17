#!/usr/bin/env bash
# Group J — Sandbox/Security on the free local HostEngine (single container).
#
# The extension's free local path opens a fresh in-process DuckDB with the SAME
# lockdown the node's strict engine applies: no LocalFileSystem, no external
# (network) access, no INSTALL/LOAD, and `lock_configuration=true` so an
# untrusted query cannot re-open any of it. We assert each lockdown via a real
# p2p_query and confirm NO sensitive data leaks back.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"
ensure_solo >/dev/null
echo "=================== GROUP J — SANDBOX/SECURITY (solo) ==================="

# SBX-LOCAL-FILE-01 — reading a local file is blocked; passwd never leaks.
out="$(solo_sql "FROM p2p_query('SELECT * FROM read_csv_auto(''/etc/passwd'')');")"
assert_have    SBX-LOCAL-FILE-01a "$out" "LocalFileSystem has been disabled"
assert_missing SBX-LOCAL-FILE-01b "$out" "root:"

# SBX-NET-EGRESS-01 — network egress (https reader) is blocked.
out="$(solo_sql "FROM p2p_query('SELECT * FROM read_csv_auto(''https://example.com/x.csv'')');")"
assert_re SBX-NET-EGRESS-01 "$out" 'LocalFileSystem has been disabled|external access|Network'

# SBX-RELOCK-01 — an untrusted query cannot re-open the lockdown via SET.
out="$(solo_sql "FROM p2p_query('SET enable_external_access=true');")"
assert_have SBX-RELOCK-01 "$out" "configuration has been locked"

# SBX-INSTALL-01 — INSTALL/LOAD of extensions is blocked.
out="$(solo_sql "FROM p2p_query('INSTALL httpfs');")"
assert_re SBX-INSTALL-01 "$out" 'disabled by configuration|autoinstall|not allowed|Permission'

# SBX-COPY-01 — COPY TO a local path is blocked (no exfiltration to disk).
out="$(solo_sql "FROM p2p_query('COPY (SELECT 1) TO ''/tmp/p2p_exfil.csv''');")"
assert_re SBX-COPY-01 "$out" 'LocalFileSystem has been disabled|disabled by configuration|Permission'

# SBX-ATTACH-01 — ATTACH a local database file is blocked.
out="$(solo_sql "FROM p2p_query('ATTACH ''attack.db''');")"
assert_re SBX-ATTACH-01 "$out" 'LocalFileSystem has been disabled|disabled by configuration|Permission'

# SBX-EXPORT-01 — EXPORT DATABASE (writes files) is blocked.
out="$(solo_sql "FROM p2p_query('EXPORT DATABASE ''/tmp/p2p_exp''');")"
assert_re SBX-EXPORT-01 "$out" 'LocalFileSystem has been disabled|disabled by configuration|Permission'

# SBX-GLOB-01 — filesystem globbing is blocked; passwd listing never leaks.
out="$(solo_sql "FROM p2p_query('SELECT count(*) FROM glob(''/**'')');")"
assert_re      SBX-GLOB-01a "$out" 'LocalFileSystem has been disabled|disabled by configuration|Permission'
assert_missing SBX-GLOB-01b "$out" "root:"

# SBX-* OS-backend / rlimit / seatbelt / scoped-secret cases (SBX-RLIMIT-01,
# SBX-BACKEND-*, SBX-EGRESS-*, SBX-FIXTURE-ALLOWED-01, SBX-SECRET-SCOPED-01,
# SBX-TEMPDIR-01, SBX-NOOP-WARN-01) are OS-sandbox library behaviors proven by
# cargo test -p p2p-node --test sandbox (see 32_units_sandbox.sh).

finish "GROUP J (Sandbox/Security)"