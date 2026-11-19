// The Simulation workspace's charts as ECharts options (analytics TODO
// A7.6–A7.8): a profile curve per input, and the scenarios' predicted
// distributions on one axis. Pure functions of the server's answers, so a
// test can check them without a browser.
//
// A profile curve is drawn by the input's kind: over a number or a date, a
// line with its interval as a band; over a category or yes/no, a point per
// level with its interval as a whisker. A classifier draws the probability of
// each class instead, as lines or grouped bars. The input's current value is
// marked on every curve, so moving one slider shows where each of the others
// stands.

import type { Option } from "../plot/echarts";
import { slotColor, type ChartPalette } from "../plot/palette";
import { currentValue, formatNumber, formatValue, levelKey, type Comparison, type Curve, type Input } from "./simulation";

/** What every chart here is drawn with. */
export type ChartLook = {
  palette: ChartPalette;
  /** For print: no animation, tooltips or highlighting. */
  still?: boolean;
  /** The text "probability", translated, for a classifier's axis. */
  probabilityLabel: string;
  /** The text "density", translated. */
  densityLabel: string;
};

/** How far an x axis's name sits below the axis line, past its tick labels. */
const X_NAME_GAP = 30;
/** The room under the plot for an x axis's name. ECharts' `containLabel`
 * makes room for the tick labels only, not the axis name, so a chart naming
 * its x axis needs this much more at the bottom or the name is clipped. */
const X_NAME_ROOM = 24;

/** Makes room under the plot for the x axis's name. */
function roomForXName(option: Option): void {
  const grid = option.grid as Option;
  option.grid = { ...grid, bottom: (grid.bottom as number) + X_NAME_ROOM };
}

function base(look: ChartLook): Option {
  const p = look.palette;
  return {
    backgroundColor: "transparent",
    textStyle: { color: p.text, fontFamily: 'system-ui, -apple-system, "Segoe UI", sans-serif' },
    animation: !look.still,
    tooltip: look.still ? { show: false } : { trigger: "axis", confine: true },
    grid: { left: 8, right: 16, top: 28, bottom: 8, containLabel: true },
  };
}

function axis(look: ChartLook, extra: Option): Option {
  const p = look.palette;
  return {
    axisLine: { lineStyle: { color: p.axis } },
    axisTick: { lineStyle: { color: p.axis } },
    axisLabel: { color: p.muted, hideOverlap: true },
    splitLine: { lineStyle: { color: p.grid } },
    nameTextStyle: { color: p.secondary },
    ...extra,
  };
}

function valueAxis(look: ChartLook, extra: Option = {}): Option {
  return axis(look, {
    type: "value",
    scale: true,
    axisLabel: { color: look.palette.muted, hideOverlap: true, formatter: (v: number) => formatNumber(v) },
    ...extra,
  });
}

function finish(option: Option, look: ChartLook): Option {
  if (!look.still) return option;
  const series = (option.series as Option[] | undefined) ?? [];
  return { ...option, series: series.map((s) => ({ ...s, silent: true, animation: false })) };
}

/** Whether an input is drawn along a continuous axis. */
function continuous(input: Input): boolean {
  return input.kind === "number" || input.kind === "integer" || input.kind === "date";
}

/** An x value on a continuous axis: a number, or a date as milliseconds. */
function xNumber(input: Input, x: unknown): number {
  if (input.kind === "date") return typeof x === "string" ? Date.parse(x) : Number(x);
  return Number(x);
}

/** The classes a curve's predictions give probabilities for, in order of
 * first appearance. Empty when they give none. */
export function curveClasses(curve: Curve): string[] {
  const seen: string[] = [];
  for (const point of curve.points) {
    for (const c of Object.keys(point.prediction.probabilities ?? {})) if (!seen.includes(c)) seen.push(c);
  }
  return seen;
}

/** The option drawing `curve`: the prediction along `input`, the others held
 * at `values`. */
