import { describe, expect, it } from "vitest";

import type { MapLayer } from "./spec";
import { layerRequest } from "./spec";
import {
  addLayer,
  addReference,
  clickFeature,
  dataKey,
  hostAllowed,
  moveLayer,
  originOf,
  placeLayer,
  readMapState,
  removeLayer,
  selectFound,
  selectRange,
  setChannel,
  sortBy,
  sortOf,
  setStyle,
  sourceOptions,
  specOf,
  stateFromLayer,
  updateLayer,
  type MapState,
} from "./workspace";

const at = { kind: "column", column: "at" } as const;
const layer = (id: string, extra: Partial<MapLayer> = {}): MapLayer => ({ id, name: id, dataset: `d-${id}`, geometry: at, ...extra });
const empty: MapState = { layers: [], reference: [], selection: null, active: null, table: { open: false } };
const columns = [
  { name: "id", type: "int" },
  { name: "district", type: "int", key: { table: "districts", field: "id" } },
  { name: "kind", type: "text" },
  { name: "count", type: "int" },
  { name: "at", type: "geometry" },
];

describe("the Map workspace's state", () => {
  it("reads what was stored, leaving out what is not a part of it", () => {
    const s = readMapState({
      layers: [layer("a"), { dataset: "d", geometry: at }, { nonsense: true }],
      reference: [{ id: "r", name: "OSM", kind: "tiles", url: "https://t/{z}/{x}/{y}.png" }, { id: 3 }],
      selection: { layer: "gone", ids: [1] },
      active: "gone",
      table: { open: true, sort: { layer: "a", column: "kind", descending: true } },
      view: { center: [0, 51], zoom: 9 },
    });
    expect(s.layers).toHaveLength(2);
    expect(s.layers[1].id).toMatch(/^[0-9a-f]{8}$/);
    expect(s.reference).toHaveLength(1);
    // A selection of a layer that is not there is none; the picked layer is the top one.
    expect(s.selection).toBeNull();
    expect(s.active).toBe(s.layers[1].id);
    expect(s.table).toEqual({ open: true, sort: { layer: "a", column: "kind", descending: true } });
    expect(s.view).toEqual({ center: [0, 51], zoom: 9 });
    expect(readMapState(null)).toEqual(empty);
  });

  it("adds, moves, places and removes layers by id", () => {
    let s = addLayer(empty, layer("a"));
    s = addLayer(s, { dataset: "d-b", geometry: at }, "Bees");
    s = addLayer(s, layer("c"));
    const [a, b, c] = s.layers.map((l) => l.id as string);
    expect(s.layers[1].name).toBe("Bees");
    expect(s.active).toBe(c);
    // An id already on the map is not taken twice.
    expect(addLayer(s, layer("a")).layers[3].id).not.toBe("a");
    expect(moveLayer(s, a, 1).layers.map((l) => l.id)).toEqual([b, a, c]);
    expect(moveLayer(s, c, 5).layers.map((l) => l.id)).toEqual([a, b, c]);
    expect(placeLayer(s, c, a).layers.map((l) => l.id)).toEqual([c, a, b]);
    s = selectFound(s, b, [1, 2]);
    const gone = removeLayer(s, b);
    expect(gone.layers.map((l) => l.id)).toEqual([a, c]);
    expect(gone.selection).toBeNull();
    expect(removeLayer(s, c).active).toBe(b);
  });

  it("clears a layer's selection when what its features are changes, not when its look does", () => {
    let s = selectFound(addLayer(empty, layer("a")), "a", [4], "kind == \"x\"");
    expect(s.selection).toEqual({ layer: "a", ids: [4], condition: "kind == \"x\"" });
    s = updateLayer(s, "a", (l) => ({ ...l, opacity: 0.5, name: "A" }));
    expect(s.selection?.ids).toEqual([4]);
    expect(updateLayer(s, "a", (l) => ({ ...l, filter: "count > 2" })).selection).toBeNull();
    expect(updateLayer(s, "a", (l) => ({ ...l, geometry: { kind: "lon_lat", longitude: "x", latitude: "y" } })).selection).toBeNull();
  });

  it("reads a layer's data again only for what changes its features or scales", () => {
    const l = layer("a", { encoding: { color: { field: "kind" } } });
    const key = dataKey(l);
    for (const look of [{ name: "x" }, { opacity: 0.3 }, { visible: false }, { legend: false }, { popup: ["kind"] }]) {
      expect(dataKey({ ...l, ...look })).toBe(key);
    }
    expect(dataKey({ ...l, filter: "count > 1" })).not.toBe(key);
    expect(dataKey({ ...l, style: { kind: "categories" } })).not.toBe(key);
    expect(dataKey({ ...l, filter: "  " })).toBe(key);
  });

  it("selects by click, adding and taking away with a modifier, and by range", () => {
    let s = addLayer(addLayer(empty, layer("a")), layer("b"));
    s = clickFeature(s, "a", 1, false);
    expect(s.selection).toEqual({ layer: "a", ids: [1] });
    expect(s.active).toBe("a");
    s = clickFeature(s, "a", 2, true);
    expect(s.selection?.ids).toEqual([1, 2]);
    s = clickFeature(s, "a", 1, true);
    expect(s.selection?.ids).toEqual([2]);
    // The one selected feature clicked again: nothing selected.
    expect(clickFeature(s, "a", 2, false).selection).toBeNull();
    // Another layer's feature: that layer's selection.
    expect(clickFeature(s, "b", 9, true).selection).toEqual({ layer: "b", ids: [9] });
    // A click on nothing clears, unless adding.
    expect(clickFeature(s, "a", undefined, false).selection).toBeNull();
    expect(clickFeature(s, "a", undefined, true).selection?.ids).toEqual([2]);
    expect(selectRange(s, "a", [5, 6, 7, 8], 8, 6).selection?.ids).toEqual([6, 7, 8]);
    expect(selectFound(s, "a", []).selection).toBeNull();
  });

  it("keeps the table's order for the layer it was chosen on", () => {
    let s = addLayer(addLayer(empty, layer("a")), layer("b"));
    s = sortBy(s, "a", "count");
    expect(sortOf(s, "a")).toEqual({ column: "count", descending: false });
    // Another layer has other columns: no order there.
    expect(sortOf(s, "b")).toBeUndefined();
    s = sortBy(s, "a", "count");
    expect(sortOf(s, "a")).toEqual({ column: "count", descending: true });
    expect(sortOf(sortBy(s, "a", "count"), "a")).toBeUndefined();
    expect(sortOf(sortBy(s, "b", "kind"), "a")).toBeUndefined();
    // A sort of a layer that is gone is dropped when read.
    expect(readMapState({ ...s, table: { open: true, sort: { layer: "gone", column: "x" } } }).table).toEqual({ open: true });
  });

  it("fills the channel a style needs from the columns", () => {
    const l = layer("a");
    const graduated = setStyle(l, "graduated", columns);
    expect(graduated.style).toEqual({ kind: "graduated", method: "natural_breaks", classes: 5 });
    // The first number that is not a key.
    expect(graduated.encoding?.color?.field).toBe("id");
    const kept = setStyle(setChannel(l, "color", "count"), "graduated", columns);
    expect(kept.encoding?.color?.field).toBe("count");
    // Not a number: replaced.
    expect(setStyle(setChannel(l, "color", "kind"), "graduated", columns).encoding?.color?.field).toBe("id");
    expect(setStyle(l, "categories", columns).encoding?.color?.field).toBe("district");
    expect(setStyle(l, "proportional", columns).encoding?.size?.field).toBe("id");
    expect(setStyle(l, "heatmap", columns).style).toEqual({ kind: "heatmap", radius: 20 });
    expect(setStyle(graduated, "auto", columns).style).toBeUndefined();
    expect(setChannel(setChannel(l, "label", "kind"), "label", null).encoding).toBeUndefined();
  });

  it("asks for a layer's features by its request, at their centres for points", () => {
    expect(layerRequest(layer("a", { filter: " ", style: { kind: "proportional" } }))).toEqual({
      dataset: "d-a",
      geometry: at,
      points: true,
    });
    expect(layerRequest(layer("a", { filter: "count > 2" }))).toEqual({ dataset: "d-a", geometry: at, filter: "count > 2" });
  });

  it("makes a spec of the shown layers for a panel, and a state of one layer for Open in map", () => {
    let s = addLayer(addLayer(empty, layer("a")), layer("b", { visible: false }));
    s = addReference(s, { name: "OSM", kind: "tiles", url: "https://t/{z}/{x}/{y}.png", visible: false });
    s = { ...s, view: { center: [1, 2], zoom: 3 }, selection: { layer: "a", ids: [1] } };
    expect(specOf(s, "visible")).toEqual({ layers: [s.layers[0]], view: { center: [1, 2], zoom: 3 } });
    expect(specOf(s).layers).toHaveLength(2);
    const opened = stateFromLayer({ dataset: "d", geometry: at, encoding: { color: { field: "kind" } } }, "Incidents");
    expect(opened.layers[0].name).toBe("Incidents");
    expect(opened.active).toBe(opened.layers[0].id);
  });

  it("checks a service's host against the hosts a map may load from", () => {
    expect(originOf("https://Tile.OpenStreetMap.org/{z}/{x}/{y}.png")).toBe("https://tile.openstreetmap.org");
    expect(originOf("http://localhost:8080/wms?x=1")).toBe("http://localhost:8080");
    expect(originOf("ftp://x.org/a")).toBeNull();
    expect(originOf("https://user@x.org/a")).toBeNull();
    expect(hostAllowed("https://tile.openstreetmap.org/{z}/{x}/{y}.png", ["https://tile.openstreetmap.org"])).toBe(true);
    expect(hostAllowed("https://evil.org/{z}/{x}/{y}.png", ["https://tile.openstreetmap.org"])).toBe(false);
  });

  it("offers the layer's own source first when its dataset no longer does", () => {
    const offered = [{ source: { kind: "column", column: "b" } as const, label: "`b`" }];
    expect(sourceOptions({ kind: "column", column: "b" }, offered)).toEqual(offered);
    expect(sourceOptions(at, offered).map((o) => o.label)).toEqual(["`at`", "`b`"]);
  });
});
