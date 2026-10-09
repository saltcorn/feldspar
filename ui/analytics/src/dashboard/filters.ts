// A dashboard's filters (analytics TODO A6.3–A6.6): the conditions its panels
// are drawn with.
//
// Three things make conditions, and each panel is drawn with all of them that
// are not its own:
//
// - **selections**: a click or a brush on a tile (`plot/select.ts`, a map's
//   feature) selects values of the columns it encodes. A tile's selection is
//   one at a time — a new click replaces it, a click with Shift or Ctrl adds
//   to it, and clicking what is selected again lets it go — and it filters
//   every tile but the one it was made on, which keeps showing everything so
//   that something else can be picked. Selections are not saved: they are
//   where someone is looking, not what the dashboard is.
// - **drill-downs** (`drill.ts`): the values picked on the way down a tile's
//   drill path, which filter only that tile.
// - **the dashboard's own filters**, made in the filter bar and saved in its
//   state (`filters`), which filter every tile.
//
// A condition is `{ id, dataset, column?, values? | range? }`, the shape the
// server reads (`sc_analytics::crossfilter::Condition`). Which panels it
// reaches — the same dataset, or another through a column that refers to the
// same table — is the server's to work out, and it says so for each panel.

import type { Translate } from "../datasets/ops";
import type { Panel } from "../panels/panel";
import { formatNumber, labelOf } from "../plot/echarts";
import type { Picked } from "../plot/select";

/** A range of values; either end may be open. */
export type Range = { min?: unknown; max?: unknown; max_exclusive?: boolean };

/** A condition on one column of a dataset (the rows themselves, by key, when
 * `column` is absent). */
export type Condition = {
  id: string;
  dataset: string;
  column?: string;
  values?: unknown[];
  range?: Range;
};

/** A selection: a condition made on a tile, by a click or a brush. */
export type Selected = Condition & { source: string; by: "click" | "brush" };

/** How often a dashboard may refresh, in seconds; 0 is never. */
export const REFRESH_CHOICES = [0, 30, 60, 300, 900, 3600];

/** The most filters a dashboard keeps of its own (as the server says). */
export const MAX_FILTERS = 20;

function isObject(v: unknown): v is Record<string, unknown> {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}

function scalar(v: unknown): boolean {
  return v === null || ["string", "number", "boolean"].includes(typeof v);
}

/** A condition read from anything, or `null`. */
export function readCondition(raw: unknown): Condition | null {
  if (!isObject(raw) || typeof raw.id !== "string" || raw.id === "" || typeof raw.dataset !== "string") return null;
  const c: Condition = { id: raw.id, dataset: raw.dataset };
  if (typeof raw.column === "string" && raw.column !== "") c.column = raw.column;
  if (Array.isArray(raw.values) && raw.values.length > 0 && raw.values.every(scalar)) {
    c.values = raw.values;
    return c;
  }
  if (isObject(raw.range)) {
    const r = raw.range;
    const range: Range = {};
    if (scalar(r.min) && r.min !== null && r.min !== undefined) range.min = r.min;
    if (scalar(r.max) && r.max !== null && r.max !== undefined) range.max = r.max;
    if (r.max_exclusive === true) range.max_exclusive = true;
    if (range.min === undefined && range.max === undefined) return null;
    c.range = range;
    return c;
  }
  return null;
}

/** What a click on a map's feature picks (A6.3): the value of the column
 * the layer finds its geometry by, when that is a key to a table with one
 * (`districtⱵoutline`: the district); else the row itself, by its key, when
 * the feature's id is that key. Nothing for features told apart only by
 * their place. */
export function featurePicks(
  layer: { geometry: { kind: string; column?: string } },
  keyed: boolean,
  feature: { id: unknown; properties: Record<string, unknown> },
): Picked[] {
  if (layer.geometry.kind === "key" && layer.geometry.column) {
    const value = feature.properties[layer.geometry.column];
    return value === undefined ? [] : [{ field: layer.geometry.column, values: [value] }];
  }
  return keyed && feature.id !== undefined && feature.id !== null ? [{ values: [feature.id] }] : [];
}

/** A dashboard's own filters, read leniently. */
export function readFilters(raw: unknown): Condition[] {
  if (!Array.isArray(raw)) return [];
  const seen = new Set<string>();
  return raw.flatMap((r) => {
    const c = readCondition(r);
    if (!c || seen.has(c.id)) return [];
    seen.add(c.id);
    return [c];
  });
}

/** How often a dashboard refreshes, in seconds: 0 for never. */
export function readRefresh(raw: unknown): number {
  return typeof raw === "number" && Number.isFinite(raw) && raw >= 10 && raw <= 86400 ? Math.round(raw) : 0;
}

/** A condition as the server reads it: nothing a screen keeps beside it. */
export function wire(c: Condition): Condition {
  const out: Condition = { id: c.id, dataset: c.dataset };
  if (c.column !== undefined) out.column = c.column;
  if (c.values !== undefined) out.values = c.values;
  if (c.range !== undefined) out.range = c.range;
  return out;
}