export function curveOption(curve: Curve, input: Input, values: Record<string, unknown>, look: ChartLook): Option {
  const p = look.palette;
  const here = currentValue(input, values);
  const classes = curveClasses(curve);
  const option = base(look);
  roomForXName(option);
  const mark = (x: unknown) => ({
    silent: true,
    symbol: "none",
    label: { show: false },
    lineStyle: { color: p.secondary, type: "dashed", width: 1 },
    data: [{ xAxis: x }],
  });

  if (continuous(input)) {
    option.xAxis = axis(look, {
      type: input.kind === "date" ? "time" : "value",
      scale: true,
      name: input.name,
      nameLocation: "middle",
      nameGap: X_NAME_GAP,
      ...(input.kind === "date" ? {} : { axisLabel: { color: p.muted, hideOverlap: true, formatter: (v: number) => formatNumber(v) } }),
    });
    const xs = curve.points.map((pt) => xNumber(input, pt.x));
    const markLine = mark(xNumber(input, here));
    if (classes.length > 0) {
      option.yAxis = valueAxis(look, { min: 0, max: 1, scale: false, name: look.probabilityLabel });
      option.legend = { top: 0, textStyle: { color: p.secondary } };
      option.series = classes.map((c, i) => ({
        type: "line",
        name: c,
        showSymbol: false,
        color: slotColor(p, i),
        data: curve.points.map((pt, j) => [xs[j], pt.prediction.probabilities?.[c] ?? null]),
        ...(i === 0 ? { markLine } : {}),
      }));
      return finish(option, look);
    }
    const numeric = curve.points.every((pt) => typeof pt.prediction.value === "number");
    if (!numeric) {
      // A class without probabilities: the class as a step along the input.
      const labels = [...new Set(curve.points.map((pt) => formatValue(pt.prediction.value)))];
      option.yAxis = axis(look, { type: "category", data: labels });
      option.series = [
        {
          type: "line",
          step: "middle",
          color: slotColor(p, 0),
          data: curve.points.map((pt, j) => [xs[j], formatValue(pt.prediction.value)]),
          markLine,
        },
      ];
      return finish(option, look);
    }
    option.yAxis = valueAxis(look);
    const banded = curve.points.every((pt) => typeof pt.prediction.lower === "number" && typeof pt.prediction.upper === "number");
    const series: Option[] = [];
    if (banded) {
      // The band: the lower edge, and the width stacked on it.
      series.push({
        type: "line",
        name: "lower",
        stack: "interval",
        symbol: "none",
        lineStyle: { opacity: 0 },
        tooltip: { show: false },
        data: curve.points.map((pt, j) => [xs[j], pt.prediction.lower]),
      });
      series.push({
        type: "line",
        name: "interval",
        stack: "interval",
        symbol: "none",
        lineStyle: { opacity: 0 },
        areaStyle: { color: slotColor(p, 0), opacity: 0.18 },
        tooltip: { show: false },
        data: curve.points.map((pt, j) => [xs[j], (pt.prediction.upper as number) - (pt.prediction.lower as number)]),
      });
    }
    series.push({
      type: "line",
      name: "prediction",
      showSymbol: false,
      color: slotColor(p, 0),
      lineStyle: { width: 2 },
      data: curve.points.map((pt, j) => [xs[j], pt.prediction.value]),
      markLine,
    });
    option.series = series;
    return finish(option, look);
  }

  // A category or yes/no: a point (or bars) per level.
  const levels = curve.points.map((pt) => levelKey(pt.x));
  const hereKey = levelKey(here);
  option.xAxis = axis(look, {
    type: "category",
    data: levels,
    name: input.name,
    nameLocation: "middle",
    nameGap: X_NAME_GAP,
    splitLine: { show: false },
  });
  if (classes.length > 0) {
    option.yAxis = valueAxis(look, { min: 0, max: 1, scale: false, name: look.probabilityLabel });
    option.legend = { top: 0, textStyle: { color: p.secondary } };
    option.series = classes.map((c, i) => ({
      type: "bar",
      name: c,
      color: slotColor(p, i),
      data: curve.points.map((pt) => pt.prediction.probabilities?.[c] ?? null),
      ...(i === 0 ? { markLine: mark(hereKey) } : {}),
    }));
    return finish(option, look);
  }
  const numeric = curve.points.every((pt) => typeof pt.prediction.value === "number");
  if (!numeric) {
    const labels = [...new Set(curve.points.map((pt) => formatValue(pt.prediction.value)))];
    option.yAxis = axis(look, { type: "category", data: labels });
    option.series = [
      {
        type: "scatter",
        symbolSize: 10,
        color: slotColor(p, 0),
        data: curve.points.map((pt, j) => [levels[j], formatValue(pt.prediction.value)]),
        markLine: mark(hereKey),
      },
    ];
    return finish(option, look);
  }
  option.yAxis = valueAxis(look);
  const series: Option[] = [];
  const whiskers = curve.points
    .map((pt, j) => [j, pt.prediction.lower, pt.prediction.upper])
    .filter((w) => typeof w[1] === "number" && typeof w[2] === "number");
  if (whiskers.length > 0) {
    const color = slotColor(p, 0);
    series.push({
      type: "custom",
      name: "interval",
      tooltip: { show: false },
      data: whiskers,
      encode: { x: 0, y: [1, 2] },
      renderItem: (_params: unknown, api: CustomApi) => {
        const top = api.coord([api.value(0), api.value(2)]);
        const bottom = api.coord([api.value(0), api.value(1)]);
        const half = 6;
        const style = { stroke: color, lineWidth: 1.5 };
        return {
          type: "group",
          children: [
            { type: "line", shape: { x1: top[0], y1: top[1], x2: bottom[0], y2: bottom[1] }, style },
            { type: "line", shape: { x1: top[0] - half, y1: top[1], x2: top[0] + half, y2: top[1] }, style },
            { type: "line", shape: { x1: bottom[0] - half, y1: bottom[1], x2: bottom[0] + half, y2: bottom[1] }, style },
          ],
        };
      },
    });
  }
  series.push({
    type: "scatter",
    name: "prediction",
    symbolSize: 10,
    color: slotColor(p, 0),
    data: curve.points.map((pt, j) => [levels[j], pt.prediction.value]),
    markLine: mark(hereKey),
  });
  option.series = series;
  return finish(option, look);
}

