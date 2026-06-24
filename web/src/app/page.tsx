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
  Zap,
  Lock,
  Network,
} from "lucide-react";
import { useLive } from "@/lib/live";
import { bytes, num, pct } from "@/lib/format";
import { meta } from "@/lib/data";

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
    <svg viewBox="0 0 48 48" className={className} aria-hidden="true">
      <circle cx="24" cy="24" r="22" fill="#0a0a0b" stroke={YELLOW} strokeWidth="2" />
      <circle cx="20" cy="23" r="9" fill={YELLOW} />
      <path d="M28 20h7a3 3 0 0 1 0 6h-7z" fill={YELLOW} />
    </svg>
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

function LiveStats() {
  const { overview, connected } = useLive();
  const verifyRate = overview.jobsRun ? overview.verified / overview.jobsRun : 0;

  return (
    <section className="border-y border-white/10 bg-white/[0.015]">
      <div className="mx-auto max-w-6xl px-5 py-12">
        <div className="mb-5 flex items-center gap-2.5">
          <span className="relative flex size-2.5">
            {connected ? (
              <span
                className="absolute inline-flex size-full animate-ping rounded-full opacity-70"
                style={{ background: YELLOW }}
              />
            ) : null}
            <span
              className="relative inline-flex size-2.5 rounded-full"
              style={{ background: connected ? YELLOW : "#52525b" }}
            />
          </span>
          <span className="text-sm font-medium text-white">
            {connected ? "Live grid" : "Network snapshot"}
          </span>
          <span className="text-sm text-white/40">
            {connected ? "streaming from a running grid" : "sample run of the p2p-* crates"}
          </span>
        </div>

        <div className="grid grid-cols-2 gap-4 md:grid-cols-3 lg:grid-cols-6">
          <StatCard
            label="Nodes online"
            value={`${overview.workersOnline}/${overview.workersTotal}`}
            sub="sharing compute"
            icon={<Server />}
          />
          <StatCard label="Queries run" value={num(overview.jobsRun)} sub="this run" icon={<Activity />} />
          <StatCard
            label="Verified"
            value={pct(verifyRate, 0)}
            sub="quorum-agreed"
            icon={<ShieldCheck />}
          />
          <StatCard label="Avg trust" value={overview.avgTrust.toFixed(2)} sub="0–1 score" icon={<Gauge />} />
          <StatCard label="Pooled RAM" value={bytes(overview.freeMemBytes, 0)} sub="across nodes" icon={<Cpu />} />
          <StatCard
            label="Staked"
            value={`${num(overview.totalStakeTon)} TON`}
            sub="at risk on TON"
            icon={<Coins />}
          />
        </div>
      </div>
    </section>
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

function MainnetPanel() {
  const live = MAINNET_GP.length > 0;
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
            participation commission κ, stake floors, and slashing — all enforced on TON.
          </p>
        </div>
        <div className="mt-6 w-full md:mt-0 md:w-80">
          {live ? (
            <div className="rounded-xl border border-white/10 bg-[#0a0a0b] p-5">
              <div className="text-xs font-medium text-white/50">GlobalParams · mainnet</div>
              <div className="mt-1 break-all font-mono text-xs text-white/80">{MAINNET_GP}</div>
              <a
                href={`https://tonviewer.com/${MAINNET_GP}`}
                target="_blank"
                rel="noreferrer"
                className="mt-3 inline-flex items-center gap-1.5 text-sm font-semibold"
                style={{ color: YELLOW }}
              >
                View on Tonviewer <ArrowRight className="size-4" />
              </a>
            </div>
          ) : (
            <div className="rounded-xl border border-dashed border-white/15 bg-[#0a0a0b] p-5 text-center">
              <div
                className="mx-auto flex size-10 items-center justify-center rounded-full [&_svg]:size-5"
                style={{ background: `${YELLOW}1a`, color: YELLOW }}
              >
                <Zap />
              </div>
              <div className="mt-3 text-sm font-semibold text-white">Mainnet launching soon</div>
              <div className="mt-1 text-xs text-white/50">
                Live today on testnet. Mainnet contracts are being deployed.
              </div>
            </div>
          )}
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

export default function LandingPage() {
  return (
    <div className="min-h-svh bg-[#0a0a0b] text-white">
      <TopBar />
      <Hero />
      <LiveStats />
      <WhatItIs />
      <HowItWorks />
      <MainnetPanel />
      <CTA />
      <Footer />
    </div>
  );
}
