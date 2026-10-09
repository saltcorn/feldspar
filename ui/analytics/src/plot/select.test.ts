import { describe, expect, it } from "vitest";

import { selectionInfo } from "./echarts";
import { brushPicks, brushType, clickPicks, columnOn, selectionsOf } from "./select";
import type { LayerData, PlotData, PlotSpec } from "./spec";

const data = { kind: "dataset" as const, dataset: "d1" };
const light = { theme: "light" as const };

function layer(partial: Partial<LayerData> & Pick<LayerData, "columns" | "rows" | "mark">): LayerData {
  return { stat: "identity", sampled: false, total: partial.rows.length, truncated: false, ...partial };
}

function plot(partial: Partial<PlotData> & Pick<PlotData, "layers">): PlotData {
  return { domains: {}, facets: {}, bins: {}, warnings: [], ...partial };
}

/** A bar of the rows counted by `field`, with these values. */
function bars(field: string, values: unknown[], extra: Partial<PlotSpec> = {}): { spec: PlotSpec; drawn: PlotData } {
  const spec: PlotSpec = { data, layers: [{ mark: "bar", stat: { kind: "count" }, encoding: { x: { field } } }], ...extra };
  const drawn = plot({
    layers: [layer({ mark: "bar", stat: "count", columns: ["x", "y"], rows: values.map((v, i) => [v, i + 1]) })],
    domains: { x: { kind: "discrete", values }, y: { kind: "continuous", min: 1, max: values.length } },
  });
  return { spec, drawn };
}

