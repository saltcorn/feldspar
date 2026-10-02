// The posterior's plots as plot specs (analytics TODO A3.6): each spec, with
// the data the browser made for it, compiles to the ECharts option the
// explorer's plots compile to — so a trace is one line per chain, a histogram
// is bars over bin edges, and a forest plot is an interval per element.

import { describe, expect, it } from "vitest";

import { toOption } from "../plot/echarts";
import { forestHeight, forestPlot, histogramPlot, tracePlot } from "./posteriorPlots";

const options = { theme: "light" as const };

type Series = { type: string; name?: string; data: unknown[] };

describe("the posterior's plots", () => {
  it("draws a trace as one line per chain, a missing draw as a gap", () => {
    const plot = tracePlot("i1", "alpha[Aitkin]", [
      { chain: 1, values: [0.1, 0.2, 0.3] },
      { chain: 2, values: [0.4, Number.NaN, 0.6] },
    ]);
    expect(plot.spec.data).toEqual({ kind: "fit_output", instance: "i1", name: "draws" });
    expect(plot.data.layers[0].rows).toEqual([
      [1, 0.1, 1],
      [2, 0.2, 1],
      [3, 0.3, 1],
      [1, 0.4, 2],
      [2, null, 2],
      [3, 0.6, 2],
    ]);
    const lines = (toOption(plot.spec, plot.data, options).series as Series[]).filter((s) => s.type === "line");
    expect(lines.map((l) => l.name)).toEqual(["1", "2"]);
    expect(lines[1].data).toEqual([
      [1, 0.4],
      [2, null],
      [3, 0.6],
    ]);
  });

  it("draws the pooled draws as a histogram over bin edges", () => {
    const values = Array.from({ length: 200 }, (_, i) => Math.sin(i));
    const plot = histogramPlot("i1", "beta", values);
    const rows = plot.data.layers[0].rows as number[][];
    expect(plot.data.layers[0].columns).toEqual(["x", "x_end", "y"]);
    expect(rows.reduce((n, r) => n + r[2], 0)).toBe(200);
    expect(rows[0][0]).toBe(Math.min(...values));
    const option = toOption(plot.spec, plot.data, options);
    expect((option.series as Series[]).length).toBeGreaterThan(0);
    // The axis spans the draws, not zero to them.
    const axis = (Array.isArray(option.xAxis) ? option.xAxis[0] : option.xAxis) as { min?: unknown; scale?: boolean };
    expect(axis.min === 0).toBe(false);
  });

  it("draws a forest plot as an interval per element, the first row at the top", () => {
    const rows = [
      { row: 0, label: "Aitkin", low: 0.1, centre: 0.5, high: 0.9 },
      { row: 1, label: "Anoka", low: -0.2, centre: 0.1, high: 0.3 },
    ];
    const plot = forestPlot("i1", "alpha", rows);
    expect(plot.spec.coord).toBe("flipped");
    // A category axis draws its first category at the bottom.
    expect(plot.data.layers[0].rows).toEqual([
      ["Anoka", 0.1, -0.2, 0.3],
      ["Aitkin", 0.5, 0.1, 0.9],
    ]);
    const option = toOption(plot.spec, plot.data, options);
    const series = option.series as Series[];
    expect(series.some((s) => s.type === "custom")).toBe(true);
    // Drawn in the order given, not alphabetically: the category axis (Y, the
    // coordinates being flipped) lists Anoka first, at the bottom.
    const axis = (Array.isArray(option.yAxis) ? option.yAxis[0] : option.yAxis) as { data?: string[] };
    expect(axis.data).toEqual(["Anoka", "Aitkin"]);
    expect(forestHeight(2)).toBe(200);
    expect(forestHeight(40)).toBe(80 + 40 * 24);
    expect(forestHeight(500)).toBe(2000);
  });
});
