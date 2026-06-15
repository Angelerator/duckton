import * as React from "react";
import { Info, CircleHelp } from "lucide-react";
import { cn } from "@/lib/utils";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { lookupTerm } from "@/lib/glossary";

/**
 * A plain-language callout that explains a page/section: what it is and how it
 * impacts the system. Render it right under a PageHeader or above a section.
 */
export function Explainer({
  what,
  impact,
  className,
}: {
  what: React.ReactNode;
  impact: React.ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "bg-card/60 flex items-start gap-3 rounded-lg border p-3.5 text-sm",
        className
      )}
    >
      <div className="bg-primary/10 text-primary mt-0.5 flex size-7 shrink-0 items-center justify-center rounded-md">
        <Info className="size-4" />
      </div>
      <div className="min-w-0">
        <p className="text-foreground/90 leading-relaxed">{what}</p>
        <p className="text-muted-foreground mt-1 leading-relaxed">
          <span className="text-foreground/80 font-medium">Impact: </span>
          {impact}
        </p>
      </div>
    </div>
  );
}

/**
 * A small "?" icon with a tooltip. Pass `text` for a custom explanation, or
 * `term` to pull the plain definition + impact from the glossary.
 */
export function InfoHint({
  text,
  term,
  className,
}: {
  text?: React.ReactNode;
  term?: string;
  className?: string;
}) {
  const entry = term ? lookupTerm(term) : undefined;
  const body: React.ReactNode = text ?? (entry ? (
    <span>
      <span className="text-foreground font-medium">{entry.term}.</span> {entry.what}
      <span className="text-muted-foreground mt-1 block">Impact: {entry.impact}</span>
    </span>
  ) : null);
  if (!body) return null;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label="explain"
          className={cn(
            "text-muted-foreground/60 hover:text-foreground inline-flex translate-y-px cursor-help align-middle transition-colors",
            className
          )}
        >
          <CircleHelp className="size-3.5" />
        </button>
      </TooltipTrigger>
      <TooltipContent className="max-w-xs text-left leading-relaxed">{body}</TooltipContent>
    </Tooltip>
  );
}
