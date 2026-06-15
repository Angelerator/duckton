"use client";

import * as React from "react";
import dynamic from "next/dynamic";
import type { Data, Layout } from "plotly.js";
import { cn } from "@/lib/utils";

// Plotly touches `window`/`document`, so load it client-only via the factory +
// the prebuilt dist bundle (avoids bundling plotly.js from source).
const Plot = dynamic(
  async () => {
    const [factoryMod, plotlyMod] = await Promise.all([
      import("react-plotly.js/factory"),
      import("plotly.js-dist-min"),
    ]);
    const createPlotlyComponent = factoryMod.default;
    const Plotly = (plotlyMod as { default?: unknown }).default ?? plotlyMod;
    return createPlotlyComponent(Plotly as object);
  },
  {
    ssr: false,
    loading: () => (
      <div className="bg-muted/30 flex h-full w-full animate-pulse items-center justify-center rounded-md text-xs text-muted-foreground">
        loading plot…
      </div>
    ),
  }
);

/** Theme palette (concrete hexes ≈ the CSS chart vars; Plotly needs literals). */
export const PALETTE = {
  emerald: "#34d399",
  blue: "#60a5fa",
  amber: "#fbbf24",
  violet: "#c084fc",
  red: "#f87171",
  teal: "#2dd4bf",
  slate: "#94a3b8",
  pink: "#f472b6",
};
const FONT = "var(--font-geist-sans), ui-sans-serif, system-ui, sans-serif";
const TEXT = "#cbd5e1";
const MUTED = "#8b93a7";
const GRID = "rgba(148,163,184,0.14)";

function baseLayout(height: number): Partial<Layout> {
  return {
    height,
    autosize: true,
    margin: { l: 48, r: 16, t: 16, b: 36 },
    paper_bgcolor: "rgba(0,0,0,0)",
    plot_bgcolor: "rgba(0,0,0,0)",
    font: { family: FONT, color: TEXT, size: 11 },
    colorway: Object.values(PALETTE),
    hoverlabel: {
      bgcolor: "#16181d",
      bordercolor: GRID,
      font: { family: FONT, color: TEXT, size: 11 },
    },
    legend: { font: { color: MUTED, size: 10 }, orientation: "h", y: -0.18 },
    xaxis: { gridcolor: GRID, zerolinecolor: GRID, tickfont: { color: MUTED }, automargin: true },
    yaxis: { gridcolor: GRID, zerolinecolor: GRID, tickfont: { color: MUTED }, automargin: true },
  };
}

export function PlotBase({
  data,
  layout,
  height = 300,
  className,
}: {
  data: Data[];
  layout?: Partial<Layout>;
  height?: number;
  className?: string;
}) {
  const merged: Partial<Layout> = { ...baseLayout(height), ...layout };
  return (
    <Plot
      data={data}
      layout={merged}
      config={{ displayModeBar: false, responsive: true }}
      style={{ width: "100%", height }}
      useResizeHandler
      className={cn("w-full", className)}
    />
  );
}

/* ---------------------------------------------------------------- network graph */

export interface GraphNode {
  id: string;
  label: string;
  group: "worker" | "cheat" | "fail" | "requester";
  degree: number;
  trust: number;
}
export interface GraphEdge {
  source: string;
  target: string;
  weight: number;
  kind: "dispatch" | "quorum";
}

const GROUP_COLOR: Record<GraphNode["group"], string> = {
  worker: PALETTE.emerald,
  requester: PALETTE.blue,
  cheat: PALETTE.red,
  fail: PALETTE.amber,
};

