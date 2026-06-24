# Live deployment (Azure VM)

The live grid backend and public seed node run on a single small Azure VM and
are exposed through Cloudflare DNS. This is what powers the **live** (non-snapshot)
stats on duckton.com / console.duckton.com and the bootstrap entry point for peers.

## Topology
- **VM**: Azure `Standard_B2ls_v2` (2 vCPU / 4 GiB) in `westus2`, Ubuntu 22.04, Docker.
  Public IP `20.57.152.157`. NSG: 22, 80, 443 TCP + 9494 UDP.
- **`console-server`** (read-only) → **Caddy** (auto Let's Encrypt) → `https://live.duckton.com`.
  The web app is built with `NEXT_PUBLIC_LIVE_URL=https://live.duckton.com`.
- **seed node** → QUIC/UDP `9494`, advertised at `20.57.152.157:9494`.
  - node id: `b3:fac9ec4a76149cf8ceda42f98080ffd5b92d600dbb426b510d20975315ad6b65`
  - peers join with: `CALL p2p_join(bootstrap => ['seed.duckton.com:9494']);`
    (or the raw `20.57.152.157:9494` / `quic://seed.duckton.com:9494`)

## DNS (Cloudflare, DNS-only / grey-cloud)
- `live.duckton.com` A → `20.57.152.157` (Caddy needs direct reach for TLS-ALPN).
- `seed.duckton.com` A → `20.57.152.157` (convenience alias).

## Operate
```bash
ssh azureuser@20.57.152.157
cd ~/duckton/deploy/vm
docker compose ps
docker compose logs -f console-server   # or seed / caddy
docker compose up -d --build             # update after a `git pull` / re-sync
```
