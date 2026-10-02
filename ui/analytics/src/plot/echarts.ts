// From a plot spec and the data the server drew for it to an ECharts option
// (analytics TODO A2.7; the goals document's "Rendering").
//
// The server has already done the statistics: every layer arrives as a small
// table whose columns are named by the channel they are drawn on (`x`, `x_end`
// for a bin's upper edge, `y_lower`, `y_q1`, `color`, `wrap`…), so this file
// only places marks. It knows the spec for what the server's columns cannot
// say — which channel a stat computed, whether an axis is flipped, the scales,
// the order of a fold's columns — and nothing about SQL.
//
// The compilation, in brief:
//
// - **Facets to grids.** One grid (with its own X and Y axis) per small
//   multiple, laid out here in percentages: a row × column table, or wrapped.
//   Free facet scales give each column of plots its own X range and each row
//   its own Y range.
// - **Layers to series.** One series per layer per small multiple per colour
//   group. Bars, lines, points and box plots are ECharts' own series; what
//   ECharts lacks — histogram bars from bin edges, confidence bands, error
//   bars, mosaic tiles — is a `custom` series drawn from the rows.
// - **Colour** to series (a discrete colour: one series per value, in the
//   palette slot of the value's place in the domain, with a legend) or to a
//   `visualMap` (a number: a continuous scale beside the plot).
// - **Coordinates.** Flipped swaps which channel is drawn across; polar puts X
//   around and Y outwards; parallel draws ECharts' parallel coordinates.
//
// Nothing here touches the DOM, so a test can compile a spec and read the
// option.

import { chartPalette, slotColor, type ChartPalette } from "./palette";
import {
  foldNameColumns,
  statOf,
  type Channel,
  type DataTable,
  type Domain,
  type Layer,
  type LayerData,
  type PlotData,
  type PlotSpec,
  type Scale,
} from "./spec";

/** An ECharts option: plain JSON plus the odd callback. */
export type Option = Record<string, unknown>;

/** How a plot is compiled. */
export type CompileOptions = {
  /** The colour scheme. */
  theme: "light" | "dark";
  /** What a missing value is called on an axis or in a legend. */
  missing?: string;
  /** What a count is called on its axis. */
  countLabel?: string;
  /** Draw for print: no animation, no tooltips. */
  still?: boolean;
  /** The columns whose values are categories even when they are numbers: a
   * foreign key's ids, a text column. The server's domains cannot tell. */
  categorical?: string[];
};

/** A small multiple: which facet values it shows, and where it is drawn. */
export type Panel = {
  index: number;
  values: Partial<Record<"row" | "column" | "wrap", unknown>>;
  /** Its own title (a wrapped plot's), or `""`. */
  title: string;
  /** The column's value, above the top row's plots. */
  columnTitle?: string;
  /** The row's value, beside the last column's plots. */
  rowTitle?: string;
  /** Position in percent of the chart. */
  left: number;
  top: number;
  width: number;
  height: number;
};

/** How a positional channel is drawn. */
type AxisKind = "category" | "value" | "log" | "time";

type Ctx = {
  spec: PlotSpec;
  data: PlotData;
  palette: ChartPalette;
  missing: string;
  countLabel: string;
  flipped: boolean;
  axes: Record<"x" | "y", AxisInfo>;
  colorValues: unknown[];
  shapeValues: unknown[];
  continuousColor: boolean;
};

type AxisInfo = {
  kind: AxisKind;
  /** The category labels, in order, for a category axis. */
  categories: string[];
  /** A row's value as its category label. */
  category: (layer: LayerData, row: unknown[]) => string;
  name: string;
  scale: Scale;
  /** Whether the axis starts at zero. */
  zero: boolean;
};

const SYMBOLS = ["circle", "rect", "triangle", "diamond", "roundRect", "pin", "arrow"];

// --- small helpers ------------------------------------------------------------------

/** A value's identity, for grouping and lookup. */
export function keyOf(v: unknown): string {
  return JSON.stringify(v ?? null);
}

/** A number as an axis or tooltip shows it. */
export function formatNumber(v: number): string {
  if (!Number.isFinite(v)) return String(v);
  const abs = Math.abs(v);
  if (abs !== 0 && (abs >= 1e7 || abs < 1e-3)) return v.toExponential(2);
  // No separator below 10,000, so that years read as years.
  return new Intl.NumberFormat("en", {
    maximumFractionDigits: abs >= 100 ? 0 : abs >= 1 ? 2 : 3,
    useGrouping: abs >= 10000,
  }).format(v);
}

/** A value as a label. */
export function labelOf(v: unknown, missing = "—"): string {
  if (v === null || v === undefined) return missing;
  if (typeof v === "number") return formatNumber(v);
  return String(v);
}

function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function idx(table: DataTable, column: string): number {
  return table.columns.indexOf(column);
}

const ISO_DATE = /^\d{4}-\d{2}-\d{2}([T ][\d:.]+(Z|[+-]\d{2}:?\d{2})?)?$/;

/** The positional channels a layer's stat reads as values rather than
 * grouping by. */
function inputs(layer: Layer): Channel[] {
  const stat = statOf(layer);
  switch (stat.kind) {
    case "aggregate":
      return [stat.channel ?? "y"];
    case "quantiles":
    case "boxplot":
    case "summary":
      return ["y"];
    case "density":
      return ["x"];
    case "smooth":
      return ["x", "y"];
    default:
      return [];
  }
}

/** Whether `channel` groups a layer's rows: encoded, not read as a value, and
 * the stat groups at all. */
function groups(layer: Layer, channel: Channel): boolean {
  const stat = statOf(layer);
  if (stat.kind === "identity") return false;
  return Boolean(layer.encoding[channel as "x"]) && !inputs(layer).includes(channel);
}

/** The order of a discrete domain's values: a fold's columns in the fold's
 * order, anything else as the server sorted it. */
function orderedValues(ctx: Pick<Ctx, "spec">, field: string | undefined, values: unknown[]): unknown[] {
  const fold = ctx.spec.fold;
  if (fold && field && foldNameColumns(fold).includes(field)) {
    const order = fold.columns;
    return [...values].sort((a, b) => order.indexOf(String(a)) - order.indexOf(String(b)));
  }
  return values;
}

/** The field shown on a channel by the first layer that encodes it. */
function fieldOn(spec: PlotSpec, channel: Channel): string | undefined {
  if (channel === "row" || channel === "column" || channel === "wrap") return spec.facet?.[channel]?.field;
  for (const layer of spec.layers) {
    const f = layer.encoding[channel as "x"];
    if (f) return f.field;
  }
  return undefined;
}

// --- facets -----------------------------------------------------------------------

