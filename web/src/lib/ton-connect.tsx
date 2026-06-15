"use client";

import * as React from "react";
import { TonConnectUIProvider, useTonAddress, useTonConnectUI } from "@tonconnect/ui-react";
import { Wallet, LogOut } from "lucide-react";
import { Button } from "@/components/ui/button";
import { short } from "@/lib/format";

/**
 * Wraps the app in the TON Connect context so any page can connect a wallet and
 * send transactions (deploys). The manifest is served from /public; the URL is
 * resolved from the current origin at runtime so it works on any host/port.
 */
export function TonConnectProvider({ children }: { children: React.ReactNode }) {
  const manifestUrl =
    typeof window === "undefined"
      ? "/tonconnect-manifest.json"
      : `${window.location.origin}/tonconnect-manifest.json`;
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
