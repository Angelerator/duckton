#!/usr/bin/env python3
"""Generate a docker-compose file for a HETEROGENEOUS P2P DuckDB grid.

Role-based, self-documenting service names so each scenario is reproducible. Each
role drives a different behavior purely through env/config the node already
supports (no invented product features):

  * `seed-1..S`          — bootstrap/seed mesh (serve `public`; bootstrap each
                           other). The stable rendezvous points.
  * `honest-worker-N`    — the workhorses: serve `public`, bootstrap the seeds.
                           (A public-only host is exactly a "free-only host".)
  * `internal-host-N`    — serve `internal,sensitive` ONLY → they REFUSE a
                           `public` offer (data-class routing). A public job sent
                           only to these surfaces InsufficientWorkers.
  * `oom-worker-N`       — a deliberately tiny donated budget (`P2P_SHARE_MEMORY`
                           below the requester's per-job lease) → admission
                           rejects every offer "at capacity" (ResourceExceeded
                           class). A job sent only to these surfaces
                           InsufficientWorkers.
  * `remote-only-node-N` — `planner.local_execution_enabled=false` → never runs a
                           query locally; with no reachable grid it surfaces
                           NoCandidates instead of a local fallback.

Behaviors the live node/extension has NO knob for are NOT faked here and are
proven at the library tier instead (see docker/SCENARIOS.md / REPORT.md):
  * cheating / wrong-result, slow/stalling, equivocating workers — the live
    extension always runs real SQL via HostEngine (no fault-injection env);
  * `staked-host` / `l2-host` — the live node wires no stake registry and emits
    L0 attestation (measured attestation / bonded stake are not env-settable);
  * `blocked-actor` — blocking is applied at RUNTIME via `p2p_block` on the
    requester (a deny-list entry), not baked into a host image.

Every container is memory/CPU/pids capped and hardened (read-only root, no-new-
privileges, all caps dropped, tmpfs for writable dirs). The `/node/state` tmpfs
MUST be owned by uid/gid 1001 (the Dockerfile's node user) or the node cannot
write runtime.toml. Workers depend_on the seeds being healthy before starting.

Usage:
  gen_compose.py --seeds 3 --honest 8 --internal 2 --oom 2 --remote-only 1 \\
                 --mem 256m --cpus 0.4 --out docker/compose.generated.yml
"""
import argparse
import yaml


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--seeds", type=int, default=3, help="bootstrap seed nodes")
    p.add_argument("--honest", type=int, default=8, help="honest public workers")
    p.add_argument("--internal", type=int, default=2, help="internal/sensitive-only hosts")
    p.add_argument("--oom", type=int, default=2, help="tiny-budget hosts (admission-reject)")
    p.add_argument("--remote-only", type=int, default=1, help="remote-only (no local fallback) nodes")
    p.add_argument("--mem", default="256m", help="per-container memory limit")
    p.add_argument("--cpus", default="0.4", help="per-container CPU limit")
    p.add_argument("--pids", type=int, default=64, help="per-container pids limit")
    # Donated budget (admission accounting). MUST exceed the per-job memory lease
    # the requester dispatches (64 MiB) or every offer is rejected "at capacity".
    p.add_argument("--share-mem", default="512MB", help="donated memory budget (healthy hosts)")
    # The oom-worker's budget is intentionally BELOW the 64 MiB requester lease so
    # admission fails — this is the only honest way to surface a resource refusal.
    p.add_argument("--oom-share-mem", default="32MB", help="tiny donated budget (oom-worker)")
    p.add_argument("--max-jobs", type=int, default=4, help="per-node concurrent job slots")
    p.add_argument("--image", default="p2p-node:latest")
    p.add_argument("--network", default="grid")
    p.add_argument("--out", default="docker/compose.generated.yml")
    args = p.parse_args()

    seeds = [f"seed-{i}" for i in range(1, args.seeds + 1)]
    seed_boot = ",".join(f"quic://{s}:9494" for s in seeds)

    security_opt = ["no-new-privileges:true"]
    cap_drop = ["ALL"]
    tmpfs_mounts = [
        "/tmp:exec,mode=1777",
        "/node/state:exec,mode=0700,uid=1001,gid=1001",
    ]

    def make_service(name, *, role, bootstrap, is_seed, data_classes="public",
                     share_mem=None, local_exec=None):
        env = {
            "P2P_BIND_ADDR": "0.0.0.0:9494",
            "P2P_SHARE_MEMORY": share_mem or args.share_mem,
            "P2P_SHARE_MAXJOBS": str(args.max_jobs),
            "P2P_SHARE_DATA_CLASSES": data_classes,
            "P2P_ROLE": role,
        }
        if bootstrap:
            env["BOOTSTRAP"] = bootstrap
        if local_exec is not None:
            env["P2P_PLANNER_LOCAL_EXEC"] = "true" if local_exec else "false"

        svc = {
            "image": args.image,
            "hostname": name,
            "environment": env,
            "networks": [args.network],
            "mem_limit": args.mem,
            "cpus": float(args.cpus),
            "pids_limit": args.pids,
            "restart": "no",
            "stop_grace_period": "2s",
            "read_only": True,
            "tmpfs": tmpfs_mounts,
            "security_opt": security_opt,
            "cap_drop": cap_drop,
        }
        if not is_seed and seeds:
            svc["depends_on"] = {s: {"condition": "service_healthy"} for s in seeds}
        return svc

    services = {}

    # Seeds bootstrap each other (excluding self) so the seed mesh is connected.
    for s in seeds:
        others = ",".join(f"quic://{o}:9494" for o in seeds if o != s)
        services[s] = make_service(s, role="seed", bootstrap=others, is_seed=True)

    # Honest public workers — the REMOTE-OK workhorses.
    for i in range(1, args.honest + 1):
        n = f"honest-worker-{i}"
        services[n] = make_service(n, role="honest-worker", bootstrap=seed_boot, is_seed=False)

    # Internal/sensitive-only hosts — refuse public offers (data-class routing).
    for i in range(1, args.internal + 1):
        n = f"internal-host-{i}"
        services[n] = make_service(
            n, role="internal-host", bootstrap=seed_boot, is_seed=False,
            data_classes="internal,sensitive",
        )

    # Tiny-budget hosts — admission rejects the per-job lease (resource refusal).
    for i in range(1, args.oom + 1):
        n = f"oom-worker-{i}"
        services[n] = make_service(
            n, role="oom-worker", bootstrap=seed_boot, is_seed=False,
            share_mem=args.oom_share_mem,
        )

    # Remote-only nodes — never fall back to local execution.
    for i in range(1, args.remote_only + 1):
        n = f"remote-only-node-{i}"
        services[n] = make_service(
            n, role="remote-only-node", bootstrap=seed_boot, is_seed=False,
            local_exec=False,
        )

    compose = {
        "services": services,
        "networks": {args.network: {"driver": "bridge"}},
    }

    total = len(services)
    header = (
        "# AUTO-GENERATED by docker/gen_compose.py — do not edit by hand.\n"
        f"# seeds={args.seeds} honest={args.honest} internal={args.internal} "
        f"oom={args.oom} remote_only={args.remote_only} total={total} "
        f"mem={args.mem} cpus={args.cpus}\n"
    )

    with open(args.out, "w") as f:
        f.write(header)
        yaml.safe_dump(compose, f, default_flow_style=False, sort_keys=False)

    print(
        f"wrote {args.out}: {len(seeds)} seeds + {args.honest} honest + "
        f"{args.internal} internal + {args.oom} oom + {args.remote_only} remote-only "
        f"= {total} containers"
    )


if __name__ == "__main__":
    main()
