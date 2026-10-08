// The Map workspace's state (analytics TODO A5.8–A5.13), and the pure
// functions that change it — so the tests change it without a map.
//
// The state is a map spec (`spec.ts`: the layers bottom first, the reference
// layers, the view last looked at), which is what the server checks
// (`sc_analytics::map::spec_of_state`), with the screen's own keys beside it:
// the selection, the layer whose settings and attribute table are open, and
// the table's order. Every layer has an id: the selection, the table and a
// source on the map name it by that, so moving a layer keeps all three.

import type { FieldDef } from "../plot/spec";
import {
  isVisible,
  type GeometrySource,
  type MapEncoding,
  type MapLayer,
  type MapSpec,
  type MapViewport,
  type ReferenceDraft,
  type ReferenceLayer,
  type Style,
  type StyleKind,
} from "./spec";

/** The selected features of one layer: their ids, and the condition they
 * were selected by when it was one (which "Save selection as dataset" keeps). */
export type MapSelection = { layer: string; ids: unknown[]; condition?: string };

/** The attribute table's settings: whether it is open, and its order — of
 * one layer, since another layer has other columns. */
export type TableSettings = { open: boolean; sort?: { layer: string; column: string; descending: boolean } };

/** A Map workspace's state. */
export type MapState = {
  layers: MapLayer[];
  reference: ReferenceLayer[];
  view?: MapViewport;
  selection: MapSelection | null;
  /** The layer whose settings and attribute table are shown. */
  active: string | null;
  table: TableSettings;
};

function isObject(v: unknown): v is Record<string, unknown> {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}

/** A new layer's id. */
export function newLayerId(): string {
  return crypto.randomUUID().slice(0, 8);
}

/** The state from what was stored: anything that is not one of its parts is
 * left out, and a layer without an id is given one. */
export function readMapState(raw: unknown): MapState {
  const s = isObject(raw) ? raw : {};
  const layers: MapLayer[] = Array.isArray(s.layers)
    ? s.layers
        .filter(
          (l): l is MapLayer =>
            isObject(l) && typeof l.dataset === "string" && isObject(l.geometry) && typeof l.geometry.kind === "string",
        )
        .map((l) => (l.id ? l : { ...l, id: newLayerId() }))
    : [];
  const reference: ReferenceLayer[] = Array.isArray(s.reference)
    ? s.reference.filter(
        (r): r is ReferenceLayer => isObject(r) && typeof r.id === "string" && typeof r.url === "string",
      )
    : [];
  const ids = new Set(layers.map((l) => l.id));
  const sel = s.selection;
  const selection: MapSelection | null =
    isObject(sel) && typeof sel.layer === "string" && ids.has(sel.layer) && Array.isArray(sel.ids)
      ? {
          layer: sel.layer,
          ids: sel.ids,
          ...(typeof sel.condition === "string" && sel.condition !== "" ? { condition: sel.condition } : {}),
        }
      : null;
  const active = typeof s.active === "string" && ids.has(s.active) ? s.active : (layers[layers.length - 1]?.id ?? null);
  const table: TableSettings = isObject(s.table)
    ? {
        open: s.table.open === true,
        ...(isObject(s.table.sort) &&
        typeof s.table.sort.column === "string" &&
        typeof s.table.sort.layer === "string" &&
        ids.has(s.table.sort.layer)
          ? {
              sort: {
                layer: s.table.sort.layer,
                column: s.table.sort.column,
                descending: s.table.sort.descending === true,
              },
            }
          : {}),
      }
    : { open: false };
  const view =
    isObject(s.view) && Array.isArray(s.view.center) && typeof s.view.zoom === "number"
      ? (s.view as MapViewport)
      : undefined;
  return { layers, reference, ...(view ? { view } : {}), selection, active, table };
}

/** The layer with this id. */
export function layerById(state: MapState, id: string | null | undefined): MapLayer | undefined {
  return id ? state.layers.find((l) => l.id === id) : undefined;
}

/** A layer's place in the spec, bottom first; -1 when it is not there. */
export function layerIndex(state: MapState, id: string | null | undefined): number {
  return id ? state.layers.findIndex((l) => l.id === id) : -1;
}

/** Add a layer on top, with an id and a name, and show its settings. */
export function addLayer(state: MapState, layer: MapLayer, name?: string): MapState {
  const made: MapLayer = { ...layer, id: layer.id && !layerById(state, layer.id) ? layer.id : newLayerId() };
  if (!made.name && name) made.name = name;
  return { ...state, layers: [...state.layers, made], active: made.id ?? null };
}

/** Take a layer off the map, its selection with it. */
export function removeLayer(state: MapState, id: string): MapState {
  const layers = state.layers.filter((l) => l.id !== id);
  return {
    ...state,
    layers,
    selection: state.selection?.layer === id ? null : state.selection,
    active: state.active === id ? (layers[layers.length - 1]?.id ?? null) : state.active,
  };
}