/** Where the plots are drawn, in percent: room is left above for the legend
 * and the vertical axis's title, below for the horizontal axis's, and on the
 * right for a colour scale. */
export type Frame = { top: number; right: number };

/** The small multiples, laid out. */
export function panels(spec: PlotSpec, data: PlotData, missing = "—", frame: Frame = { top: 0, right: 0 }): Panel[] {
  const facet = spec.facet ?? {};
  const values = (c: "row" | "column" | "wrap"): unknown[] =>
    facet[c] ? orderedValues({ spec }, facet[c]?.field, data.facets[c] ?? []) : [];
  const title = (c: "row" | "column" | "wrap", v: unknown): string => {
    const field = facet[c];
    const bins = field ? data.bins[field.field] : undefined;
    if (field?.bin && bins && typeof v === "number") {
      return `${formatNumber(v)}–${formatNumber(v + bins.width)}`;
    }
    return labelOf(v, missing);
  };
  const x0 = 1;
  const y0 = 6 + frame.top;
  // The last tick label on the right is centred on the edge.
  const x1 = 97 - frame.right;
  const y1 = 92;
  if (facet.wrap) {
    const ws = values("wrap");
    const n = Math.max(ws.length, 1);
    const cols = Math.max(1, Math.min(n, facet.columns ?? Math.ceil(Math.sqrt(n))));
    const rows = Math.ceil(n / cols);
    const w = (x1 - x0) / cols;
    const h = (y1 - y0) / rows;
    return ws.map((v, i) => ({
      index: i,
      values: { wrap: v },
      title: title("wrap", v),
      left: x0 + (i % cols) * w + 0.5,
      top: y0 + Math.floor(i / cols) * h + 4,
      width: w - 1,
      height: h - 5.5,
    }));
  }
  if (facet.row || facet.column) {
    const rs = facet.row ? values("row") : [undefined];
    const cs = facet.column ? values("column") : [undefined];
    const top = y0 + (facet.column ? 4 : 0);
    const right = x1 - (facet.row ? 8 : 0);
    const w = (right - x0) / Math.max(cs.length, 1);
    const h = (y1 - top) / Math.max(rs.length, 1);
    const out: Panel[] = [];
    rs.forEach((r, i) =>
      cs.forEach((c, j) => {
        const v: Panel["values"] = {};
        if (facet.column) v.column = c;
        if (facet.row) v.row = r;
        out.push({
          index: out.length,
          values: v,
          title: "",
          columnTitle: facet.column && i === 0 ? title("column", c) : undefined,
          rowTitle: facet.row && j === cs.length - 1 ? title("row", r) : undefined,
          left: x0 + j * w + 0.5,
          top: top + i * h + 0.5,
          width: w - 1,
          height: h - 1.5,
        });
      }),
    );
    return out;
  }
  return [{ index: 0, values: {}, title: "", left: x0, top: y0, width: x1 - x0, height: y1 - y0 }];
}

/** The rows of a table that belong in a panel. */
function inPanel(table: DataTable, panel: Panel): unknown[][] {
  const checks = (Object.keys(panel.values) as ("row" | "column" | "wrap")[]).map((c) => ({
    i: idx(table, c),
    key: keyOf(panel.values[c]),
  }));
  if (checks.length === 0) return table.rows;
  return table.rows.filter((row) => checks.every(({ i, key }) => i === -1 || keyOf(row[i]) === key));
}

// --- axes ---------------------------------------------------------------------------

function axisInfo(
  spec: PlotSpec,
  data: PlotData,
  channel: "x" | "y",
  missing: string,
  countLabel: string,
  categorical: string[],
): AxisInfo {
  const domain: Domain | undefined = data.domains[channel];
  const scale = spec.scales?.[channel] ?? {};
  const binned = data.layers.some((l) => idx(l, `${channel}_end`) !== -1);
  const field = fieldOn(spec, channel);
  const forcesCategory = spec.layers.some((l) => {
    const f = l.encoding[channel as "x"];
    if (f && !f.bin && categorical.includes(f.field)) return true;
    return groups(l, channel) && (l.mark === "box" || l.mark === "rect" || (l.mark === "bar" && !f?.bin));
  });
  const discrete = domain?.kind === "discrete";
  const dates =
    discrete && (domain?.values ?? []).length > 0 && (domain?.values ?? []).every((v) => typeof v === "string" && ISO_DATE.test(v));
  let kind: AxisKind;
  if (forcesCategory || (discrete && !dates)) kind = "category";
  else if (dates) kind = "time";
  else kind = scale.kind === "log" ? "log" : "value";

  // Category labels: a binned channel's bins, else the domain's values.
  let categories: string[] = [];
  let category: AxisInfo["category"] = (_l, _r) => "";
  if (kind === "category") {
    if (binned) {
      const edges = new Map<number, number>();
      for (const l of data.layers) {
        const [i, j] = [idx(l, channel), idx(l, `${channel}_end`)];
        if (i === -1 || j === -1) continue;
        for (const r of l.rows) {
          const lo = num(r[i]);
          const hi = num(r[j]);
          if (lo !== null && hi !== null) edges.set(lo, hi);
        }
      }
      const sorted = [...edges.entries()].sort((a, b) => a[0] - b[0]);
      categories = sorted.map(([lo, hi]) => `${formatNumber(lo)}–${formatNumber(hi)}`);
      category = (l, r) => {
        const lo = num(r[idx(l, channel)]);
        const hi = num(r[idx(l, `${channel}_end`)]);
        return lo === null || hi === null ? missing : `${formatNumber(lo)}–${formatNumber(hi)}`;
      };
    } else {
      // Every value the layers have (the domain lists at most a hundred), a
      // missing one last.
      const seen = new Map<string, unknown>();
      for (const l of data.layers) {
        for (const t of [l, l.outliers]) {
          const i = t ? idx(t, channel) : -1;
          if (t && i !== -1) for (const r of t.rows) seen.set(keyOf(r[i]), r[i]);
        }
      }
      const values = orderedValues({ spec }, field, [...seen.values()].sort(compareValues));
      categories = values.map((v) => labelOf(v, missing));
      category = (l, r) => labelOf(r[idx(l, channel)], missing);
    }
  }
  const computed = spec.layers.some((l, i) => {
    const d = data.layers[i];
    return d && idx(d, channel) !== -1 && !l.encoding[channel as "x"];
  });
  const zeroMarks = spec.layers.some((l) => l.mark === "bar" || l.mark === "area" || l.mark === "mosaic");
  return {
    kind,
    categories,
    category,
    name: computed && !field ? countName(spec, channel, countLabel) : axisTitle(spec, field),
    scale,
    zero: scale.zero ?? (zeroMarks && kind !== "log"),
  };
}

