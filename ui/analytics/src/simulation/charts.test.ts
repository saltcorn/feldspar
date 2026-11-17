import { describe, expect, it } from "vitest";

import { chartPalette } from "../plot/palette";
import { comparisonOption, curveClasses, curveOption, type ChartLook } from "./charts";
import type { Comparison, Curve, Input } from "./simulation";

const look: ChartLook = { palette: chartPalette("light"), probabilityLabel: "probability", densityLabel: "density" };

type S = Record<string, unknown> & { type: string; data: unknown[] };
const series = (o: Record<string, unknown>) => o.series as S[];

const area: Input = { name: "area", kind: "number", typical: 120, min: 40, max: 300 };
const areaCurve: Curve = {
  input: "area",
  points: [
    { x: 40, prediction: { value: 100, lower: 80, upper: 120 } },
    { x: 170, prediction: { value: 200, lower: 190, upper: 210 } },
    { x: 300, prediction: { value: 300, lower: 270, upper: 330 } },
  ],
};

describe("a profile curve", () => {
  it("draws a number input as a line with its interval as a band, the current value marked", () => {
    const o = curveOption(areaCurve, area, { area: 170 }, look);
    expect((o.xAxis as { type: string }).type).toBe("value");
    const [lower, band, line] = series(o);
    expect(lower.stack).toBe("interval");
    expect(lower.data).toEqual([
      [40, 80],
      [170, 190],
      [300, 270],
    ]);
    // The band is stacked on the lower edge: its width.
    expect(band.data).toEqual([
      [40, 40],
      [170, 20],
      [300, 60],
    ]);
    expect(line.data).toEqual([
      [40, 100],
      [170, 200],
      [300, 300],
    ]);
    expect((line.markLine as { data: unknown[] }).data).toEqual([{ xAxis: 170 }]);
  });

  it("marks the typical value when the input has not been moved, and has no band without an interval", () => {
    const bare: Curve = { input: "area", points: areaCurve.points.map((p) => ({ x: p.x, prediction: { value: p.prediction.value } })) };
    const o = curveOption(bare, area, {}, look);
    expect(series(o)).toHaveLength(1);
    expect((series(o)[0].markLine as { data: unknown[] }).data).toEqual([{ xAxis: 120 }]);
  });

  it("draws a date input on a time axis", () => {
    const built: Input = { name: "built", kind: "date", typical: "2000-01-01" };
    const curve: Curve = { input: "built", points: [{ x: "1990-01-01", prediction: { value: 1 } }, { x: "2010-01-01", prediction: { value: 2 } }] };
    const o = curveOption(curve, built, {}, look);
    expect((o.xAxis as { type: string }).type).toBe("time");
    expect(series(o)[0].data[0]).toEqual([Date.parse("1990-01-01"), 1]);
    expect((series(o)[0].markLine as { data: unknown[] }).data).toEqual([{ xAxis: Date.parse("2000-01-01") }]);
  });

  it("draws a category input as a point per level with whiskers", () => {
    const hood: Input = { name: "neighbourhood", kind: "category", typical: 2, levels: [1, 2, 3] };
    const curve: Curve = {
      input: "neighbourhood",
      points: [1, 2, 3].map((l) => ({ x: l, prediction: { value: l * 10, lower: l * 10 - 1, upper: l * 10 + 1 } })),
    };
    const o = curveOption(curve, hood, { neighbourhood: 3 }, look);
    expect((o.xAxis as { type: string; data: string[] }).data).toEqual(["1", "2", "3"]);
    const [whiskers, points] = series(o);
    expect(whiskers.type).toBe("custom");
    expect(whiskers.data).toEqual([
      [0, 9, 11],
      [1, 19, 21],
      [2, 29, 31],
    ]);
    expect(points.type).toBe("scatter");
    expect(points.data).toEqual([
      ["1", 10],
      ["2", 20],
      ["3", 30],
    ]);
    expect((points.markLine as { data: unknown[] }).data).toEqual([{ xAxis: "3" }]);
  });

  it("draws a classifier's probability of each class", () => {
    const curve: Curve = {
      input: "area",
      points: [
        { x: 40, prediction: { value: "no", probabilities: { no: 0.8, yes: 0.2 } } },
        { x: 300, prediction: { value: "yes", probabilities: { no: 0.3, yes: 0.7 } } },
      ],
    };
    expect(curveClasses(curve)).toEqual(["no", "yes"]);
    const o = curveOption(curve, area, {}, look);
    expect(series(o).map((s) => s.name)).toEqual(["no", "yes"]);
    expect(series(o)[1].data).toEqual([
      [40, 0.2],
      [300, 0.7],
    ]);
    expect((o.yAxis as { max: number }).max).toBe(1);
  });

  it("is still for print", () => {
    const o = curveOption(areaCurve, area, {}, { ...look, still: true });
    expect(o.animation).toBe(false);
    expect(series(o).every((s) => s.silent === true)).toBe(true);
  });
});

describe("the scenarios compared", () => {
  const base = { model: "m", fit: { id: "f", name: "fit", active: true }, uncertainty: "interval", level: 0.9, inputs: [] };

  it("draws each scenario's density on the shared grid, its prediction marked", () => {
    const c: Comparison = {
      ...base,
      outcome: "regression",
      grid: [0, 1, 2],
      scenarios: [
        { name: "baseline", values: {}, prediction: { value: 1 }, spread: { kind: "interval", df: 10, density: [0.1, 0.4, 0.1] } },
        { name: "north", values: {}, prediction: { value: 2 }, spread: { kind: "draws", draws: [2], total: 1, density: [0, 0.2, 0.5] } },
      ],
    };
    const o = comparisonOption(c, look);
    expect(series(o).map((s) => s.name)).toEqual(["baseline", "north"]);
    expect(series(o)[1].data).toEqual([
      [0, 0],
      [1, 0.2],
      [2, 0.5],
    ]);
    expect((series(o)[0].markLine as { data: unknown[] }).data).toEqual([{ xAxis: 1 }]);
  });

  it("draws a classifier's classes as grouped bars", () => {
    const c: Comparison = {
      ...base,
      outcome: "classification",
      grid: [],
      scenarios: [
        { name: "a", values: {}, prediction: { value: "no" }, spread: { kind: "classes", probabilities: { no: 0.6, yes: 0.4 } } },
        { name: "b", values: {}, prediction: { value: "yes" }, spread: { kind: "classes", probabilities: { yes: 0.9, no: 0.1 } } },
      ],
    };
    const o = comparisonOption(c, look);
    expect((o.xAxis as { data: string[] }).data).toEqual(["no", "yes"]);
    expect(series(o)[1].data).toEqual([0.1, 0.9]);
  });

  it("draws only the predictions when the model gives no uncertainty", () => {
    const c: Comparison = {
      ...base,
      outcome: "regression",
      grid: [],
      scenarios: [{ name: "a", values: {}, prediction: { value: 5 }, spread: { kind: "point" } }],
    };
    const o = comparisonOption(c, look);
    expect(series(o)[0].type).toBe("bar");
    expect((series(o)[0].data[0] as { value: number }).value).toBe(5);
  });
});
