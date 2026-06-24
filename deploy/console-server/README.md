# Live grid backend (console-server) on a VPS

This runs the **real** Duckton grid (`console-server`) on an always-on host and
exposes its live SSE metrics at a public URL (e.g. `https://live.duckton.com`)
via a Cloudflare Tunnel. The web console/homepage then stream live data from it
instead of falling back to the baked snapshot.

It is **read-only** in this configuration: only `/api/state`, `/api/stream`, and
`/api/health` are served. The `/api/query` endpoint (unauthenticated arbitrary
SQL) is disabled for public hosting via `CONSOLE_READONLY=1`.

## Prerequisites
- A small always-on Linux host (1–2 vCPU, 2 GB RAM is plenty) with Docker +
  Docker Compose.
- The `duckton.com` zone on Cloudflare (already set up).

## 1. Create a Cloudflare Tunnel
1. Cloudflare dashboard → **Zero Trust** → **Networks** → **Tunnels** → **Create a tunnel**.
2. Type **Cloudflared**, name it `duckton-live`, **Save**.
3. Copy the **token** from the shown `cloudflared ... run <TOKEN>` command.
4. Under **Public Hostnames**, add:
   - Subdomain: `live`  •  Domain: `duckton.com`
   - Service: **HTTP** → `http://console-server:8787`
   (The hostname resolves to `console-server` because both run on the same
   Compose network.)

## 2. Run it on the host
```bash
git clone https://github.com/Angelerator/duckton.git
cd duckton/deploy/console-server
echo "TUNNEL_TOKEN=<paste-token>" > .env
docker compose up -d --build      # first build compiles bundled DuckDB (~5–10 min)
```

Verify:
```bash
curl -s https://live.duckton.com/api/health     # -> ok
curl -s https://live.duckton.com/api/state | head -c 200
```

## 3. Point the site at it
Once `https://live.duckton.com/api/health` returns `ok`, the web app is
rebuilt with `NEXT_PUBLIC_LIVE_URL=https://live.duckton.com` and redeployed
(done from the repo: `NEXT_PUBLIC_LIVE_URL=https://live.duckton.com \
  infisical run --env=prod -- npm run deploy` in `web/`). The console header
will then show **LIVE** instead of **snapshot**, and the homepage grid stats go
live.

## Notes
- No inbound ports are opened on the VPS; cloudflared dials out to Cloudflare.
- Update: `git pull && docker compose up -d --build`.
- Logs: `docker compose logs -f console-server`.
- Resource use is bounded (a fixed loopback grid + ambient jobs every ~2.5s).