/** Numbers by value, missing last, the rest as text. */
function compareValues(a: unknown, b: unknown): number {
  if (a === b) return 0;
  if (a === null || a === undefined) return 1;
  if (b === null || b === undefined) return -1;
  if (typeof a === "number" && typeof b === "number") return a - b;
  return String(a).localeCompare(String(b));
}

/** An axis's title: its column, except the columns a fold makes, which the
 * legend or the small multiples' titles name better. */
function axisTitle(spec: PlotSpec, field: string | undefined): string {
  const fold = spec.fold;
  if (!field) return "";
  if (fold) {
    if (foldNameColumns(fold).includes(field)) return "";
    const value = (fold.value ?? "value").trim();
    if (fold.pairs && (field === `${value}_x` || field === `${value}_y`)) return "";
    if (!fold.pairs && field === value) return fold.columns.join(", ");
  }
  return field;
}

/** Round bounds a fixed scale shares across small multiples. */
export function niceRange([lo, hi]: [number, number], zero: boolean, log: boolean): [number, number] {
  if (log) {
    return [lo > 0 ? 10 ** Math.floor(Math.log10(lo)) : lo, hi > 0 ? 10 ** Math.ceil(Math.log10(hi)) : hi];
  }
  if (zero) {
    lo = Math.min(lo, 0);
    hi = Math.max(hi, 0);
  }
  const span = hi - lo || Math.abs(hi) || 1;
  const raw = span / 5;
  const power = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * power).find((s) => s >= raw) ?? raw;
  return [Math.floor(lo / step) * step, Math.ceil(hi / step) * step];
}

/** What a channel a stat drew on is called: the count, or the density. */
function countName(spec: PlotSpec, channel: Channel, countLabel: string): string {
  for (const l of spec.layers) {
    if (l.encoding[channel as "x"]) continue;
    const stat = statOf(l);
    if (stat.kind === "density") return "density";
    if (stat.kind === "count") return countLabel;
  }
  return "";
}

/** The extent of the values on `channel` (and its `_end`, `_lower`… columns)
 * in some rows of every layer. */
function extent(data: PlotData, channel: string, pick: (l: LayerData) => unknown[][]): [number, number] | null {
  let lo = Infinity;
  let hi = -Infinity;
  for (const l of data.layers) {
    l.columns.forEach((c, i) => {
      if (c.split("_")[0] !== channel) return;
      for (const r of pick(l)) {
        const v = num(r[i]);
        if (v === null) continue;
        lo = Math.min(lo, v);
        hi = Math.max(hi, v);
      }
    });
  }
  return lo <= hi ? [lo, hi] : null;
}

function axisOption(ctx: Ctx, info: AxisInfo, gridIndex: number, range: [number, number] | null): Option {
  const p = ctx.palette;
  const base: Option = {
    gridIndex,
    axisLine: { lineStyle: { color: p.axis } },
    axisTick: { lineStyle: { color: p.axis } },
    axisLabel: { color: p.muted, hideOverlap: true },
    splitLine: { lineStyle: { color: p.grid } },
    inverse: Boolean(info.scale.reverse),
  };
  if (info.kind === "category") {
    return { ...base, type: "category", data: info.categories, splitLine: { show: false } };
  }
  if (info.kind === "time") return { ...base, type: "time" };
  const out: Option = {
    ...base,
    type: info.kind,
    scale: !info.zero,
    axisLabel: { ...(base.axisLabel as Option), formatter: (v: number) => formatNumber(v) },
  };
  const fixed = info.scale.domain;
  if (fixed && fixed.length >= 2 && typeof fixed[0] === "number" && typeof fixed[fixed.length - 1] === "number") {
    out.min = fixed[0];
    out.max = fixed[fixed.length - 1];
  } else if (range) {
    [out.min, out.max] = niceRange(range, info.zero, info.kind === "log");
  }
  return out;
}

// --- colour ------------------------------------------------------------------------

/** Whether the colour channel is a scale of numbers rather than groups. */
function continuousColor(spec: PlotSpec, data: PlotData, categorical: string[]): boolean {
  const domain = data.domains.color;
  if (!domain) return false;
  if (spec.layers.some((l) => l.encoding.color && categorical.includes(l.encoding.color.field))) return false;
  // A stat that computes Color (a heatmap's count, a correlation) is a
  // magnitude; a column on Color that groups the rows is a set of groups.
  return spec.layers.some((l, i) => {
    const d = data.layers[i];
    if (!d || idx(d, "color") === -1) return false;
    if (!l.encoding.color) return true;
    return statOf(l).kind === "identity" && domain.kind === "continuous" && idx(d, "color_end") === -1;
  });
}

function colorLabel(ctx: Ctx, l: LayerData, row: unknown[]): string {
  const i = idx(l, "color");
  if (i === -1) return "";
  const j = idx(l, "color_end");
  const v = row[i];
  if (j !== -1 && typeof v === "number" && typeof row[j] === "number") {
    return `${formatNumber(v)}–${formatNumber(row[j] as number)}`;
  }
  return labelOf(v, ctx.missing);
}

function colorOf(ctx: Ctx, l: LayerData, row: unknown[]): string {
  const i = idx(l, "color");
  if (i === -1 || ctx.continuousColor) return slotColor(ctx.palette, 0);
  const k = keyOf(row[i]);
  return slotColor(
    ctx.palette,
    ctx.colorValues.findIndex((v) => keyOf(v) === k),
  );
}

/** The rows of a layer split by its colour and shape groups, in the domain's
 * order. */
function splitGroups(ctx: Ctx, l: LayerData, rows: unknown[][]): { name: string; color: string; symbol: string; rows: unknown[][] }[] {
  const ci = ctx.continuousColor ? -1 : idx(l, "color");
  const si = idx(l, "shape");
  const out = new Map<string, { name: string; color: string; symbol: string; rows: unknown[][]; order: number }>();
  for (const r of rows) {
    const key = `${ci === -1 ? "" : keyOf(r[ci])}|${si === -1 ? "" : keyOf(r[si])}`;
    let g = out.get(key);
    if (!g) {
      const names: string[] = [];
      if (ci !== -1) names.push(colorLabel(ctx, l, r));
      if (si !== -1) names.push(labelOf(r[si], ctx.missing));
      const colorAt = ci === -1 ? 0 : ctx.colorValues.findIndex((v) => keyOf(v) === keyOf(r[ci]));
      const shapeAt = si === -1 ? 0 : ctx.shapeValues.findIndex((v) => keyOf(v) === keyOf(r[si]));
      g = {
        name: names.join(" · "),
        color: ci === -1 ? slotColor(ctx.palette, 0) : colorOf(ctx, l, r),
        symbol: SYMBOLS[Math.max(shapeAt, 0) % SYMBOLS.length],
        rows: [],
        order: colorAt * 1000 + shapeAt,
      };
      out.set(key, g);
    }
    g.rows.push(r);
  }
  return [...out.values()].sort((a, b) => a.order - b.order);
}

