import type { NavItem } from "./types";

export const nav: NavItem[] = [
  { title: "Overview", href: "/", icon: "LayoutDashboard", group: "Operate", description: "Grid health, throughput, and live activity" },
  { title: "Query Console", href: "/query", icon: "Terminal", group: "Operate", description: "Run p2p_query / p2p_join / p2p_share against the grid" },
  { title: "Jobs", href: "/jobs", icon: "ListChecks", group: "Operate", description: "Job lifecycle + hedged execution timeline" },

  { title: "Network", href: "/network", icon: "Waypoints", group: "Grid", description: "Discovery, DHT, gossip, and NAT traversal" },
  { title: "Workers", href: "/workers", icon: "Server", group: "Grid", description: "Hosts donating compute — capacity & trust" },
  { title: "Transport", href: "/transport", icon: "Gauge", group: "Grid", description: "QUIC tuning, streaming, compression, benches" },
  { title: "Storage", href: "/storage", icon: "Database", group: "Grid", description: "Object-store providers, formats, encryption" },

  { title: "Trust & Attestation", href: "/trust", icon: "ShieldCheck", group: "Trust", description: "Reputation, attestation tiers, receipts, canaries" },
  { title: "Settlement", href: "/settlement", icon: "Coins", group: "Trust", description: "TON staking, escrow, earnings, slashing, anchoring" },
  { title: "On-chain (TON)", href: "/ton", icon: "Landmark", group: "Trust", badge: "live", description: "Deployed contracts, opcodes, GlobalParams, deploy/verify" },
  { title: "Deploy", href: "/deploy", icon: "Rocket", group: "Trust", badge: "wallet", description: "Connect a wallet, edit config, deploy contracts on-chain" },

  { title: "Configuration", href: "/config", icon: "Settings2", group: "System", description: "Layered config: defaults < file < env < per-call" },
  { title: "Protocol", href: "/protocol", icon: "Network", group: "System", description: "Wire messages, versioning, and the request lifecycle" },
  { title: "Glossary", href: "/glossary", icon: "BookOpen", group: "System", description: "Plain-language explanations of every term" },
];

export const navGroups = ["Operate", "Grid", "Trust", "System"] as const;