describe("selections on a plot", () => {
  it("clicks a bar's category by default, and has no brush along categories", () => {
    const { spec, drawn } = bars("category", ["burglary", "theft"]);
    const info = selectionInfo(spec, drawn, light);
    expect(selectionsOf(spec, info)).toEqual([{ name: "click", kind: "point", channels: ["x"] }]);
    expect(brushType(spec, info)).toBeNull();
    expect(clickPicks(spec, info, { componentType: "series", seriesType: "bar", seriesName: "", value: ["theft", 2] })).toEqual([
      { field: "category", channel: "x", values: ["theft"] },
    ]);
    // A click on something that is not a category picks nothing.
    expect(clickPicks(spec, info, { componentType: "series", seriesType: "bar", value: ["arson", 1] })).toEqual([]);
    expect(clickPicks(spec, info, { componentType: "markLine", value: 3 })).toEqual([]);
  });

  it("reads a foreign key's label back as its id, and a missing value as null", () => {
    const { spec, drawn } = bars("district", [1, 2, null]);
    const info = selectionInfo(spec, drawn, { ...light, categorical: ["district"], missing: "(missing)" });
    expect(clickPicks(spec, info, { seriesType: "bar", value: ["2", 5] })).toEqual([{ field: "district", channel: "x", values: [2] }]);
    expect(clickPicks(spec, info, { seriesType: "bar", value: ["(missing)", 1] })).toEqual([
      { field: "district", channel: "x", values: [null] },
    ]);
  });

  it("reads X from where a flipped plot draws it", () => {
    const { spec, drawn } = bars("category", ["burglary", "theft"], { coord: "flipped" });
    const info = selectionInfo(spec, drawn, light);
    expect(clickPicks(spec, info, { seriesType: "bar", value: [4, "burglary"] })).toEqual([
      { field: "category", channel: "x", values: ["burglary"] },
    ]);
  });

  it("picks a colour group by its series' name, with the category", () => {
    const spec: PlotSpec = {
      data,
      layers: [{ mark: "bar", stat: { kind: "count" }, encoding: { x: { field: "category" }, color: { field: "region" } } }],
    };
    const drawn = plot({
      layers: [
        layer({
          mark: "bar",
          stat: "count",
          columns: ["x", "color", "y"],
          rows: [
            ["burglary", "north", 3],
            ["burglary", "south", 1],
          ],
        }),
      ],
      domains: { x: { kind: "discrete", values: ["burglary"] }, color: { kind: "discrete", values: ["north", "south"] } },
    });
    const info = selectionInfo(spec, drawn, light);
    expect(selectionsOf(spec, info)[0].channels).toEqual(["x", "color"]);
    expect(clickPicks(spec, info, { seriesType: "bar", seriesName: "south", value: ["burglary", 1] })).toEqual([
      { field: "category", channel: "x", values: ["burglary"] },
      { field: "region", channel: "color", values: ["south"] },
    ]);
  });

  it("clicks a histogram's bin as its range, and brushes along it", () => {
    const spec: PlotSpec = { data, layers: [{ mark: "bar", stat: { kind: "count" }, encoding: { x: { field: "price", bin: {} } } }] };
    const drawn = plot({
      layers: [
        layer({
          mark: "bar",
          stat: "count",
          columns: ["x", "x_end", "y"],
          rows: [
            [0, 200, 3],
            [200, 400, 5],
          ],
        }),
      ],
      domains: { x: { kind: "continuous", min: 0, max: 400 } },
    });
    const info = selectionInfo(spec, drawn, light);
    expect(info.binnedOn).toBe("x");
    expect(selectionsOf(spec, info).map((s) => s.kind)).toEqual(["point", "interval"]);
    expect(clickPicks(spec, info, { seriesType: "custom", value: [200, 400, 0, 5, 5] })).toEqual([
      { field: "price", channel: "x", range: { min: 200, max: 400, max_exclusive: true } },
    ]);
    expect(brushType(spec, info)).toBe("lineX");
    expect(brushPicks(spec, info, [{ brushType: "lineX", coordRange: [350, 120] }])).toEqual([
      { field: "price", channel: "x", range: { min: 120, max: 350 } },
    ]);
  });

  it("brushes a scatter plot's X, and a click on a point picks nothing", () => {
    const spec: PlotSpec = { data, layers: [{ mark: "point", encoding: { x: { field: "area" }, y: { field: "price" } } }] };
    const drawn = plot({
      layers: [layer({ mark: "point", columns: ["x", "y"], rows: [[60, 100], [80, 140]] })],
      domains: { x: { kind: "continuous", min: 60, max: 80 }, y: { kind: "continuous", min: 100, max: 140 } },
    });
    const info = selectionInfo(spec, drawn, light);
    expect(selectionsOf(spec, info)).toEqual([{ name: "brush", kind: "interval", channels: ["x"] }]);
    expect(clickPicks(spec, info, { seriesType: "scatter", value: [60, 100] })).toEqual([]);
    // A cleared brush picks nothing.
    expect(brushPicks(spec, info, [])).toEqual([]);
  });

  it("brushes dates along a time axis as days", () => {
    const spec: PlotSpec = { data, layers: [{ mark: "line", stat: { kind: "count" }, encoding: { x: { field: "occurred_on" } } }] };
    const days = ["2025-01-10", "2025-02-03", "2025-03-01"];
    const drawn = plot({
      layers: [layer({ mark: "line", stat: "count", columns: ["x", "y"], rows: days.map((d) => [d, 1]) })],
      domains: { x: { kind: "discrete", values: days } },
    });
    const info = selectionInfo(spec, drawn, light);
    expect(info.axes.x.kind).toBe("time");
    expect(brushType(spec, info)).toBe("lineX");
    expect(brushPicks(spec, info, [{ brushType: "lineX", coordRange: [Date.UTC(2025, 1, 1, 13), Date.UTC(2025, 2, 1, 2)] }])).toEqual([
      { field: "occurred_on", channel: "x", range: { min: "2025-02-01", max: "2025-03-01" } },
    ]);
  });

  it("follows the selections a spec declares: a brush along categories picks the ones it covers", () => {
    const { spec, drawn } = bars("category", ["arson", "burglary", "theft"], {
      selections: [{ name: "pick", kind: "interval", channels: ["x"] }],
    });
    const info = selectionInfo(spec, drawn, light);
    expect(brushType(spec, info)).toBe("lineX");
    // No click, since none is declared.
    expect(clickPicks(spec, info, { seriesType: "bar", value: ["theft", 2] })).toEqual([]);
    expect(brushPicks(spec, info, [{ brushType: "lineX", coordRange: [1, 2] }])).toEqual([
      { field: "category", channel: "x", values: ["burglary", "theft"] },
    ]);
  });

  it("picks a box by its place, and nothing on a plot drawn on shares or folded columns", () => {
    const spec: PlotSpec = { data, layers: [{ mark: "box", stat: { kind: "boxplot" }, encoding: { x: { field: "area_name" }, y: { field: "price" } } }] };
    const drawn = plot({
      layers: [
        layer({
          mark: "box",
          stat: "boxplot",
          columns: ["x", "y_lower", "y_q1", "y_median", "y_q3", "y_upper", "n"],
          rows: [
            ["north", 1, 2, 3, 4, 5, 10],
            ["south", 1, 2, 3, 4, 5, 10],
          ],
        }),
      ],
      domains: { x: { kind: "discrete", values: ["north", "south"] } },
    });
    const info = selectionInfo(spec, drawn, light);
    expect(clickPicks(spec, info, { seriesType: "boxplot", dataIndex: 1, value: [1, 2, 3, 4, 5] })).toEqual([
      { field: "area_name", channel: "x", values: ["south"] },
    ]);
    expect(selectionsOf({ ...spec, coord: "polar" }, { ...info, drawnApart: true })).toEqual([]);
    const folded: PlotSpec = {
      data,
      fold: { columns: ["before", "after"] },
      layers: [{ mark: "box", stat: { kind: "boxplot" }, encoding: { x: { field: "variable" }, y: { field: "value" } } }],
    };
    expect(columnOn(folded, "x")).toBeUndefined();
    expect(columnOn(folded, "y")).toBeUndefined();
    expect(columnOn({ ...spec, data: { kind: "fit_output", instance: "i", name: "rows" } }, "x")).toBeUndefined();
  });
});