// --- positions -----------------------------------------------------------------------

/** A row's position on an axis: its category's label, the middle of its bin,
 * or its value. `edge` reads another column of the channel (`_lower`, `_q1`). */
function position(ctx: Ctx, channel: "x" | "y", l: LayerData, row: unknown[], edge = ""): unknown {
  const info = ctx.axes[channel];
  if (info.kind === "category" && edge === "") return info.category(l, row);
  const i = idx(l, channel + edge);
  if (i === -1) return null;
  const end = edge === "" ? idx(l, `${channel}_end`) : -1;
  const v = row[i];
  if (end !== -1 && typeof v === "number" && typeof row[end] === "number") {
    return (v + (row[end] as number)) / 2;
  }
  return v;
}

/** `[across, up]` for a point at channel values `x` and `y`. */
function at(ctx: Ctx, x: unknown, y: unknown): [unknown, unknown] {
  return ctx.flipped ? [y, x] : [x, y];
}

/** The channel drawn across. */
function across(ctx: Ctx): "x" | "y" {
  return ctx.flipped ? "y" : "x";
}

// --- series ------------------------------------------------------------------------

type Built = { series: Option[]; legend: string[] };

function seriesForLayer(ctx: Ctx, layer: Layer, l: LayerData, panel: Panel, layerIndex: number): Built {
  const rows = inPanel(l, panel);
  const where = { xAxisIndex: panel.index, yAxisIndex: panel.index };
  const out: Built = { series: [], legend: [] };
  const groupsOf = splitGroups(ctx, l, rows);
  const named = (name: string) => {
    if (name && !out.legend.includes(name)) out.legend.push(name);
    return name || `layer ${layerIndex + 1}`;
  };
  const stat = statOf(layer);
  const hasBand = idx(l, "y_lower") !== -1 && idx(l, "y_upper") !== -1;

  if (l.mark === "box" || l.mark === "rect" || l.mark === "mosaic") {
    const built =
      l.mark === "box"
        ? boxSeries(ctx, l, panel, rows, named)
        : l.mark === "rect"
          ? heatmapSeries(ctx, l, panel, rows)
          : mosaicSeries(ctx, l, panel, rows, named);
    return { series: built.series, legend: [...out.legend, ...built.legend.filter((n) => !out.legend.includes(n))] };
  }

  // Histogram bars from bin edges on a value axis — along X, or along Y when
  // a number on Y alone is binned and X is its count.
  const binnedOn = (["x", "y"] as const).find((c) => idx(l, `${c}_end`) !== -1);
  if (l.mark === "bar" && binnedOn && ctx.axes[binnedOn].kind !== "category") {
    out.series.push(...histogramSeries(ctx, l, panel, groupsOf, named, binnedOn));
    return out;
  }

  const stacked = l.mark === "bar" && (stat.kind === "count" || (stat.kind === "aggregate" && (stat.function === "sum" || stat.function === "count")));
  for (const g of groupsOf) {
    const name = named(g.name);
    const point = (r: unknown[]) => {
      const p: unknown[] = at(ctx, position(ctx, "x", l, r), position(ctx, "y", l, r));
      return p;
    };
    let points = g.rows.map((r) => ({ r, p: point(r) }));
    if (l.mark === "line" || l.mark === "area" || l.mark === "band") {
      // Joined in order of what is drawn across (category order on a category axis).
      const axis = ctx.axes[across(ctx)];
      const order = (v: unknown) =>
        axis.kind === "category" ? axis.categories.indexOf(String(v)) : typeof v === "number" ? v : String(v);
      points = [...points].sort((a, b) => {
        const [p, q] = [order(a.p[0]), order(b.p[0])];
        return p < q ? -1 : p > q ? 1 : 0;
      });
    }
    const common: Option = { name, ...where, itemStyle: { color: g.color } };
    if (l.mark === "band") {
      out.series.push(bandSeries(ctx, l, points.map((x) => x.r), g.color, where, name));
      continue;
    }
    if (l.mark === "errorbar") {
      out.series.push(errorbarSeries(ctx, l, g.rows, g.color, where, name));
      out.series.push({
        ...common,
        type: "scatter",
        symbolSize: 6,
        data: points.map(({ r, p }) => withExtras(l, r, p)),
      });
      continue;
    }
    if (l.mark === "line" || l.mark === "area") {
      if (hasBand && stat.kind !== "summary") out.series.push(bandSeries(ctx, l, points.map((x) => x.r), g.color, where, name));
      out.series.push({
        ...common,
        type: "line",
        data: points.map(({ p }) => p),
        showSymbol: rows.length <= 40 || stat.kind === "summary",
        symbolSize: 6,
        lineStyle: { width: 2, color: g.color },
        areaStyle: l.mark === "area" ? { opacity: 0.25, color: g.color } : undefined,
        connectNulls: false,
      });
      if (stat.kind === "summary") out.series.push(errorbarSeries(ctx, l, g.rows, g.color, where, name));
      continue;
    }
    if (l.mark === "bar") {
      out.series.push({
        ...common,
        type: "bar",
        data: points.map(({ p }) => p),
        stack: stacked ? `stack-${layerIndex}-${panel.index}` : undefined,
        barMaxWidth: 48,
        itemStyle: { color: g.color, borderRadius: ctx.flipped ? [0, 4, 4, 0] : [4, 4, 0, 0] },
      });
      if (stat.kind === "summary") out.series.push(errorbarSeries(ctx, l, g.rows, g.color, where, name));
      continue;
    }
    if (l.mark === "text") {
      const li = idx(l, "label");
      out.series.push({
        ...common,
        type: "scatter",
        symbolSize: 0,
        data: points.map(({ r, p }) => [...p, r[li]]),
        label: {
          show: true,
          color: ctx.palette.text,
          formatter: (d: { value: unknown[] }) =>
            stat.kind === "correlation" && typeof d.value[2] === "number"
              ? d.value[2].toFixed(2)
              : labelOf(d.value[2], ctx.missing),
        },
        tooltip: { show: false },
      });
      continue;
    }
    // Points: a scatter, sized and coloured by the extra columns.
    out.series.push({
      ...common,
      type: "scatter",
      symbol: g.symbol,
      symbolSize: sizeFunction(ctx, l),
      data: points.map(({ r, p }) => withExtras(l, r, p)),
      itemStyle: { color: ctx.continuousColor ? undefined : g.color, opacity: 0.8, borderColor: ctx.palette.surface, borderWidth: 0.5 },
      large: points.length > 2000,
    });
    if (stat.kind === "summary") out.series.push(errorbarSeries(ctx, l, g.rows, g.color, where, name));
  }
  return out;
}

