"use client";

import * as React from "react";
import { ArrowUpRight, Coins, ServerCog, Terminal } from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { PageHeader } from "@/components/common/atoms";

const SEED = "seed.duckton.com:9494";
const GLOBAL_PARAMS = "EQCV59kSoDDgmE8cheBNYwl2oYL9h5nkbyCqn99tN-N1w9Gg";

const EXAMPLES = [
  {
    id: "query",
    label: "Run a verified query",
    icon: <Terminal className="size-4" />,
    desc: "Install the extension, join the public network through the live seed node, and run SQL that independent hosts execute redundantly — accepted only when a quorum agrees byte-for-byte.",
    code: `-- Install + load the extension (DuckDB Community Extensions)
INSTALL duckton FROM community;
LOAD duckton;

-- Join the public Duckton network via the live seed node
CALL p2p_join(bootstrap => ['${SEED}']);

-- Run SQL across independent nodes, verified by quorum
SELECT * FROM p2p_query('SELECT 42 AS answer');

-- Target a subset of nodes (your company network / region / group)
SELECT * FROM p2p_query('SELECT count(*) FROM read_parquet(''s3://...'')',
                        groups => ['eu-internal'], regions => ['eu']);`,
  },
  {
    id: "earn",
    label: "Share your machine & earn",
    icon: <ServerCog className="size-4" />,
    desc: "Donate a slice of an idle laptop, PC, or server and start serving others' jobs. Set your own rate (whole TON) to accept paid work — no central broker, no sign-up.",
    code: `LOAD duckton;

-- Donate compute and start serving others' jobs (become a host)
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
    icon: <Coins className="size-4" />,
    desc: "Paid jobs lock the requester's max bid in a per-job escrow on TON. On settle, the winner, agreeing verifiers, and the platform treasury are paid and the remainder refunds — enforced on-chain by the live GlobalParams contract.",
    code: `-- Turn on the TON money rail (mainnet = real funds, needs confirm)
CALL p2p_economics(enabled => true, settlement => 'ton',
                   network => 'mainnet', confirm => true,
                   fee_recipient => 'EQ...your-treasury...');

-- Point at the LIVE mainnet GlobalParams contract
CALL p2p_contracts(global_params => '${GLOBAL_PARAMS}');

-- Wallet via secure file refs (never paste secrets into SQL)
CALL p2p_wallet(rpc => 'https://toncenter.com/api/v2/',
                mnemonic_file => '~/.duckton/wallet.mnemonic',
                address => 'EQ...');

-- Run a PAID job: escrow opens on TON, splits enforced on-chain
SELECT * FROM p2p_query('SELECT ...', payment => 'paid',
                        replicas => 3, quorum => 2);`,
  },
];

export default function ConnectPage() {
  const [active, setActive] = React.useState(EXAMPLES[0].id);
  const ex = EXAMPLES.find((e) => e.id === active) ?? EXAMPLES[0];
  return (
    <div className="space-y-8">
      <PageHeader
        title="Connect"
        description="Everything is a DuckDB table function — join the network, share your machine, and settle paid jobs on TON, all from plain SQL. Public jobs are free and fully off-chain."
        icon={<Terminal />}
      >
        <Button asChild variant="outline">
          <a href="https://docs.duckton.com" target="_blank" rel="noreferrer">
            Full docs <ArrowUpRight />
          </a>
        </Button>
      </PageHeader>

      <div className="flex flex-wrap gap-2">
        {EXAMPLES.map((e) => (
          <Button key={e.id} variant={active === e.id ? "default" : "outline"} size="sm" onClick={() => setActive(e.id)}>
            {e.icon}
            {e.label}
          </Button>
        ))}
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">{ex.icon} {ex.label}</CardTitle>
          <CardDescription>{ex.desc}</CardDescription>
        </CardHeader>
        <CardContent>
          <pre className="bg-muted/50 overflow-x-auto rounded-lg border p-4 text-xs leading-relaxed">
            <code>{ex.code}</code>
          </pre>
        </CardContent>
      </Card>

      <p className="text-muted-foreground text-sm">
        Live seed node: <span className="text-foreground font-mono">{SEED}</span> · public jobs are free, off-chain,
        and need no wallet.
      </p>
    </div>
  );
}
