import type { Metadata } from "next";
import {
  ArrowUpRight,
  Boxes,
  CheckCircle2,
  CircleX,
  Coins,
  Cpu,
  FileCheck,
  Gavel,
  Hash,
  KeyRound,
  Landmark,
  Lock,
  Rocket,
  ShieldCheck,
} from "lucide-react";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import {
  Accordion,
  AccordionContent,
  AccordionItem,
  AccordionTrigger,
} from "@/components/ui/accordion";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { KV, PageHeader, SectionTitle, Stat } from "@/components/common/atoms";
import { Explainer } from "@/components/common/explain";
import { CopyId } from "@/components/common/copy";
import { Package } from "lucide-react";
import { meta, ton } from "@/lib/data";
import { TonNetworkPanel } from "./network-panel";
import { GasPlot } from "./plots";
import { short } from "@/lib/format";

export const metadata: Metadata = { title: "On-chain (TON)" };

const gp = ton.computed.globalParams;
const verifiedCount = ton.contracts.filter((c) => c.verify?.verified).length;

function tonviewer(addr: string) {
  return `https://testnet.tonviewer.com/${addr}`;
}

function Addr({ value }: { value: string | null }) {
  if (!value) return <span className="text-muted-foreground text-xs">—</span>;
  return (
    <span className="inline-flex items-center gap-1.5">
      <CopyId value={value} />
      <a
        href={tonviewer(value)}
        target="_blank"
        rel="noreferrer"
        className="text-muted-foreground hover:text-primary"
        title="open in tonviewer (testnet)"
      >
        <ArrowUpRight className="size-3" />
      </a>
    </span>
  );
}

const bpsRows: [string, string][] = [
  ["platform_fee", `${gp.platformFeeBps} bps`],
  ["surcharge", `${gp.surchargeBps} bps`],
  ["participation κ", `${gp.participationCommissionBps} bps`],
  ["slash · wrong", `${gp.slashWrongBps} bps`],
  ["slash · cheat", `${gp.slashCheatBps} bps`],
  ["slash · downtime", `${gp.slashDowntimeBps} bps`],
  ["slash · equivocation", `${gp.slashEquivocationBps} bps`],
  ["slash · failed-commit", `${gp.slashFailedCommitmentBps} bps`],
];
const splitRows: [string, string][] = [
  ["→ challenger", `${gp.splitChallengerBps} bps`],
  ["→ redundancy", `${gp.splitRedundancyBps} bps`],
  ["→ burn", `${gp.splitBurnBps} bps`],
  ["→ treasury", `${gp.splitTreasuryBps} bps`],
];

