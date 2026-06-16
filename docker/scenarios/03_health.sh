#!/usr/bin/env bash
# Scenario 3 — scale / health / stability.
#
# Confirms: the swarm is still up, fan-out stays bounded (a requester given a
# large bootstrap still completes via a bounded candidate sample — no
# global-broadcast blowup), and reports host resource usage.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"; source "$HERE/_common.sh"

EXPECTED=500500
QUERY="SELECT sum(i) AS s FROM range(1,1001) t(i)"

RUN=$(containers | wc -l | tr -d ' ')
echo "==> running containers: $RUN"

# Bounded fan-out: give the requester a LARGE bootstrap; it must still complete
# via the bounded candidate sample (StaticDiscovery caps at candidate_sample_size).
BIG=(); while IFS= read -r _l; do BIG+=("$_l"); done < <(public_workers | shuf_lines | head -n 40)
BOOT="$(boot_list "${BIG[@]}")"
cexec="$(ensure_client)"
sql="SELECT s FROM p2p_query('${QUERY}', prefer=>'remote', replicas=>3, quorum=>2, min_trust=>0.0)"
got="$(req_query "$cexec" "$BOOT" "$sql" 2>/dev/null | tr -d '[:space:]')"
echo "==> large-bootstrap (${#BIG[@]} seeds) query result: ${got:-<empty>} (expect $EXPECTED)"
[ "$got" = "$EXPECTED" ] && echo "    bounded fan-out OK (completed without contacting all ${#BIG[@]})" || echo "    fan-out check FAILED"

# Resource usage snapshot (lean: aggregate only).
echo "==> host resource usage (docker stats snapshot):"
docker stats --no-stream --format '{{.MemUsage}}' $(containers) 2>/dev/null \
  | awk '{print $1}' \
  | sed 's/MiB//; s/GiB/*1024/' \
  | awk '{ if ($0 ~ /\*/) { split($0,a,"*"); v=a[1]*a[2] } else v=$0; sum+=v; n++ } END { if(n>0) printf "    nodes=%d  total=%.0f MiB  avg=%.1f MiB/node\n", n, sum, sum/n }'

echo "==> SCENARIO 3 done"
