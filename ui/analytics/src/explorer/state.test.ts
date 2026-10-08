import { describe, expect, it } from "vitest";

import type { StageShape } from "../datasets/ops";
import type { PlotSpec } from "../plot/spec";
import {
  addLayer,
  addReference,
  clear,
  composeSpec,
  drop,
  mapAssignment,
  noExtras,
  pickDataset,
  pickGeometry,
  pickMark,
  readState,
  remove,
  removeLayer,
  setScale,
  showMap,
  tableSpecOf,
  toggleBin,
  updateLayer,
  type ExplorerState,
  type Extras,
} from "./state";

const empty = (): ExplorerState => readState({ dataset: "d1" });

const houses: StageShape = {
  columns: [
    { name: "id", type: "int" },
    { name: "price", type: "float" },
    { name: "area", type: "float" },
    { name: "neighbourhood", type: "int", key: { table: "neighbourhoods", field: "id" } },
    { name: "year_built", type: "int" },
    { name: "sold", type: "bool" },
  ],
  grain: { kind: "table", table: "houses", key: "id" },
};

describe("the drop zones", () => {
  it("take one column each, and several on Y when added", () => {
    let s = drop(empty(), "x", "area");
    s = drop(s, "y", "bedrooms");
    // A drop replaces what is on Y…
    s = drop(s, "y", "price");
    expect(s.assignment.y).toEqual([{ field: "price" }]);
    // …and an added one goes beside it, once.
    s = drop(s, "y", "area", true);
    s = drop(s, "y", "price", true);
    s = drop(s, "color", "neighbourhood");
    s = drop(s, "x", "year_built");
    expect(s.assignment).toEqual({
      x: { field: "year_built" },
      y: [{ field: "price" }, { field: "area" }],
      color: { field: "neighbourhood" },
    });
    s = remove(s, "y", "price");
    s = remove(s, "color", "neighbourhood");
    expect(s.assignment).toEqual({ x: { field: "year_built" }, y: [{ field: "area" }] });
    s = remove(s, "y", "area");
    expect(s.assignment.y).toBeUndefined();
  });

  it("bin a column and stop binning it", () => {
    let s = drop(empty(), "wrap", "year_built");
    s = toggleBin(s, "wrap", "year_built");
    expect(s.assignment.wrap).toEqual({ field: "year_built", bin: {} });
    s = toggleBin(s, "wrap", "year_built");
    expect(s.assignment.wrap).toEqual({ field: "year_built" });
  });

  it("start again when the dataset changes, or on Clear, and the mark palette ends a reshaping preset", () => {
    let s = drop(empty(), "x", "area");
    s = { ...s, mark: "line", preset: "splom", extras: addLayer(noExtras(), "linear") };
    expect(pickDataset(s, "d1")).toBe(s);
    const other = pickDataset(s, "d2");
    expect(other).toMatchObject({ dataset: "d2", assignment: {}, mark: undefined, preset: undefined });
    expect(other.extras.layers).toEqual([]);
    expect(clear(s).dataset).toBe("d1");
    expect(pickMark(s, "point")).toMatchObject({ mark: "point", preset: undefined });
  });
});

describe("readState", () => {
  it("reads what a workspace stored, and defaults the rest", () => {
    expect(readState(null)).toEqual({
      dataset: undefined,
      assignment: {},
      mark: undefined,
      preset: undefined,
      view: "plot",
      map: {},
      table: { function: "mean", totals: true },
      extras: noExtras(),
      tests: { show: true, paired: false, mu: 0 },
    });
    const s = readState({
      dataset: "d1",
      assignment: { x: { field: "area" }, y: [{ field: "price" }, "junk"], color: 3 },
      view: "table",
      table: { function: "median", totals: false },
      extras: { layers: [{ mark: "line" }], references: [{ channel: "y", value: 1 }] },
    });
    expect(s.assignment).toEqual({ x: { field: "area" }, y: [{ field: "price" }] });
    expect(s.view).toBe("table");
    expect(s.table).toEqual({ function: "median", totals: false });
    expect(s.extras.layers).toEqual([{ mark: "line" }]);
  });
});

describe("tableSpecOf", () => {
  it("reads the same drop zones as a table: X as rows, Color as columns, numbers on Y as cells", () => {
    let s = drop(empty(), "x", "neighbourhood");
    s = drop(s, "y", "price");
    expect(tableSpecOf(s, houses)).toEqual({
      data: { kind: "dataset", dataset: "d1" },
      rows: [{ field: "neighbourhood" }],
      columns: [],
      cells: [{ field: "price", function: "mean" }],
      totals: true,
    });
    s = drop(s, "color", "area");
    s = drop(s, "y", "sold", true);
    s = { ...s, table: { function: "median", totals: false } };
    const spec = tableSpecOf(s, houses);
    // A float as a column is binned; a category on Y is another column.
    expect(spec?.columns).toEqual([{ field: "area", bin: {} }, { field: "sold" }]);
    expect(spec?.cells).toEqual([{ field: "price", function: "median" }]);
    expect(spec?.totals).toBe(false);
    expect(tableSpecOf(readState({}), houses)).toBeNull();
  });
});

