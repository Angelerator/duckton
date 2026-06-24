"use client";

import * as React from "react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import { Menu, BookOpen } from "lucide-react";
import { Sheet, SheetContent, SheetTitle, SheetTrigger } from "@/components/ui/sheet";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { SidebarNav } from "./sidebar-nav";
import {
  MainnetWarningBar,
  NetworkModeProvider,
  NetworkToggle,
} from "@/lib/network-mode";
import { TonConnectProvider, WalletButton } from "@/lib/ton-connect";
import { LiveProvider, LiveStatus } from "@/lib/live";
import { meta } from "@/lib/data";

function Brand({ onNavigate }: { onNavigate?: () => void }) {
  return (
    <Link href="/overview" onClick={onNavigate} className="flex items-center gap-2.5 px-2">
      {/* eslint-disable-next-line @next/next/no-img-element */}
      <img src="/duckton-logo.png" alt="Duckton" className="size-8 rounded-lg shadow-sm" />
      <div className="leading-tight">
        <div className="text-sm font-semibold">Duckton</div>
        <div className="text-muted-foreground text-[10px]">p2p · console</div>
      </div>
    </Link>
  );
}

function SidebarFooter() {
  return (
    <div className="border-t px-4 py-3 text-xs">
      <div className="flex items-center justify-between">
        <span className="text-muted-foreground">this node</span>
        <span className="font-mono">node_7af3…d2b8</span>
      </div>
      <div className="mt-1.5 flex items-center justify-between">
        <span className="text-muted-foreground">protocol</span>
        <Badge variant="muted" className="font-mono">
          p2p/{meta.protocolVersion}
        </Badge>
      </div>
      <div className="mt-1.5 flex items-center justify-between">
        <span className="text-muted-foreground">duckton</span>
        <span className="inline-flex items-center gap-1.5">
          <Badge variant="ok" className="font-mono">
            community v{meta.workspaceVersion}
          </Badge>
        </span>
      </div>
      <div className="mt-1.5 flex items-center justify-between">
        <span className="text-muted-foreground">engine</span>
        <span className="font-mono">{meta.engineVersion}</span>
      </div>
    </div>
  );
}

export function AppShell({ children }: { children: React.ReactNode }) {
  const [open, setOpen] = React.useState(false);
  const pathname = usePathname();
  // The public marketing landing (apex `/`) renders chrome-free (no sidebar/header)
  // but still inside the data providers, so it can show live grid stats.
  const bare = pathname === "/";

  if (bare) {
    return (
      <TonConnectProvider>
        <NetworkModeProvider>
          <LiveProvider>{children}</LiveProvider>
        </NetworkModeProvider>
      </TonConnectProvider>
    );
  }

  return (
    <TonConnectProvider>
    <NetworkModeProvider>
    <LiveProvider>
      <a
        href="#main-content"
        className="bg-primary text-primary-foreground sr-only z-50 rounded-md px-3 py-2 text-sm font-medium focus:not-sr-only focus:fixed focus:left-4 focus:top-3"
      >
        Skip to content
      </a>
      <div className="flex min-h-svh">
      {/* Desktop sidebar */}
      <aside className="bg-sidebar fixed inset-y-0 left-0 z-30 hidden w-64 flex-col border-r lg:flex">
        <div className="flex h-14 items-center border-b">
          <Brand />
        </div>
        <div className="flex-1 overflow-y-auto">
          <SidebarNav />
        </div>
        <SidebarFooter />
      </aside>

      {/* Main column */}
      <div className="flex min-w-0 flex-1 flex-col lg:pl-64">
        <header className="bg-background/80 supports-[backdrop-filter]:bg-background/60 sticky top-0 z-20 flex h-14 items-center gap-3 border-b px-4 backdrop-blur md:px-6">
          {/* Mobile menu */}
          <Sheet open={open} onOpenChange={setOpen}>
            <SheetTrigger asChild>
              <Button variant="ghost" size="icon" className="lg:hidden" aria-label="Open navigation menu">
                <Menu />
              </Button>
            </SheetTrigger>
            <SheetContent side="left" className="w-72 p-0">
              <SheetTitle className="sr-only">Navigation</SheetTitle>
              <div className="flex h-14 items-center border-b">
                <Brand onNavigate={() => setOpen(false)} />
              </div>
              <div className="overflow-y-auto">
                <SidebarNav onNavigate={() => setOpen(false)} />
              </div>
            </SheetContent>
          </Sheet>

          <LiveStatus />

          <div className="ml-auto flex items-center gap-2">
            <NetworkToggle />
            <Button variant="outline" size="sm" asChild className="hidden md:inline-flex">
              <a
                href="https://duckdb.org/docs/current/core_extensions/quack"
                target="_blank"
                rel="noreferrer"
              >
                <BookOpen className="size-3.5" />
                Quack docs
              </a>
            </Button>
            <WalletButton />
          </div>
        </header>

        <MainnetWarningBar />

        <main
          id="main-content"
          className="mx-auto w-full max-w-[1400px] flex-1 px-4 py-6 md:px-6 lg:py-8"
        >
          {children}
        </main>
      </div>
      </div>
    </LiveProvider>
    </NetworkModeProvider>
    </TonConnectProvider>
  );
}