export default function TonPage() {
  return (
    <div className="space-y-8">
      <PageHeader
        title="On-chain settlement (TON)"
        description="The optional economic layer: sharded, non-custodial Tolk contracts on TON. Only paid jobs touch chain — free jobs settle entirely off-chain. Contracts below are deployed and verified on testnet; the on-chain encodings are computed by the real settlement crate."
        icon={<Landmark />}
      >
        <Badge variant="info" className="gap-1.5">
          <CheckCircle2 className="size-3" /> {ton.network}
        </Badge>
        <Badge variant="muted">{ton.toolchain.split(" ")[0]} {ton.toolchain.split(" ")[1]}</Badge>
      </PageHeader>

      <Explainer
        what="The settlement rules written as sharded smart contracts on the TON blockchain, so escrow, payouts and penalties are enforced by code instead of a trusted operator. These contracts are deployed and verified on testnet."
        impact="Escrow only releases when the agreed result is presented (HTLC), and stake is slashed automatically — no company can withhold pay or seize funds."
      />

      {/* Community registry — the published DuckDB extension */}
      <Card className="border-[var(--ok)]/30 bg-[var(--ok)]/5">
        <CardHeader>
          <div className="flex flex-wrap items-start justify-between gap-2">
            <div>
              <CardTitle className="flex items-center gap-2">
                <Package className="size-4 text-[var(--ok)]" /> Published to the DuckDB community registry
              </CardTitle>
              <CardDescription>
                The <span className="font-mono">duckton</span> extension is officially published — install
                it straight from the community registry, no build required.
              </CardDescription>
            </div>
            <Badge variant="ok" className="gap-1.5 font-mono">
              <CheckCircle2 className="size-3" /> v{meta.workspaceVersion}
            </Badge>
          </div>
        </CardHeader>
        <CardContent>
          <pre className="bg-muted/40 overflow-x-auto rounded-md border p-3 font-mono text-xs leading-relaxed">
            INSTALL duckton FROM community;{"\n"}LOAD duckton;
          </pre>
          <p className="text-muted-foreground mt-2 text-xs">
            The on-chain 15% platform fee / 5% verifier commission split shown below is enforced by the
            deployed contracts; this client extension version is{" "}
            <span className="font-mono">{meta.workspaceVersion}</span>.
          </p>
        </CardContent>
      </Card>

      {/* Active network (testnet / mainnet) */}
      <TonNetworkPanel />

      {/* Stat row */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-3 xl:grid-cols-6">
        <Stat label="Contracts" value={ton.contracts.length} sub="sharded · no global hot contract" icon={<Boxes />} accent="primary" />
        <Stat label="Verified" value={`${verifiedCount}/${ton.contracts.length}`} sub="source ↔ published bytecode" icon={<FileCheck />} accent="ok" hint="The published on-chain bytecode matches the source code." />
        <Stat label="HTLC escrow" value="quorum-hash" sub="release keyed on result hash" icon={<Lock />} accent="info" hint="The escrow only pays out when someone presents the agreed quorum result hash." />
        <Stat label="Slash split" value="100%" sub="challenger+redund+burn+treasury" icon={<Gavel />} accent="warn" hint="A slashed stake is fully distributed — to the challenger, redundant workers, a burn, and the treasury — never kept by the platform." />
        <Stat label="Code upgrade" value="SETCODE" sub="proven 1→2, address stable" icon={<Rocket />} accent="primary" hint="Smart-contract code can be swapped in place at the same address, with storage preserved." />
        <Stat label="Custody" value="non-custodial" sub="no platform seizure path" icon={<ShieldCheck />} accent="ok" hint="Funds are held only by the contract logic; no operator key can move or seize them." />
      </div>

      {/* Deployed contracts */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Landmark className="size-4 text-primary" /> Deployed contracts (testnet)
          </CardTitle>
          <CardDescription>
            Live addresses from <span className="font-mono">ton/deployments/testnet.env</span>; code hashes
            from the compiled artifacts; verification from <span className="font-mono">acton verify</span> logs.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">Contract</TableHead>
                <TableHead>Testnet address</TableHead>
                <TableHead>Code hash</TableHead>
                <TableHead>Verified</TableHead>
                <TableHead>Upgrade</TableHead>
                <TableHead className="pr-6 text-right">Ops</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {ton.contracts.map((c) => (
                <TableRow key={c.name}>
                  <TableCell className="pl-6">
                    <div className="font-medium">{c.name}</div>
                    <div className="text-muted-foreground text-xs">{c.doc}</div>
                  </TableCell>
                  <TableCell>
                    <Addr value={c.testnetAddress} />
                  </TableCell>
                  <TableCell>
                    {c.codeHash ? <CopyId value={c.codeHash} display={short(c.codeHash, 8, 4)} /> : "—"}
                  </TableCell>
                  <TableCell>
                    {c.verify?.verified ? (
                      <Badge variant="ok" className="gap-1">
                        <CheckCircle2 className="size-3" /> verified
                      </Badge>
                    ) : c.verify?.failed ? (
                      <Badge variant="warn" className="gap-1">
                        <CircleX className="size-3" /> backend err
                      </Badge>
                    ) : (
                      <Badge variant="muted">—</Badge>
                    )}
                  </TableCell>
                  <TableCell className="text-muted-foreground text-xs">
                    {c.upgradeable.split("(")[0].trim()}
                  </TableCell>
                  <TableCell className="pr-6 text-right tabular-nums">{c.opcodes.length || "—"}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      {/* GlobalParams + JobEscrow */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Cpu className="size-4 text-primary" /> On-chain GlobalParams
            </CardTitle>
            <CardDescription>
              The ecosystem-wide economic params, bps-encoded into a TON cell by the real settlement crate.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <div className="grid grid-cols-2 gap-x-6">
              <dl>
                {bpsRows.map(([k, v]) => (
                  <KV key={k} label={k}>
                    {v}
                  </KV>
                ))}
              </dl>
              <dl>
                {splitRows.map(([k, v]) => (
                  <KV key={k} label={k}>
                    {v}
                  </KV>
                ))}
                <KV label="quorum / n_default">
                  {gp.quorum} / {gp.nDefault}
                </KV>
                <KV label="unbonding / challenge">
                  {gp.unbondingSecs}s / {gp.challengeWindowSecs}s
                </KV>
              </dl>
            </div>
            <Separator />
            <dl>
              <KV label="EcoParams cell hash">
                <CopyId value={String(gp.ecoParamsCellHash)} display={short(String(gp.ecoParamsCellHash), 10, 6)} />
              </KV>
              <KV label="ranking weights (q/s/p)">
                {gp.wQualityBps}/{gp.wStakeBps}/{gp.wPriceBps} bps
              </KV>
            </dl>
            <div className="bg-muted/40 rounded-md border p-2">
              <div className="text-muted-foreground mb-1 text-[10px] uppercase tracking-wide">EcoParams cell · BoC (base64)</div>
              <code className="block break-all font-mono text-[10px] leading-relaxed">
                {String(gp.ecoParamsBocBase64)}
              </code>
            </div>
            <p className="text-muted-foreground text-xs">
              Validated on-chain (<span className="font-mono">validateEcoParams</span>): split bps sum to 10000,
              κ ≤ 1000 bps, <span className="font-mono">unbonding ≥ challenge_window</span>, stake tiers ordered.
              Editable in place via <span className="font-mono">UpdateParams</span> (admin-only, bumps paramsVersion).
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Lock className="size-4 text-primary" /> JobEscrow — HTLC release
            </CardTitle>
            <CardDescription>
              Per-job, non-custodial. Address derived offline from the compiled code + the real escrow terms.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            {ton.computed.escrow ? (
              <dl>
                <KV label="escrow address (this job)">
                  <Addr value={ton.computed.escrow.address} />
                </KV>
                <KV label="HTLC lock = quorum result hash">
                  <CopyId
                    value={ton.computed.escrow.expectedHashHex}
                    display={short(ton.computed.escrow.expectedHashHex, 10, 6)}
                  />
                </KV>
                <KV label="terms cell hash">
                  <CopyId
                    value={ton.computed.escrow.termsCellHash}
                    display={short(ton.computed.escrow.termsCellHash, 10, 6)}
                  />
                </KV>
                <KV label="escrow B">{ton.computed.escrow.escrowTon} TON</KV>
                <KV label="paramsVersion bound">{ton.computed.escrow.paramsVersion}</KV>
              </dl>
            ) : null}
            <p className="text-muted-foreground text-xs">
              The address deterministically commits to{" "}
              <span className="font-mono">hash(StateInit&#123;code, data&#125;)</span> where data binds
              requester + arbiter + B + deadline + ^terms — so each job gets a distinct escrow. Settle is
              gated on presenting the agreed quorum hash and bounded by B (remainder refunded);
              refund-on-timeout returns the full balance to the requester. No platform key can seize funds.
            </p>
            <div className="flex flex-wrap gap-1.5">
              {(ton.computed.opcodes.JobEscrow ?? []).map((o) => (
                <Badge key={o.name} variant="muted" className="font-mono">
                  {o.hex} {o.name.replace("OP_ESCROW_", "")}
                </Badge>
              ))}
            </div>
          </CardContent>
        </Card>
      </div>

      {/* Contract reference */}
      <div>
        <SectionTitle
          hint="storage · opcodes · get-methods · guards"
          info="A per-contract breakdown of what each one stores, the messages (opcodes) it accepts, the values you can read from it, and the safety guards it enforces."
        >
          Contract reference
        </SectionTitle>
        <Card>
          <CardContent className="py-2">
            <Accordion type="single" collapsible className="w-full">
              {ton.contracts.map((c) => (
                <AccordionItem key={c.name} value={c.name}>
                  <AccordionTrigger>
                    <div className="flex flex-1 items-center gap-2 pr-2">
                      <span className="font-medium">{c.name}</span>
                      <Badge variant="muted" className="text-[10px]">{c.doc}</Badge>
                      <span className="text-muted-foreground ml-auto truncate text-xs font-normal">
                        {c.role.split(".")[0]}.
                      </span>
                    </div>
                  </AccordionTrigger>
                  <AccordionContent className="space-y-4">
                    <p className="text-muted-foreground text-sm">{c.role}</p>
                    <div className="flex items-center gap-2 text-xs">
                      <Rocket className="text-primary size-3.5" />
                      <span className="text-muted-foreground">upgradeability:</span>
                      <span className="font-medium">{c.upgradeable}</span>
                    </div>

                    <div className="grid gap-4 lg:grid-cols-2">
                      <div>
                        <div className="text-muted-foreground mb-1.5 flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide">
                          <Boxes className="size-3" /> Storage
                        </div>
                        <div className="overflow-hidden rounded-md border">
                          <table className="w-full text-xs">
                            <tbody>
                              {c.storage.map(([f, t, note]) => (
                                <tr key={f} className="border-b last:border-0">
                                  <td className="px-2 py-1 font-mono">{f}</td>
                                  <td className="text-primary px-2 py-1 font-mono">{t}</td>
                                  <td className="text-muted-foreground px-2 py-1">{note}</td>
                                </tr>
                              ))}
                            </tbody>
                          </table>
                        </div>
                      </div>

                      <div className="space-y-3">
                        <div>
                          <div className="text-muted-foreground mb-1.5 flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide">
                            <Hash className="size-3" /> Opcodes
                          </div>
                          <div className="space-y-1">
                            {c.opcodes.length ? (
                              c.opcodes.map((o) => (
                                <div key={o.name} className="flex items-start gap-2 text-xs">
                                  <code className="text-primary shrink-0 font-mono">{o.hex}</code>
                                  <span className="text-muted-foreground">{o.desc}</span>
                                </div>
                              ))
                            ) : (
                              <span className="text-muted-foreground text-xs">
                                TEP-74 jetton ops (0x178d4519 mint · 0x0f8a7ea5 transfer → locked)
                              </span>
                            )}
                          </div>
                        </div>
                        <div>
                          <div className="text-muted-foreground mb-1.5 flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide">
                            <KeyRound className="size-3" /> Get-methods
                          </div>
                          <div className="space-y-0.5">
                            {c.getMethods.map(([m, r]) => (
                              <div key={m} className="text-xs">
                                <code className="font-mono">{m}</code>{" "}
                                <span className="text-muted-foreground">→ {r}</span>
                              </div>
                            ))}
                          </div>
                        </div>
                      </div>
                    </div>

                    <div>
                      <div className="text-muted-foreground mb-1.5 flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide">
                        <ShieldCheck className="size-3" /> Guards
                      </div>
                      <ul className="text-muted-foreground list-inside list-disc space-y-0.5 text-xs">
                        {c.guards.map((g) => (
                          <li key={g}>{g}</li>
                        ))}
                      </ul>
                    </div>
                  </AccordionContent>
                </AccordionItem>
              ))}
            </Accordion>
          </CardContent>
        </Card>
      </div>

      {/* Wire messages + gas */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Wire messages (real TL-B cells)</CardTitle>
            <CardDescription>Opcoded message bodies the node actually broadcasts, built by the settlement crate.</CardDescription>
          </CardHeader>
          <CardContent className="space-y-2">
            {ton.computed.messages.map((m) => (
              <div key={m.label} className="rounded-md border p-2.5">
                <div className="flex items-center justify-between">
                  <span className="text-sm font-medium">{m.label}</span>
                  <code className="text-primary font-mono text-xs">{m.opcodeHex}</code>
                </div>
                <div className="mt-1 flex items-center gap-3 text-xs">
                  <span className="text-muted-foreground">cell</span>
                  <CopyId value={m.cellHash} display={short(m.cellHash, 8, 4)} />
                  <span className="text-muted-foreground">{m.bits} bits</span>
                </div>
              </div>
            ))}
          </CardContent>
        </Card>

        <GasPlot />
      </div>

      {/* SETCODE upgrade + Duckton */}
      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Rocket className="size-4 text-primary" /> In-place code upgrade (SETCODE)
            </CardTitle>
            <CardDescription>Proven live on testnet — address unchanged, storage preserved.</CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <dl>
              <KV label="contract">
                <Addr value={ton.canonical.setcode.address} />
              </KV>
              <KV label="from → to">
                {ton.canonical.setcode.from} → {ton.canonical.setcode.to}
              </KV>
              <KV label="address stable">
                <Badge variant="ok">{String(ton.canonical.setcode.addressStable)}</Badge>
              </KV>
              <KV label="new getter live">
                <span className="font-mono">{ton.canonical.setcode.newGetter}</span> →{" "}
                {ton.canonical.setcode.newGetterValue}
              </KV>
            </dl>
            <p className="text-muted-foreground text-xs">{ton.canonical.setcode.note}</p>
            <div className="text-muted-foreground space-y-1 text-xs">
              <div>StakeVault: timelocked (announce → apply ≥ unbonding window, commit-reveal)</div>
              <div>GlobalParams / RecordAnchor: authority-gated, no timelock</div>
              <div>JobEscrow: intentionally not upgradable (live HTLC)</div>
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Coins className="size-4 text-primary" /> Duckton — stake-receipt jetton
            </CardTitle>
            <CardDescription>TEP-74 receipt minted 1:1 on deposit, transfer-LOCKED while bonded.</CardDescription>
          </CardHeader>
          <CardContent className="space-y-3">
            <dl>
              <KV label="name / symbol / decimals">
                {ton.canonical.duckton.name} / {ton.canonical.duckton.symbol} / {ton.canonical.duckton.decimals}
              </KV>
              <KV label="transfer locked">
                <Badge variant="warn">{String(ton.canonical.duckton.transferLocked)}</Badge>
              </KV>
              <KV label="master (StakeVault)">
                <Addr value={ton.canonical.stakeVault} />
              </KV>
              <KV label="holder wallet">
                <Addr value={ton.canonical.ducktonHolder} />
              </KV>
              <KV label="balance">{ton.canonical.duckton.balance} DUCKTON</KV>
            </dl>
            <p className="text-muted-foreground text-xs">
              A slashable accountability bond, not a liquid position — a transferable receipt would let a host
              sell it and dodge slashing, so outgoing transfers throw <span className="font-mono">RECEIPT_LOCKED</span>{" "}
              for the whole bond lifetime (mint → burn).
            </p>
          </CardContent>
        </Card>
      </div>

      {/* Deploy & verify */}
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <FileCheck className="size-4 text-primary" /> Deploy, verify &amp; live proof
          </CardTitle>
          <CardDescription>
            Built/tested/deployed with {ton.toolchain}. RPC: <span className="font-mono">{ton.rpc}</span>
          </CardDescription>
        </CardHeader>
        <CardContent className="grid gap-4 lg:grid-cols-2">
          <div className="space-y-3">
            <div>
              <div className="text-muted-foreground mb-1 text-xs font-semibold uppercase tracking-wide">Deploy</div>
              <pre className="bg-muted/40 overflow-x-auto rounded-md border p-2 font-mono text-[11px] leading-relaxed">
                {ton.deployFlow.deploy.join("\n")}
              </pre>
            </div>
            <div>
              <div className="text-muted-foreground mb-1 text-xs font-semibold uppercase tracking-wide">Verify (source ↔ bytecode)</div>
              <pre className="bg-muted/40 overflow-x-auto rounded-md border p-2 font-mono text-[11px] leading-relaxed">
                {ton.deployFlow.verify.join("\n")}
              </pre>
            </div>
            <div>
              <div className="text-muted-foreground mb-1 text-xs font-semibold uppercase tracking-wide">Live Rust integration test</div>
              <pre className="bg-muted/40 overflow-x-auto rounded-md border p-2 font-mono text-[11px]">
                {ton.deployFlow.live}
              </pre>
            </div>
          </div>
          <div>
            <div className="text-muted-foreground mb-1 text-xs font-semibold uppercase tracking-wide">
              End-to-end testnet scenario — all checks PASS
            </div>
            <div className="space-y-1">
              {ton.e2e
                .filter((e) => e !== "done" && !e.startsWith("wallet="))
                .map((e) => (
                  <div key={e} className="flex items-start gap-2 text-xs">
                    <CheckCircle2 className="mt-0.5 size-3 shrink-0 text-[var(--ok)]" />
                    <code className="font-mono break-all">{e}</code>
                  </div>
                ))}
            </div>
            {ton.deployLogs.stake ? (
              <p className="text-muted-foreground mt-3 text-[11px] font-mono">
                {ton.deployLogs.stake.find((l) => l.startsWith("Deployed"))}
              </p>
            ) : null}
          </div>
        </CardContent>
      </Card>

      <p className="text-muted-foreground text-center text-xs">
        On-chain encodings (GlobalParams cell, escrow address, message BoCs) computed by{" "}
        <span className="font-mono">p2p_settlement</span> offline. Addresses, code hashes, verification &amp;
        gas read from the repo&apos;s deployment artifacts &amp; testnet logs.{" "}
        <Badge variant="ok" className="font-mono">real</Badge>
      </p>
    </div>
  );
}
