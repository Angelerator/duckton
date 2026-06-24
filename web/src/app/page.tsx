"use client";

import * as React from "react";
import Link from "next/link";
import {
  Activity,
  ArrowRight,
  Coins,
  Cpu,
  Gauge,
  Server,
  ShieldCheck,
  Lock,
  Network,
} from "lucide-react";
import { num } from "@/lib/format";
import { meta } from "@/lib/data";
import { useRealNet, shortId } from "@/lib/real-net";

const YELLOW = "#FFD400";

const DOCS_URL = "https://docs.duckton.com/";
const CONSOLE_URL = "https://console.duckton.com/";
const GITHUB_URL = "https://github.com/Angelerator/duckton";

/** Mainnet GlobalParams address. Defaults to the live deployment; override at
 *  build time via NEXT_PUBLIC_MAINNET_GLOBALPARAMS (e.g. a future redeploy). */
const MAINNET_GP =
  process.env.NEXT_PUBLIC_MAINNET_GLOBALPARAMS?.trim() ||
  "EQCV59kSoDDgmE8cheBNYwl2oYL9h5nkbyCqn99tN-N1w9Gg";

function DuckMark({ className = "" }: { className?: string }) {
  return (
    // eslint-disable-next-line @next/next/no-img-element
    <img
      src="/duckton-logo.png"
      alt="Duckton"
      className={`rounded-[22%] ${className}`}
    />
  );
}

function NavLink({ href, children }: { href: string; children: React.ReactNode }) {
  return (
    <a
      href={href}
      target={href.startsWith("http") ? "_blank" : undefined}
      rel="noreferrer"
      className="text-white/70 transition-colors hover:text-white"
    >
      {children}
    </a>
  );
}

function TopBar() {
  return (
    <header className="sticky top-0 z-30 border-b border-white/10 bg-[#0a0a0b]/80 backdrop-blur">
      <div className="mx-auto flex h-16 max-w-6xl items-center gap-3 px-5">
        <Link href="/" className="flex items-center gap-2.5">
          <DuckMark className="size-8" />
          <span className="text-lg font-semibold tracking-tight text-white">Duckton</span>
        </Link>
        <nav className="ml-auto hidden items-center gap-7 text-sm md:flex">
          <NavLink href="#what">What it is</NavLink>
          <NavLink href="#how">How it works</NavLink>
          <NavLink href="#connect">Connect</NavLink>
          <NavLink href={DOCS_URL}>Docs</NavLink>
          <NavLink href={GITHUB_URL}>GitHub</NavLink>
        </nav>
        <a
          href={CONSOLE_URL}
          target="_blank"
          rel="noreferrer"
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg px-4 py-2 text-sm font-semibold text-black transition-transform hover:scale-[1.03] md:ml-7"
          style={{ background: YELLOW }}
        >
          Open console <ArrowRight className="size-4" />
        </a>
      </div>
    </header>
  );
}