/** Circular node-communication graph (Plotly scatter). */
export function NetworkGraph({
  nodes,
  edges,
  height = 460,
}: {
  nodes: GraphNode[];
  edges: GraphEdge[];
  height?: number;
}) {
  // Stable circular layout: requesters first, then workers, evenly spaced.
  const ordered = [...nodes].sort((a, b) => {
    if (a.group === "requester" && b.group !== "requester") return -1;
    if (b.group === "requester" && a.group !== "requester") return 1;
    return b.degree - a.degree;
  });
  const N = ordered.length || 1;
  const pos = new Map<string, { x: number; y: number }>();
  ordered.forEach((n, i) => {
    const t = (i / N) * Math.PI * 2 - Math.PI / 2;
    pos.set(n.id, { x: Math.cos(t), y: Math.sin(t) });
  });

  const maxW = Math.max(1, ...edges.map((e) => e.weight));
  const edgeTraces: Data[] = edges.map((e) => {
    const a = pos.get(e.source);
    const b = pos.get(e.target);
    const color = e.kind === "quorum" ? "rgba(52,211,153,0.5)" : "rgba(96,165,250,0.35)";
    return {
      type: "scatter",
      mode: "lines",
      x: [a?.x ?? 0, b?.x ?? 0],
      y: [a?.y ?? 0, b?.y ?? 0],
      line: { color, width: 0.6 + (e.weight / maxW) * 4 },
      hoverinfo: "skip",
      showlegend: false,
    } as unknown as Data;
  });

  const groups: GraphNode["group"][] = ["requester", "worker", "fail", "cheat"];
  const nodeTraces: Data[] = groups
    .map((g) => {
      const ns = ordered.filter((n) => n.group === g);
      if (!ns.length) return null;
      const maxDeg = Math.max(1, ...ordered.map((n) => n.degree));
      return {
        type: "scatter",
        mode: "markers+text",
        name: g,
        x: ns.map((n) => pos.get(n.id)!.x),
        y: ns.map((n) => pos.get(n.id)!.y),
        text: ns.map((n) => n.label),
        textposition: "top center",
        textfont: { color: MUTED, size: 9 },
        marker: {
          size: ns.map((n) => 10 + (n.degree / maxDeg) * 26),
          color: GROUP_COLOR[g],
          line: { color: "#0c0d10", width: 1.5 },
          opacity: 0.95,
        },
        customdata: ns.map((n) => [n.degree, n.trust.toFixed(2)]),
        hovertemplate: "<b>%{text}</b><br>links: %{customdata[0]}<br>trust: %{customdata[1]}<extra></extra>",
      } as unknown as Data;
    })
    .filter(Boolean) as Data[];

  const axis = { visible: false, showgrid: false, zeroline: false, fixedrange: true, range: [-1.35, 1.35] };
  return (
    <PlotBase
      data={[...edgeTraces, ...nodeTraces]}
      height={height}
      layout={{
        margin: { l: 8, r: 8, t: 8, b: 28 },
        xaxis: { ...axis },
        yaxis: { ...axis, scaleanchor: "x", scaleratio: 1 },
        showlegend: true,
        legend: { font: { color: MUTED, size: 10 }, orientation: "h", y: -0.02, x: 0.5, xanchor: "center" },
      }}
    />
  );
}

/* ---------------------------------------------------------------- radar */

export function Radar({
  categories,
  series,
  height = 320,
}: {
  categories: string[];
  series: { name: string; values: number[]; color?: string }[];
  height?: number;
}) {
  const data: Data[] = series.map((s, i) => ({
    type: "scatterpolar",
    r: [...s.values, s.values[0]],
    theta: [...categories, categories[0]],
    fill: "toself",
    name: s.name,
    line: { color: s.color ?? Object.values(PALETTE)[i % 8] },
    fillcolor: (s.color ?? Object.values(PALETTE)[i % 8]) + "33",
  })) as unknown as Data[];
  return (
    <PlotBase
      data={data}
      height={height}
      layout={{
        polar: {
          bgcolor: "rgba(0,0,0,0)",
          radialaxis: { range: [0, 1], gridcolor: GRID, tickfont: { color: MUTED, size: 9 }, angle: 90 },
          angularaxis: { gridcolor: GRID, tickfont: { color: TEXT, size: 10 } },
        },
        margin: { l: 40, r: 40, t: 24, b: 24 },
      }}
    />
  );
}

/* ---------------------------------------------------------------- waterfall */

