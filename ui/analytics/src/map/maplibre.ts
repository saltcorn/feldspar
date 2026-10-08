// The compiler from a map spec and its data to MapLibre sources and layers
// (analytics TODO A5.6), as `plot/echarts.ts` is for plots: pure, so the tests
// give it a spec and read the layers back, and `MapView` only adds what it
// returns to a map.
//
// Each layer of the spec becomes one source and up to four MapLibre layers,
// by the kinds of geometry the server says it has:
//
// | geometry | MapLibre layers |
// |---|---|
// | polygons | a `fill` coloured by Color, and a `line` for the outlines |
// | lines | a `line` coloured by Color and as wide as Size |
// | points | a `circle` coloured by Color and as large as Size — or, with a column on Shape, a `symbol` of the shape images (`shapes.ts`) |
// | any | a `symbol` with the Label column's text |
//
// A small layer's source is its GeoJSON; a large one's is its vector tiles,
// from `layerTile` on this origin. The colour and size scales are MapLibre
// expressions over each feature's properties, built from the domains the
// server computed over every feature — so a tiled layer is coloured the same
// at every zoom — in the plots' palette, so a category has the colour on a
// map that it has in a bar chart.
//
// The Map workspace (A5.8–A5.11) adds, per layer, its **style** — one colour,
// categories, graduated colours from the classes the server computed (a
// `step` over the breaks), proportional circles, or a `heatmap` layer — its
// opacity and visibility, and whether the legend shows it; the **selected**
// features drawn over the rest, matched by their ids; and **reference
// layers**, raster tile services under every data layer.

import type {
  LayerSpecification,
  SourceSpecification,
} from "@maplibre/maplibre-gl-style-spec";

import { formatNumber, labelOf } from "../plot/echarts";
import { chartPalette, slotColor, type ChartPalette } from "../plot/palette";
import type { Domain } from "../plot/spec";
import { SHAPE_CSS, shapeAt, shapeImageName, type ShapeName } from "./shapes";
import {
  isVisible,
  opacityOf,
  type GeometryKind,
  type MapData,
  type MapLayer,
  type MapSpec,
  type ReferenceLayer,
  type RenderedLayer,
  type Style,
} from "./spec";

/** What a compile needs besides the spec and its data. */
export type CompileOptions = {
  theme: "light" | "dark";
  /** The page's origin, put in front of a tile URL (MapLibre wants absolute ones). */
  origin: string;
  /** The fonts the base map's glyphs have, for labels; none when the base
   * map has no glyphs, and then labels cannot be drawn. */
  font: string[] | null;
  /** What a missing value is called in a legend. */
  missing: string;
  /** The selected features of one layer (by its place in the spec), drawn
   * over the rest. */
  selection?: { layer: number; ids: unknown[] } | null;
  /** The words a heatmap's legend uses. */
  words?: { density: string; low: string; high: string };
};

/** One entry of a map's legend. */
export type LegendEntry = {
  /** The layer it belongs to, by its place in the spec. */
  layer: number;
  /** The layer's name, when it has one: a workspace's legend says whose
   * entry it is. */
  title?: string;
  /** The channel. */
  channel: "color" | "size" | "shape";
  /** The column. */
  field: string;
  /** Swatches or symbols, one per value. */
  items?: { label: string; color: string; shape?: ShapeName }[];
  /** A colour ramp from the smallest value to the largest. */
  gradient?: { colors: string[]; min: string; max: string };
  /** The smallest and largest symbol, in CSS pixels across. */
  sizes?: { min: string; max: string; minPx: number; maxPx: number };
};

/** Something the reader should know about how a map was drawn. */
export type MapNote =
  | { kind: "size-on-polygons"; field: string }
  | { kind: "shape-not-points"; field: string }
  | { kind: "labels-need-fonts"; field: string }
  | { kind: "many-values"; field: string; count: number }
  | { kind: "refused"; layer: number; error: string };

