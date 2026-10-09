import { describe, expect, it } from "vitest";

import { chartPalette } from "../plot/palette";
import {
  FILL_OPACITY,
  HEATMAP_OPACITY,
  PROPORTIONAL_OPACITY,
  classColors,
  MAX_RADIUS,
  MIN_RADIUS,
  POINT_RADIUS,
  absoluteTiles,
  compileMap,
  sourceBounds,
  styleFont,
  tooltipRows,
  type CompileOptions,
} from "./maplibre";
import { shapeImageName } from "./shapes";
import { mapBounds, sameSource, type MapData, type MapLayer, type MapSpec, type RenderedLayer } from "./spec";

const opts: CompileOptions = {
  theme: "light",
  origin: "https://feldspar.example",
  font: ["Noto Sans Regular"],
  missing: "(missing)",
};
const light = chartPalette("light");

const points: RenderedLayer = {
  data: {
    delivery: "geojson",
    count: 2,
    bounds: [0.01, 51.5, 0.02, 51.51],
    geometry: ["point"],
    properties: [
      { name: "kind", type: "text" },
      { name: "count", type: "int" },
    ],
    keyed: true,
    data: {
      type: "FeatureCollection",
      features: [
        { type: "Feature", id: 1, geometry: { type: "Point", coordinates: [0.01, 51.5] }, properties: { kind: "a", count: 2 } },
        { type: "Feature", id: 2, geometry: { type: "Point", coordinates: [0.02, 51.51] }, properties: { kind: "b", count: 8 } },
      ],
    },
  },
  domains: {},
};

const layerOn = (encoding: MapLayer["encoding"] = {}): MapLayer => ({
  dataset: "d1",
  geometry: { kind: "lon_lat", longitude: "lon", latitude: "lat" },
  encoding,
});

function compile(layer: MapLayer, rendered: RenderedLayer, o: CompileOptions = opts) {
  const spec: MapSpec = { layers: [layer] };
  return compileMap(spec, { layers: [rendered] }, o);
}

function paint(compiled: ReturnType<typeof compile>, id: string): Record<string, unknown> {
  const layer = compiled.layers.find((l) => l.id === id) as unknown as { paint?: Record<string, unknown> };
  expect(layer, id).toBeDefined();
  return layer.paint ?? {};
}

function layout(compiled: ReturnType<typeof compile>, id: string): Record<string, unknown> {
  const layer = compiled.layers.find((l) => l.id === id) as unknown as { layout?: Record<string, unknown> };
  return layer.layout ?? {};
}

