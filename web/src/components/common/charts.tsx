"use client";

import * as React from "react";
import { useId } from "react";
import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  Cell,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";

const axis = {
  stroke: "var(--muted-foreground)",
  fontSize: 11,
  tickLine: false,
  axisLine: false,
};

function TipBox({
  active,
  payload,
  label,
  unit,
}: {
  active?: boolean;
  payload?: { name: string; value: number; color: string }[];
  label?: string | number;
  unit?: string;
}) {
  if (!active || !payload?.length) return null;
  return (
    <div className="bg-popover/95 rounded-lg border px-3 py-2 text-xs shadow-md backdrop-blur">
      {label !== undefined ? (
        <div className="mb-1 font-medium">{label}</div>
      ) : null}
      {payload.map((p) => (
        <div key={p.name} className="flex items-center gap-2">
          <span
            className="inline-block size-2 rounded-full"
            style={{ background: p.color }}
          />
          <span className="text-muted-foreground capitalize">{p.name}</span>
          <span className="ml-auto font-medium tabular-nums">
            {p.value}
            {unit ?? ""}
          </span>
        </div>
      ))}
    </div>
  );
}

export function AreaTrend({
  data,
  series,
  height = 240,
  unit,
}: {
  data: Record<string, number | string>[];
  series: { key: string; color: string; label?: string }[];
  height?: number;
  unit?: string;
}) {
  return (
    <ResponsiveContainer width="100%" height={height}>
      <AreaChart data={data} margin={{ left: -18, right: 8, top: 8, bottom: 0 }}>
        <defs>
          {series.map((s) => (
            <linearGradient key={s.key} id={`g-${s.key}`} x1="0" y1="0" x2="0" y2="1">
              <stop offset="5%" stopColor={s.color} stopOpacity={0.35} />
              <stop offset="95%" stopColor={s.color} stopOpacity={0} />
            </linearGradient>
          ))}
        </defs>
        <XAxis dataKey="label" {...axis} minTickGap={28} />
        <YAxis {...axis} width={42} />
        <Tooltip content={<TipBox unit={unit} />} />
        {series.map((s) => (
          <Area
            key={s.key}
            type="monotone"
            dataKey={s.key}
            name={s.label ?? s.key}
            stroke={s.color}
            strokeWidth={2}
            fill={`url(#g-${s.key})`}
            isAnimationActive={false}
          />
        ))}
      </AreaChart>
    </ResponsiveContainer>
  );
}

export function Spark({
  data,
  color = "var(--primary)",
  height = 48,
}: {
  data: number[];
  color?: string;
  height?: number;
}) {
  const uid = useId();
  // Combine the React 19 useId() with the color so the gradient id is unique
  // per Spark instance even when multiple instances share the same color prop.
  const gradId = `sp-${uid.replace(/:/g, "")}-${color.replace(/[^a-zA-Z0-9_-]/g, "")}`;
  const d = data.map((v, i) => ({ i, v }));
  return (
    <ResponsiveContainer width="100%" height={height}>
      <AreaChart data={d} margin={{ left: 0, right: 0, top: 4, bottom: 0 }}>
        <defs>
          <linearGradient id={gradId} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={color} stopOpacity={0.4} />
            <stop offset="100%" stopColor={color} stopOpacity={0} />
          </linearGradient>
        </defs>
        <Area
          type="monotone"
          dataKey="v"
          stroke={color}
          strokeWidth={1.75}
          fill={`url(#${gradId})`}
          isAnimationActive={false}
        />
      </AreaChart>
    </ResponsiveContainer>
  );
}

export function BarMini({
  data,
  xKey,
  yKey,
  color = "var(--chart-1)",
  height = 220,
}: {
  data: Record<string, number | string>[];
  xKey: string;
  yKey: string;
  color?: string;
  height?: number;
}) {
  return (
    <ResponsiveContainer width="100%" height={height}>
      <BarChart data={data} margin={{ left: -20, right: 8, top: 8, bottom: 0 }}>
        <XAxis dataKey={xKey} {...axis} interval={0} />
        <YAxis {...axis} width={42} />
        <Tooltip cursor={{ fill: "var(--muted)", opacity: 0.4 }} content={<TipBox />} />
        <Bar dataKey={yKey} fill={color} radius={[4, 4, 0, 0]} isAnimationActive={false} />
      </BarChart>
    </ResponsiveContainer>
  );
}

export function Donut({
  data,
  height = 200,
}: {
  data: { name: string; value: number; fill: string }[];
  height?: number;
}) {
  const total = data.reduce((a, b) => a + b.value, 0);
  return (
    <ResponsiveContainer width="100%" height={height}>
      <PieChart>
        <Pie
          data={data}
          dataKey="value"
          nameKey="name"
          innerRadius={55}
          outerRadius={80}
          paddingAngle={2}
          strokeWidth={0}
          isAnimationActive={false}
        >
          {data.map((d) => (
            <Cell key={d.name} fill={d.fill} />
          ))}
        </Pie>
        <Tooltip content={<TipBox />} />
        <text
          x="50%"
          y="50%"
          textAnchor="middle"
          dominantBaseline="middle"
          className="fill-foreground text-xl font-semibold"
        >
          {total}
        </text>
      </PieChart>
    </ResponsiveContainer>
  );
}
