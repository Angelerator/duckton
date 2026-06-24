import type { NavItem } from "./types";

export const nav: NavItem[] = [
  { title: "Network", href: "/overview", icon: "Network", group: "Console", badge: "live", description: "Live nodes, distributed jobs, and on-chain settlement" },
  { title: "Connect", href: "/connect", icon: "Terminal", group: "Console", description: "Join the network and run verified queries in SQL" },

  { title: "Docs", href: "https://docs.duckton.com", icon: "BookOpen", group: "Resources", description: "Full documentation" },
  { title: "GitHub", href: "https://github.com/Angelerator/duckton", icon: "BookOpen", group: "Resources", description: "Source code" },
];

export const navGroups = ["Console", "Resources"] as const;