describe("compileMap", () => {
  it("draws points as circles over a GeoJSON source, in the first colour", () => {
    const out = compile(layerOn(), points);
    expect(out.sources["fd-0"]).toEqual({ type: "geojson", data: (points.data as { data: unknown }).data });
    expect(out.layers.map((l) => [l.id, l.type])).toEqual([["fd-0-point", "circle"]]);
    const p = paint(out, "fd-0-point");
    expect(p["circle-color"]).toBe(light.categorical[0]);
    expect(p["circle-radius"]).toBe(POINT_RADIUS);
    expect(out.interactive).toEqual(["fd-0-point"]);
    expect(out.legend).toEqual([]);
    expect(out.images).toEqual([]);
  });

  it("colours a category as a plot does, value by value", () => {
    const out = compile(layerOn({ color: { field: "kind" } }), {
      ...points,
      domains: { color: { kind: "discrete", values: ["a", "b"] } },
    });
    expect(paint(out, "fd-0-point")["circle-color"]).toEqual([
      "match",
      ["to-string", ["get", "kind"]],
      "a",
      light.categorical[0],
      "b",
      light.categorical[1],
      light.other,
    ]);
    expect(out.legend).toEqual([
      {
        layer: 0,
        channel: "color",
        field: "kind",
        items: [
          { label: "a", color: light.categorical[0] },
          { label: "b", color: light.categorical[1] },
        ],
      },
    ]);
  });

  it("matches numbers and flags as text, and says when values outrun the palette", () => {
    const values = Array.from({ length: 10 }, (_, i) => i + 1);
    const out = compile(layerOn({ color: { field: "count" } }), {
      ...points,
      domains: { color: { kind: "discrete", values } },
    });
    const expr = paint(out, "fd-0-point")["circle-color"] as unknown[];
    expect(expr.slice(0, 4)).toEqual(["match", ["to-string", ["get", "count"]], "1", light.categorical[0]]);
    // The ninth value and on are drawn in the muted ink.
    expect(expr[2 + 2 * 8 + 1]).toBe(light.other);
    expect(out.notes).toContainEqual({ kind: "many-values", field: "count", count: 10 });
    const items = out.legend[0].items ?? [];
    expect(items[items.length - 1]).toEqual({ label: "…", color: light.other });
  });

  it("ramps a number from its smallest value to its largest, missing in grey", () => {
    const out = compile(layerOn({ color: { field: "count" } }), {
      ...points,
      domains: { color: { kind: "continuous", min: 2, max: 8 } },
    });
    const expr = paint(out, "fd-0-point")["circle-color"] as unknown[];
    expect(expr[0]).toBe("case");
    expect(expr[1]).toEqual(["!=", ["get", "count"], null]);
    const ramp = expr[2] as unknown[];
    expect(ramp.slice(0, 3)).toEqual(["interpolate", ["linear"], ["to-number", ["get", "count"], 0]]);
    expect(ramp[3]).toBe(2);
    expect(ramp[4]).toBe(light.sequential[0]);
    expect(ramp[ramp.length - 2]).toBe(8);
    expect(ramp[ramp.length - 1]).toBe(light.sequential[light.sequential.length - 1]);
    expect(expr[3]).toBe(light.muted);
    expect(out.legend[0].gradient).toEqual({ colors: light.sequential, min: "2", max: "8" });
  });

  it("sizes points by area from zero, within the radii", () => {
    const out = compile(layerOn({ size: { field: "count" } }), {
      ...points,
      domains: { size: { kind: "continuous", min: 2, max: 8 } },
    });
    expect(paint(out, "fd-0-point")["circle-radius"]).toEqual([
      "case",
      ["!=", ["get", "count"], null],
      ["max", MIN_RADIUS, ["*", MAX_RADIUS, ["sqrt", ["/", ["max", 0, ["to-number", ["get", "count"], 0]], 8]]]],
      MIN_RADIUS,
    ]);
    const sizes = out.legend[0].sizes;
    expect(sizes?.min).toBe("2");
    expect(sizes?.maxPx).toBe(2 * MAX_RADIUS);
    // √(2/8) of the largest radius.
    expect(sizes?.minPx).toBeCloseTo(2 * MAX_RADIUS * 0.5);
  });

  it("draws a Shape as SDF symbols, coloured and sized like circles", () => {
    const out = compile(layerOn({ shape: { field: "kind" }, color: { field: "kind" } }), {
      ...points,
      domains: {
        shape: { kind: "discrete", values: ["a", "b"] },
        color: { kind: "discrete", values: ["a", "b"] },
      },
    });
    const symbol = out.layers.find((l) => l.id === "fd-0-point");
    expect(symbol?.type).toBe("symbol");
    expect(layout(out, "fd-0-point")["icon-image"]).toEqual([
      "match",
      ["to-string", ["get", "kind"]],
      "a",
      shapeImageName("circle"),
      "b",
      shapeImageName("square"),
      shapeImageName("circle"),
    ]);
    expect(layout(out, "fd-0-point")["icon-allow-overlap"]).toBe(true);
    // Coloured by Color as a circle would be.
    expect((paint(out, "fd-0-point")["icon-color"] as unknown[]).slice(0, 4)).toEqual([
      "match",
      ["to-string", ["get", "kind"]],
      "a",
      light.categorical[0],
    ]);
    expect(out.images.sort()).toEqual(["circle", "square"]);
    const shapes = out.legend.find((e) => e.channel === "shape");
    expect(shapes?.items?.map((i) => i.shape)).toEqual(["circle", "square"]);
  });

  it("fills polygons with outlines, and says Size and Shape are not a region's", () => {
    const regions: RenderedLayer = {
      data: { ...(points.data as Extract<RenderedLayer["data"], { delivery: "geojson" }>), geometry: ["polygon"] },
      domains: { color: { kind: "discrete", values: ["a", "b"] } },
    };
    const out = compile(layerOn({ color: { field: "kind" }, size: { field: "count" }, shape: { field: "kind" } }), regions);
    expect(out.layers.map((l) => [l.id, l.type])).toEqual([
      ["fd-0-fill", "fill"],
      ["fd-0-outline", "line"],
    ]);
    expect(paint(out, "fd-0-fill")["fill-opacity"]).toBe(FILL_OPACITY);
    expect((out.layers[0] as unknown as { filter: unknown }).filter).toEqual([
      "match",
      ["geometry-type"],
      ["Polygon", "MultiPolygon"],
      true,
      false,
    ]);
    // The outline is not what a pointer picks: the fill under it is.
    expect(out.interactive).toEqual(["fd-0-fill"]);
    expect(out.notes).toEqual([
      { kind: "size-on-polygons", field: "count" },
      { kind: "shape-not-points", field: "kind" },
    ]);
  });

  it("fetches a large layer's tiles from this origin, every kind of geometry when it says none", () => {
    const tiles: RenderedLayer = {
      data: {
        delivery: "tiles",
        count: 9000,
        vertices: 9000,
        bounds: [0.01, 51.5, 0.01, 51.5],
        geometry: [],
        properties: [],
        source_layer: "features",
        keyed: true,
        tiles: "/api/layers/tiles/{z}/{x}/{y}?layer=%7B%7D",
      },
      domains: {},
    };
    const out = compile(layerOn(), tiles);
    expect(out.sources["fd-0"]).toEqual({
      type: "vector",
      tiles: ["https://feldspar.example/api/layers/tiles/{z}/{x}/{y}?layer=%7B%7D"],
      maxzoom: 16,
      bounds: sourceBounds([0.01, 51.5, 0.01, 51.5]),
    });
    expect(out.layers.map((l) => l.id)).toEqual(["fd-0-fill", "fd-0-outline", "fd-0-line", "fd-0-point"]);
    for (const l of out.layers) expect((l as unknown as Record<string, unknown>)["source-layer"]).toBe("features");
  });

  it("labels with the base map's fonts, numbers formatted, or says it cannot", () => {
    const out = compile(layerOn({ label: { field: "count" } }), points);
    const label = layout(out, "fd-0-label");
    expect(label["text-field"]).toEqual(["number-format", ["to-number", ["get", "count"], 0], { "max-fraction-digits": 2 }]);
    expect(label["text-font"]).toEqual(["Noto Sans Regular"]);
    expect(label["text-anchor"]).toBe("top");
    const text = layout(compile(layerOn({ label: { field: "kind" } }), points), "fd-0-label");
    expect(text["text-field"]).toEqual(["to-string", ["get", "kind"]]);
    const fontless = compile(layerOn({ label: { field: "kind" } }), points, { ...opts, font: null });
    expect(fontless.layers.some((l) => l.id === "fd-0-label")).toBe(false);
    expect(fontless.notes).toEqual([{ kind: "labels-need-fonts", field: "kind" }]);
  });

  it("stacks layers bottom first and passes a refusal on as a note", () => {
    const spec: MapSpec = { layers: [layerOn(), layerOn()] };
    const data: MapData = { layers: [{ data: { delivery: "none", error: "the layer's geometry does not work" }, domains: {} }, points] };
    const out = compileMap(spec, data, opts);
    expect(Object.keys(out.sources)).toEqual(["fd-1"]);
    expect(out.layers.map((l) => l.id)).toEqual(["fd-1-point"]);
    expect(out.notes).toEqual([{ kind: "refused", layer: 0, error: "the layer's geometry does not work" }]);
  });

  it("draws in the dark palette on a dark page", () => {
    const out = compile(layerOn(), points, { ...opts, theme: "dark" });
    expect(paint(out, "fd-0-point")["circle-color"]).toBe(chartPalette("dark").categorical[0]);
    expect(paint(out, "fd-0-point")["circle-stroke-color"]).toBe(chartPalette("dark").surface);
  });
});