describe("the layers panel", () => {
  const scatter: PlotSpec = {
    data: { kind: "dataset", dataset: "d1" },
    layers: [
      {
        mark: "point",
        encoding: { x: { field: "area" }, y: { field: "price" }, color: { field: "neighbourhood" } },
      },
    ],
  };

  it("adds layers that take their columns from the first", () => {
    let e = addLayer(noExtras(), "linear");
    e = addLayer(e, "density");
    e = addLayer(e, "nonsense");
    const spec = composeSpec(scatter, e);
    expect(spec.layers.slice(1)).toEqual([
      {
        mark: "line",
        stat: { kind: "smooth", method: "linear" },
        encoding: { x: { field: "area" }, y: { field: "price" }, color: { field: "neighbourhood" } },
      },
      // A density makes its own Y.
      {
        mark: "line",
        stat: { kind: "density" },
        encoding: { x: { field: "area" }, color: { field: "neighbourhood" } },
      },
    ]);
    // One smoother for all the points: Color not taken.
    e = updateLayer(removeLayer(e, 1), 0, { encoding: { color: null } });
    expect(composeSpec(scatter, e).layers[1].encoding).toEqual({ x: { field: "area" }, y: { field: "price" } });
  });

  it("does not bin what a stat reads as values", () => {
    const histogram: PlotSpec = {
      data: { kind: "dataset", dataset: "d1" },
      layers: [{ mark: "bar", stat: { kind: "count" }, encoding: { x: { field: "price", bin: {} } } }],
    };
    const spec = composeSpec(histogram, addLayer(noExtras(), "density"));
    expect(spec.layers[1].encoding.x).toEqual({ field: "price" });
  });

  it("sets the first layer's stat, scales, reference lines and coordinates", () => {
    let e: Extras = { ...noExtras(), stat: { kind: "summary" }, coord: "flipped" };
    e = setScale(e, "y", { kind: "log" });
    e = addReference(e, "y", " 100000 ", "target");
    e = addReference(e, "x", "North");
    e = addReference(e, "x", "  ");
    const spec = composeSpec(scatter, e);
    expect(spec.layers[0].stat).toEqual({ kind: "summary" });
    expect(spec.scales).toEqual({ y: { kind: "log" } });
    expect(spec.references).toEqual([
      { channel: "y", value: 100000, label: "target" },
      { channel: "x", value: "North" },
    ]);
    expect(spec.coord).toBe("flipped");
    expect(setScale(e, "y", undefined).scales).toEqual({});
    // Nothing set: the explorer's spec as it was.
    expect(composeSpec(scatter, noExtras())).toEqual(scatter);
  });
});

describe("the map view (A5.7)", () => {
  it("is read back with its geometry source, a malformed one dropped", () => {
    const s = readState({ dataset: "d1", view: "map", map: { geometry: { kind: "lon_lat", longitude: "lon", latitude: "lat" } } });
    expect(s.view).toBe("map");
    expect(s.map.geometry).toEqual({ kind: "lon_lat", longitude: "lon", latitude: "lat" });
    expect(readState({ view: "map", map: { geometry: { kind: "lon_lat", longitude: "lon" } } }).map).toEqual({});
    expect(readState({ view: "atlas" }).view).toBe("plot");
  });

  it("keeps the drop zones, and a reshaping preset gives way", () => {
    const before = { ...drop(drop(empty(), "x", "area"), "color", "kind"), preset: "splom" };
    const s = showMap(before);
    expect(s.view).toBe("map");
    expect(s.preset).toBeUndefined();
    expect(s.assignment).toEqual(before.assignment);
  });

  it("is suggested from Color, Size, Shape and Label only", () => {
    const s = drop(drop(drop(drop(empty(), "x", "area"), "y", "price"), "color", "kind"), "label", "name");
    expect(mapAssignment(s.assignment)).toEqual({ color: { field: "kind" }, label: { field: "name" } });
  });

  it("forgets the geometry with the dataset, and can go back to automatic", () => {
    const source = { kind: "column" as const, column: "location" };
    const s = pickGeometry(empty(), source);
    expect(s.map).toEqual({ geometry: source });
    expect(pickGeometry(s, undefined).map).toEqual({});
    expect(pickDataset(s, "d2").map).toEqual({});
  });
});
