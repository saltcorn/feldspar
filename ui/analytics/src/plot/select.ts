// Selections on a drawn plot (analytics TODO A6.3): a click or a brush read
// back as the values of the columns the plot encodes.
//
// A plot's spec declares what a click (`point`) and a brush (`interval`)
// select, and on which channels (`PlotSpec.selections`). A plot that declares
// none offers the defaults: a click picks what groups the rows — the category
// or bin under it on X or Y, and its colour group — and a brush picks a range
// along a continuous X. A dashboard turns what is picked into conditions on
// those columns (`dashboard/filters.ts`), which the server applies as a
// Filter (`sc_analytics::crossfilter`).
//
// What is under the pointer is read from what ECharts says about the item
// clicked or the area brushed, and what the compiled option's axes stand for
// (`selectionInfo`): a category axis's labels map back to their values, a
// binned one's to the bins' ranges.

import type { AxisPick, SelectionInfo } from "./echarts";
import { foldNameColumns, foldValue, type Channel, type PlotSpec, type Selection } from "./spec";

/** What a click or a brush picked of one column. */
export type Picked = {
  /** The column; none for the rows themselves (a map's feature, by key). */
  field?: string;
  channel?: Channel;
  values?: unknown[];
  range?: { min?: unknown; max?: unknown; max_exclusive?: boolean };
};

/** The column a channel shows, when it is one of the dataset's: not a column
 * a fold makes, and not a value a stat computed. */
export function columnOn(spec: PlotSpec, channel: Channel): string | undefined {
  if (spec.data.kind !== "dataset") return undefined;
  let field: string | undefined;
  if (channel === "row" || channel === "column" || channel === "wrap") field = spec.facet?.[channel]?.field;
  else field = spec.layers.map((l) => l.encoding[channel as "x"]?.field).find((f) => f !== undefined);
  if (!field) return undefined;
  const fold = spec.fold;
  if (fold) {
    const made = [...foldNameColumns(fold), foldValue(fold), `${foldValue(fold)}_x`, `${foldValue(fold)}_y`];
    if (made.includes(field)) return undefined;
  }
  return field;
}

/** The selections a plot offers: the ones it declares, or the defaults — a
 * click on what groups its rows, a brush along a continuous X. None on a plot
 * drawn on shares, around a centre or along parallel axes. */
export function selectionsOf(spec: PlotSpec, info: SelectionInfo): Selection[] {
  if (info.drawnApart || spec.data.kind !== "dataset") return [];
  if (spec.selections && spec.selections.length > 0) return spec.selections;
  const point: Channel[] = (["x", "y"] as const).filter(
    (c) => columnOn(spec, c) !== undefined && (info.axes[c].kind === "category" || info.binnedOn === c),
  );
  if (info.colors && columnOn(spec, "color") !== undefined) point.push("color");
  const out: Selection[] = [];
  if (point.length > 0) out.push({ name: "click", kind: "point", channels: point });
  if (columnOn(spec, "x") !== undefined && info.axes.x.kind !== "category") {
    out.push({ name: "brush", kind: "interval", channels: ["x"] });
  }
  return out;
}

/** The channels a click selects, and the ones a brush does. */
export function selectable(spec: PlotSpec, info: SelectionInfo): { point: Channel[]; interval: Channel[] } {
  const point = new Set<Channel>();
  const interval = new Set<Channel>();
  for (const s of selectionsOf(spec, info)) {
    for (const c of s.channels) {
      if (columnOn(spec, c) === undefined) continue;
      if (s.kind === "point") point.add(c);
      else if (c === "x" || c === "y") interval.add(c);
    }
  }
  return { point: [...point], interval: [...interval] };
}

/** How ECharts is asked to brush: along the screen's X, its Y, or a box. */
export type BrushType = "lineX" | "lineY" | "rect";

/** The brush a plot offers, if any. */
export function brushType(spec: PlotSpec, info: SelectionInfo): BrushType | null {
  const { interval } = selectable(spec, info);
  const across = interval.includes(info.flipped ? "y" : "x");
  const up = interval.includes(info.flipped ? "x" : "y");
  if (across && up) return "rect";
  if (across) return "lineX";
  if (up) return "lineY";
  return null;
}

/** What ECharts says about an item clicked. */
export type ClickParams = {
  componentType?: string;
  seriesType?: string;
  seriesName?: string;
  dataIndex?: number;
  value?: unknown;
};

/** Where a channel's value is in a data item: `[across, up, …]`. */
function slot(info: SelectionInfo, c: "x" | "y"): number {
  return (c === "x") !== info.flipped ? 0 : 1;
}

/** One pick of a channel's column. */
function picked(spec: PlotSpec, channel: Channel, pick: AxisPick): Picked | null {
  const field = columnOn(spec, channel);
  if (!field) return null;
  if ("range" in pick) return { field, channel, range: { min: pick.range[0], max: pick.range[1], max_exclusive: true } };
  return { field, channel, values: [pick.value] };
}