describe("the Map workspace's styles", () => {
  const counted = (extra: Partial<RenderedLayer> = {}): RenderedLayer => ({ ...points, ...extra });

  it("names a workspace layer's source by its id, and says which layer each source draws", () => {
    const spec: MapSpec = { layers: [{ ...layerOn(), id: "inc:1" }, { ...layerOn(), id: "b" }] };
    const out = compileMap(spec, { layers: [points, points] }, opts);
    expect(Object.keys(out.sources)).toEqual(["fd-inc_1", "fd-b"]);
    expect(out.layerOf).toEqual({ "fd-inc_1": 0, "fd-b": 1 });
    expect(out.layers.map((l) => l.id)).toEqual(["fd-inc_1-point", "fd-b-point"]);
  });

  it("draws one colour, whatever is on Color", () => {
    const out = compile({ ...layerOn({ color: { field: "kind" } }), style: { kind: "single", color: "#aa3300" } }, counted({
      domains: { color: { kind: "discrete", values: ["a", "b"] } },
    }));
    expect(paint(out, "fd-0-point")["circle-color"]).toBe("#aa3300");
    expect(out.legend).toEqual([]);
  });

  it("colours graduated classes by steps over the server's breaks", () => {
    const out = compile(
      { ...layerOn({ color: { field: "count" } }), style: { kind: "graduated", method: "quantile", classes: 3 } },
      counted({ classes: [2, 4, 6, 8] }),
    );
    const colors = classColors(light, 3);
    expect(colors).toEqual([light.sequential[0], light.sequential[3], light.sequential[6]]);
    expect(paint(out, "fd-0-point")["circle-color"]).toEqual([
      "case",
      ["!=", ["get", "count"], null],
      ["step", ["to-number", ["get", "count"], 0], colors[0], 4, colors[1], 6, colors[2]],
      light.muted,
    ]);
    expect(out.legend[0].items).toEqual([
      { label: "2 – 4", color: colors[0] },
      { label: "4 – 6", color: colors[1] },
      { label: "6 – 8", color: colors[2] },
    ]);
    // A last class of the largest value alone is labelled with it.
    const top = compile(
      { ...layerOn({ color: { field: "count" } }), style: { kind: "graduated", method: "natural_breaks", classes: 3 } },
      counted({ classes: [1, 2, 3, 3] }),
    );
    expect(top.legend[0].items?.map((i) => i.label)).toEqual(["1 – 2", "2 – 3", "3"]);
  });

  it("draws proportional circles, the small ones on top, and a heatmap weighted by Size", () => {
    const proportional = compile(
      { ...layerOn({ size: { field: "count" } }), style: { kind: "proportional" } },
      counted({ domains: { size: { kind: "continuous", min: 2, max: 8 } } }),
    );
    expect(layout(proportional, "fd-0-point")["circle-sort-key"]).toEqual(["-", 0, ["to-number", ["get", "count"], 0]]);
    expect(paint(proportional, "fd-0-point")["circle-opacity"]).toBeCloseTo(PROPORTIONAL_OPACITY);

    const heat = compile(
      { ...layerOn({ size: { field: "count" }, color: { field: "kind" } }), style: { kind: "heatmap", radius: 30 } },
      counted({ domains: { size: { kind: "continuous", min: 2, max: 8 } } }),
      { ...opts, words: { density: "Dichte", low: "wenig", high: "viel" } },
    );
    expect(heat.layers.map((l) => [l.id, l.type])).toEqual([["fd-0-heat", "heatmap"]]);
    const p = paint(heat, "fd-0-heat");
    expect(p["heatmap-weight"]).toEqual(["interpolate", ["linear"], ["to-number", ["get", "count"], 0], 2, 0, 8, 1]);
    expect(p["heatmap-radius"]).toBe(30);
    expect(p["heatmap-opacity"]).toBeCloseTo(HEATMAP_OPACITY);
    // Nothing a pointer picks; the legend is the density.
    expect(heat.interactive).toEqual([]);
    expect(heat.legend).toEqual([
      { layer: 0, channel: "color", field: "count", gradient: { colors: light.sequential, min: "wenig", max: "viel" } },
    ]);
  });

  it("fades a layer by its opacity, hides it but keeps its source, and leaves it out of the legend", () => {
    const faded = compile({ ...layerOn({ color: { field: "kind" } }), opacity: 0.5, name: "Incidents" }, counted({
      domains: { color: { kind: "discrete", values: ["a"] } },
    }));
    expect(paint(faded, "fd-0-point")["circle-opacity"]).toBeCloseTo(0.45);
    expect(paint(faded, "fd-0-point")["circle-stroke-opacity"]).toBe(0.5);
    expect(faded.legend[0].title).toBe("Incidents");
    const hidden = compile({ ...layerOn(), visible: false }, points);
    expect(Object.keys(hidden.sources)).toEqual(["fd-0"]);
    expect(hidden.layers).toEqual([]);
    const quiet = compile({ ...layerOn({ color: { field: "kind" } }), legend: false }, counted({
      domains: { color: { kind: "discrete", values: ["a"] } },
    }));
    expect(quiet.legend).toEqual([]);
  });

  it("rings the selected features over every layer, matched by id", () => {
    const spec: MapSpec = { layers: [layerOn(), layerOn()] };
    const out = compileMap(spec, { layers: [points, points] }, { ...opts, selection: { layer: 0, ids: [2, { no: 1 }] } });
    expect(out.layers.map((l) => l.id)).toEqual(["fd-0-point", "fd-1-point", "fd-0-selected-point"]);
    const ring = out.layers[2] as unknown as { filter: unknown; paint: Record<string, unknown> };
    expect(ring.filter).toEqual(["all", ["match", ["geometry-type"], ["Point", "MultiPoint"], true, false], ["in", ["id"], ["literal", [2]]]]);
    expect(ring.paint["circle-stroke-color"]).toBe(light.text);
    // Nothing selected, no ring.
    expect(compileMap(spec, { layers: [points, points] }, { ...opts, selection: { layer: 0, ids: [] } }).layers).toHaveLength(2);
  });

  it("draws reference layers under every data layer, as raster tiles", () => {
    const spec: MapSpec = {
      layers: [layerOn()],
      reference: [
        { id: "osm", name: "OSM", kind: "tiles", url: "https://tile.openstreetmap.org/{z}/{x}/{y}.png", opacity: 0.4, attribution: "© OSM" },
        { id: "off", name: "Off", kind: "arcgis", url: "https://g.example/MapServer", visible: false },
      ],
    };
    const out = compileMap(spec, { layers: [points] }, opts);
    expect(out.layers.map((l) => l.id)).toEqual(["fd-ref-osm-raster", "fd-0-point"]);
    expect(out.sources["fd-ref-osm"]).toEqual({
      type: "raster",
      tiles: ["https://tile.openstreetmap.org/{z}/{x}/{y}.png"],
      tileSize: 256,
      attribution: "© OSM",
    });
    expect(paint(out, "fd-ref-osm-raster")["raster-opacity"]).toBe(0.4);
    // A hidden one keeps its source and draws nothing.
    expect(out.sources["fd-ref-off"]).toBeDefined();
  });
});

