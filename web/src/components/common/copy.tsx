"use client";

import * as React from "react";
import { Check, Copy } from "lucide-react";
import { cn } from "@/lib/utils";
import { short } from "@/lib/format";

export function CopyId({
  value,
  display,
  className,
  truncate = true,
}: {
  value: string;
  display?: string;
  className?: string;
  truncate?: boolean;
}) {
  const [copied, setCopied] = React.useState(false);
  const label = display ?? (truncate ? short(value) : value);

  function onCopy() {
    navigator.clipboard?.writeText(value).then(
      () => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      },
      () => {}
    );
  }

  return (
    <button
      type="button"
      onClick={onCopy}
      title={value}
      className={cn(
        "group inline-flex items-center gap-1.5 rounded-md font-mono text-xs text-muted-foreground transition-colors hover:text-foreground",
        className
      )}
    >
      <span>{label}</span>
      {copied ? (
        <Check className="size-3 text-[var(--ok)]" />
      ) : (
        <Copy className="size-3 opacity-0 transition-opacity group-hover:opacity-100" />
      )}
    </button>
  );
}