/** What a click on an item of the plot picks, one entry per column: nothing
 * when the item is not something the plot's selections pick. */
export function clickPicks(spec: PlotSpec, info: SelectionInfo, p: ClickParams): Picked[] {
  if (p.componentType && p.componentType !== "series") return [];
  const { point } = selectable(spec, info);
  const out: Picked[] = [];
  const add = (x: Picked | null) => {
    if (x) out.push(x);
  };
  const value = Array.isArray(p.value) ? (p.value as unknown[]) : null;
  for (const c of ["x", "y"] as const) {
    if (!point.includes(c)) continue;
    const axis = info.axes[c];
    if (p.seriesType === "custom") {
      // A histogram's bar: [bin start, bin end, bar start, bar end, count].
      const [lo, hi] = [value?.[0], value?.[1]];
      if (info.binnedOn === c && typeof lo === "number" && typeof hi === "number") {
        add(picked(spec, c, { range: [lo, hi] }));
      }
      continue;
    }
    if (p.seriesType === "boxplot") {
      // A box stands at its category's place along X.
      if (c === "x" && axis.kind === "category" && typeof p.dataIndex === "number" && axis.picks[p.dataIndex]) {
        add(picked(spec, c, axis.picks[p.dataIndex]));
      }
      continue;
    }
    const raw = value?.[slot(info, c)];
    if (raw === undefined) continue;
    if (axis.kind === "category") {
      const at = axis.categories.indexOf(String(raw));
      if (at !== -1 && axis.picks[at]) add(picked(spec, c, axis.picks[at]));
    } else {
      add(picked(spec, c, { value: raw }));
    }
  }
  if (point.includes("color") && info.colors && p.seriesName) {
    // A group's series is named by its colour, then its shape.
    const at = info.colors.labels.indexOf(p.seriesName.split(" · ")[0]);
    if (at !== -1) add(picked(spec, "color", { value: info.colors.values[at] }));
  }
  return out;
}

/** An area brushed, as ECharts reports it: a range along one axis, or a box
 * (`[[x0, x1], [y0, y1]]` on the screen's axes). Category axes are brushed
 * by the categories' places. */
export type BrushArea = { brushType?: string; coordRange?: unknown };

/** The ends of a brushed range as the column's values: a time axis brushes
 * milliseconds, which are written as dates or instants. */
function ends(info: SelectionInfo, c: "x" | "y", lo: number, hi: number): [unknown, unknown] {
  if (info.axes[c].kind !== "time") return [lo, hi];
  const iso = (ms: number) => {
    const text = new Date(ms).toISOString();
    return info.axes[c].dateOnly ? text.slice(0, 10) : text;
  };
  return [iso(lo), iso(hi)];
}

/** What a range brushed along a channel picks. */
function rangePick(spec: PlotSpec, info: SelectionInfo, c: "x" | "y", range: unknown): Picked | null {
  if (!Array.isArray(range) || typeof range[0] !== "number" || typeof range[1] !== "number") return null;
  const [a, b] = [Math.min(range[0], range[1]), Math.max(range[0], range[1])];
  const field = columnOn(spec, c);
  if (!field) return null;
  const axis = info.axes[c];
  if (axis.kind === "category") {
    const chosen = axis.picks.slice(Math.max(0, Math.round(a)), Math.round(b) + 1);
    if (chosen.length === 0) return null;
    if (chosen.every((p) => "range" in p)) {
      const ranges = chosen.map((p) => (p as { range: [number, number] }).range);
      return { field, channel: c, range: { min: ranges[0][0], max: ranges[ranges.length - 1][1], max_exclusive: true } };
    }
    return { field, channel: c, values: chosen.map((p) => ("value" in p ? p.value : null)) };
  }
  const [min, max] = ends(info, c, a, b);
  return { field, channel: c, range: { min, max } };
}

/** What a brush picks: a range of each brushed channel's column, or the
 * categories it covers. Nothing when the brush was cleared. */
export function brushPicks(spec: PlotSpec, info: SelectionInfo, areas: BrushArea[]): Picked[] {
  const area = areas.find((a) => Array.isArray(a.coordRange));
  if (!area) return [];
  const { interval } = selectable(spec, info);
  const across: "x" | "y" = info.flipped ? "y" : "x";
  const up: "x" | "y" = info.flipped ? "x" : "y";
  const ranges: [("x" | "y"), unknown][] =
    area.brushType === "rect"
      ? [
          [across, (area.coordRange as unknown[])[0]],
          [up, (area.coordRange as unknown[])[1]],
        ]
      : [[area.brushType === "lineY" ? up : across, area.coordRange]];
  return ranges
    .filter(([c]) => interval.includes(c))
    .map(([c, r]) => rangePick(spec, info, c, r))
    .filter((p): p is Picked => p !== null);
}