/** What a compile makes. */
export type CompiledMap = {
  sources: Record<string, SourceSpecification>;
  /** Bottom first. */
  layers: LayerSpecification[];
  /** The shape images the layers use, to be added to the map first. */
  images: ShapeName[];
  /** The layers a pointer over a feature shows the feature's row for. */
  interactive: string[];
  /** The spec's layer each source draws, by its place. */
  layerOf: Record<string, number>;
  legend: LegendEntry[];
  notes: MapNote[];
};

function emptyCompiled(): CompiledMap {
  return { sources: {}, layers: [], images: [], interactive: [], layerOf: {}, legend: [], notes: [] };
}

/** A MapLibre expression, typed loosely: the style spec's own types are a
 * tuple per operator, which an expression built up from parts cannot meet. */
type Expr = unknown;

/** A point's radius with nothing on Size, and the range Size spans. */
export const POINT_RADIUS = 5;
export const MIN_RADIUS = 2;
export const MAX_RADIUS = 18;
/** A line's width with nothing on Size, and the range Size spans. */
export const LINE_WIDTH = 2;
export const MIN_WIDTH = 1;
export const MAX_WIDTH = 8;
/** A polygon's fill opacity: the base map shows through. */
export const FILL_OPACITY = 0.6;
/** Proportional circles' opacity: the ones under them show through. */
export const PROPORTIONAL_OPACITY = 0.7;
/** A heatmap's radius in pixels when its style does not say. */
export const HEATMAP_RADIUS = 20;
/** A heatmap's opacity. */
export const HEATMAP_OPACITY = 0.85;
/** A vector tile source's deepest zoom: deeper tiles are drawn from these. */
export const TILE_MAX_ZOOM = 16;

/** The geometry types a MapLibre filter matches each kind by. */
const TYPES: Record<GeometryKind, string[]> = {
  point: ["Point", "MultiPoint"],
  line: ["LineString", "MultiLineString"],
  polygon: ["Polygon", "MultiPolygon"],
};

function kindFilter(kind: GeometryKind): Expr {
  return ["match", ["geometry-type"], TYPES[kind], true, false];
}

/** The source id of the spec's `index`th layer, and its layers' ids: by the
 * layer's own id when it has one, so moving a layer up or down keeps its
 * source — and its GeoJSON is not sent to the map's worker again. */
export function sourceId(index: number, layer?: { id?: string }): string {
  return layer?.id ? `fd-${layer.id.replace(/[^A-Za-z0-9_-]/g, "_")}` : `fd-${index}`;
}

/** The colours of `k` graduated classes: spread along the sequential ramp. */
export function classColors(palette: ChartPalette, k: number): string[] {
  const ramp = palette.sequential;
  if (k <= 1) return [ramp[Math.floor(ramp.length / 2)]];
  return Array.from({ length: k }, (_, i) => ramp[Math.round((i * (ramp.length - 1)) / (k - 1))]);
}

/** The Color channel as graduated classes: a `step` over the breaks the
 * server computed, a legend item per class. */
function classScale(field: string, breaks: number[], palette: ChartPalette): Scale {
  if (breaks.length < 2) return { expr: palette.categorical[0] };
  const k = breaks.length - 1;
  const colors = classColors(palette, k);
  const step: unknown[] = ["step", value(field), colors[0]];
  for (let i = 1; i < k; i++) step.push(breaks[i], colors[i]);
  const items = colors.map((color, i) => ({
    label:
      i === k - 1 && breaks[i] === breaks[i + 1]
        ? formatNumber(breaks[i])
        : `${formatNumber(breaks[i])} – ${formatNumber(breaks[i + 1])}`,
    color,
  }));
  return { expr: ["case", present(field), step, palette.muted], legend: { items } };
}

/** The colour a selected feature is ringed in. */
function highlightColor(palette: ChartPalette): string {
  return palette.text;
}

function num(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && v.trim() !== "" && Number.isFinite(Number(v))) return Number(v);
  return null;
}

/** A feature's value on a column, as a number, and whether it has one. */
function value(field: string): Expr {
  return ["to-number", ["get", field], 0];
}

