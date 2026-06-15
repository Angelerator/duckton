"use client";

import "@/lib/polyfills";
import * as React from "react";
import { CHAIN, useTonAddress, useTonConnectUI } from "@tonconnect/ui-react";
import { toNano } from "@ton/core";
import {
  AlertTriangle,
  ArrowUpRight,
  BadgeCheck,
  CircleCheck,
  Coins,
  Copy,
  Landmark,
  Rocket,
  ShieldAlert,
  Wallet,
} from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { PageHeader, SectionTitle, Stat } from "@/components/common/atoms";
import { Explainer } from "@/components/common/explain";
import { useNetworkMode } from "@/lib/network-mode";
import {
  buildGlobalParamsDeploy,
  buildJobEscrowDeploy,
  buildRecordAnchorDeploy,
  buildStakeVaultDeploy,
  codeHashOk,
  ecoEncoderOk,
  GP_DEFAULT,
  gpToToml,
  validateGp,
  type DeployArtifact,
  type GpConfig,
} from "@/lib/ton-build";

const ECO_OK = ecoEncoderOk();

/* ----------------------------------------------------------------- helpers */

function NumberField({
  label,
  value,
  onChange,
  step = 1,
}: {
  label: string;
  value: number;
  onChange: (n: number) => void;
  step?: number;
}) {
  return (
    <label className="flex flex-col gap-1">
      <span className="text-muted-foreground text-xs">{label}</span>
      <Input
        type="number"
        value={Number.isFinite(value) ? value : 0}
        step={step}
        onChange={(e) => onChange(Number(e.target.value))}
        className="h-8 font-mono text-xs"
      />
    </label>
  );
}

const GP_GROUPS: { title: string; fields: [keyof GpConfig, string][] }[] = [
  {
    title: "Fees (bps)",
    fields: [
      ["platformFeeBps", "platform fee"],
      ["surchargeBps", "verification surcharge"],
      ["participationCommissionBps", "participation κ"],
    ],
  },
  {
    title: "Slashing (bps)",
    fields: [
      ["slashWrongBps", "wrong result"],
      ["slashCheatBps", "cheat"],
      ["slashDowntimeBps", "downtime"],
      ["slashEquivocationBps", "equivocation"],
      ["slashFailedCommitmentBps", "failed commitment"],
    ],
  },
  {
    title: "Slash split (bps — must sum to 10000)",
    fields: [
      ["splitChallengerBps", "→ challenger"],
      ["splitRedundancyBps", "→ redundancy"],
      ["splitBurnBps", "→ burn"],
      ["splitTreasuryBps", "→ treasury"],
    ],
  },
  {
    title: "Stake (whole TON) + windows (secs)",
    fields: [
      ["minStakeTon", "min stake"],
      ["minStakeInternalTon", "min · internal"],
      ["minStakeSensitiveTon", "min · sensitive"],
      ["stakeCapTon", "stake cap"],
      ["unbondingSecs", "unbonding secs"],
      ["challengeWindowSecs", "challenge secs"],
    ],
  },
  {
    title: "Selection + ranking",
    fields: [
      ["nPublic", "n public"],
      ["nDefault", "n default"],
      ["nMax", "n max"],
      ["quorum", "quorum"],
      ["checksumMin", "checksum min"],
      ["wQualityBps", "w quality (bps)"],
      ["wStakeBps", "w stake (bps)"],
      ["wPriceBps", "w price (bps)"],
    ],
  },
  {
    title: "Resilience",
    fields: [
      ["attemptDeadlineMs", "attempt deadline ms"],
      ["progressIntervalMs", "progress interval ms"],
      ["progressStallMult", "progress stall mult"],
    ],
  },
];

function AddrResult({ art, explorer }: { art: DeployArtifact; explorer: (a: string) => string }) {
  if (!art.ok) return <p className="text-destructive text-xs">⚠ {art.error}</p>;
  return (
    <div className="text-xs">
      <span className="text-muted-foreground">deploy address: </span>
      <a
        href={explorer(art.address!)}
        target="_blank"
        rel="noreferrer"
        className="text-primary inline-flex items-center gap-1 font-mono hover:underline"
      >
        {art.address}
        <ArrowUpRight className="size-3" />
      </a>
    </div>
  );
}

/* ------------------------------------------------------------------- page */