function Hero() {
  return (
    <section className="bg-grid relative overflow-hidden">
      <div
        className="pointer-events-none absolute -top-40 left-1/2 size-[640px] -translate-x-1/2 rounded-full opacity-20 blur-3xl"
        style={{ background: `radial-gradient(circle, ${YELLOW}, transparent 60%)` }}
      />
      <div className="mx-auto max-w-6xl px-5 py-20 md:py-28">
        <span
          className="inline-flex items-center gap-2 rounded-full border px-3 py-1 text-xs font-medium"
          style={{ borderColor: `${YELLOW}33`, color: YELLOW }}
        >
          <span className="size-1.5 rounded-full" style={{ background: YELLOW }} />
          Open source · Apache-2.0 · on the DuckDB Community Extensions registry
        </span>

        <h1 className="mt-6 max-w-3xl text-4xl font-bold leading-[1.05] tracking-tight text-white md:text-6xl">
          Squeeze the compute
          <br />
          you already{" "}
          <span style={{ color: YELLOW }}>own.</span>
        </h1>

        <p className="mt-6 max-w-2xl text-lg leading-relaxed text-white/70">
          Duckton turns idle laptops, desktops, and servers into a secure, peer-to-peer{" "}
          <span className="text-white">DuckDB</span> compute grid over QUIC. Queries run redundantly
          across independent nodes, results are verified byte-for-byte by a quorum, and nodes can
          optionally earn — settled directly on{" "}
          <span className="text-white">The Open Network (TON)</span>, with no central broker.
        </p>

        <div className="mt-8 flex flex-wrap items-center gap-3">
          <a
            href={DOCS_URL}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-2 rounded-lg px-5 py-3 text-sm font-semibold text-black transition-transform hover:scale-[1.03]"
            style={{ background: YELLOW }}
          >
            Read the docs <ArrowRight className="size-4" />
          </a>
          <a
            href={GITHUB_URL}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-2 rounded-lg border border-white/15 px-5 py-3 text-sm font-semibold text-white transition-colors hover:bg-white/5"
          >
            Star on GitHub
          </a>
        </div>

        <div className="mt-10 max-w-xl rounded-xl border border-white/10 bg-black/40 p-4 font-mono text-sm">
          <div className="text-white/40">{"# install + run a verified query, out of the box"}</div>
          <div className="mt-2 text-white/90">
            <span style={{ color: YELLOW }}>INSTALL</span> duckton <span style={{ color: YELLOW }}>FROM</span> community;
          </div>
          <div className="text-white/90">
            <span style={{ color: YELLOW }}>LOAD</span> duckton;
          </div>
          <div className="text-white/90">
            <span style={{ color: YELLOW }}>SELECT</span> * <span style={{ color: YELLOW }}>FROM</span>{" "}
            p2p_query(<span className="text-white/60">&apos;SELECT 42 AS x&apos;</span>);
          </div>
        </div>
      </div>
    </section>
  );
}

function StatCard({
  label,
  value,
  sub,
  icon,
}: {
  label: string;
  value: string;
  sub: string;
  icon: React.ReactNode;
}) {
  return (
    <div className="rounded-xl border border-white/10 bg-white/[0.02] p-4">
      <div className="flex items-center justify-between">
        <span className="text-xs font-medium text-white/50">{label}</span>
        <span style={{ color: YELLOW }} className="[&_svg]:size-4">
          {icon}
        </span>
      </div>
      <div className="mt-2 text-2xl font-semibold tabular-nums text-white">{value}</div>
      <div className="mt-1 text-xs text-white/40">{sub}</div>
    </div>
  );
}

const features = [
  {
    icon: <Cpu />,
    title: "Use what you already have",
    body: "Idle laptops, desktops, and servers become independent compute nodes — privately inside your company network, or across the world.",
  },
  {
    icon: <ShieldCheck />,
    title: "Verifiable by quorum",
    body: "Every query runs redundantly. Results are reduced to a canonical hash and accepted only when a configurable quorum agrees byte-for-byte.",
  },
  {
    icon: <Lock />,
    title: "Secure by design",
    body: "Mutually-authenticated QUIC (TLS 1.3), Ed25519 node identity, OS-sandboxed execution, and short-lived scoped credentials — hosts never hold your keys.",
  },
  {
    icon: <Coins />,
    title: "Earn on TON, no middleman",
    body: "Public jobs are free and fully off-chain. Paid jobs settle through a per-job on-chain escrow on TON — nodes set their own rates.",
  },
];

function WhatItIs() {
  return (
    <section id="what" className="mx-auto max-w-6xl px-5 py-20">
      <h2 className="text-3xl font-bold tracking-tight text-white md:text-4xl">
        A trustless query grid, in plain SQL
      </h2>
      <p className="mt-4 max-w-2xl text-white/60">
        No central broker sits in the data path. You broadcast a query; independent nodes execute it
        and cross-check each other. You rely on the answer, not on any single machine.
      </p>
      <div className="mt-10 grid gap-4 md:grid-cols-2">
        {features.map((f) => (
          <div
            key={f.title}
            className="rounded-2xl border border-white/10 bg-white/[0.02] p-6 transition-colors hover:border-white/20"
          >
            <div
              className="flex size-10 items-center justify-center rounded-lg [&_svg]:size-5"
              style={{ background: `${YELLOW}1a`, color: YELLOW }}
            >
              {f.icon}
            </div>
            <h3 className="mt-4 text-lg font-semibold text-white">{f.title}</h3>
            <p className="mt-2 text-sm leading-relaxed text-white/60">{f.body}</p>
          </div>
        ))}
      </div>
    </section>
  );
}

