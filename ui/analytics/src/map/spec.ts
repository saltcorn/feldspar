// The map spec and what `renderMap` answers, as the Analytics UI reads them
// (analytics TODO A5.6–A5.13; the server's types are `sc_analytics::map`'s and
// `sc_analytics::layer`'s, and `crates/sc-analytics/src/map.rs` says what each
// field means). A Map workspace's state is a map spec with the screen's own
// keys beside it (`workspace.ts`), and a map dragged into a report is one.

import type { StageColumn } from "../datasets/ops";
import type { Domain, FieldDef } from "../plot/spec";

/** Where a layer's geometry comes from. */
export type GeometrySource =
  | { kind: "column"; column: string }
  | { kind: "lon_lat"; longitude: string; latitude: string }
  | { kind: "key"; column: string; geometry: string };

/** The channels a map layer encodes: a plot layer's, without the positions. */
export type MapEncoding = Partial<Record<"color" | "size" | "shape" | "label", FieldDef>>;

/** How a number column is cut into classes (A5.9). */
export type Classification = "quantile" | "equal_interval" | "natural_breaks";

/** How a layer's features are drawn (A5.9). */
export type Style =
  | { kind: "auto" }
  | { kind: "single"; color?: string }
  | { kind: "categories" }
  | { kind: "graduated"; method: Classification; classes: number }
  | { kind: "proportional" }
  | { kind: "heatmap"; radius?: number };

export type StyleKind = Style["kind"];

/** One layer of a map. A workspace's layers have an `id`; the rest of the
 * settings are left out at their defaults (shown, opaque, in the legend). */
export type MapLayer = {
  id?: string;
  name?: string;
  dataset: string;
  geometry: GeometrySource;
  encoding?: MapEncoding;
  filter?: string;
  style?: Style;
  /** The columns a feature's popup shows; every column when empty. */
  popup?: string[];
  visible?: boolean;
  opacity?: number;
  legend?: boolean;
};

/** A tile or map service drawn for context (A5.11), before it has an id. */
export type ReferenceDraft = {
  name: string;
  opacity?: number;
  visible?: boolean;
  attribution?: string;
} & ReferenceService;

/** A tile or map service drawn for context (A5.11). */
export type ReferenceLayer = { id: string } & ReferenceDraft;

export type ReferenceService =
  | { kind: "tiles"; url: string }
  | { kind: "wms"; url: string; layers: string }
  | { kind: "arcgis"; url: string };

/** Where a map is looked at. */
export type MapViewport = { center: [number, number]; zoom: number };

/** A map: layers over a base map, the first at the bottom. */
export type MapSpec = { layers: MapLayer[]; reference?: ReferenceLayer[]; view?: MapViewport };

/** What a layer's features are read by (`sc_analytics::layer::LayerRequest`). */
export type LayerRequest = {
  dataset: string;
  geometry: GeometrySource;
  filter?: string;
  points?: boolean;
};

/** Whether a style draws each feature as a point at its centre. */
export function drawsPoints(style: Style | undefined): boolean {
  return style?.kind === "proportional" || style?.kind === "heatmap";
}

/** The request a layer's features are read by: what the attribute table and
 * a selection read too. */
export function layerRequest(layer: MapLayer): LayerRequest {
  const req: LayerRequest = { dataset: layer.dataset, geometry: layer.geometry };
  if (layer.filter && layer.filter.trim() !== "") req.filter = layer.filter;
  if (drawsPoints(layer.style)) req.points = true;
  return req;
}

/** Whether a layer is drawn. */
export function isVisible(layer: { visible?: boolean }): boolean {
  return layer.visible !== false;
}

/** A layer's opacity, from 0 to 1. */
export function opacityOf(layer: { opacity?: number }): number {
  const o = layer.opacity;
  return typeof o === "number" && Number.isFinite(o) ? Math.max(0, Math.min(1, o)) : 1;
}

/** One way a dataset's rows can be put on a map, as `suggestMap` offers it. */
export type SourceChoice = { source: GeometrySource; label: string };

/** The kinds of geometry a layer's features are. */
export type GeometryKind = "point" | "line" | "polygon";

/** A layer's features, or where to fetch them, or why there are none. */
export type LayerData =
  | {
      delivery: "geojson";
      count: number;
      bounds: [number, number, number, number] | null;
      geometry: GeometryKind[];
      properties: StageColumn[];
      /** Whether a feature's id is its row's key (else its place, from 1). */
      keyed: boolean;
      data: GeoJSON.FeatureCollection;
    }
  | {
      delivery: "tiles";
      count: number;
      vertices: number;
      bounds: [number, number, number, number] | null;
      geometry: GeometryKind[];
      properties: StageColumn[];
      source_layer: string;
      keyed: boolean;
      /** The URL template, on this origin: `/api/layers/tiles/{z}/{x}/{y}?layer=…`. */
      tiles: string;
    }
  | { delivery: "none"; error: string };

/** One layer as `renderMap` answers it. */
export type RenderedLayer = {
  data: LayerData;
  /** What each encoded column spans, by channel. */
  domains: Partial<Record<"color" | "size" | "shape", Domain>>;
  /** Graduated colours' breaks: the smallest value, each class's lower bound
   * after the first, the largest. */
  classes?: number[];
};

/** What `renderMap` answers. */
export type MapData = { layers: RenderedLayer[] };

/** Whether two geometry sources are the same. */
export function sameSource(a: GeometrySource | undefined, b: GeometrySource | undefined): boolean {
  return JSON.stringify(a ?? null) === JSON.stringify(b ?? null);
}

/** The bounds around every drawn layer, `[west, south, east, north]`. */
export function mapBounds(data: MapData): [number, number, number, number] | null {
  let out: [number, number, number, number] | null = null;
  for (const l of data.layers) {
    if (l.data.delivery === "none" || !l.data.bounds) continue;
    const [w, s, e, n] = l.data.bounds;
    out = out ? [Math.min(out[0], w), Math.min(out[1], s), Math.max(out[2], e), Math.max(out[3], n)] : [w, s, e, n];
  }
  return out;
}