/** Move a layer `by` places: up (drawn over more) for a positive number. */
export function moveLayer(state: MapState, id: string, by: number): MapState {
  const from = layerIndex(state, id);
  if (from < 0) return state;
  const to = Math.max(0, Math.min(state.layers.length - 1, from + by));
  if (to === from) return state;
  const layers = [...state.layers];
  const [moved] = layers.splice(from, 1);
  layers.splice(to, 0, moved);
  return { ...state, layers };
}

/** Put the layer `id` at the place of the layer `before` (a drag in the
 * layer list, which lists the top layer first). */
export function placeLayer(state: MapState, id: string, at: string): MapState {
  const from = layerIndex(state, id);
  const to = layerIndex(state, at);
  if (from < 0 || to < 0 || from === to) return state;
  const layers = [...state.layers];
  const [moved] = layers.splice(from, 1);
  layers.splice(to, 0, moved);
  return { ...state, layers };
}

/** Change one layer. A change of what the features are — the dataset, the
 * geometry or the filter — clears that layer's selection, whose ids may no
 * longer be features of it. */
export function updateLayer(state: MapState, id: string, change: (l: MapLayer) => MapLayer): MapState {
  let cleared = false;
  const layers = state.layers.map((l) => {
    if (l.id !== id) return l;
    const next = change(l);
    if (
      next.dataset !== l.dataset ||
      JSON.stringify(next.geometry) !== JSON.stringify(l.geometry) ||
      (next.filter ?? "") !== (l.filter ?? "")
    ) {
      cleared = true;
    }
    return next;
  });
  const selection = cleared && state.selection?.layer === id ? null : state.selection;
  return { ...state, layers, selection };
}

/** What a layer's data is read by: only what changes the features or their
 * scales. Its name, visibility, opacity, legend and popup are drawn in the
 * browser, so changing them reads nothing again. */
export function dataKey(layer: MapLayer): string {
  return JSON.stringify({
    dataset: layer.dataset,
    geometry: layer.geometry,
    filter: layer.filter?.trim() || undefined,
    encoding: layer.encoding ?? {},
    style: layer.style ?? { kind: "auto" },
  });
}

/** The layer's encoding with one channel set, or cleared with `null`. */
export function setChannel(
  layer: MapLayer,
  channel: keyof MapEncoding,
  field: string | null,
): MapLayer {
  const encoding: MapEncoding = { ...(layer.encoding ?? {}) };
  if (field) encoding[channel] = { field } as FieldDef;
  else delete encoding[channel];
  const next: MapLayer = { ...layer, encoding };
  if (Object.keys(encoding).length === 0) delete next.encoding;
  return next;
}

/** A column as the style forms offer it. */
export type ColumnInfo = { name: string; type: string; key?: unknown };

/** Whether a column is a number a scale can spread over (not a key). */
export function isMeasure(c: ColumnInfo): boolean {
  return !c.key && ["int", "float", "decimal"].includes(c.type);
}

/** The layer with a new style: the classes and radius it starts with, and
 * the channel it needs filled from the columns when it is empty — the first
 * number for graduated colours and proportional symbols. */
export function setStyle(layer: MapLayer, kind: StyleKind, columns: ColumnInfo[]): MapLayer {
  const firstNumber = columns.find((c) => isMeasure(c))?.name ?? null;
  let next: MapLayer = { ...layer };
  let style: Style;
  switch (kind) {
    case "auto":
      style = { kind: "auto" };
      break;
    case "single":
      style = { kind: "single" };
      break;
    case "categories":
      style = { kind: "categories" };
      if (!next.encoding?.color) {
        const category = columns.find((c) => !isMeasure(c) && c.type !== "geometry")?.name ?? null;
        next = setChannel(next, "color", category);
      }
      break;
    case "graduated": {
      style = { kind: "graduated", method: "natural_breaks", classes: 5 };
      const current = next.encoding?.color?.field;
      if (!current || !columns.some((c) => c.name === current && isMeasure(c))) {
        next = setChannel(next, "color", firstNumber);
      }
      break;
    }
    case "proportional":
      style = { kind: "proportional" };
      if (!next.encoding?.size) next = setChannel(next, "size", firstNumber);
      break;
    case "heatmap":
      style = { kind: "heatmap", radius: 20 };
      break;
  }
  if (style.kind === "auto") delete next.style;
  else next.style = style;
  return next;
}

// --- selection --------------------------------------------------------------------

/** Whether a feature is selected. */
export function isSelected(state: MapState, layer: string, id: unknown): boolean {
  return state.selection?.layer === layer && state.selection.ids.some((s) => s === id);
}

/** A feature clicked (on the map or in the table): selected alone, or with
 * `add`, added to the layer's selection — or taken out of it if it was in.
 * A click on nothing (`id` undefined) without `add` clears the selection. */
