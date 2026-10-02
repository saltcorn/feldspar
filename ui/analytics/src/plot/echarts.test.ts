import { describe, expect, it } from "vitest";

import { mosaicTiles, niceRange, panels, plotNotes, toOption, type Option } from "./echarts";
import { chartPalette } from "./palette";
import type { LayerData, PlotData, PlotSpec } from "./spec";

const data = { kind: "dataset" as const, dataset: "d1" };

function layer(partial: Partial<LayerData> & Pick<LayerData, "columns" | "rows" | "mark">): LayerData {
  return { stat: "identity", sampled: false, total: partial.rows.length, truncated: false, ...partial };
}

function plot(partial: Partial<PlotData> & Pick<PlotData, "layers">): PlotData {
  return { domains: {}, facets: {}, bins: {}, warnings: [], ...partial };
}

const light = { theme: "light" as const };
const series = (o: Option) => o.series as Option[];
const axes = (o: Option, which: "xAxis" | "yAxis") => o[which] as Option[];
const titles = (o: Option) => (o.title as Option[]).map((t) => t.text);

describe("toOption", () => {
  it("draws a histogram's bins as bars between their edges, stacked by colour", () => {
    const spec: PlotSpec = {
      data,
      layers: [
        {
          mark: "bar",
          stat: { kind: "count" },
          encoding: { x: { field: "price", bin: {} }, color: { field: "kind" } },
        },
      ],
    };
    const drawn = plot({
      layers: [
        layer({
          mark: "bar",
          stat: "count",
          columns: ["x", "x_end", "color", "y"],
          rows: [
            [0, 200, "a", 3],
            [0, 200, "b", 2],
            [200, 400, "a", 5],
          ],
        }),
      ],
      domains: {
        x: { kind: "continuous", min: 0, max: 400 },
        y: { kind: "continuous", min: 2, max: 5 },
        color: { kind: "discrete", values: ["a", "b"] },
      },
    });
    const o = toOption(spec, drawn, light);
    expect(axes(o, "xAxis")[0].type).toBe("value");
    // The axes' titles: the count above, the column below.
    expect(titles(o)).toEqual(["count", "price"]);
    const s = series(o);
    expect(s.map((x) => x.type)).toEqual(["custom", "custom"]);
    expect(s.map((x) => x.name)).toEqual(["a", "b"]);
    // `b` starts where `a` ended in the same bin.
    expect(s[0].data).toEqual([
      [0, 200, 0, 3, 3],
      [200, 400, 0, 5, 5],
    ]);
    expect(s[1].data).toEqual([[0, 200, 3, 5, 2]]);
    // Colour follows the value's slot.
    const palette = chartPalette("light");
    expect((s[1].itemStyle as Option).color).toBe(palette.categorical[1]);
    expect((o.legend as Option).data).toEqual(["a", "b"]);
  });

  it("draws a histogram binned along Y as bars lying down, from the count on X", () => {
    // Two numbers on Y and nothing on X: the folded values binned on Y.
    const spec: PlotSpec = {
      data,
      fold: { columns: ["before", "after"], key: "variable", value: "value" },
      layers: [
        {
          mark: "bar",
          stat: { kind: "count" },
          encoding: { y: { field: "value", bin: {} }, color: { field: "variable" } },
        },
      ],
    };
    const drawn = plot({
      layers: [
        layer({
          mark: "bar",
          stat: "count",
          columns: ["y", "y_end", "color", "x"],
          rows: [
            [110, 120, "after", 6],
            [110, 120, "before", 3],
            [120, 130, "after", 21],
          ],
        }),
      ],
      domains: {
        x: { kind: "continuous", min: 3, max: 21 },
        y: { kind: "continuous", min: 110, max: 130 },
        color: { kind: "discrete", values: ["after", "before"] },
      },
    });
    const o = toOption(spec, drawn, light);
    expect(axes(o, "yAxis")[0].type).toBe("value");
    const s = series(o);
    expect(s.map((x) => x.type)).toEqual(["custom", "custom"]);
    const [first, second] = s;
    expect(s.map((x) => x.name)).toEqual(["before", "after"]);
    // `after` starts where `before` ended in the same bin.
    expect(first.data).toEqual([[110, 120, 0, 3, 3]]);
    expect(second.data).toEqual([
      [110, 120, 3, 9, 6],
      [120, 130, 0, 21, 21],
    ]);
    // The edges on Y, the counts on X.
    expect(first.encode).toEqual({ x: [2, 3], y: [0, 1], tooltip: [4] });
    // A bar from the count's start to its end across, between the edges
    // down: with the axes as the identity, [0, 3] across and [110, 120] down,
    // less the gap between bars.
    const item = [110, 120, 0, 3, 3];
    const render = first.renderItem as (p: unknown, api: unknown) => { shape: Record<string, number> };
    const { shape } = render(
      {},
      { value: (i: number) => item[i], coord: (p: number[]) => p, style: () => ({}) },
    );
    expect(shape).toEqual({ x: 0, y: 111, width: 3, height: 8 });
  });

  it("draws a box plot from the five numbers, with its outliers, on a category axis", () => {
    const spec: PlotSpec = {
      data,
      layers: [
        {
          mark: "box",
          stat: { kind: "boxplot" },
          encoding: { x: { field: "neighbourhood" }, y: { field: "price" } },
        },
      ],
    };
    const drawn = plot({
      layers: [
        layer({
          mark: "box",
          stat: "boxplot",
          columns: ["x", "n", "y_lower", "y_q1", "y_median", "y_q3", "y_upper"],
          rows: [
            [1, 4, 100, 175, 250, 325, 400],
            [2, 4, 150, 225, 300, 512.5, 350],
          ],
          outliers: { columns: ["x", "y"], rows: [[2, 1000]] },
        }),
      ],
      // The neighbourhood's ids are numbers, but a box plot groups by them.
      domains: { x: { kind: "continuous", min: 1, max: 2, values: [1, 2] } },
    });
    const o = toOption(spec, drawn, light);
    expect(axes(o, "xAxis")[0]).toMatchObject({ type: "category", data: ["1", "2"] });
    const [box, outliers] = series(o);
    expect(box.type).toBe("boxplot");
    expect((box.data as Option[]).map((d) => d.value)).toEqual([
      [100, 175, 250, 325, 400],
      [150, 225, 300, 512.5, 350],
    ]);
    expect(outliers).toMatchObject({ type: "scatter", data: [["2", 1000]] });
  });

  it("flips: a category drawn up and the values across", () => {
    const spec: PlotSpec = {
      data,
      coord: "flipped",
      layers: [
        {
          mark: "bar",
          stat: { kind: "aggregate", function: "mean" },
          encoding: { x: { field: "street" }, y: { field: "price" } },
        },
      ],
    };
    const drawn = plot({
      layers: [layer({ mark: "bar", stat: "aggregate", columns: ["x", "y"], rows: [["High St", 10], ["Low Rd", 20]] })],
      domains: { x: { kind: "discrete", values: ["High St", "Low Rd"] }, y: { kind: "continuous", min: 10, max: 20 } },
    });
    const o = toOption(spec, drawn, light);
    expect(axes(o, "yAxis")[0]).toMatchObject({ type: "category", data: ["High St", "Low Rd"] });
    expect(axes(o, "xAxis")[0].type).toBe("value");
    expect(series(o)[0].data).toEqual([
      [10, "High St"],
      [20, "Low Rd"],
    ]);
  });

  it("puts a log scale, a reference line and a smoother's band on a scatter plot", () => {
    const spec: PlotSpec = {
      data,
      scales: { y: { kind: "log" } },
      references: [{ channel: "y", value: 100000, label: "target" }],
      layers: [
        { mark: "point", encoding: { x: { field: "area" }, y: { field: "price" } } },
        {
          mark: "line",
          stat: { kind: "smooth", method: "linear" },
          encoding: { x: { field: "area" }, y: { field: "price" } },
        },
      ],
    };
    const drawn = plot({
      layers: [
        layer({ mark: "point", columns: ["x", "y"], rows: [[50, 100000], [60, 200000]] }),
        layer({
          mark: "line",
          stat: "smooth",
          columns: ["x", "y", "y_lower", "y_upper"],
          rows: [
            [60, 200000, 190000, 210000],
            [50, 100000, 90000, 110000],
          ],
        }),
      ],
      domains: { x: { kind: "continuous", min: 50, max: 60 }, y: { kind: "continuous", min: 90000, max: 210000 } },
    });
    const o = toOption(spec, drawn, light);
    expect(axes(o, "yAxis")[0].type).toBe("log");
    const s = series(o);
    expect(s.map((x) => x.type)).toEqual(["scatter", "custom", "line"]);
    // The line is joined in order of X.
    expect(s[2].data).toEqual([
      [50, 100000],
      [60, 200000],
    ]);
    expect((s[0].markLine as Option).data).toEqual([{ yAxis: 100000, name: "target" }]);
    // Its label inside the plot, where the chart's edge does not cut it off.
    expect(((s[0].markLine as Option).label as Option).position).toBe("insideEndTop");
    // One series, no colour: no legend.
    expect(o.legend).toBeUndefined();
  });

  it("makes a grid and a pair of axes per small multiple", () => {
    const spec: PlotSpec = {
      data,
      facet: { wrap: { field: "year_built", bin: { width: 10 } } },
      layers: [{ mark: "bar", stat: { kind: "count" }, encoding: { x: { field: "neighbourhood" } } }],
    };
    const drawn = plot({
      layers: [
        layer({
          mark: "bar",
          stat: "count",
          columns: ["x", "wrap", "wrap_end", "y"],
          rows: [
            [1, 1990, 2000, 2],
            [2, 1990, 2000, 2],
            [1, 2000, 2010, 2],
            [2, 2010, 2020, 1],
          ],
        }),
      ],
      domains: { x: { kind: "continuous", values: [1, 2] } },
      facets: { wrap: [1990, 2000, 2010] },
      bins: { year_built: { origin: 1980, width: 10 } },
    });
    const ps = panels(spec, drawn);
    expect(ps.map((p) => p.title)).toEqual(["1990–2000", "2000–2010", "2010–2020"]);
    const o = toOption(spec, drawn, light);
    expect((o.grid as Option[]).length).toBe(3);
    expect(axes(o, "xAxis").length).toBe(3);
    const s = series(o);
    expect(s.map((x) => [x.xAxisIndex, x.data])).toEqual([
      [0, [["1", 2], ["2", 2]]],
      [1, [["1", 2]]],
      [2, [["2", 1]]],
    ]);
    expect(titles(o)).toEqual([...ps.map((p) => p.title), "count", "neighbourhood"]);
    // Fixed scales: every plot's Y has the same round bounds.
    expect(axes(o, "yAxis").map((a) => [a.min, a.max])).toEqual([
      [0, 2],
      [0, 2],
      [0, 2],
    ]);
  });

  it("gives a scatterplot matrix free axes per column and row, in the fold's order", () => {
    const spec: PlotSpec = {
      data,
      fold: { columns: ["price", "area"], pairs: {} },
      facet: { row: { field: "variable_y" }, column: { field: "variable_x" }, scales: "free" },
      layers: [{ mark: "point", encoding: { x: { field: "value_x" }, y: { field: "value_y" } } }],
    };
    const drawn = plot({
      layers: [
        layer({
          mark: "point",
          columns: ["x", "y", "row", "column"],
          rows: [
            [100, 50, "area", "price"],
            [200, 60, "area", "price"],
            [50, 100, "price", "area"],
            [60, 200, "price", "area"],
          ],
        }),
      ],
      facets: { row: ["area", "price"], column: ["area", "price"] },
    });
    const ps = panels(spec, drawn);
    // price before area, as the fold has them: the columns' names above the
    // top row, the rows' beside the last column.
    expect(ps.map((p) => [p.columnTitle, p.rowTitle])).toEqual([
      ["price", undefined],
      ["area", "price"],
      [undefined, undefined],
      [undefined, "area"],
    ]);
    const o = toOption(spec, drawn, light);
    // Free scales: no shared bounds, so ECharts fits each plot's axes to it.
    expect(axes(o, "xAxis").map((a) => a.min)).toEqual([undefined, undefined, undefined, undefined]);
    // The diagonal names its column; `value_x` and `value_y` are no titles.
    expect(titles(o)).toEqual(["price", "price", "area", "price", "area", "area"]);
    // Each plot draws only its own pair; the diagonal draws nothing.
    expect(series(o).map((x) => [x.xAxisIndex, (x.data as unknown[]).length])).toEqual([
      [1, 2],
      [2, 2],
    ]);
  });

  it("draws a foreign key's ids as categories when told the column is one", () => {
    const spec: PlotSpec = {
      data,
      layers: [{ mark: "point", encoding: { x: { field: "area" }, y: { field: "price" }, color: { field: "neighbourhood" } } }],
    };
    const drawn = plot({
      layers: [layer({ mark: "point", columns: ["x", "y", "color"], rows: [[50, 100, 1], [60, 200, 2], [70, 300, 1]] })],
      domains: { color: { kind: "continuous", min: 1, max: 2, values: [1, 2] } },
    });
    // Without being told, ids are a scale of numbers…
    expect(toOption(spec, drawn, light).visualMap).toBeDefined();
    // …and told, they are groups, one series each, with a legend.
    const o = toOption(spec, drawn, { ...light, categorical: ["neighbourhood"] });
    expect(o.visualMap).toBeUndefined();
    expect(series(o).map((x) => x.name)).toEqual(["1", "2"]);
    expect((o.legend as Option).data).toEqual(["1", "2"]);
  });
});