const steps = [
  { n: "01", t: "Fund & submit", d: "The requester broadcasts a query over QUIC and, for paid jobs, locks a bid in a per-job escrow on TON." },
  { n: "02", t: "Execute & read", d: "Independent nodes run the query in parallel, reading data straight from your encrypted object storage." },
  { n: "03", t: "Verify by quorum", d: "Each result is hashed; the first answer a quorum agrees on byte-for-byte wins. Losers are cancelled." },
  { n: "04", t: "Settle & pay out", d: "On success the escrow pays the winner, agreeing verifiers, and the treasury — the remainder refunds to you." },
];

function HowItWorks() {
  return (
    <section id="how" className="border-y border-white/10 bg-white/[0.015]">
      <div className="mx-auto max-w-6xl px-5 py-20">
        <h2 className="text-3xl font-bold tracking-tight text-white md:text-4xl">How it works</h2>
        <div className="mt-10 grid gap-4 md:grid-cols-2 lg:grid-cols-4">
          {steps.map((s) => (
            <div key={s.n} className="rounded-2xl border border-white/10 bg-[#0a0a0b] p-6">
              <div className="font-mono text-sm font-semibold" style={{ color: YELLOW }}>
                {s.n}
              </div>
              <h3 className="mt-3 text-base font-semibold text-white">{s.t}</h3>
              <p className="mt-2 text-sm leading-relaxed text-white/60">{s.d}</p>
            </div>
          ))}
        </div>
      </div>
    </section>
  );
}

interface OnchainStats {
  address: string;
  explorer: string;
  status: string | null;
  balanceTon: number | null;
  paramsVersion: number | null;
  platformFeeBps: number | null;
  participationBps: number | null;
  fetchedAt: number;
}

/** Fetch genuinely live state for the mainnet GlobalParams contract (read from
 *  TON via the edge-cached /api/onchain route — not from the baked snapshot). */