describe("helpers", () => {
  it("makes tile URLs absolute and source bounds valid", () => {
    expect(absoluteTiles("/api/x", "https://a.example/")).toBe("https://a.example/api/x");
    expect(absoluteTiles("https://b.example/t", "https://a.example")).toBe("https://b.example/t");
    expect(sourceBounds([-180, -90, 180, 90])).toEqual([-180, -85.0511, 180, 85.0511]);
  });

  it("takes the fonts from the base map's own labels", () => {
    expect(styleFont(null)).toBeNull();
    expect(styleFont({ layers: [] })).toBeNull();
    expect(
      styleFont({
        glyphs: "https://g/{fontstack}/{range}.pbf",
        layers: [
          { layout: { "text-font": ["get", "x"] } },
          { layout: { "text-font": ["literal", ["Open Sans Bold"]] } },
        ],
      }),
    ).toEqual(["Open Sans Bold"]);
    expect(styleFont({ glyphs: "https://g", layers: [] })).toEqual(["Noto Sans Regular"]);
  });

  it("shows a feature's row in the layer's column order", () => {
    expect(
      tooltipRows({ count: 2.5, kind: null, _fd_key: 4 }, [{ name: "kind" }, { name: "count" }], "(missing)"),
    ).toEqual([
      ["kind", "(missing)"],
      ["count", "2.5"],
    ]);
  });

  it("bounds every drawn layer, and compares sources by value", () => {
    const geojson = points.data as Extract<RenderedLayer["data"], { delivery: "geojson" }>;
    const data: MapData = {
      layers: [points, { data: { ...geojson, bounds: [-1, 50, 0, 51] }, domains: {} }],
    };
    expect(mapBounds(data)).toEqual([-1, 50, 0.02, 51.51]);
    expect(mapBounds({ layers: [] })).toBeNull();
    expect(sameSource({ kind: "column", column: "a" }, { kind: "column", column: "a" })).toBe(true);
    expect(sameSource({ kind: "column", column: "a" }, undefined)).toBe(false);
  });
});
