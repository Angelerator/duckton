"use client";

import * as React from "react";
import { AlertTriangle } from "lucide-react";
import { ton } from "@/lib/data";
import { cn } from "@/lib/utils";

export type NetMode = "testnet" | "mainnet";

export interface NetworkInfo {
  mode: NetMode;
  label: string;
  rpc: string;
  /** build an explorer URL for an address */
  explorer: (addr: string) => string;
  deployed: boolean;
  realFunds: boolean;
  /** mainnet is guarded until explicitly confirmed (economics.mainnet_confirmed) */
  confirmed: boolean;
  contracts: Record<string, string | null>;
  wallet: string | null;
  resultHash: string | null;
}

const NO_CONTRACTS = {
  StakeVault: null,
  JobEscrow: null,
  RecordAnchor: null,
  GlobalParams: null,
} as const;

/** Real per-network descriptors, derived from the snapshot. */
export const NETWORKS: Record<NetMode, NetworkInfo> = {
  testnet: {
    mode: "testnet",
    label: "Testnet",
    rpc: ton.rpc || "https://testnet.toncenter.com/api/v2/",
    explorer: (a) => `https://testnet.tonviewer.com/${a}`,
    deployed: true,
    realFunds: false,
    confirmed: true,
    contracts: ton.deployments,
    wallet: ton.wallet,
    resultHash: ton.resultHash,
  },
  mainnet: {
    mode: "mainnet",
    label: "Mainnet",
    rpc: "https://toncenter.com/api/v2/",
    explorer: (a) => `https://tonviewer.com/${a}`,
    // GlobalParams (platform-wide singleton) is live on mainnet. StakeVault /
    // JobEscrow / RecordAnchor are deployed on demand (per-node / per-job), so
    // they stay null here until created.
    deployed: true,
    realFunds: true,
    confirmed: false, // economics.mainnet_confirmed === false (settlement still guarded)
    contracts: {
      ...NO_CONTRACTS,
      GlobalParams: "EQCV59kSoDDgmE8cheBNYwl2oYL9h5nkbyCqn99tN-N1w9Gg",
    },
    wallet: "EQABq1UU-PLPTQlDUwFNju_4xXyHLxSfPPyfqEvLPrjoxS82",
    resultHash: null,
  },
};

interface Ctx {
  mode: NetMode;
  net: NetworkInfo;
  setMode: (m: NetMode) => void;
}

const NetworkModeContext = React.createContext<Ctx | null>(null);
const STORAGE_KEY = "duckgrid-net-mode";

// localStorage-backed external store for the selected network mode. Using
// useSyncExternalStore (rather than a mount effect that calls setState) keeps
// SSR deterministic — the server snapshot is always "testnet" so first paint
// matches — while the client reads the persisted value on hydration.
const modeListeners = new Set<() => void>();

function readMode(): NetMode {
  try {
    const saved = window.localStorage.getItem(STORAGE_KEY);
    if (saved === "mainnet" || saved === "testnet") return saved;
  } catch {
    /* ignore */
  }
  return "testnet";
}

function subscribeMode(onChange: () => void): () => void {
  modeListeners.add(onChange);
  window.addEventListener("storage", onChange);
  return () => {
    modeListeners.delete(onChange);
    window.removeEventListener("storage", onChange);
  };
}

function writeMode(m: NetMode): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, m);
  } catch {
    /* ignore */
  }
  // Notify same-tab subscribers (the native `storage` event only fires cross-tab).
  modeListeners.forEach((l) => l());
}

export function NetworkModeProvider({ children }: { children: React.ReactNode }) {
  const mode = React.useSyncExternalStore(subscribeMode, readMode, (): NetMode => "testnet");
  const setMode = React.useCallback((m: NetMode) => writeMode(m), []);

  const value = React.useMemo<Ctx>(() => ({ mode, net: NETWORKS[mode], setMode }), [mode, setMode]);
  return <NetworkModeContext.Provider value={value}>{children}</NetworkModeContext.Provider>;
}

export function useNetworkMode(): Ctx {
  const ctx = React.useContext(NetworkModeContext);
  if (!ctx) throw new Error("useNetworkMode must be used within NetworkModeProvider");
  return ctx;
}

/** Header segmented control to switch network mode. */
export function NetworkToggle() {
  const { mode, setMode } = useNetworkMode();
  return (
    <div className="bg-card inline-flex items-center rounded-md border p-0.5 text-xs">
      {(["testnet", "mainnet"] as NetMode[]).map((m) => (
        <button
          key={m}
          type="button"
          onClick={() => setMode(m)}
          className={cn(
            "rounded px-2 py-1 font-medium capitalize transition-colors",
            mode === m
              ? m === "mainnet"
                ? "bg-destructive/20 text-destructive"
                : "bg-primary/15 text-primary"
              : "text-muted-foreground hover:text-foreground"
          )}
        >
          {m}
        </button>
      ))}
    </div>
  );
}

/** App-wide strip warning shown only while mainnet is selected. */
export function MainnetWarningBar() {
  const { mode } = useNetworkMode();
  if (mode !== "mainnet") return null;
  return (
    <div className="border-destructive/30 bg-destructive/10 text-destructive flex items-center justify-center gap-2 border-b px-4 py-1.5 text-center text-xs md:px-6">
      <AlertTriangle className="size-3.5 shrink-0" />
      <span>
        <strong>Mainnet selected — real funds.</strong> GlobalParams is live on mainnet, but settlement
        is still guarded (<code className="font-mono">mainnet_confirmed = false</code>);
        the node refuses on-chain mainnet settlement until explicitly enabled.
      </span>
    </div>
  );
}