export default function DeployPage() {
  const { mode, net } = useNetworkMode();
  const wallet = useTonAddress();
  const [tonConnectUI] = useTonConnectUI();
  const testnet = mode === "testnet";

  const [cfg, setCfg] = React.useState<GpConfig>(GP_DEFAULT);
  const [admin, setAdmin] = React.useState("");
  const [feeRecipient, setFeeRecipient] = React.useState("");
  const [upgradeDelay, setUpgradeDelay] = React.useState(GP_DEFAULT.unbondingSecs);
  const [status, setStatus] = React.useState<string | null>(null);
  const [escrowTon, setEscrowTon] = React.useState(100);

  // Prefill admin / fee recipient with the connected wallet.
  React.useEffect(() => {
    if (wallet) {
      setAdmin((a) => a || wallet);
      setFeeRecipient((a) => a || wallet);
    }
  }, [wallet]);

  const set = <K extends keyof GpConfig>(k: K, v: number) => setCfg((c) => ({ ...c, [k]: v }));
  const errs = validateGp(cfg);
  const adminOk = admin.trim().length > 0;
  const gpArt =
    adminOk && errs.length === 0
      ? buildGlobalParamsDeploy(cfg, admin, feeRecipient || admin, upgradeDelay, testnet)
      : null;

  async function send(art: DeployArtifact, value = "0.1") {
    if (!art.ok || !art.address || !art.stateInitBoc) {
      setStatus(`✗ ${art.error ?? "could not build deploy"}`);
      return;
    }
    if (!wallet) {
      tonConnectUI.openModal();
      return;
    }
    try {
      setStatus("Awaiting wallet confirmation…");
      await tonConnectUI.sendTransaction({
        validUntil: Math.floor(Date.now() / 1000) + 600,
        network: testnet ? CHAIN.TESTNET : CHAIN.MAINNET,
        messages: [
          { address: art.address, amount: toNano(value).toString(), stateInit: art.stateInitBoc },
        ],
      });
      setStatus(`✓ Deploy sent to ${net.label} → ${art.address}`);
    } catch (e) {
      setStatus(`✗ ${e instanceof Error ? e.message : "transaction rejected"}`);
    }
  }

  function copyToml() {
    navigator.clipboard?.writeText(gpToToml(cfg, mode));
    setStatus("✓ Config TOML copied to clipboard");
  }

  const walletShort = wallet ? `${wallet.slice(0, 6)}…${wallet.slice(-4)}` : null;

  return (
    <div className="space-y-8">
      <PageHeader
        title="Configure & deploy"
        description="Connect a TON wallet, edit the on-chain economic parameters, and deploy the sharded settlement contracts to testnet or mainnet — signed and broadcast by your wallet."
        icon={<Rocket />}
      >
        <Badge variant={testnet ? "info" : "destructive"} className="gap-1.5">
          {testnet ? <CircleCheck className="size-3" /> : <ShieldAlert className="size-3" />}
          {net.label}
        </Badge>
      </PageHeader>

      <Explainer
        what="This turns the console into a real dApp: your wallet signs the deploy, the contract code comes from the project's compiled artifacts, and the init data is built from the config you edit below (the GlobalParams EcoParams encoding is cryptographically verified against the node's reference)."
        impact="You can stand up the economic layer yourself on testnet (play money) or mainnet (real funds) without any trusted operator — switch the target from the network toggle in the header."
      />

      {/* Status row */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <Stat
          label="Wallet"
          value={walletShort ?? "not connected"}
          sub={wallet ? "signs the deploy" : "connect to deploy"}
          icon={<Wallet />}
          accent={wallet ? "ok" : "warn"}
        />
        <Stat
          label="Target network"
          value={net.label}
          sub={testnet ? "play money" : "REAL FUNDS"}
          icon={<Landmark />}
          accent={testnet ? "info" : "destructive"}
          hint="Set by the header toggle. The deploy transaction is tagged to this chain; your wallet rejects it if it is on the other network."
        />
        <Stat
          label="EcoParams encoder"
          value={ECO_OK ? "verified" : "mismatch"}
          sub="vs. Rust reference hash"
          icon={<BadgeCheck />}
          accent={ECO_OK ? "ok" : "destructive"}
          hint="Our in-browser cell encoding reproduces the exact hash the settlement crate computed — so the config you deploy is byte-correct."
        />
        <Stat
          label="Contract code"
          value={`${["GlobalParams", "JobEscrow", "StakeVault", "RecordAnchor"].filter(codeHashOk).length}/4 ok`}
          sub="artifact hash check"
          icon={<CircleCheck />}
          accent="ok"
          hint="Each deploy uses the compiled code BoC from the repo; we re-hash it to confirm it matches the recorded code hash."
        />
      </div>

      {!wallet ? (
        <Card className="border-primary/40">
          <CardContent className="flex flex-col items-center gap-3 py-8 text-center">
            <Wallet className="text-primary size-7" />
            <div>
              <div className="font-medium">Connect a TON wallet to deploy</div>
              <p className="text-muted-foreground mt-1 text-sm">
                Use Tonkeeper, MyTonWallet, or any TON Connect wallet. You can still edit and validate
                the config below without connecting.
              </p>
            </div>
            <Button onClick={() => tonConnectUI.openModal()}>
              <Wallet className="size-4" /> Connect wallet
            </Button>
          </CardContent>
        </Card>
      ) : null}

      {/* GlobalParams editor */}
      <Card>
        <CardHeader>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <CardTitle className="flex items-center gap-2">
                <Coins className="size-4 text-primary" /> GlobalParams — economic config
              </CardTitle>
              <CardDescription>
                The single on-chain contract every node reads. Edit, validate, and deploy it.
              </CardDescription>
            </div>
            <div className="flex items-center gap-2">
              <Button variant="outline" size="sm" onClick={copyToml}>
                <Copy className="size-3.5" /> Copy TOML
              </Button>
              <Button variant="outline" size="sm" onClick={() => setCfg(GP_DEFAULT)}>
                Reset
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-5">
          {/* addresses */}
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1">
              <span className="text-muted-foreground text-xs">admin address</span>
              <Input value={admin} onChange={(e) => setAdmin(e.target.value)} placeholder="EQ… / 0:…" className="h-8 font-mono text-xs" />
            </label>
            <label className="flex flex-col gap-1">
              <span className="text-muted-foreground text-xs">fee recipient</span>
              <Input value={feeRecipient} onChange={(e) => setFeeRecipient(e.target.value)} placeholder="defaults to admin" className="h-8 font-mono text-xs" />
            </label>
          </div>

          {GP_GROUPS.map((g) => (
            <div key={g.title}>
              <SectionTitle className="mb-2">{g.title}</SectionTitle>
              <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4">
                {g.fields.map(([k, label]) => (
                  <NumberField key={k} label={label} value={cfg[k]} onChange={(v) => set(k, v)} />
                ))}
              </div>
            </div>
          ))}

          <div>
            <SectionTitle className="mb-2">Upgrade timelock</SectionTitle>
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              <NumberField label="upgrade delay secs" value={upgradeDelay} onChange={setUpgradeDelay} />
            </div>
          </div>

          <Separator />

          {/* validation + deploy */}
          {errs.length ? (
            <div className="border-destructive/30 bg-destructive/10 rounded-md border p-3 text-xs">
              <div className="text-destructive mb-1 flex items-center gap-1.5 font-medium">
                <AlertTriangle className="size-3.5" /> Fix before deploying
              </div>
              <ul className="text-destructive/90 list-inside list-disc space-y-0.5">
                {errs.map((e) => (
                  <li key={e}>{e}</li>
                ))}
              </ul>
            </div>
          ) : (
            <div className="text-[var(--ok)] flex items-center gap-1.5 text-xs">
              <CircleCheck className="size-3.5" /> config valid (passes on-chain validateEcoParams)
            </div>
          )}

          <div className="flex flex-wrap items-center justify-between gap-3">
            {gpArt ? <AddrResult art={gpArt} explorer={net.explorer} /> : <span className="text-muted-foreground text-xs">enter an admin address to compute the deploy address</span>}
            <Button
              disabled={!gpArt?.ok}
              onClick={() => gpArt && send(gpArt, "0.1")}
              variant={testnet ? "default" : "destructive"}
            >
              <Rocket className="size-4" /> Deploy GlobalParams to {net.label}
            </Button>
          </div>
        </CardContent>
      </Card>

      {/* Core contracts */}
      <div>
        <SectionTitle hint="owner/authority default to your wallet" info="The other sharded contracts. They reference the deployer wallet as owner/authority by default; edit before mainnet.">
          Core contracts
        </SectionTitle>
        <div className="grid gap-4 lg:grid-cols-3">
          {/* JobEscrow */}
          <Card>
            <CardHeader>
              <CardTitle className="font-mono text-sm">JobEscrow</CardTitle>
              <CardDescription>Per-job HTLC escrow.</CardDescription>
            </CardHeader>
            <CardContent className="space-y-3">
              <NumberField label="escrow B (TON)" value={escrowTon} onChange={setEscrowTon} />
              <Button
                size="sm"
                className="w-full"
                disabled={!wallet}
                variant={testnet ? "default" : "destructive"}
                onClick={() =>
                  send(
                    buildJobEscrowDeploy({
                      requester: admin || wallet,
                      arbiter: admin || wallet,
                      treasury: feeRecipient || admin || wallet,
                      escrowAmountTon: escrowTon,
                      deadlineUnix: Math.floor(Date.now() / 1000) + 3600,
                      expectedHashHex: "0",
                      paramsVersion: 1,
                      testnet,
                    }),
                    String(escrowTon + 0.1)
                  )
                }
              >
                <Rocket className="size-3.5" /> Deploy
              </Button>
            </CardContent>
          </Card>

          {/* StakeVault */}
          <Card>
            <CardHeader>
              <CardTitle className="font-mono text-sm">StakeVault</CardTitle>
              <CardDescription>Per-node bond + Duckton master.</CardDescription>
            </CardHeader>
            <CardContent className="space-y-3">
              <p className="text-muted-foreground text-xs">
                owner/slasher = your wallet · splits + windows from the config above.
              </p>
              <Button
                size="sm"
                className="w-full"
                disabled={!wallet}
                variant={testnet ? "default" : "destructive"}
                onClick={() =>
                  send(
                    buildStakeVaultDeploy({
                      owner: admin || wallet,
                      slasher: admin || wallet,
                      upgradeAuthority: admin || wallet,
                      treasury: feeRecipient || admin || wallet,
                      redundancyPool: feeRecipient || admin || wallet,
                      minStakeTon: cfg.minStakeInternalTon,
                      unbondingSecs: cfg.unbondingSecs,
                      challengeWindowSecs: cfg.challengeWindowSecs,
                      keeperGraceSecs: 86400,
                      keeperBountyBps: 200,
                      splitChallengerBps: cfg.splitChallengerBps,
                      splitRedundancyBps: cfg.splitRedundancyBps,
                      splitBurnBps: cfg.splitBurnBps,
                      splitTreasuryBps: cfg.splitTreasuryBps,
                      testnet,
                    })
                  )
                }
              >
                <Rocket className="size-3.5" /> Deploy
              </Button>
            </CardContent>
          </Card>

          {/* RecordAnchor */}
          <Card>
            <CardHeader>
              <CardTitle className="font-mono text-sm">RecordAnchor</CardTitle>
              <CardDescription>Per-epoch Merkle anchor + disputes.</CardDescription>
            </CardHeader>
            <CardContent className="space-y-3">
              <p className="text-muted-foreground text-xs">
                authority/keeper/treasury = your wallet · min weight 100 TON, bond 1 TON.
              </p>
              <Button
                size="sm"
                className="w-full"
                disabled={!wallet}
                variant={testnet ? "default" : "destructive"}
                onClick={() =>
                  send(
                    buildRecordAnchorDeploy({
                      verdictAuthority: admin || wallet,
                      treasury: feeRecipient || admin || wallet,
                      keeper: admin || wallet,
                      upgradeAuthority: admin || wallet,
                      minStakeWeightTon: 100,
                      disputeBondMinTon: 1,
                      testnet,
                    })
                  )
                }
              >
                <Rocket className="size-3.5" /> Deploy
              </Button>
            </CardContent>
          </Card>
        </div>
      </div>

      {status ? (
        <div
          className={`rounded-md border p-3 text-sm ${
            status.startsWith("✗")
              ? "border-destructive/30 bg-destructive/10 text-destructive"
              : "border-[var(--ok)]/30 bg-[var(--ok)]/10 text-[var(--ok)]"
          }`}
        >
          {status}
        </div>
      ) : null}

      <div className="text-muted-foreground space-y-1 rounded-lg border p-3.5 text-xs">
        <div className="text-foreground/80 flex items-center gap-1.5 font-medium">
          <AlertTriangle className="size-3.5" /> Before you deploy
        </div>
        <p>
          • Deploys are real on-chain transactions signed by your wallet. Start on{" "}
          <span className="font-mono">testnet</span> (header toggle) and fund the wallet from a faucet.
        </p>
        <p>
          • The <span className="font-mono">GlobalParams</span> EcoParams encoding is verified against the
          node&apos;s reference hash; the contract code is the repo&apos;s compiled artifact (hash-checked).
          The surrounding storage layout follows the current Tolk structs.
        </p>
        <p>
          • For mainnet, confirm the published source with <span className="font-mono">acton verify</span>{" "}
          and review every parameter — these contracts custody stake and escrow.
        </p>
      </div>
    </div>
  );
}
