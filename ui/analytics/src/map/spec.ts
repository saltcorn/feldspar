// The map spec and what `renderMap` answers, as the Analytics UI reads them
// (analytics TODO A5.6–A5.7; the server's types are `sc_analytics::map`'s and
// `sc_analytics::layer`'s, and `crates/sc-analytics/src/map.rs` says what each
// field means).

import type { StageColumn } from "../datasets/ops";
import type { Domain, FieldDef } from "../plot/spec";

/** Where a layer's geometry comes from. */
export type GeometrySource =
  | { kind: "column"; column: string }
  | { kind: "lon_lat"; longitude: string; latitude: string }
  | { kind: "key"; column: string; geometry: string };

/** The channels a map layer encodes: a plot layer's, without the positions. */
export type MapEncoding = Partial<Record<"color" | "size" | "shape" | "label", FieldDef>>;

/** One layer of a map. */
export type MapLayer = {
  dataset: string;
  geometry: GeometrySource;
  encoding?: MapEncoding;
  filter?: string;
};

/** A map: layers over a base map, the first at the bottom. */
export type MapSpec = { layers: MapLayer[] };

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