export function clickFeature(state: MapState, layer: string, id: unknown, add: boolean): MapState {
  if (id === undefined || id === null) return add ? state : { ...state, selection: null };
  const current = state.selection?.layer === layer ? state.selection.ids : [];
  if (!add) {
    const only = current.length === 1 && current[0] === id;
    return { ...state, active: layer, selection: only ? null : { layer, ids: [id] } };
  }
  const ids = current.includes(id) ? current.filter((x) => x !== id) : [...current, id];
  return { ...state, active: layer, selection: ids.length > 0 ? { layer, ids } : null };
}

/** The selection a server answer made: its ids and the condition it found
 * them by. */
export function selectFound(state: MapState, layer: string, ids: unknown[], condition?: string): MapState {
  return {
    ...state,
    active: layer,
    selection: ids.length > 0 ? { layer, ids, ...(condition ? { condition } : {}) } : null,
  };
}

/** The table's order for `layer`: its own, or none. */
export function sortOf(state: MapState, layer: string | null | undefined): { column: string; descending: boolean } | undefined {
  const sort = state.table.sort;
  return sort && sort.layer === layer ? { column: sort.column, descending: sort.descending } : undefined;
}

/** A header of `layer`'s table clicked: ascending, descending, then none. */
export function sortBy(state: MapState, layer: string, column: string): MapState {
  const current = sortOf(state, layer);
  const next =
    current?.column !== column
      ? { column, descending: false }
      : !current.descending
        ? { column, descending: true }
        : undefined;
  const table: TableSettings = { open: state.table.open };
  if (next) table.sort = { layer, ...next };
  return { ...state, table };
}

/** Rows selected in the table: a range with `shift`, as a spreadsheet's. */
export function selectRange(state: MapState, layer: string, order: unknown[], from: unknown, to: unknown): MapState {
  const a = order.indexOf(from);
  const b = order.indexOf(to);
  if (a < 0 || b < 0) return clickFeature(state, layer, to, false);
  const range = order.slice(Math.min(a, b), Math.max(a, b) + 1);
  return { ...state, active: layer, selection: { layer, ids: range } };
}

// --- reference layers -------------------------------------------------------------

/** Add a reference layer, under the others' layers. */
export function addReference(state: MapState, ref: ReferenceDraft): MapState {
  return { ...state, reference: [...state.reference, { ...ref, id: newLayerId() } as ReferenceLayer] };
}

export function updateReference(
  state: MapState,
  id: string,
  change: (r: ReferenceLayer) => ReferenceLayer,
): MapState {
  return { ...state, reference: state.reference.map((r) => (r.id === id ? change(r) : r)) };
}

export function removeReference(state: MapState, id: string): MapState {
  return { ...state, reference: state.reference.filter((r) => r.id !== id) };
}

/** The origin of an http(s) URL, as Settings → Maps lists them; `null` for
 * anything else. A tile template's braces are in its path, which an origin
 * does not have. */
export function originOf(url: string): string | null {
  const m = /^(https?):\/\/([^/?#\s]+)/i.exec(url.trim());
  if (!m) return null;
  const host = m[2].toLowerCase();
  // A user name in the authority is not a host's.
  if (!/^(\[[0-9a-f:]+\]|[a-z0-9.-]+)(:\d{1,5})?$/.test(host)) return null;
  return `${m[1].toLowerCase()}://${host}`;
}

/** Whether a map may load from `url`: its origin is one Settings → Maps
 * names. */
export function hostAllowed(url: string, hosts: string[]): boolean {
  const origin = originOf(url);
  return origin !== null && hosts.includes(origin);
}

// --- the map as a spec and as a panel ---------------------------------------------

/** The map as a spec: what `renderMap` draws and a report keeps. */
export function specOf(state: MapState, only: "all" | "visible" = "all"): MapSpec {
  const layers = only === "visible" ? state.layers.filter(isVisible) : state.layers;
  const reference = only === "visible" ? state.reference.filter(isVisible) : state.reference;
  return {
    layers,
    ...(reference.length > 0 ? { reference } : {}),
    ...(state.view ? { view: state.view } : {}),
  };
}

/** A Map workspace's state with one layer: what "Open in map" makes of the
 * explorer's map (A5.13). */
export function stateFromLayer(layer: MapLayer, name: string): MapState {
  return addLayer(
    { layers: [], reference: [], selection: null, active: null, table: { open: false } },
    { ...layer },
    name,
  );
}

/** The geometry sources a layer's form offers, with the one it has first
 * when the dataset no longer offers it. */
export function sourceOptions(
  current: GeometrySource,
  offered: { source: GeometrySource; label: string }[],
): { source: GeometrySource; label: string }[] {
  const key = JSON.stringify(current);
  if (offered.some((o) => JSON.stringify(o.source) === key)) return offered;
  return [{ source: current, label: describeSource(current) }, ...offered];
}

/** A geometry source in words, as `suggestMap` labels them. */
export function describeSource(source: GeometrySource): string {
  switch (source.kind) {
    case "column":
      return `\`${source.column}\``;
    case "lon_lat":
      return `\`${source.longitude}\` and \`${source.latitude}\``;
    case "key":
      return `\`${source.column}\` → \`${source.geometry}\``;
  }
}
