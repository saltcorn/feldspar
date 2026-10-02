// The three plots a posterior's chosen element is read by (Stan TODO §18;
// analytics TODO A3.6): its **trace per chain** (did the chains mix?), its
// **histogram** (what is its posterior?), and for a one-axis labelled variable
// the **forest plot** — an interval per group, which is how a hierarchical
// model is read.
//
// They were hand-drawn SVG in the admin UI. Here each is a **plot spec** with
// the data `renderPlot` would answer for it, drawn by the explorer's ECharts
// compiler (`PlotView`), so they look like every other plot and a report (A4)
// can copy one. The data is made in the browser rather than by the server
// because it is one element of one variable, chosen by a click, out of draws
// the fit keeps whole: `getModelDraws` already answers exactly that element.
// The spec's data reference names the fit's draws.

import type { DataTable, Domain, LayerData, PlotData, PlotSpec } from "../plot/spec";
import { histogram, type ChainTrace, type ForestRow } from "./models";

/** A drawn plot made in the browser: the spec and its data. */
export type BrowserPlot = { spec: PlotSpec; data: PlotData };

/** The data reference a posterior plot carries: the fit's draws. */
function drawsOf(instance: string): PlotSpec["data"] {
  return { kind: "fit_output", instance, name: "draws" };
}

/** A continuous domain over some numbers, or none when there are none. */
function continuous(values: number[]): Domain | undefined {
  const finite = values.filter(Number.isFinite);
  if (finite.length === 0) return undefined;
  return { kind: "continuous", min: Math.min(...finite), max: Math.max(...finite) };
}

/** One layer's data. */
function layer(mark: LayerData["mark"], stat: string, table: DataTable): LayerData {
  return { ...table, mark, stat, sampled: false, total: table.rows.length, truncated: false };
}

/** What every browser-made plot has empty. */
function plotData(layers: LayerData[], domains: Record<string, Domain | undefined>): PlotData {
  const present: Record<string, Domain> = {};
  for (const [k, d] of Object.entries(domains)) if (d) present[k] = d;
  return { layers, domains: present, facets: {}, bins: {}, warnings: [] };
}

/**
 * Each chain's draws of one element against the iteration, one line per chain
 * (`color` is the chain, a category). A NaN draw — `null` on the wire — is a
 * gap in the line rather than a zero.
 */
export function tracePlot(instance: string, label: string, traces: ChainTrace[]): BrowserPlot {
  const rows: unknown[][] = [];
  const values: number[] = [];
  let longest = 0;
  for (const trace of traces) {
    trace.values.forEach((v, i) => {
      rows.push([i + 1, Number.isFinite(v) ? v : null, trace.chain]);
      if (Number.isFinite(v)) values.push(v);
    });
    longest = Math.max(longest, trace.values.length);
  }
  const spec: PlotSpec = {
    data: drawsOf(instance),
    layers: [
      {
        mark: "line",
        encoding: { x: { field: "iteration" }, y: { field: label }, color: { field: "chain" } },
      },
    ],
  };
  return {
    spec,
    data: plotData([layer("line", "identity", { columns: ["x", "y", "color"], rows })], {
      x: longest > 0 ? { kind: "continuous", min: 1, max: longest } : undefined,
      y: continuous(values),
      color: { kind: "discrete", values: traces.map((t) => t.chain) },
    }),
  };
}

/**
 * The pooled draws of one element as a histogram, binned by `histogram`'s
 * Freedman–Diaconis rule: bars from each bin's lower edge (`x`) to its upper
 * (`x_end`), their height the count.
 */
export function histogramPlot(instance: string, label: string, values: number[]): BrowserPlot {
  const bins = histogram(values);
  const spec: PlotSpec = {
    data: drawsOf(instance),
    layers: [{ mark: "bar", encoding: { x: { field: label, bin: {} } }, stat: { kind: "count" } }],
    // The draws' own range: a bar chart's axis starts at zero, and a
    // posterior about 3.2 would be a sliver at the edge of one from 0.
    scales: { x: { zero: false } },
  };
  return {
    spec,
    data: plotData(
      [
        layer("bar", "count", {
          columns: ["x", "x_end", "y"],
          rows: bins.map((b) => [b.x0, b.x1, b.count]),
        }),
      ],
      {
        x: bins.length > 0 ? { kind: "continuous", min: bins[0].x0, max: bins[bins.length - 1].x1 } : undefined,
        y: { kind: "continuous", min: 0, max: Math.max(0, ...bins.map((b) => b.count)) },
      },
    ),
  };
}

/**
 * The forest plot: each element's mean and its 90 % interval (`q5`–`q95`), one
 * row per element in the order `rows` is sorted in, the elements down the side
 * (flipped coordinates: `x` is the element, drawn vertically). The ECharts
 * category axis draws its first category at the bottom, so the rows are given
 * in reverse to read top to bottom.
 */
export function forestPlot(instance: string, variable: string, rows: ForestRow[]): BrowserPlot {
  const ordered = [...rows].reverse();
  const spec: PlotSpec = {
    data: drawsOf(instance),
    coord: "flipped",
    // The rows in the order asked for, not sorted by label.
    scales: { x: { domain: ordered.map((r) => r.label) } },
    layers: [
      {
        mark: "errorbar",
        encoding: { x: { field: variable }, y: { field: "mean" } },
      },
    ],
  };
  return {
    spec,
    data: plotData(
      [
        layer("errorbar", "identity", {
          columns: ["x", "y", "y_lower", "y_upper"],
          rows: ordered.map((r) => [r.label, r.centre, r.low, r.high]),
        }),
      ],
      {
        x: { kind: "discrete", values: ordered.map((r) => r.label) },
        y: continuous(rows.flatMap((r) => [r.low, r.high])),
      },
    ),
  };
}

/** The height a forest plot needs, in pixels: room for the axis, and for
 * each row's label. */
export function forestHeight(rows: number): number {
  return Math.max(200, Math.min(2000, 80 + rows * 24));
}
