"use client";

import { ArrowUpRight, CircleCheck, CircleX, Globe, ShieldAlert } from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { KV } from "@/components/common/atoms";
import { CopyId } from "@/components/common/copy";
import { useNetworkMode } from "@/lib/network-mode";
import { short } from "@/lib/format";

export function TonNetworkPanel() {
  const { net } = useNetworkMode();
  const order = ["GlobalParams", "StakeVault", "JobEscrow", "RecordAnchor"];

  return (
    <Card className={net.realFunds ? "border-destructive/40" : undefined}>
      <CardHeader>
        <div className="flex items-center justify-between gap-2">
          <CardTitle className="flex items-center gap-2">
            <Globe className="size-4 text-primary" /> Active network — {net.label}
          </CardTitle>
          {net.deployed ? (
            <Badge variant="ok" className="gap-1">
              <CircleCheck className="size-3" /> deployed &amp; verified
            </Badge>
          ) : (
            <Badge variant="destructive" className="gap-1">
              <CircleX className="size-3" /> not deployed
            </Badge>
          )}
        </div>
        <CardDescription>
          Switch network from the header. Addresses, RPC and explorer below reflect the selected mode.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="grid gap-x-8 sm:grid-cols-2">
          <dl>
            <KV label="RPC endpoint">
              <span className="font-mono text-xs">{net.rpc}</span>
            </KV>
            <KV label="funds">
              {net.realFunds ? (
                <Badge variant="destructive">real funds</Badge>
              ) : (
                <Badge variant="muted">play money</Badge>
              )}
            </KV>
            <KV label="settlement guard">
              {net.confirmed ? (
                <Badge variant="ok">enabled</Badge>
              ) : (
                <Badge variant="warn" className="gap-1">
                  <ShieldAlert className="size-3" /> mainnet_confirmed = false
                </Badge>
              )}
            </KV>
          </dl>
          <dl>
            {order.map((name) => {
              const addr = net.contracts[name] ?? null;
              return (
                <KV key={name} label={name}>
                  {addr ? (
                    <span className="inline-flex items-center gap-1.5">
                      <CopyId value={addr} display={short(addr, 6, 4)} />
                      <a
                        href={net.explorer(addr)}
                        target="_blank"
                        rel="noreferrer"
                        className="text-muted-foreground hover:text-primary"
                      >
                        <ArrowUpRight className="size-3" />
                      </a>
                    </span>
                  ) : (
                    <span className="text-muted-foreground text-xs">—</span>
                  )}
                </KV>
              );
            })}
          </dl>
        </div>
        {net.realFunds ? (
          <p className="text-destructive/90 border-destructive/30 bg-destructive/10 rounded-md border px-3 py-2 text-xs">
            GlobalParams (the platform-wide singleton) is live on mainnet; StakeVault, JobEscrow and
            RecordAnchor are deployed on demand. Settlement remains guarded by{" "}
            <code className="font-mono">economics.mainnet_confirmed = false</code> — the node refuses on-chain
            mainnet settlement until explicitly enabled. Contracts and economic params are identical to
            testnet (network-agnostic design); only the RPC, explorer and real-funds posture change.
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}