function useOnchain(): { data: OnchainStats | null; loading: boolean } {
  const [data, setData] = React.useState<OnchainStats | null>(null);
  const [loading, setLoading] = React.useState(true);
  React.useEffect(() => {
    let alive = true;
    const load = () =>
      fetch("/api/onchain")
        .then((r) => (r.ok ? r.json() : null))
        .then((d: OnchainStats | null) => {
          if (alive && d) setData(d);
        })
        .catch(() => {})
        .finally(() => alive && setLoading(false));
    load();
    const t = setInterval(load, 30_000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);
  return { data, loading };
}

function StatRow({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between border-t border-white/5 py-2 text-sm first:border-t-0">
      <span className="text-white/50">{label}</span>
      <span className="font-medium tabular-nums text-white">{value}</span>
    </div>
  );
}

function RealNetwork() {
  const net = useRealNet();
  const live = net !== null && net.onlineHosts > 0;
  const recent = net?.recent ?? [];
  return (
    <section id="network" className="mx-auto max-w-6xl px-5 py-16">
      <div className="rounded-2xl border p-6 md:p-8" style={{ borderColor: `${YELLOW}30`, background: `${YELLOW}08` }}>
        <div className="flex flex-wrap items-center gap-3">
          <Network className="size-5" style={{ color: YELLOW }} />
          <h2 className="text-2xl font-bold tracking-tight text-white">Live network — real nodes</h2>
          <span className="inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-medium" style={{ borderColor: `${YELLOW}40`, color: YELLOW }}>
            <span className="relative flex size-2">
              {live ? <span className="absolute inline-flex size-full animate-ping rounded-full opacity-70" style={{ background: YELLOW }} /> : null}
              <span className="relative inline-flex size-2 rounded-full" style={{ background: live ? YELLOW : "#52525b" }} />
            </span>
            {live ? "live" : "connecting…"}
          </span>
        </div>
        <p className="mt-3 max-w-2xl text-sm leading-relaxed text-white/60">
          Real, not a simulation — <span className="text-white">independent node processes</span> with distinct
          Ed25519 identities, running real distributed <span className="font-mono text-white/80">p2p_query</span> jobs
          across each other over QUIC with quorum verification. Counts climb live as jobs execute.
        </p>

        <div className="mt-6 grid grid-cols-2 gap-4 md:grid-cols-4">
          <StatCard label="Online host nodes" value={`${net?.onlineHosts ?? "—"}`} sub="independent peers" icon={<Server />} />
          <StatCard label="Real jobs executed" value={net ? num(net.realJobsRun) : "—"} sub="distributed + verified" icon={<Activity />} />
          <StatCard label="Verified" value={net?.verifiedRatePct != null ? `${net.verifiedRatePct}%` : "—"} sub="quorum-agreed" icon={<ShieldCheck />} />
          <StatCard label="Avg latency" value={net?.avgLatencyMs != null ? `${net.avgLatencyMs} ms` : "—"} sub="cross-node commit" icon={<Gauge />} />
        </div>

        <div className="mt-5 grid gap-2 md:grid-cols-3">
          {(net?.hosts ?? []).map((h) => (
            <div key={h} className="flex items-center gap-2 rounded-lg border border-white/10 bg-[#0a0a0b] px-3 py-2">
              <span className="size-1.5 rounded-full" style={{ background: YELLOW }} />
              <span className="truncate font-mono text-xs text-white/70" title={h}>{shortId(h)}</span>
            </div>
          ))}
        </div>

        {recent.length > 0 ? (
          <div className="mt-5">
            <div className="mb-2 text-xs font-medium text-white/40">Recent real jobs</div>
            <div className="space-y-1">
              {recent.slice(0, 5).map((j, i) => (
                <div key={i} className="flex items-center justify-between gap-3 rounded-md border border-white/5 bg-[#0a0a0b] px-3 py-1.5 font-mono text-xs">
                  <span className="truncate text-white/60">{j.query}</span>
                  <span className="shrink-0 text-white/40">
                    <span style={{ color: YELLOW }}>{shortId(j.winner)}</span> · {j.latencyMs}ms · q{j.participants}
                  </span>
                </div>
              ))}
            </div>
          </div>
        ) : null}
      </div>
    </section>
  );
}

function MainnetPanel() {
  const { data, loading } = useOnchain();
  const fee = data?.platformFeeBps != null ? `${(data.platformFeeBps / 100).toFixed(1)}%` : "—";
  const kappa = data?.participationBps != null ? `${(data.participationBps / 100).toFixed(1)}%` : "—";
  const balance = data?.balanceTon != null ? `${data.balanceTon.toFixed(3)} TON` : "—";
  const active = data?.status === "active";

  return (
    <section className="mx-auto max-w-6xl px-5 py-20">
      <div className="rounded-2xl border border-white/10 bg-white/[0.02] p-8 md:flex md:items-center md:gap-10">
        <div className="flex-1">
          <div className="flex items-center gap-2">
            <Network className="size-5" style={{ color: YELLOW }} />
            <h2 className="text-2xl font-bold tracking-tight text-white">TON settlement layer</h2>
          </div>
          <p className="mt-3 max-w-xl text-sm leading-relaxed text-white/60">
            Paid jobs settle through on-chain escrow governed by a platform-wide{" "}
            <span className="font-mono text-white/80">GlobalParams</span> contract: platform fee φ,
            participation commission κ, stake floors, and slashing — all enforced on TON. The figures
            on the right are read live from TON mainnet.
          </p>
        </div>
        <div className="mt-6 w-full md:mt-0 md:w-96">
          <div className="rounded-xl border border-white/10 bg-[#0a0a0b] p-5">
            <div className="flex items-center justify-between">
              <span className="text-xs font-medium text-white/50">GlobalParams · mainnet</span>
              <span className="inline-flex items-center gap-1.5 text-xs font-medium">
                <span className="relative flex size-2">
                  {active ? (
                    <span
                      className="absolute inline-flex size-full animate-ping rounded-full opacity-70"
                      style={{ background: YELLOW }}
                    />
                  ) : null}
                  <span
                    className="relative inline-flex size-2 rounded-full"
                    style={{ background: active ? YELLOW : "#52525b" }}
                  />
                </span>
                <span style={{ color: active ? YELLOW : undefined }} className={active ? "" : "text-white/40"}>
                  {loading ? "reading chain…" : active ? "live" : "unreachable"}
                </span>
              </span>
            </div>

            <div className="mt-3">
              <StatRow label="Status" value={data?.status ?? "—"} />
              <StatRow label="Balance" value={balance} />
              <StatRow label="Params version" value={data?.paramsVersion ?? "—"} />
              <StatRow label="Platform fee φ" value={fee} />
              <StatRow label="Participation κ" value={kappa} />
            </div>

            <div className="mt-3 break-all font-mono text-[11px] leading-snug text-white/50">{MAINNET_GP}</div>
            <a
              href={`https://tonviewer.com/${MAINNET_GP}`}
              target="_blank"
              rel="noreferrer"
              className="mt-2 inline-flex items-center gap-1.5 text-sm font-semibold"
              style={{ color: YELLOW }}
            >
              View on Tonviewer <ArrowRight className="size-4" />
            </a>
          </div>
        </div>
      </div>
    </section>
  );
}

function CTA() {
  return (
    <section className="mx-auto max-w-6xl px-5 pb-24">
      <div
        className="overflow-hidden rounded-3xl border p-10 text-center md:p-14"
        style={{ borderColor: `${YELLOW}33`, background: `linear-gradient(180deg, ${YELLOW}0f, transparent)` }}
      >
        <h2 className="text-3xl font-bold tracking-tight text-white md:text-4xl">
          Put your idle hardware to work.
        </h2>
        <p className="mx-auto mt-4 max-w-xl text-white/60">
          Load the extension, join a swarm, and run verified distributed queries — or share your
          machine and earn.
        </p>
        <div className="mt-8 flex flex-wrap justify-center gap-3">
          <a
            href={DOCS_URL}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-2 rounded-lg px-6 py-3 text-sm font-semibold text-black transition-transform hover:scale-[1.03]"
            style={{ background: YELLOW }}
          >
            Get started <ArrowRight className="size-4" />
          </a>
          <a
            href={CONSOLE_URL}
            target="_blank"
            rel="noreferrer"
            className="inline-flex items-center gap-2 rounded-lg border border-white/15 px-6 py-3 text-sm font-semibold text-white transition-colors hover:bg-white/5"
          >
            Explore the console
          </a>
        </div>
      </div>
    </section>
  );
}

function Footer() {
  return (
    <footer className="border-t border-white/10">
      <div className="mx-auto flex max-w-6xl flex-col items-center justify-between gap-4 px-5 py-8 text-sm text-white/40 md:flex-row">
        <div className="flex items-center gap-2.5">
          <DuckMark className="size-6" />
          <span className="font-semibold text-white/70">Duckton</span>
          <span>· Apache-2.0</span>
        </div>
        <div className="flex items-center gap-6">
          <NavLink href={DOCS_URL}>Docs</NavLink>
          <NavLink href={CONSOLE_URL}>Console</NavLink>
          <NavLink href={GITHUB_URL}>GitHub</NavLink>
          <span className="font-mono text-xs text-white/30">
            p2p/{meta.protocolVersion} · v{meta.workspaceVersion}
          </span>
        </div>
      </div>
    </footer>
  );
}

function CodeBlock({ code }: { code: string }) {
  return (
    <pre className="overflow-x-auto rounded-xl border border-white/10 bg-black/60 p-4 text-[13px] leading-relaxed">
      <code className="font-mono">
        {code.split("\n").map((line, i) => (
          <div
            key={i}
            className={line.trimStart().startsWith("--") ? "text-white/40" : "text-white/90"}
          >
            {line || "\u00A0"}
          </div>
        ))}
      </code>
    </pre>
  );
}

const EXAMPLES = [
  {
    id: "query",
    label: "Run a verified query",
    desc: "Install the extension, join the public Duckton network through the live seed node, and run SQL that independent hosts execute redundantly — accepted only when a quorum agrees byte-for-byte.",
    code: `-- 1. Install + load the extension (DuckDB Community Extensions)
INSTALL duckton FROM community;
LOAD duckton;

-- 2. Join the public Duckton network via the live seed node
CALL p2p_join(bootstrap => ['seed.duckton.com:9494']);

-- 3. Run SQL across independent nodes, verified by quorum
SELECT * FROM p2p_query('SELECT 42 AS answer');

-- Target a subset: only nodes in a network / group / region
SELECT * FROM p2p_query('SELECT count(*) FROM read_parquet(''s3://...'')',
                        groups => ['eu-internal'], regions => ['eu']);`,
  },
  {
    id: "earn",
    label: "Share your machine & earn",
    desc: "Donate a slice of an idle laptop, PC, or server and start serving others' jobs. Set your own rate (whole TON) to accept paid work — no central broker, no sign-up.",
    code: `LOAD duckton;

-- Donate compute and start serving others' jobs (becomes a host)
CALL p2p_share(memory => '2GB', threads => 4, max_jobs => 4,
               data_classes => ['public']);

-- Set your rate to accept PAID work, then see who's around
CALL p2p_pricing(unit_price => 5, max_bid => 100);   -- whole TON
SELECT * FROM p2p_peers();

-- Optional: bond stake for eligibility/priority on paid jobs
CALL p2p_stake(amount => 100);`,
  },
  {
    id: "pay",
    label: "Pay & settle on TON",
    desc: "Paid jobs lock the requester's max bid in a per-job escrow on TON. On settle, the winner, agreeing verifiers, and the platform treasury are paid and the remainder is refunded — all enforced on-chain by the live GlobalParams contract.",
    code: `-- Turn on the TON money rail (mainnet = real funds, needs confirm)
CALL p2p_economics(enabled => true, settlement => 'ton',
                   network => 'mainnet', confirm => true,
                   fee_recipient => 'EQ...your-treasury...');

-- Point at the LIVE mainnet GlobalParams contract
CALL p2p_contracts(
  global_params => 'EQCV59kSoDDgmE8cheBNYwl2oYL9h5nkbyCqn99tN-N1w9Gg');

-- Wallet via secure file refs (never paste secrets into SQL)
CALL p2p_wallet(rpc => 'https://toncenter.com/api/v2/',
                mnemonic_file => '~/.duckton/wallet.mnemonic',
                address => 'EQ...');

-- Run a PAID job: escrow opens on TON, splits enforced on-chain
SELECT * FROM p2p_query('SELECT ...', payment => 'paid',
                        replicas => 3, quorum => 2);`,
  },
];

function Connect() {
  const [active, setActive] = React.useState(EXAMPLES[0].id);
  const ex = EXAMPLES.find((e) => e.id === active) ?? EXAMPLES[0];
  return (
    <section id="connect" className="border-y border-white/10 bg-white/[0.015]">
      <div className="mx-auto max-w-6xl px-5 py-20">
        <h2 className="text-3xl font-bold tracking-tight text-white md:text-4xl">
          Connect in plain SQL
        </h2>
        <p className="mt-4 max-w-2xl text-white/60">
          No daemon, no SDK. Everything is a DuckDB table function — join the network, share your
          machine, and settle paid jobs on TON, all from SQL.
        </p>

        <div className="mt-8 flex flex-wrap gap-2">
          {EXAMPLES.map((e) => (
            <button
              key={e.id}
              type="button"
              onClick={() => setActive(e.id)}
              className={`rounded-lg px-4 py-2 text-sm font-semibold transition-colors ${
                active === e.id
                  ? "text-black"
                  : "border border-white/15 text-white/70 hover:text-white"
              }`}
              style={active === e.id ? { background: YELLOW } : undefined}
            >
              {e.label}
            </button>
          ))}
        </div>

        <div className="mt-6 grid gap-5 md:grid-cols-[1fr_1.4fr] md:items-start">
          <p className="text-sm leading-relaxed text-white/60">{ex.desc}</p>
          <CodeBlock code={ex.code} />
        </div>

        <p className="mt-5 text-sm text-white/40">
          Public jobs are free and fully off-chain. The seed node{" "}
          <span className="font-mono text-white/60">seed.duckton.com:9494</span> is live now — full
          reference in the{" "}
          <a href={DOCS_URL} target="_blank" rel="noreferrer" className="font-semibold" style={{ color: YELLOW }}>
            docs
          </a>
          .
        </p>
      </div>
    </section>
  );
}

export default function LandingPage() {
  return (
    <div className="min-h-svh bg-[#0a0a0b] text-white">
      <TopBar />
      <Hero />
      <RealNetwork />
      <WhatItIs />
      <HowItWorks />
      <Connect />
      <MainnetPanel />
      <CTA />
      <Footer />
    </div>
  );
}
