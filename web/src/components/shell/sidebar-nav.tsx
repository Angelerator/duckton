"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { cn } from "@/lib/utils";
import { nav, navGroups } from "@/lib/nav";
import { navIcons } from "./icons";

export function SidebarNav({ onNavigate }: { onNavigate?: () => void }) {
  const pathname = usePathname();

  return (
    <nav className="flex flex-col gap-5 px-3 py-2">
      {navGroups.map((group) => (
        <div key={group}>
          <div className="text-muted-foreground/70 mb-1.5 px-3 text-[10px] font-semibold tracking-[0.12em] uppercase">
            {group}
          </div>
          <ul className="flex flex-col gap-0.5">
            {nav
              .filter((i) => i.group === group)
              .map((item) => {
                const Icon = navIcons[item.icon] ?? navIcons.LayoutDashboard;
                const active =
                  item.href === "/"
                    ? pathname === "/"
                    : pathname.startsWith(item.href);
                return (
                  <li key={item.href}>
                    <Link
                      href={item.href}
                      onClick={onNavigate}
                      className={cn(
                        "group relative flex items-center gap-2.5 rounded-md px-3 py-2 text-sm transition-colors",
                        active
                          ? "bg-sidebar-accent text-sidebar-accent-foreground font-medium"
                          : "text-muted-foreground hover:bg-sidebar-accent/60 hover:text-foreground"
                      )}
                    >
                      {active ? (
                        <span className="bg-primary absolute left-0 top-1/2 h-5 w-0.5 -translate-y-1/2 rounded-full" />
                      ) : null}
                      <Icon
                        className={cn(
                          "size-4 shrink-0",
                          active ? "text-primary" : "text-muted-foreground"
                        )}
                      />
                      <span>{item.title}</span>
                      {item.badge ? (
                        <span className="bg-primary/15 text-primary ml-auto rounded px-1.5 py-0.5 text-[10px] font-semibold">
                          {item.badge}
                        </span>
                      ) : null}
                    </Link>
                  </li>
                );
              })}
          </ul>
        </div>
      ))}
    </nav>
  );
}