export function Waterfall({
  labels,
  values,
  measures,
  height = 300,
}: {
  labels: string[];
  values: number[];
  measures: ("relative" | "total")[];
  height?: number;
}) {
  return (
    <PlotBase
      height={height}
      data={[
        {
          type: "waterfall",
          orientation: "v",
          x: labels,
          y: values,
          measure: measures,
          connector: { line: { color: GRID } },
          increasing: { marker: { color: PALETTE.emerald } },
          decreasing: { marker: { color: PALETTE.red } },
          totals: { marker: { color: PALETTE.blue } },
          textposition: "outside",
          text: values.map((v) => (v >= 0 ? `+${v}` : `${v}`)),
          textfont: { color: MUTED, size: 9 },
        } as unknown as Data,
      ]}
      layout={{ margin: { l: 48, r: 16, t: 24, b: 60 }, xaxis: { tickangle: -20 } }}
    />
  );
}

/* ---------------------------------------------------------------- box / hist */

export function Box({
  groups,
  height = 300,
  yTitle,
}: {
  groups: { name: string; y: number[]; color?: string }[];
  height?: number;
  yTitle?: string;
}) {
  const data: Data[] = groups.map((g, i) => ({
    type: "box",
    name: g.name,
    y: g.y,
    boxpoints: "outliers",
    marker: { color: g.color ?? Object.values(PALETTE)[i % 8], size: 4 },
    line: { width: 1.5 },
    fillcolor: (g.color ?? Object.values(PALETTE)[i % 8]) + "22",
  })) as unknown as Data[];
  return (
    <PlotBase
      data={data}
      height={height}
      layout={{ yaxis: { title: { text: yTitle ?? "", font: { color: MUTED } } }, showlegend: false }}
    />
  );
}

export function Histogram({
  values,
  color = PALETTE.violet,
  height = 280,
  xTitle,
}: {
  values: number[];
  color?: string;
  height?: number;
  xTitle?: string;
}) {
  return (
    <PlotBase
      height={height}
      data={[{ type: "histogram", x: values, marker: { color, line: { color: "#0c0d10", width: 1 } }, opacity: 0.85 } as unknown as Data]}
      layout={{ bargap: 0.06, xaxis: { title: { text: xTitle ?? "", font: { color: MUTED } } } }}
    />
  );
}

/* ---------------------------------------------------------------- bars / lines */

export function HBar({
  y,
  x,
  color = PALETTE.amber,
  height = 320,
  xTitle,
}: {
  y: string[];
  x: number[];
  color?: string;
  height?: number;
  xTitle?: string;
}) {
  return (
    <PlotBase
      height={height}
      data={[
        {
          type: "bar",
          orientation: "h",
          x,
          y,
          marker: { color, line: { color: "#0c0d10", width: 1 } },
          hovertemplate: "%{y}: %{x}<extra></extra>",
        } as unknown as Data,
      ]}
      layout={{ margin: { l: 130, r: 16, t: 12, b: 36 }, xaxis: { title: { text: xTitle ?? "", font: { color: MUTED } } } }}
    />
  );
}

export function Lines({
  x,
  series,
  height = 280,
  yTitle,
  mode = "lines+markers",
}: {
  x: (number | string)[];
  series: { name: string; y: number[]; color?: string }[];
  height?: number;
  yTitle?: string;
  mode?: "lines" | "lines+markers";
}) {
  const data: Data[] = series.map((s, i) => ({
    type: "scatter",
    mode,
    name: s.name,
    x,
    y: s.y,
    line: { color: s.color ?? Object.values(PALETTE)[i % 8], width: 2, shape: "spline" },
    marker: { size: 6 },
  })) as unknown as Data[];
  return (
    <PlotBase
      data={data}
      height={height}
      layout={{ yaxis: { title: { text: yTitle ?? "", font: { color: MUTED } } } }}
    />
  );
}

export function Heatmap({
  z,
  x,
  y,
  height = 320,
  colorscale = "Viridis",
}: {
  z: number[][];
  x: string[];
  y: string[];
  height?: number;
  colorscale?: string;
}) {
  return (
    <PlotBase
      height={height}
      data={[
        {
          type: "heatmap",
          z,
          x,
          y,
          colorscale: colorscale as unknown as undefined,
          showscale: true,
          colorbar: { tickfont: { color: MUTED, size: 9 }, thickness: 10, outlinewidth: 0 },
          hovertemplate: "%{y} · %{x}: %{z}<extra></extra>",
        } as unknown as Data,
      ]}
      layout={{ margin: { l: 96, r: 16, t: 12, b: 70 }, xaxis: { tickangle: -30 } }}
    />
  );
}