/** A point's data: `[across, up, color, size]` (colour and size as numbers
 * for the visual map and the symbol size). */
function withExtras(l: LayerData, r: unknown[], p: unknown[]): unknown[] {
  const ci = idx(l, "color");
  const si = idx(l, "size");
  return [...p, ci === -1 ? null : r[ci], si === -1 ? null : r[si]];
}

function sizeFunction(ctx: Ctx, l: LayerData): number | ((v: unknown[]) => number) {
  const si = idx(l, "size");
  const d = ctx.data.domains.size;
  if (si === -1 || !d || d.kind !== "continuous") return 8;
  const lo = num(d.min) ?? 0;
  const hi = num(d.max) ?? 1;
  return (v: unknown[]) => {
    const x = num(v[3]);
    if (x === null || hi <= lo) return 8;
    return 4 + 20 * Math.sqrt(Math.max(0, (x - lo) / (hi - lo)));
  };
}

function histogramSeries(
  ctx: Ctx,
  l: LayerData,
  panel: Panel,
  groupsOf: ReturnType<typeof splitGroups>,
  named: (n: string) => string,
  binned: "x" | "y",
): Option[] {
  const counted = binned === "x" ? "y" : "x";
  const [lo, hi, v] = [idx(l, binned), idx(l, `${binned}_end`), idx(l, counted)];
  // Whether the bins run across the screen (bars standing up) or down it.
  const standing = binned === across(ctx);
  // A data item is [bin start, bin end, bar start, bar end, count]; a corner
  // of its bar, as the axes take it.
  const corner = (edge: number, value: number) =>
    binned === "x" ? at(ctx, edge, value) : at(ctx, value, edge);
  // Stacked: each group's bars start where the groups before ended.
  const base = new Map<number, number>();
  return groupsOf.map((g) => {
    const name = named(g.name);
    const data = g.rows
      .map((r) => {
        const [x0, x1, y] = [num(r[lo]), num(r[hi]), num(r[v]) ?? 0];
        if (x0 === null || x1 === null) return null;
        const b = base.get(x0) ?? 0;
        base.set(x0, b + y);
        return [x0, x1, b, b + y, y];
      })
      .filter((d): d is number[] => d !== null);
    return {
      type: "custom",
      name,
      xAxisIndex: panel.index,
      yAxisIndex: panel.index,
      itemStyle: { color: g.color },
      encode: standing ? { x: [0, 1], y: [2, 3], tooltip: [4] } : { x: [2, 3], y: [0, 1], tooltip: [4] },
      data,
      renderItem: (_params: unknown, api: RenderApi) => {
        const [e0, e1, b0, b1] = [api.value(0), api.value(1), api.value(2), api.value(3)];
        const p0 = api.coord(corner(e0, b0));
        const p1 = api.coord(corner(e1, b1));
        const gap = 1;
        const [left, top] = [Math.min(p0[0], p1[0]), Math.min(p0[1], p1[1])];
        const [width, height] = [Math.abs(p1[0] - p0[0]), Math.abs(p1[1] - p0[1])];
        return {
          type: "rect",
          shape: standing
            ? { x: left + gap, y: top, width: width - 2 * gap, height }
            : { x: left, y: top + gap, width, height: height - 2 * gap },
          style: api.style(),
        };
      },
    };
  });
}

/** What a custom series' `renderItem` is handed. */
type RenderApi = {
  value: (dim: number) => number;
  coord: (point: unknown[]) => number[];
  style: (extra?: Option) => Option;
};

function bandSeries(ctx: Ctx, l: LayerData, rows: unknown[][], color: string, where: Option, name: string): Option {
  type BandPoint = { x: unknown; lo: unknown; hi: unknown };
  const pts: BandPoint[] = [];
  for (const r of rows) {
    const [lo, hi] = [r[idx(l, "y_lower")], r[idx(l, "y_upper")]];
    if (lo !== null && hi !== null) pts.push({ x: position(ctx, "x", l, r), lo, hi });
  }
  return {
    type: "custom",
    name,
    ...where,
    silent: true,
    z: 1,
    tooltip: { show: false },
    // Every point of the band is a data item, so that the axes span it; the
    // first draws the whole polygon and the rest nothing.
    data: pts.map((p) => [p.x, p.lo, p.hi]),
    encode: ctx.flipped ? { y: 0, x: [1, 2] } : { x: 0, y: [1, 2] },
    renderItem: (params: { dataIndex: number }, api: RenderApi) => {
      if (params.dataIndex !== 0) return null;
      return {
        type: "polygon",
        shape: {
          points: [
            ...pts.map((p) => api.coord(at(ctx, p.x, p.lo))),
            ...[...pts].reverse().map((p) => api.coord(at(ctx, p.x, p.hi))),
          ],
        },
        style: { fill: color, opacity: 0.18 },
      };
    },
  };
}

function errorbarSeries(ctx: Ctx, l: LayerData, rows: unknown[][], color: string, where: Option, name: string): Option {
  const [li, ui] = [idx(l, "y_lower"), idx(l, "y_upper")];
  const data = rows
    .filter((r) => r[li] !== null && r[ui] !== null)
    .map((r) => [position(ctx, "x", l, r), r[li], r[ui]]);
  return {
    type: "custom",
    name,
    ...where,
    z: 3,
    data,
    encode: ctx.flipped ? { y: 0, x: [1, 2] } : { x: 0, y: [1, 2] },
    renderItem: (_params: unknown, api: RenderApi) => {
      const x = (data as unknown[][])[(_params as { dataIndex: number }).dataIndex][0];
      const p0 = api.coord(at(ctx, x, api.value(1)));
      const p1 = api.coord(at(ctx, x, api.value(2)));
      const cap = 5;
      const capLine = (p: number[]) =>
        ctx.flipped
          ? { type: "line", shape: { x1: p[0], y1: p[1] - cap, x2: p[0], y2: p[1] + cap } }
          : { type: "line", shape: { x1: p[0] - cap, y1: p[1], x2: p[0] + cap, y2: p[1] } };
      const style = { stroke: color, lineWidth: 1.5 };
      return {
        type: "group",
        children: [
          { type: "line", shape: { x1: p0[0], y1: p0[1], x2: p1[0], y2: p1[1] }, style },
          { ...capLine(p0), style },
          { ...capLine(p1), style },
        ],
      };
    },
  };
}