function present(field: string): Expr {
  return ["!=", ["get", field], null];
}

/** The distinct labels of a discrete domain, in its order: the values as
 * MapLibre's `match` compares them (`to-string` of the property). */
function labels(domain: Domain): string[] {
  const out: string[] = [];
  for (const v of domain.values ?? []) {
    const s = typeof v === "string" ? v : JSON.stringify(v);
    if (!out.includes(s)) out.push(s);
  }
  return out;
}

type Scale = { expr: Expr; legend?: Omit<LegendEntry, "layer" | "channel" | "field"> };

/** The Color channel's expression and legend. */
function colorScale(field: string | undefined, domain: Domain | undefined, palette: ChartPalette, missing: string, notes: MapNote[]): Scale {
  const plain = palette.categorical[0];
  if (!field || !domain) return { expr: plain };
  if (domain.kind === "continuous") {
    const lo = num(domain.min);
    const hi = num(domain.max);
    if (lo === null || hi === null) return { expr: plain };
    const ramp = palette.sequential;
    const label = (v: number) => formatNumber(v);
    if (hi <= lo) {
      const mid = ramp[Math.floor(ramp.length / 2)];
      return { expr: ["case", present(field), mid, palette.muted], legend: { items: [{ label: label(lo), color: mid }] } };
    }
    const stops: unknown[] = [];
    ramp.forEach((c, i) => stops.push(lo + ((hi - lo) * i) / (ramp.length - 1), c));
    return {
      expr: ["case", present(field), ["interpolate", ["linear"], value(field), ...stops], palette.muted],
      legend: { gradient: { colors: ramp, min: label(lo), max: label(hi) } },
    };
  }
  const values = labels(domain);
  if (values.length === 0) return { expr: plain };
  const arms: unknown[] = [];
  values.forEach((v, i) => arms.push(v, slotColor(palette, i)));
  const shown = values.slice(0, palette.categorical.length);
  const items = shown.map((v, i) => ({ label: labelOf(domain.values?.[i] ?? v, missing), color: slotColor(palette, i) }));
  if (values.length > palette.categorical.length) {
    notes.push({ kind: "many-values", field, count: values.length });
    items.push({ label: "…", color: palette.other });
  }
  return { expr: ["match", ["to-string", ["get", field]], ...arms, palette.other], legend: { items } };
}

/** The Size channel's expression — a radius in CSS pixels for points, a width
 * for lines — and legend. Points are sized by area from zero where every value
 * is at least zero, as a proportional symbol is; otherwise in proportion
 * across the range. */
function sizeScale(field: string | undefined, domain: Domain | undefined, kind: "point" | "line"): Scale {
  const [plain, small, large] = kind === "point" ? [POINT_RADIUS, MIN_RADIUS, MAX_RADIUS] : [LINE_WIDTH, MIN_WIDTH, MAX_WIDTH];
  if (!field || !domain) return { expr: plain };
  const lo = num(domain.min);
  const hi = num(domain.max);
  if (lo === null || hi === null || hi <= lo) return { expr: plain };
  const across = (px: number) => (kind === "point" ? 2 * px : px);
  const legend = { sizes: { min: formatNumber(lo), max: formatNumber(hi), minPx: across(small), maxPx: across(large) } };
  if (kind === "point" && lo >= 0) {
    // r = R·√(v / max), never below the smallest radius.
    const expr = ["max", small, ["*", large, ["sqrt", ["/", ["max", 0, value(field)], hi]]]];
    return {
      expr: ["case", present(field), expr, small],
      legend: { sizes: { ...legend.sizes, minPx: across(Math.max(small, large * Math.sqrt(lo / hi))) } },
    };
  }
  return {
    expr: ["case", present(field), ["interpolate", ["linear"], value(field), lo, small, hi, large], small],
    legend,
  };
}

