import type { Metadata } from "next";
import { Cpu, Gauge, HardDrive, Server, ShieldCheck, Wifi } from "lucide-react";
import { PageHeader, Stat } from "@/components/common/atoms";
import { Explainer } from "@/components/common/explain";
import { workers } from "@/lib/data";
import { bytes, num, pct } from "@/lib/format";
import { WorkersClient } from "./workers-client";

export const metadata: Metadata = {
  title: "Workers",
  description: "Hosts donating RAM and CPU to the Duckton grid, with real trust scores, attestation, and capacity.",
};

// Online count is derived from a static module import — hoist to module scope
// so the computation runs once at render, not per-component-render.
const onlineCount = workers.filter((w) => w.online).length;

export default function WorkersPage() {
  const total = workers.length;
  const online = onlineCount;
  const l2 = workers.filter((w) => w.attestation === "L2").length;
  const totalThreads = workers.reduce((a, w) => a + w.totalThreads, 0);
  const totalRam = workers.reduce((a, w) => a + w.donatedMemBytes, 0);
  const honest = workers.filter((w) => w.behavior === "honest");
  const avgSuccess =
    honest.length === 0
      ? 0
      : honest.reduce((a, w) => a + w.successRate, 0) / honest.length;

  return (
    <div className="space-y-8">
      <PageHeader
        icon={<Server />}
        title="Workers"
        description="Hosts donating RAM/CPU. Each advertises capacity and an attestation tier; trust is built from this run's signed receipts."
      />

      <Explainer
        what="The machines donating RAM and CPU. Each carries a trust score the grid computes from its past results, hardware-attestation tier, vouches and stake."
        impact="Requesters automatically send important or sensitive work to high-trust, attested workers and avoid unreliable ones — so good behavior earns more jobs."
      />

      <p className="text-muted-foreground -mt-4 text-xs">
        Trust and reputation are computed by the real{" "}
        <span className="text-foreground font-mono">p2p-trust</span> engine from
        this run&apos;s signed receipts — the cheat/fail nodes really dropped to
        trust 0 and were deselected.
      </p>

      {/* Stat row */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-6">
        <Stat
          label="Workers"
          value={total}
          sub="registered hosts"
          icon={<Server />}
          accent="primary"
        />
        <Stat
          label="Online"
          value={online}
          sub={`${total - online} offline`}
          icon={<Wifi />}
          accent="ok"
        />
        <Stat
          label="L2 enclaves"
          value={l2}
          sub="TEE-attested"
          icon={<ShieldCheck />}
          accent="info"
          hint="L0 anonymous, L1 verified-boot (TPM), L2 confidential enclave (TEE) whose RAM even the owner cannot read."
        />
        <Stat
          label="Donated threads"
          value={num(totalThreads)}
          sub="across all hosts"
          icon={<Cpu />}
          accent="info"
          hint="Total CPU hardware threads workers have offered to run jobs across the whole grid."
        />
        <Stat
          label="Donated RAM"
          value={bytes(totalRam, 0)}
          sub="advertised"
          icon={<HardDrive />}
          accent="info"
        />
        <Stat
          label="Avg success"
          value={pct(avgSuccess, 1)}
          sub="honest hosts"
          icon={<Gauge />}
          accent="primary"
          hint="Recency-weighted rate of correct results from signed receipts — the basis of each worker's reputation."
        />
      </div>

      {/* Interactive search/sort + slide-out sheet — client island */}
      <WorkersClient />
    </div>
  );
}