function boxSeries(ctx: Ctx, l: LayerData, panel: Panel, rows: unknown[][], named: (n: string) => string): Built {
  const out: Built = { series: [], legend: [] };
  const where = { xAxisIndex: panel.index, yAxisIndex: panel.index };
  const cats = ctx.axes.x.categories;
  const cols = ["y_lower", "y_q1", "y_median", "y_q3", "y_upper"].map((c) => idx(l, c));
  const ni = idx(l, "n");
  for (const g of splitGroups(ctx, l, rows)) {
    const name = named(g.name);
    const data: unknown[] = cats.map(() => "-");
    for (const r of g.rows) {
      const at = cats.indexOf(ctx.axes.x.category(l, r));
      if (at === -1) continue;
      data[at] = { value: cols.map((i) => r[i]), n: r[ni] };
    }
    out.series.push({
      type: "boxplot",
      name,
      ...where,
      layout: ctx.flipped ? "horizontal" : "vertical",
      data,
      itemStyle: { color: ctx.palette.surface, borderColor: g.color, borderWidth: 1.5 },
      boxWidth: [7, 40],
    });
  }
  // Outliers, as points beside their box.
  const o = l.outliers;
  if (o && o.rows.length > 0) {
    const yi = idx(o, "y");
    const rowsIn = inPanel(o, panel);
    for (const g of splitGroups(ctx, o as LayerData, rowsIn)) {
      out.series.push({
        type: "scatter",
        name: g.name || "outliers",
        ...where,
        symbolSize: 5,
        itemStyle: { color: g.color, opacity: 0.7 },
        data: g.rows.map((r) => at(ctx, ctx.axes.x.category(o as LayerData, r), r[yi])),
      });
    }
  }
  return out;
}

function heatmapSeries(ctx: Ctx, l: LayerData, panel: Panel, rows: unknown[][]): Built {
  // The cell's value: whatever the stat drew on Color (a count, a mean, a
  // correlation).
  const vi = idx(l, "color");
  return {
    legend: [],
    series: [
      {
        type: "heatmap",
        xAxisIndex: panel.index,
        yAxisIndex: panel.index,
        data: rows.map((r) => [...at(ctx, ctx.axes.x.category(l, r), ctx.axes.y.category(l, r)), r[vi]]),
        itemStyle: { borderColor: ctx.palette.surface, borderWidth: 2, borderRadius: 2 },
        emphasis: { itemStyle: { borderColor: ctx.palette.text, borderWidth: 1 } },
      },
    ],
  };
}

/** A mosaic's tiles: each value of X a column as wide as its share of the
 * rows, split by the values of Y in proportion. Drawn on 0–1 axes. */
export function mosaicTiles(
  l: DataTable,
  rows: unknown[][],
  missing = "—",
): { x: string; y: string; yKey: string; x0: number; x1: number; y0: number; y1: number; n: number }[] {
  const [xi, yi, ni] = [idx(l, "x"), idx(l, "y"), idx(l, "size")];
  const columns = new Map<string, { label: string; n: number; parts: { y: unknown; n: number }[] }>();
  let total = 0;
  for (const r of rows) {
    const n = num(r[ni]) ?? 0;
    const key = keyOf(r[xi]);
    let c = columns.get(key);
    if (!c) {
      c = { label: labelOf(r[xi], missing), n: 0, parts: [] };
      columns.set(key, c);
    }
    c.n += n;
    c.parts.push({ y: r[yi], n });
    total += n;
  }
  const tiles = [];
  let x = 0;
  for (const c of columns.values()) {
    const w = total > 0 ? c.n / total : 0;
    let y = 0;
    for (const p of c.parts) {
      const h = c.n > 0 ? p.n / c.n : 0;
      tiles.push({ x: c.label, y: labelOf(p.y, missing), yKey: keyOf(p.y), x0: x, x1: x + w, y0: y, y1: y + h, n: p.n });
      y += h;
    }
    x += w;
  }
  return tiles;
}

function mosaicSeries(ctx: Ctx, l: LayerData, panel: Panel, rows: unknown[][], named: (n: string) => string): Built {
  const out: Built = { series: [], legend: [] };
  const tiles = mosaicTiles(l, rows, ctx.missing);
  const yValues = orderedValues(ctx, fieldOn(ctx.spec, "y"), ctx.data.domains.y?.values ?? []);
  const keys = [...new Set(tiles.map((t) => t.yKey))].sort(
    (a, b) => yValues.findIndex((v) => keyOf(v) === a) - yValues.findIndex((v) => keyOf(v) === b),
  );
  const where = { xAxisIndex: panel.index, yAxisIndex: panel.index };
  const rect = (api: RenderApi) => {
    const p0 = api.coord([api.value(0), api.value(2)]);
    const p1 = api.coord([api.value(1), api.value(3)]);
    return {
      type: "rect",
      shape: { x: p0[0] + 1, y: p1[1] + 1, width: Math.max(0, p1[0] - p0[0] - 2), height: Math.max(0, p0[1] - p1[1] - 2), r: 2 },
      style: api.style(),
    };
  };
  keys.forEach((key) => {
    const mine = tiles.filter((t) => t.yKey === key);
    const index = yValues.findIndex((v) => keyOf(v) === key);
    const name = named(mine[0]?.y ?? "");
    out.series.push({
      type: "custom",
      name,
      ...where,
      itemStyle: { color: slotColor(ctx.palette, index === -1 ? yValues.length : index) },
      encode: { x: [0, 1], y: [2, 3], tooltip: [4] },
      dimensions: ["x0", "x1", "y0", "y1", "n"],
      data: mine.map((t) => ({ value: [t.x0, t.x1, t.y0, t.y1, t.n], name: `${t.x} · ${t.y}` })),
      renderItem: (_p: unknown, api: RenderApi) => rect(api),
    });
  });
  // The values of X, under their columns.
  const columns = new Map<string, number>();
  for (const t of tiles) columns.set(t.x, (t.x0 + t.x1) / 2);
  out.series.push({
    type: "custom",
    ...where,
    silent: true,
    tooltip: { show: false },
    data: [...columns.entries()].map(([label, center]) => ({ value: [center], name: label })),
    renderItem: (params: { dataIndex: number }, api: RenderApi) => {
      const p = api.coord([api.value(0), 0]);
      return {
        type: "text",
        x: p[0],
        y: p[1] + 6,
        style: { text: [...columns.keys()][params.dataIndex], fill: ctx.palette.muted, align: "center", verticalAlign: "top" },
      };
    },
  });
  return out;
}

// --- the option -----------------------------------------------------------------------