/** The Shape channel's image expression and legend. */
function shapeScale(field: string, domain: Domain | undefined, color: string, missing: string): { expr: Expr; images: ShapeName[]; legend: Scale["legend"] } {
  const values = domain ? labels(domain) : [];
  if (values.length === 0) return { expr: shapeImageName("circle"), images: ["circle"], legend: undefined };
  const arms: unknown[] = [];
  const images = new Set<ShapeName>(["circle"]);
  values.forEach((v, i) => {
    arms.push(v, shapeImageName(shapeAt(i)));
    images.add(shapeAt(i));
  });
  const items = values.map((v, i) => ({ label: labelOf(domain?.values?.[i] ?? v, missing), color, shape: shapeAt(i) }));
  return {
    expr: ["match", ["to-string", ["get", field]], ...arms, shapeImageName("circle")],
    images: [...images],
    legend: { items },
  };
}

/** A tile URL's template on this origin, as MapLibre wants it. */
export function absoluteTiles(template: string, origin: string): string {
  return /^https?:\/\//.test(template) ? template : `${origin.replace(/\/$/, "")}${template}`;
}

/** Bounds MapLibre accepts for a source: a point's widened a little, and
 * latitudes within Web Mercator's. */
export function sourceBounds(b: [number, number, number, number]): [number, number, number, number] {
  const pad = 1e-4;
  const lat = (v: number) => Math.max(-85.0511, Math.min(85.0511, v));
  return [Math.max(-180, b[0] - pad), lat(b[1] - pad), Math.min(180, b[2] + pad), lat(b[3] + pad)];
}

