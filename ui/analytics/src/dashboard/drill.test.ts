import { describe, expect, it } from "vitest";

import { makePanel, type Panel } from "../panels/panel";
import type { PlotSpec } from "../plot/spec";
import { defaultChannel, drillConditions, drillDown, drilledPanel, readDrill, type Drill } from "./drill";
import { addTile, dropInto, readDashboard, setDrill } from "./layout";

const spec: PlotSpec = {
  data: { kind: "dataset", dataset: "d1" },
  layers: [{ mark: "bar", stat: { kind: "count" }, encoding: { x: { field: "district" }, color: { field: "district" } } }],
  scales: { x: { domain: [3, 1, 2] } },
};
const bars = makePanel({ kind: "plot", content: { spec } }, "Incidents");
const drill: Drill = { channel: "x", path: ["district", "category", "street"] };

function specOf(panel: Panel): PlotSpec {
  if (panel.kind !== "plot") throw new Error("not a plot");
  return panel.content.spec;
}

describe("drill paths", () => {
  it("reads a path of two to eight different columns along X, Y or Color", () => {
    expect(readDrill(drill)).toEqual(drill);
    expect(readDrill({ channel: "x", path: ["district"] })).toBeNull();
    expect(readDrill({ channel: "x", path: ["a", "a"] })).toBeNull();
    expect(readDrill({ channel: "size", path: ["a", "b"] })).toBeNull();
    expect(readDrill({ channel: "x", path: ["a", 3] })).toBeNull();
    expect(readDrill(null)).toBeNull();
  });

  it("shows the next column at each level, filtered to the values picked above", () => {
    expect(drilledPanel(bars, drill, [])).toBe(bars);
    const down = specOf(drilledPanel(bars, drill, [2]));
    expect(down.layers[0].encoding.x).toEqual({ field: "category" });
    // Colour is not the drill's channel, and keeps its column.
    expect(down.layers[0].encoding.color).toEqual({ field: "district" });
    // The first column's order is not the next one's.
    expect(down.scales?.x?.domain).toBeUndefined();
    // The panel itself is left as it was.
    expect(specOf(bars).layers[0].encoding.x).toEqual({ field: "district" });
    expect(specOf(drilledPanel(bars, drill, [2, "theft"])).layers[0].encoding.x).toEqual({ field: "street" });
    expect(drillConditions("t", "d1", drill, [2, "theft"])).toEqual([
      { id: "drill:t:0", dataset: "d1", column: "district", values: [2] },
      { id: "drill:t:1", dataset: "d1", column: "category", values: ["theft"] },
    ]);
    expect(drillConditions("t", undefined, drill, [2])).toEqual([]);
  });

  it("goes down on a click on the level's column, and selects at the last level", () => {
    expect(drillDown(drill, [], [{ field: "district", values: [2] }])).toEqual([2]);
    expect(drillDown(drill, [2], [{ field: "category", values: ["theft"] }])).toEqual([2, "theft"]);
    // The last level selects.
    expect(drillDown(drill, [2, "theft"], [{ field: "street", values: ["High St"] }])).toBeNull();
    // A click on another column, or on a range, selects too.
    expect(drillDown(drill, [], [{ field: "region", values: ["n"] }])).toBeNull();
    expect(drillDown(drill, [], [{ field: "district", range: { min: 1, max: 2 } }])).toBeNull();
  });

  it("goes along the first of X, Color and Y a plot shows a column on", () => {
    expect(defaultChannel(spec)).toBe("x");
    expect(defaultChannel({ ...spec, layers: [{ mark: "point", encoding: { color: { field: "kind" }, y: { field: "n" } } }] })).toBe("color");
  });

  it("is kept with the tile, and goes with a copy into another dashboard", () => {
    let d = addTile({ tiles: [] }, bars);
    d = setDrill(d, bars.id, drill);
    const stored = JSON.parse(JSON.stringify({ ...d, filters: [{ id: "f", dataset: "d1", values: [1] }], refresh: 60 })) as Record<
      string,
      unknown
    >;
    const read = readDashboard(stored);
    expect(read.tiles[0].drill).toEqual(drill);
    expect(read.filters).toEqual([{ id: "f", dataset: "d1", values: [1] }]);
    expect(read.refresh).toBe(60);
    expect(readDashboard({ tiles: [], refresh: 2 })).toEqual({ tiles: [] });
    expect(setDrill(d, bars.id, null).tiles[0].drill).toBeUndefined();
    const other = dropInto({ tiles: [] }, "elsewhere", { tile: { source: "here", tile: read.tiles[0] }, panel: null });
    expect(other?.tiles[0].drill).toEqual(drill);
    expect(other?.tiles[0].id).not.toBe(bars.id);
  });
});