/** Compile `spec`, drawn as `data`, to an ECharts option. */
export function toOption(spec: PlotSpec, data: PlotData, options: CompileOptions): Option {
  const palette = chartPalette(options.theme);
  const missing = options.missing ?? "—";
  const countLabel = options.countLabel ?? "count";
  const base: Option = {
    backgroundColor: "transparent",
    color: palette.categorical,
    textStyle: { color: palette.text, fontFamily: 'system-ui, -apple-system, "Segoe UI", sans-serif' },
    animation: !options.still,
  };
  if (spec.coord === "parallel") return { ...base, ...parallelOption(spec, data, palette, missing, options) };

  const colorDomain = data.domains.color;
  const categorical = options.categorical ?? [];
  const ctx: Ctx = {
    spec,
    data,
    palette,
    missing,
    countLabel,
    flipped: spec.coord === "flipped",
    axes: {
      x: axisInfo(spec, data, "x", missing, countLabel, categorical),
      y: axisInfo(spec, data, "y", missing, countLabel, categorical),
    },
    colorValues: orderedValues({ spec }, fieldOn(spec, "color"), colorDomain?.values ?? []),
    shapeValues: data.domains.shape?.values ?? [],
    continuousColor: continuousColor(spec, data, categorical),
  };
  const mosaic = spec.layers.some((l) => l.mark === "mosaic");
  if (mosaic) {
    // Tiles are drawn on shares, 0 to 1 each way.
    ctx.axes.x = { ...ctx.axes.x, kind: "value", categories: [], zero: true };
    ctx.axes.y = { ...ctx.axes.y, kind: "value", categories: [], zero: true };
  }
  // A legend whenever a column (or a mosaic's Y) is told apart by colour.
  const legendShown =
    !ctx.continuousColor && spec.layers.some((l) => l.encoding.color !== undefined || l.mark === "mosaic");
  const frame: Frame = { top: legendShown ? 5 : 0, right: ctx.continuousColor ? 7 : 0 };
  const ps = panels(spec, data, missing, frame);
  const free = spec.facet?.scales === "free";
  const grids: Option[] = [];
  const xAxes: Option[] = [];
  const yAxes: Option[] = [];
  const series: Option[] = [];
  const legend: string[] = [];
  const polar = spec.coord === "polar";
  const polars: Option[] = [];
  const angleAxes: Option[] = [];
  const radiusAxes: Option[] = [];
  const titleStyle = { fontSize: 12, fontWeight: "normal", color: palette.secondary };
  const titles: Option[] = [];

  const hInfo = ctx.flipped ? ctx.axes.y : ctx.axes.x;
  const vInfo = ctx.flipped ? ctx.axes.x : ctx.axes.y;
  const hChannel = ctx.flipped ? "y" : "x";
  const vChannel = ctx.flipped ? "x" : "y";
  // Small multiples on fixed scales share round bounds; one plot, or free
  // scales, are left to ECharts, which rounds each axis by itself — except on
  // a log scale, whose extent ECharts does not find from the data.
  const shared = (channel: "x" | "y"): [number, number] | null =>
    (ps.length > 1 && !free) || ctx.axes[channel].kind === "log" ? extent(data, channel, (l) => l.rows) : null;
  const pairs = spec.fold?.pairs;

  for (const p of ps) {
    if (polar) {
      polars.push({
        center: [`${p.left + p.width / 2}%`, `${p.top + p.height / 2}%`],
        radius: `${Math.min(p.width, p.height) / 2}%`,
      });
      angleAxes.push({ ...axisOption(ctx, ctx.axes.x, 0, shared("x")), polarIndex: p.index, gridIndex: undefined });
      radiusAxes.push({ ...axisOption(ctx, ctx.axes.y, 0, shared("y")), polarIndex: p.index, gridIndex: undefined });
    } else {
      grids.push({ left: `${p.left}%`, top: `${p.top}%`, width: `${p.width}%`, height: `${p.height}%`, containLabel: true });
      const h = axisOption(ctx, hInfo, p.index, mosaic ? [0, 1] : shared(hChannel));
      const v = axisOption(ctx, vInfo, p.index, mosaic ? [0, 1] : shared(vChannel));
      if (mosaic) {
        h.axisLabel = { show: false };
        h.axisTick = { show: false };
        h.splitLine = { show: false };
        v.axisLabel = { color: palette.muted, formatter: (x: number) => `${Math.round(x * 100)}%` };
      }
      xAxes.push(h);
      yAxes.push(v);
    }
    const center = `${p.left + p.width / 2}%`;
    if (p.title) titles.push({ text: p.title, left: center, top: `${p.top - 3.5}%`, textAlign: "center", textStyle: titleStyle });
    if (p.columnTitle) {
      titles.push({ text: p.columnTitle, left: center, top: `${p.top - 3.5}%`, textAlign: "center", textStyle: titleStyle });
    }
    if (p.rowTitle) {
      titles.push({
        text: p.rowTitle,
        left: `${p.left + p.width + 0.8}%`,
        top: `${p.top + p.height / 2}%`,
        textVerticalAlign: "middle",
        textStyle: titleStyle,
      });
    }
    // A scatterplot matrix's diagonal pairs a column with itself: it names
    // the column instead.
    if (pairs && !pairs.diagonal && p.values.row !== undefined && keyOf(p.values.row) === keyOf(p.values.column)) {
      titles.push({
        text: labelOf(p.values.row, missing),
        left: center,
        top: `${p.top + p.height / 2}%`,
        textAlign: "center",
        textVerticalAlign: "middle",
        textStyle: { fontSize: 14, fontWeight: "bold", color: palette.secondary },
      });
    }
    const before = series.length;
    spec.layers.forEach((layer, i) => {
      const l = data.layers[i];
      if (!l) return;
      const built = seriesForLayer(ctx, layer, l, p, i);
      for (const s of built.series) {
        if (polar) {
          delete s.xAxisIndex;
          delete s.yAxisIndex;
          s.coordinateSystem = "polar";
          s.polarIndex = p.index;
        }
        series.push(s);
      }
      for (const n of built.legend) if (!legend.includes(n)) legend.push(n);
    });
    // Reference lines, on the first series of the plot (or an empty one).
    const refs = spec.references ?? [];
    if (refs.length > 0 && !polar) {
      if (series.length === before) {
        series.push({ type: "line", data: [], xAxisIndex: p.index, yAxisIndex: p.index });
      }
      const holder = series[before];
      holder.markLine = {
        silent: true,
        symbol: "none",
        lineStyle: { color: palette.secondary, type: "dashed", width: 1 },
        // Inside the plot, above the line's end: past it, the label is cut
        // off by the edge of the chart.
        label: {
          color: palette.secondary,
          position: "insideEndTop",
          formatter: (d: { name?: string; value?: unknown }) => d.name || labelOf(d.value),
        },
        data: refs.map((r) => {
          const onAcross = (r.channel === "x") !== ctx.flipped;
          return { [onAcross ? "xAxis" : "yAxis"]: r.value, name: r.label ?? "" };
        }),
      };
    }
  }

  // The axes' titles, once for all the plots: the vertical one above them,
  // the horizontal one below.
  if (!polar) {
    const top = Math.min(...ps.map((p) => p.top)) - (ps.some((p) => p.title || p.columnTitle) ? 3.5 : 0);
    if (vInfo.name) titles.push({ text: vInfo.name, left: "1%", top: `${Math.max(top - 4.5, frame.top)}%`, textStyle: titleStyle });
    if (hInfo.name) titles.push({ text: hInfo.name, left: "50%", bottom: 0, textAlign: "center", textStyle: titleStyle });
  }

  const option: Option = {
    ...base,
    title: titles,
    tooltip: options.still
      ? { show: false }
      : {
          trigger: "item",
          confine: true,
          backgroundColor: palette.surface,
          borderColor: palette.grid,
          textStyle: { color: palette.text },
          valueFormatter: (v: unknown) => (typeof v === "number" ? formatNumber(v) : labelOf(v, missing)),
        },
    series,
  };
  if (polar) {
    option.polar = polars;
    option.angleAxis = angleAxes;
    option.radiusAxis = radiusAxes;
  } else {
    option.grid = grids;
    option.xAxis = xAxes;
    option.yAxis = yAxes;
  }
  if (legendShown && legend.length > 0) {
    option.legend = {
      type: "scroll",
      top: 0,
      data: legend,
      textStyle: { color: palette.secondary },
      inactiveColor: palette.grid,
    };
  }
  if (ctx.continuousColor) {
    const scale = spec.scales?.color ?? {};
    const fixed = scale.domain;
    const min = typeof fixed?.[0] === "number" ? fixed[0] : (num(colorDomain?.min) ?? 0);
    const max = typeof fixed?.[fixed.length - 1] === "number" ? (fixed[fixed.length - 1] as number) : (num(colorDomain?.max) ?? 1);
    const colors = scale.scheme === "diverging" ? palette.diverging : palette.sequential;
    const heat = spec.layers.some((l) => l.mark === "rect");
    option.visualMap = {
      type: "continuous",
      min,
      max: max > min ? max : min + 1,
      calculable: true,
      orient: "vertical",
      right: 0,
      top: "middle",
      itemHeight: 120,
      text: [labelOf(max), labelOf(min)],
      textStyle: { color: palette.secondary },
      inRange: { color: scale.reverse ? [...colors].reverse() : colors },
      // A heatmap's value is its third dimension; a point's colour is too.
      dimension: 2,
      seriesIndex: series
        .map((s, i) => ((heat ? s.type === "heatmap" : s.type === "scatter") ? i : -1))
        .filter((i) => i !== -1),
    };
  }
  return option;
}

