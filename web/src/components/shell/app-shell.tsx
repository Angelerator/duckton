"use client";

import * as React from "react";
import Link from "next/link";
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

function Brand() {
  return (
    <Link href="/" className="flex items-center gap-2.5 px-2">
      <div className="bg-primary text-primary-foreground grid size-8 place-items-center rounded-lg font-bold shadow-sm">
        <svg viewBox="0 0 24 24" className="size-5" fill="none">
          <path
            d="M4 7h5v5H4zM10 4h5v5h-5zM15 11h5v5h-5zM6 14h5v5H6z"
            fill="currentColor"
            opacity="0.9"
          />
          <path
            d="M9 9.5l4-2M14.5 11.5l-3 4M9.5 13.5l4.5-1"
            stroke="currentColor"
            strokeWidth="1.1"
            opacity="0.6"
          />
        </svg>
      </div>
      <div className="leading-tight">
        <div className="text-sm font-semibold">DuckGrid</div>
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
          p2p/0.2.0
        </Badge>
      </div>
      <div className="mt-1.5 flex items-center justify-between">
        <span className="text-muted-foreground">engine</span>
        <span className="font-mono">duckdb-1.1.3</span>
      </div>
    </div>
  );
}

export function AppShell({ children }: { children: React.ReactNode }) {
  const [open, setOpen] = React.useState(false);

  return (
    <TonConnectProvider>
    <NetworkModeProvider>
    <LiveProvider>
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
              <Button variant="ghost" size="icon" className="lg:hidden">
                <Menu />
              </Button>
            </SheetTrigger>
            <SheetContent side="left" className="w-72 p-0">
              <SheetTitle className="sr-only">Navigation</SheetTitle>
              <div className="flex h-14 items-center border-b">
                <Brand />
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

        <main className="mx-auto w-full max-w-[1400px] flex-1 px-4 py-6 md:px-6 lg:py-8">
          {children}
        </main>
      </div>
      </div>
    </LiveProvider>
    </NetworkModeProvider>
    </TonConnectProvider>
  );
}