describe("niceRange", () => {
  it("rounds outwards to a round step, from zero when asked, by powers of ten on a log scale", () => {
    expect(niceRange([122480, 541520], false, false)).toEqual([100000, 600000]);
    expect(niceRange([3, 33.24], true, false)).toEqual([0, 40]);
    expect(niceRange([138000, 526000], false, true)).toEqual([100000, 1000000]);
  });
});

describe("toOption, more", () => {
  it("colours a correlation heatmap on a diverging scale from −1 to 1", () => {
    const spec: PlotSpec = {
      data,
      fold: { columns: ["price", "area"], pairs: { diagonal: true } },
      scales: { color: { domain: [-1, 1], scheme: "diverging" } },
      layers: [
        {
          mark: "rect",
          stat: { kind: "correlation", x: "value_x", y: "value_y" },
          encoding: { x: { field: "variable_x" }, y: { field: "variable_y" } },
        },
        {
          mark: "text",
          stat: { kind: "correlation", x: "value_x", y: "value_y" },
          encoding: { x: { field: "variable_x" }, y: { field: "variable_y" } },
        },
      ],
    };
    const rows = [
      ["area", "area", 1, 10],
      ["area", "price", 0.8, 10],
      ["price", "area", 0.8, 10],
      ["price", "price", 1, 10],
    ];
    const drawn = plot({
      layers: [
        layer({ mark: "rect", stat: "correlation", columns: ["x", "y", "color", "n"], rows }),
        layer({ mark: "text", stat: "correlation", columns: ["x", "y", "label", "n"], rows }),
      ],
      domains: {
        x: { kind: "discrete", values: ["area", "price"] },
        y: { kind: "discrete", values: ["area", "price"] },
        color: { kind: "continuous", min: 0.8, max: 1 },
      },
    });
    const o = toOption(spec, drawn, light);
    expect(axes(o, "xAxis")[0].data).toEqual(["price", "area"]);
    const vm = o.visualMap as Option;
    expect([vm.min, vm.max]).toEqual([-1, 1]);
    expect((vm.inRange as Option).color).toEqual(chartPalette("light").diverging);
    expect(vm.seriesIndex).toEqual([0]);
    const [heat, text] = series(o);
    expect(heat.type).toBe("heatmap");
    expect((heat.data as unknown[][])[1]).toEqual(["area", "price", 0.8]);
    expect(text).toMatchObject({ type: "scatter", symbolSize: 0 });
  });

  it("draws parallel coordinates with an axis per column", () => {
    const spec: PlotSpec = {
      data,
      coord: "parallel",
      fold: { columns: ["price", "area"] },
      layers: [{ mark: "line", encoding: { x: { field: "variable" }, y: { field: "value" }, color: { field: "sold" } } }],
    };
    const drawn = plot({
      layers: [
        layer({
          mark: "line",
          columns: ["color", "y_0", "y_1"],
          rows: [
            [true, 100, 50],
            [false, 200, 60],
            [true, 300, 70],
          ],
          info: { axes: ["price", "area"] },
        }),
      ],
      domains: { color: { kind: "discrete", values: [false, true] } },
    });
    const o = toOption(spec, drawn, light);
    expect((o.parallelAxis as Option[]).map((a) => a.name)).toEqual(["price", "area"]);
    const s = series(o);
    expect(s.map((x) => x.name)).toEqual(["false", "true"]);
    expect(s[1].data).toEqual([
      [100, 50],
      [300, 70],
    ]);
  });

  it("follows the dark scheme", () => {
    const spec: PlotSpec = { data, layers: [{ mark: "point", encoding: { x: { field: "a" }, y: { field: "b" } } }] };
    const drawn = plot({ layers: [layer({ mark: "point", columns: ["x", "y"], rows: [[1, 2]] })] });
    const o = toOption(spec, drawn, { theme: "dark" });
    expect(o.color).toEqual(chartPalette("dark").categorical);
    expect((o.textStyle as Option).color).toBe("#ffffff");
  });
});

describe("mosaicTiles", () => {
  it("makes each column as wide as its share and splits it by Y", () => {
    const tiles = mosaicTiles(
      { columns: ["x", "y", "size"], rows: [] },
      [
        [1, false, 1],
        [1, true, 3],
        [2, true, 4],
      ],
    );
    expect(tiles.map((t) => [t.x, t.y, t.x0, t.x1, t.y0, t.y1])).toEqual([
      ["1", "false", 0, 0.5, 0, 0.25],
      ["1", "true", 0, 0.5, 0.25, 1],
      ["2", "true", 0.5, 1, 0, 1],
    ]);
  });
});

describe("plotNotes", () => {
  it("says when a layer is a sample", () => {
    const t = (text: string, args?: Record<string, string | number>) =>
      text.replace(/\{(\w+)\}/g, (_, k: string) => String(args?.[k] ?? k));
    const drawn = plot({
      layers: [layer({ mark: "point", columns: ["x"], rows: [[1], [2]], sampled: true, total: 1000000 })],
      warnings: ["3 rows are left out"],
    });
    expect(plotNotes(drawn, t)).toEqual([
      "Showing a random sample of 2 of 1,000,000 rows.",
      "3 rows are left out",
    ]);
  });
});