/** The selections a click or a brush on tile `source`, over `dataset`, makes:
 * one condition per column picked. */
export function selectionOf(source: string, dataset: string, picks: Picked[], by: "click" | "brush"): Selected[] {
  return picks.flatMap((p) => {
    const c: Selected = { id: `sel:${source}:${p.field ?? "*"}`, dataset, source, by };
    if (p.field !== undefined) c.column = p.field;
    if (p.values && p.values.length > 0) c.values = p.values;
    else if (p.range) c.range = p.range;
    else return [];
    return [c];
  });
}

function sameValue(a: unknown, b: unknown): boolean {
  return JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
}

/** The selections after tile `source` picked `made` — replacing what it had
 * picked, or (`add`) toggling the values clicked in and out of it. Clicking
 * the one value selected again lets it go; an empty brush lets the tile's
 * brushed range go. */
export function select(current: Selected[], source: string, made: Selected[], add = false): Selected[] {
  const others = current.filter((s) => s.source !== source);
  const own = current.filter((s) => s.source === source);
  if (made.length === 0) return [...others, ...own.filter((s) => s.by !== "brush")];
  if (add) {
    const merged = [...own];
    for (const m of made) {
      const at = merged.findIndex((s) => s.id === m.id && s.values && m.values);
      if (at === -1) {
        merged.push(m);
        continue;
      }
      const values = [...(merged[at].values ?? [])];
      for (const v of m.values ?? []) {
        const i = values.findIndex((x) => sameValue(x, v));
        if (i === -1) values.push(v);
        else values.splice(i, 1);
      }
      if (values.length === 0) merged.splice(at, 1);
      else merged[at] = { ...merged[at], values };
    }
    return [...others, ...merged];
  }
  const same =
    own.length === made.length &&
    made.every((m) => own.some((s) => s.id === m.id && JSON.stringify(wire(s)) === JSON.stringify(wire(m))));
  return same && made.every((m) => m.by === "click") ? others : [...others, ...made];
}

/** The conditions tile `tile` is drawn with: the dashboard's filters, every
 * other tile's selections, and its own drill-down. */
export function conditionsFor(
  tile: string,
  { filters, selections, drill = [] }: { filters: Condition[]; selections: Selected[]; drill?: Condition[] },
): Condition[] {
  return [...filters, ...selections.filter((s) => s.source !== tile), ...drill].map(wire);
}

/** The datasets a panel reads: what a filter on it may be made on. */
export function datasetsOf(panel: Panel): string[] {
  const out = new Set<string>();
  const data = (d: { kind: string; dataset?: string } | undefined) => {
    if (d?.kind === "dataset" && d.dataset) out.add(d.dataset);
  };
  switch (panel.kind) {
    case "plot":
    case "summary_table":
      data(panel.content.spec.data);
      break;
    case "test_result":
      data(panel.content.tests.data);
      data(panel.content.plot?.data);
      break;
    case "map":
      for (const l of panel.content.spec.layers) out.add(l.dataset);
      break;
    case "stat_card":
      out.add(panel.content.dataset);
      break;
    default:
      break;
  }
  return [...out];
}

/** A value as a filter's label shows it: a date or an instant as it reads. */
export function valueText(v: unknown, missing: string): string {
  if (typeof v === "string" && /^\d{4}-\d{2}-\d{2}T00:00:00(\.000)?Z$/.test(v)) return v.slice(0, 10);
  if (typeof v === "string" && /^\d{4}-\d{2}-\d{2}T/.test(v)) return v.replace("T", " ").replace(/(:\d{2})(\.\d+)?Z$/, "$1");
  if (typeof v === "number") return formatNumber(v);
  return labelOf(v, missing);
}

/** A condition in words: what it is on, and what it keeps — "category",
 * "burglary, theft"; "occurred_on", "2025-02-01 – 2025-03-01". */
export function describe(c: Condition, t: Translate, dataset?: string): { on: string; keeps: string } {
  const missing = t("(missing)");
  const on = c.column ?? dataset ?? t("rows");
  if (c.values) {
    const shown = c.values.slice(0, 3).map((v) => valueText(v, missing));
    const more = c.values.length - shown.length;
    return { on, keeps: more > 0 ? t("{values} and {count} more", { values: shown.join(", "), count: more }) : shown.join(", ") };
  }
  const r = c.range ?? {};
  const lo = r.min === undefined ? null : valueText(r.min, missing);
  const hi = r.max === undefined ? null : valueText(r.max, missing);
  if (lo !== null && hi !== null) return { on, keeps: `${lo} – ${hi}` };
  if (lo !== null) return { on, keeps: t("from {value}", { value: lo }) };
  return { on, keeps: t("up to {value}", { value: hi ?? "" }) };
}

/** A new id for a dashboard filter. */
export function newFilterId(): string {
  return `f:${crypto.randomUUID()}`;
}
