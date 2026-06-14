#!/usr/bin/env bash
# Tear the swarm down.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
PROJECT="${PROJECT:-p2pgrid}"
COMPOSE="$HERE/compose.generated.yml"
docker compose -p "$PROJECT" -f "$COMPOSE" down --remove-orphans -t 2 >/tmp/p2pgrid_down.log 2>&1 || true
echo "==> swarm '$PROJECT' down ($(docker ps --filter "label=com.docker.compose.project=${PROJECT}" -q | wc -l | tr -d ' ') containers remaining)"