/** One layer of the spec, compiled; its ids start with `sourceId(index, layer)`. */
export function compileLayer(index: number, layer: MapLayer, rendered: RenderedLayer, opts: CompileOptions): CompiledMap {
  const out = emptyCompiled();
  const data = rendered.data;
  if (data.delivery === "none") {
    out.notes.push({ kind: "refused", layer: index, error: data.error });
    return out;
  }
  const palette = chartPalette(opts.theme);
  const id = sourceId(index, layer);
  const enc = layer.encoding ?? {};
  const style: Style = layer.style ?? { kind: "auto" };
  const opacity = opacityOf(layer);
  const title = layer.name && layer.name.trim() !== "" ? layer.name : undefined;
  if (data.delivery === "geojson") {
    out.sources[id] = { type: "geojson", data: data.data };
  } else {
    out.sources[id] = {
      type: "vector",
      tiles: [absoluteTiles(data.tiles, opts.origin)],
      maxzoom: TILE_MAX_ZOOM,
      ...(data.bounds ? { bounds: sourceBounds(data.bounds) } : {}),
    };
  }
  out.layerOf[id] = index;
  // A hidden layer keeps its source, so showing it again sends nothing.
  if (!isVisible(layer)) return out;
  const from = (kind: string): Record<string, unknown> => ({
    id: `${id}-${kind}`,
    source: id,
    ...(data.delivery === "tiles" ? { "source-layer": data.source_layer } : {}),
  });
  // A tiled layer with no features said what it holds; one that did not say is
  // drawn as every kind, which costs nothing where there is none.
  const kinds: GeometryKind[] =
    data.geometry.length > 0 ? data.geometry : data.delivery === "tiles" ? ["point", "line", "polygon"] : [];
  const add = (spec: Record<string, unknown>, interactive = true) => {
    out.layers.push(spec as unknown as LayerSpecification);
    if (interactive) out.interactive.push(spec.id as string);
  };
  const showLegend = layer.legend !== false;
  const legend = (channel: LegendEntry["channel"], field: string, entry: Scale["legend"]) => {
    if (entry && showLegend) out.legend.push({ layer: index, ...(title ? { title } : {}), channel, field, ...entry });
  };

  if (style.kind === "heatmap") {
    const weight = enc.size ? sizeWeight(enc.size.field, rendered.domains.size) : 1;
    const ramp = palette.sequential;
    const stops: unknown[] = [0, "rgba(0, 0, 0, 0)"];
    ramp.forEach((c, i) => stops.push((i + 1) / ramp.length, c));
    add(
      {
        ...from("heat"),
        type: "heatmap",
        paint: {
          "heatmap-weight": weight,
          "heatmap-radius": style.radius ?? HEATMAP_RADIUS,
          "heatmap-color": ["interpolate", ["linear"], ["heatmap-density"], ...stops],
          "heatmap-opacity": HEATMAP_OPACITY * opacity,
        },
      },
      false,
    );
    const words = opts.words ?? { density: "Density", low: "low", high: "high" };
    legend("color", enc.size?.field ?? words.density, { gradient: { colors: ramp, min: words.low, max: words.high } });
    return withSelection(out, index, layer, kinds, from, palette, opts);
  }

  const color: Scale =
    style.kind === "single"
      ? { expr: style.color && /^#[0-9a-fA-F]{6}$/.test(style.color) ? style.color : palette.categorical[0] }
      : style.kind === "graduated" && enc.color
        ? classScale(enc.color.field, rendered.classes ?? [], palette)
        : colorScale(enc.color?.field, rendered.domains.color, palette, opts.missing, out.notes);
  if (enc.color && style.kind !== "single") legend("color", enc.color.field, color.legend);

  if (kinds.includes("polygon")) {
    add({
      ...from("fill"),
      type: "fill",
      filter: kindFilter("polygon"),
      paint: { "fill-color": color.expr, "fill-opacity": FILL_OPACITY * opacity },
    });
    add(
      {
        ...from("outline"),
        type: "line",
        filter: kindFilter("polygon"),
        paint: { "line-color": palette.surface, "line-width": 0.8, "line-opacity": 0.9 * opacity },
      },
      false,
    );
    if (enc.size && !kinds.includes("point") && !kinds.includes("line")) {
      out.notes.push({ kind: "size-on-polygons", field: enc.size.field });
    }
  }
  if (kinds.includes("line")) {
    const width = sizeScale(enc.size?.field, rendered.domains.size, "line");
    if (enc.size && !kinds.includes("point")) legend("size", enc.size.field, width.legend);
    add({
      ...from("line"),
      type: "line",
      filter: kindFilter("line"),
      layout: { "line-cap": "round", "line-join": "round" },
      paint: { "line-color": color.expr, "line-width": width.expr, "line-opacity": opacity },
    });
  }
  if (kinds.includes("point")) {
    const radius = sizeScale(enc.size?.field, rendered.domains.size, "point");
    if (enc.size) legend("size", enc.size.field, radius.legend);
    if (enc.shape && style.kind !== "proportional") {
      const shape = shapeScale(enc.shape.field, rendered.domains.shape, enc.color ? palette.muted : palette.categorical[0], opts.missing);
      legend("shape", enc.shape.field, shape.legend);
      out.images.push(...shape.images.filter((s) => !out.images.includes(s)));
      add({
        ...from("point"),
        type: "symbol",
        filter: kindFilter("point"),
        layout: {
          "icon-image": shape.expr,
          // The image is `SHAPE_CSS` pixels across at size 1; a radius of r
          // is 2r across.
          "icon-size": ["/", ["*", 2, radius.expr], SHAPE_CSS],
          "icon-allow-overlap": true,
          "icon-ignore-placement": true,
        },
        paint: {
          "icon-color": color.expr,
          "icon-halo-color": palette.surface,
          "icon-halo-width": 1,
          "icon-opacity": opacity,
        },
      });
    } else {
      const proportional = style.kind === "proportional";
      add({
        ...from("point"),
        type: "circle",
        filter: kindFilter("point"),
        // Proportional circles overlap: the small ones are drawn on top.
        ...(proportional && enc.size ? { layout: { "circle-sort-key": ["-", 0, value(enc.size.field)] } } : {}),
        paint: {
          "circle-color": color.expr,
          "circle-radius": radius.expr,
          "circle-stroke-color": palette.surface,
          "circle-stroke-width": 1,
          "circle-opacity": (proportional ? PROPORTIONAL_OPACITY : 0.9) * opacity,
          ...(opacity < 1 ? { "circle-stroke-opacity": opacity } : {}),
        },
      });
    }
  } else if (enc.shape) {
    out.notes.push({ kind: "shape-not-points", field: enc.shape.field });
  }
  if (enc.label) {
    if (!opts.font) {
      out.notes.push({ kind: "labels-need-fonts", field: enc.label.field });
    } else {
      const numeric = data.properties.some(
        (p) => p.name === enc.label?.field && !p.key && ["int", "float", "decimal"].includes(p.type),
      );
      const text = numeric
        ? ["number-format", value(enc.label.field), { "max-fraction-digits": 2 }]
        : ["to-string", ["get", enc.label.field]];
      const onPoints = kinds.length === 1 && kinds[0] === "point";
      add(
        {
          ...from("label"),
          type: "symbol",
          filter: present(enc.label.field),
          layout: {
            "text-field": text,
            "text-font": opts.font,
            "text-size": 11,
            "text-optional": true,
            ...(onPoints ? { "text-anchor": "top", "text-offset": [0, 0.9] } : {}),
            ...(kinds.includes("line") && !kinds.includes("polygon") ? { "symbol-placement": "line" } : {}),
          },
          paint: {
            "text-color": palette.text,
            "text-halo-color": palette.surface,
            "text-halo-width": 1.5,
            ...(opacity < 1 ? { "text-opacity": opacity } : {}),
          },
        },
        false,
      );
    }
  }
  return withSelection(out, index, layer, kinds, from, palette, opts);
}

/** A number on Size as a heatmap's weight, from 0 to 1 over its range. */
function sizeWeight(field: string, domain: Domain | undefined): Expr {
  const lo = num(domain?.min);
  const hi = num(domain?.max);
  if (lo === null || hi === null || hi <= lo) return 1;
  return ["interpolate", ["linear"], value(field), lo, 0, hi, 1];
}

/** The selected features of this layer, ringed over the rest: matched by
 * their ids, which are the rows' keys (or places) the attribute table shows. */
function withSelection(
  out: CompiledMap,
  index: number,
  layer: MapLayer,
  kinds: GeometryKind[],
  from: (kind: string) => Record<string, unknown>,
  palette: ChartPalette,
  opts: CompileOptions,
): CompiledMap {
  const selected = opts.selection;
  if (!selected || selected.layer !== index || selected.ids.length === 0) return out;
  const ids = selected.ids.filter((id) => typeof id === "number" || typeof id === "string");
  if (ids.length === 0) return out;
  const which = ["in", ["id"], ["literal", ids]];
  const ring = highlightColor(palette);
  const heat = layer.style?.kind === "heatmap";
  if (kinds.includes("polygon") && !heat) {
    out.layers.push({
      ...from("selected-outline"),
      type: "line",
      filter: ["all", kindFilter("polygon"), which],
      paint: { "line-color": ring, "line-width": 3 },
    } as unknown as LayerSpecification);
  }
  if (kinds.includes("line") && !heat) {
    out.layers.push({
      ...from("selected-line"),
      type: "line",
      filter: ["all", kindFilter("line"), which],
      layout: { "line-cap": "round", "line-join": "round" },
      paint: { "line-color": ring, "line-width": LINE_WIDTH + 3 },
    } as unknown as LayerSpecification);
  }
  if (kinds.includes("point") || heat) {
    out.layers.push({
      ...from("selected-point"),
      type: "circle",
      filter: heat ? which : ["all", kindFilter("point"), which],
      paint: {
        "circle-radius": POINT_RADIUS + 3,
        "circle-color": "rgba(0, 0, 0, 0)",
        "circle-stroke-color": ring,
        "circle-stroke-width": 2.5,
      },
    } as unknown as LayerSpecification);
  }
  return out;
}

/** A reference layer's raster tile template, as MapLibre asks for it: a
 * template as given, a WMS GetMap in Web Mercator, an ArcGIS service's tiles. */
export function referenceTiles(ref: ReferenceLayer): string {
  const url = ref.url.trim();
  switch (ref.kind) {
    case "tiles":
      return url;
    case "arcgis":
      return `${url.replace(/\/+$/, "")}/tile/{z}/{y}/{x}`;
    case "wms": {
      const params = new URLSearchParams({
        SERVICE: "WMS",
        VERSION: "1.3.0",
        REQUEST: "GetMap",
        LAYERS: ref.layers.trim(),
        STYLES: "",
        FORMAT: "image/png",
        TRANSPARENT: "true",
        CRS: "EPSG:3857",
        WIDTH: "256",
        HEIGHT: "256",
      });
      // The bounding box is MapLibre's to fill in, so it is not escaped.
      return `${url}${url.includes("?") ? "&" : "?"}${params.toString()}&BBOX={bbox-epsg-3857}`;
    }
  }
}

/** A reference layer's source and layer, under every data layer. */
function compileReference(ref: ReferenceLayer, index: number): CompiledMap {
  const out = emptyCompiled();
  const id = `fd-ref-${ref.id ? ref.id.replace(/[^A-Za-z0-9_-]/g, "_") : index}`;
  out.sources[id] = {
    type: "raster",
    tiles: [referenceTiles(ref)],
    tileSize: 256,
    ...(ref.attribution ? { attribution: ref.attribution } : {}),
  };
  if (isVisible(ref)) {
    out.layers.push({
      id: `${id}-raster`,
      type: "raster",
      source: id,
      paint: { "raster-opacity": opacityOf(ref) },
    } as unknown as LayerSpecification);
  }
  return out;
}

/** A whole map, compiled: its reference layers, then its layers' sources
 * and layers, bottom first; the selection over everything. */
export function compileMap(spec: MapSpec, data: MapData, opts: CompileOptions): CompiledMap {
  const out = emptyCompiled();
  const merge = (one: CompiledMap) => {
    Object.assign(out.sources, one.sources);
    Object.assign(out.layerOf, one.layerOf);
    out.layers.push(...one.layers);
    for (const s of one.images) if (!out.images.includes(s)) out.images.push(s);
    out.interactive.push(...one.interactive);
    out.legend.push(...one.legend);
    out.notes.push(...one.notes);
  };
  (spec.reference ?? []).forEach((ref, i) => merge(compileReference(ref, i)));
  spec.layers.forEach((layer, i) => {
    const rendered = data.layers[i];
    if (!rendered) return;
    merge(compileLayer(i, layer, rendered, opts));
  });
  // The rings of the selected features last, so no layer hides them.
  const selected = out.layers.filter((l) => l.id.includes("-selected-"));
  out.layers = [...out.layers.filter((l) => !l.id.includes("-selected-")), ...selected];
  return out;
}

/** The fonts a base map's style draws its own labels in: the first
 * `text-font` of its layers that is a plain list. None when it has no glyphs. */
export function styleFont(style: { glyphs?: unknown; layers?: unknown[] } | null | undefined): string[] | null {
  if (!style || typeof style.glyphs !== "string") return null;
  for (const layer of style.layers ?? []) {
    let font = (layer as { layout?: Record<string, unknown> }).layout?.["text-font"];
    if (Array.isArray(font) && font[0] === "literal") font = font[1];
    // A list of names, not an expression (`["get", "font"]`): operators are
    // lower-case words, font names are not.
    if (
      Array.isArray(font) &&
      font.length > 0 &&
      font.every((f) => typeof f === "string") &&
      !/^[a-z][a-z-]*$/.test(font[0] as string)
    ) {
      return font as string[];
    }
  }
  return ["Noto Sans Regular"];
}

/** A feature's row as its tooltip shows it: each property but the row key,
 * in the order the layer carries them. */
export function tooltipRows(
  properties: Record<string, unknown>,
  columns: { name: string }[],
  missing: string,
): [string, string][] {
  const names = columns.length > 0 ? columns.map((c) => c.name) : Object.keys(properties);
  return names.filter((n) => n !== "_fd_key").map((n) => [n, labelOf(properties[n], missing)]);
}