function parallelOption(spec: PlotSpec, data: PlotData, palette: ChartPalette, missing: string, options: CompileOptions): Option {
  const l = data.layers[0];
  const axes = ((l?.info?.axes as string[]) ?? []).map((name, dim) => ({
    dim,
    name,
    nameTextStyle: { color: palette.secondary },
    axisLine: { lineStyle: { color: palette.axis } },
    axisLabel: { color: palette.muted, formatter: (v: number) => formatNumber(v) },
    // Each axis spans its column's values, not from zero.
    scale: true,
  }));
  const ci = l ? idx(l, "color") : -1;
  const yStart = l ? l.columns.findIndex((c) => c.startsWith("y_")) : 0;
  const values = (r: unknown[]) => r.slice(yStart);
  const colorValues = orderedValues({ spec }, fieldOn(spec, "color"), data.domains.color?.values ?? []);
  const colorField = fieldOn(spec, "color");
  const continuous =
    ci !== -1 &&
    data.domains.color?.kind === "continuous" &&
    idx(l, "color_end") === -1 &&
    !(colorField && (options.categorical ?? []).includes(colorField));
  const lineStyle = { width: 1, opacity: (l?.rows.length ?? 0) > 500 ? 0.25 : 0.6 };
  const series: Option[] = [];
  const legend: string[] = [];
  if (!l) return { series };
  if (ci === -1 || continuous) {
    series.push({
      type: "parallel",
      name: "rows",
      lineStyle: { ...lineStyle, color: palette.categorical[0] },
      data: l.rows.map((r) => (continuous ? [...values(r), r[ci]] : values(r))),
      progressive: 500,
    });
  } else {
    const groups = new Map<string, unknown[][]>();
    for (const r of l.rows) {
      const k = keyOf(r[ci]);
      groups.set(k, [...(groups.get(k) ?? []), r]);
    }
    const keys = [...groups.keys()].sort(
      (a, b) => colorValues.findIndex((v) => keyOf(v) === a) - colorValues.findIndex((v) => keyOf(v) === b),
    );
    for (const k of keys) {
      const rows = groups.get(k) ?? [];
      const name = labelOf(rows[0][ci], missing);
      legend.push(name);
      series.push({
        type: "parallel",
        name,
        lineStyle: { ...lineStyle, color: slotColor(palette, colorValues.findIndex((v) => keyOf(v) === k)) },
        data: rows.map(values),
        progressive: 500,
      });
    }
  }
  const option: Option = {
    parallel: { left: "4%", right: continuous ? "12%" : "8%", top: legend.length ? "14%" : "8%", bottom: "8%" },
    parallelAxis: axes,
    series,
    tooltip: options.still ? { show: false } : { trigger: "item", confine: true },
  };
  if (legend.length > 0) option.legend = { type: "scroll", top: 0, data: legend, textStyle: { color: palette.secondary } };
  if (continuous) {
    option.visualMap = {
      type: "continuous",
      min: num(data.domains.color?.min) ?? 0,
      max: num(data.domains.color?.max) ?? 1,
      dimension: axes.length,
      right: 0,
      top: "middle",
      calculable: true,
      inRange: { color: palette.sequential },
      textStyle: { color: palette.secondary },
    };
  }
  return option;
}

// --- what to tell the reader --------------------------------------------------------

/** The sentences shown under a plot: samples, cut-off groups, and the
 * server's warnings. */
export function plotNotes(
  data: PlotData,
  t: (text: string, args?: Record<string, string | number>) => string,
): string[] {
  const notes: string[] = [];
  for (const l of data.layers) {
    if (l.sampled) {
      notes.push(
        t("Showing a random sample of {shown} of {total} rows.", {
          shown: new Intl.NumberFormat().format(l.rows.length),
          total: new Intl.NumberFormat().format(l.total),
        }),
      );
    }
    if (l.truncated) notes.push(t("There are more groups than can be drawn; only the first are shown."));
  }
  return [...new Set([...notes, ...data.warnings])];
}
