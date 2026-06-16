"use client";

import * as React from "react";
import { TonConnectUIProvider, useTonAddress, useTonConnectUI } from "@tonconnect/ui-react";
import { Wallet, LogOut } from "lucide-react";
import { Button } from "@/components/ui/button";
import { short } from "@/lib/format";

/**
 * Browser `fetch` failures reject with one of these messages, depending on the
 * engine. Used to recognise the TON Connect SDK's offline init failure (below).
 */
const OFFLINE_FETCH_MESSAGES = [
  "failed to fetch", // Chromium
  "load failed", // WebKit / Safari
  "networkerror when attempting to fetch resource", // Firefox
];

/**
 * The TON Connect SDK reaches out to the network at init (its wallet registry
 * and telemetry endpoints). In a no-egress environment those requests reject
 * with `TypeError: Failed to fetch`, and the SDK's async init surfaces it as an
 * *unhandled* promise rejection (the stack has no app frames). It fires on
 * every page because the provider mounts app-wide.
 *
 * This narrowly recognises only that offline fetch failure. Every fetch in this
 * app is awaited and handled, so an unhandled `Failed to fetch` TypeError can
 * only come from the SDK — we don't risk masking a real application error.
 */
function isOfflineFetchRejection(reason: unknown): boolean {
  if (!(reason instanceof TypeError)) return false;
  const message = reason.message.toLowerCase();
  return OFFLINE_FETCH_MESSAGES.some((m) => message.includes(m));
}

let warnedOffline = false;

/**
 * Resolve the TonConnect manifest URL. A wallet (often on a different device)
 * must be able to FETCH this, so it has to be an ABSOLUTE, reachable URL — never
 * a bare relative path or `localhost` that a phone can't reach.
 *
 * Precedence:
 *   1. NEXT_PUBLIC_TONCONNECT_MANIFEST_URL — set this to a public origin's
 *      manifest when serving behind a tunnel (cloudflared/ngrok) or in prod.
 *   2. `${window.location.origin}/tonconnect-manifest.json` — absolute URL built
 *      from the current origin at runtime (works on dev, a LAN IP, or prod).
 *   3. The relative path (SSR only; the provider re-resolves on the client).
 */
function resolveManifestUrl(): string {
  const fromEnv = process.env.NEXT_PUBLIC_TONCONNECT_MANIFEST_URL;
  if (fromEnv) return fromEnv;
  if (typeof window !== "undefined") {
    return `${window.location.origin}/tonconnect-manifest.json`;
  }
  return "/tonconnect-manifest.json";
}

/**
 * Wraps the app in the TON Connect context so any page can connect a wallet and
 * send transactions (deploys). The manifest is served from /public; the URL is
 * resolved from the current origin at runtime (or NEXT_PUBLIC_TONCONNECT_MANIFEST_URL)
 * so it works on any host/port and behind a public tunnel.
 *
 * We also absorb the SDK's offline init rejection (see
 * {@link isOfflineFetchRejection}) so it degrades quietly instead of logging an
 * uncaught error on every page. The real connect flow is untouched: when the
 * network is reachable nothing rejects, and the SDK already falls back to its
 * built-in wallet list when it isn't — so opening the modal still works.
 */
export function TonConnectProvider({ children }: { children: React.ReactNode }) {
  const manifestUrl = resolveManifestUrl();

  React.useEffect(() => {
    const onUnhandledRejection = (event: PromiseRejectionEvent) => {
      if (!isOfflineFetchRejection(event.reason)) return;
      event.preventDefault();
      if (!warnedOffline) {
        warnedOffline = true;
        console.warn(
          "[ton-connect] wallet registry unreachable — wallet connect is offline; it will work once the network is available.",
        );
      }
    };
    window.addEventListener("unhandledrejection", onUnhandledRejection);
    return () =>
      window.removeEventListener("unhandledrejection", onUnhandledRejection);
  }, []);

  return (
    <TonConnectUIProvider manifestUrl={manifestUrl}>{children}</TonConnectUIProvider>
  );
}

/** Themed connect / connected-address button for the header. */
export function WalletButton() {
  const address = useTonAddress();
  const [tonConnectUI] = useTonConnectUI();
  if (!address) {
    return (
      <Button size="sm" onClick={() => tonConnectUI.openModal()}>
        <Wallet className="size-3.5" />
        <span className="hidden sm:inline">Connect wallet</span>
        <span className="sm:hidden">Connect</span>
      </Button>
    );
  }
  return (
    <Button
      variant="outline"
      size="sm"
      onClick={() => tonConnectUI.disconnect()}
      title={`${address} — click to disconnect`}
      className="font-mono"
    >
      <Wallet className="size-3.5 text-[var(--ok)]" />
      {short(address, 4, 4)}
      <LogOut className="size-3 opacity-50" />
    </Button>
  );
}