/** The part of ECharts' custom series API a whisker needs. */
type CustomApi = { value: (dim: number) => number; coord: (point: number[]) => number[] };

/** The option drawing the scenarios of `comparison` side by side: their
 * densities on the shared grid, a classifier's class probabilities as
 * grouped bars, or — with no uncertainty — the predictions as bars. */
export function comparisonOption(comparison: Comparison, look: ChartLook): Option {
  const p = look.palette;
  const option = base(look);
  option.legend = { top: 0, textStyle: { color: p.secondary } };
  const scenarios = comparison.scenarios;
  const dense = scenarios.some((s) => s.spread.kind === "draws" || s.spread.kind === "interval");
  if (dense && comparison.grid.length > 0) {
    option.tooltip = look.still ? { show: false } : { trigger: "axis", confine: true, valueFormatter: (v: number) => formatNumber(v) };
    option.xAxis = valueAxis(look);
    option.yAxis = valueAxis(look, { name: look.densityLabel, scale: false, min: 0, axisLabel: { show: false } });
    option.series = scenarios.map((s, i) => {
      const density = s.spread.kind === "draws" || s.spread.kind === "interval" ? s.spread.density : [];
      const value = s.prediction.value;
      return {
        type: "line",
        name: s.name,
        smooth: true,
        showSymbol: false,
        color: slotColor(p, i),
        areaStyle: { opacity: 0.12 },
        data: comparison.grid.map((x, j) => [x, density[j] ?? null]),
        ...(typeof value === "number"
          ? {
              markLine: {
                silent: true,
                symbol: "none",
                label: { show: false },
                lineStyle: { color: slotColor(p, i), type: "dashed", width: 1 },
                data: [{ xAxis: value }],
              },
            }
          : {}),
      };
    });
    return finish(option, look);
  }
  const classes: string[] = [];
  for (const s of scenarios) {
    if (s.spread.kind === "classes") for (const c of Object.keys(s.spread.probabilities)) if (!classes.includes(c)) classes.push(c);
  }
  if (classes.length > 0) {
    option.tooltip = look.still ? { show: false } : { trigger: "axis", confine: true };
    option.xAxis = axis(look, { type: "category", data: classes, splitLine: { show: false } });
    option.yAxis = valueAxis(look, { min: 0, max: 1, scale: false, name: look.probabilityLabel });
    option.series = scenarios.map((s, i) => ({
      type: "bar",
      name: s.name,
      color: slotColor(p, i),
      data: classes.map((c) => (s.spread.kind === "classes" ? (s.spread.probabilities[c] ?? 0) : null)),
    }));
    return finish(option, look);
  }
  // Only the predictions.
  option.xAxis = axis(look, { type: "category", data: scenarios.map((s) => s.name), splitLine: { show: false } });
  option.yAxis = valueAxis(look);
  delete option.legend;
  option.series = [
    {
      type: "bar",
      data: scenarios.map((s, i) => ({
        value: typeof s.prediction.value === "number" ? s.prediction.value : null,
        itemStyle: { color: slotColor(p, i) },
      })),
    },
  ];
  return finish(option, look);
}
