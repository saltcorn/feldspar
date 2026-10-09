// Drill paths (analytics TODO A6.5): a tile's plot that goes down a hierarchy
// of columns one click at a time.
//
// A drill path is a channel and the columns it shows at each level, outermost
// first: `{ channel: "x", path: ["district", "category"] }`. At the top the
// plot is as it was made, with the first column on the channel. Clicking a
// value there shows the next level: the same plot with the next column on the
// channel, filtered to the value clicked — "North's categories". A breadcrumb
// goes back up. At the last level a click selects, as on any tile.
//
// The path is the tile's, saved with it (`tile.drill`); where someone is on
// it is not saved, as a selection is not. The values picked on the way down
// filter only the tile they were picked on.

import type { Panel } from "../panels/panel";
import type { Channel, PlotSpec } from "../plot/spec";
import type { Picked } from "../plot/select";
import type { Condition } from "./filters";

/** The channels a plot drills down along. */
export type DrillChannel = "x" | "y" | "color";

export const DRILL_CHANNELS: DrillChannel[] = ["x", "y", "color"];

/** The most levels a path has (as the server says). */
export const MAX_LEVELS = 8;

/** A tile's drill path. */
export type Drill = { channel: DrillChannel; path: string[] };

/** A drill path read from anything, or `null`. */
export function readDrill(raw: unknown): Drill | null {
  if (!raw || typeof raw !== "object") return null;
  const r = raw as { channel?: unknown; path?: unknown };
  if (!DRILL_CHANNELS.includes(r.channel as DrillChannel) || !Array.isArray(r.path)) return null;
  const path = r.path.filter((p): p is string => typeof p === "string" && p.trim() !== "");
  if (path.length !== r.path.length || path.length < 2 || path.length > MAX_LEVELS || new Set(path).size !== path.length) {
    return null;
  }
  return { channel: r.channel as DrillChannel, path };
}

/** The plot of a panel that can drill down: a plot panel's. */
export function drillSpec(panel: Panel): PlotSpec | null {
  if (panel.kind === "plot") return panel.content.spec;
  return null;
}

/** The column a plot shows on a channel, if one layer encodes it. */
export function fieldOn(spec: PlotSpec, channel: Channel): string | undefined {
  return spec.layers.map((l) => l.encoding[channel as "x"]?.field).find((f) => f !== undefined);
}

/** The channel a new drill path goes along: the first of X, Color and Y the
 * plot shows a column on. */
export function defaultChannel(spec: PlotSpec): DrillChannel {
  return (["x", "color", "y"] as const).find((c) => fieldOn(spec, c) !== undefined) ?? "x";
}

/** The panel at a level of its drill path: the channel shows the level's
 * column, unbinned, on a scale of its own. `picked` holds the values chosen at
 * the levels above. */
export function drilledPanel(panel: Panel, drill: Drill, picked: unknown[]): Panel {
  const spec = drillSpec(panel);
  const level = Math.min(picked.length, drill.path.length - 1);
  if (!spec || level === 0) return panel;
  const field = drill.path[level];
  const replaced = new Set(drill.path);
  const next: PlotSpec = JSON.parse(JSON.stringify(spec)) as PlotSpec;
  for (const layer of next.layers) {
    const f = layer.encoding[drill.channel];
    if (f && replaced.has(f.field)) layer.encoding[drill.channel] = { field };
  }
  // A fixed order of the first column's values is not the next one's.
  const scale = next.scales?.[drill.channel];
  if (scale?.domain) delete scale.domain;
  return { ...panel, content: { ...panel.content, spec: next } } as Panel;
}

/** The conditions the values picked on the way down make: the tile's own. */
export function drillConditions(tile: string, dataset: string | undefined, drill: Drill, picked: unknown[]): Condition[] {
  if (!dataset) return [];
  return picked.slice(0, drill.path.length - 1).map((value, level) => ({
    id: `drill:${tile}:${level}`,
    dataset,
    column: drill.path[level],
    values: [value],
  }));
}

/** Where a click goes on a drilled tile: the values picked with the one
 * clicked added, when it picked one value of the level's column and there is
 * a level below; else `null`, and the click selects as on any tile. */
export function drillDown(drill: Drill, picked: unknown[], picks: Picked[]): unknown[] | null {
  const level = picked.length;
  if (level >= drill.path.length - 1) return null;
  const hit = picks.find((p) => p.field === drill.path[level] && p.values?.length === 1);
  return hit?.values ? [...picked, hit.values[0]] : null;
}

/** The dataset a drilled plot reads. */
export function drillDataset(panel: Panel): string | undefined {
  const spec = drillSpec(panel);
  return spec?.data.kind === "dataset" ? spec.data.dataset : undefined;
}
