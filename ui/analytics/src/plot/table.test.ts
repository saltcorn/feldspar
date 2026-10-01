import { describe, expect, it } from "vitest";

import type { TableData } from "./spec";
import { tableModel } from "./table";

// The ten houses of the server's tests: mean price by neighbourhood and sold.
const byHoodAndSold: TableData = {
  rows: ["neighbourhood"],
  columns: ["sold"],
  cells: ["mean of price", "rows"],
  body: {
    columns: ["r0", "c0", "n", "v0", "v1"],
    rows: [
      [1, false, 1, 200, 1],
      [1, true, 3, 266.6666666666667, 3],
      [2, false, 2, 250, 2],
      [2, true, 2, 625, 2],
      [3, true, 1, 220, 1],
      [3, null, 1, 120, 1],
    ],
  },
  row_totals: {
    columns: ["r0", "n", "v0", "v1"],
    rows: [
      [1, 4, 250, 4],
      [2, 4, 437.5, 4],
      [3, 2, 170, 2],
    ],
  },
  column_totals: {
    columns: ["c0", "n", "v0", "v1"],
    rows: [
      [false, 3, 233.33333333333334, 3],
      [true, 6, 378.3333333333333, 6],
      [null, 1, 120, 1],
    ],
  },
  grand_total: { columns: ["n", "v0", "v1"], rows: [[10, 309, 10]] },
  bins: {},
  total: 10,
  truncated: false,
};

describe("tableModel", () => {
  it("places every part: body, Total column, Total row and the corner", () => {
    const m = tableModel(byHoodAndSold, "(missing)");
    expect(m.groups.map((g) => [g.labels, g.total])).toEqual([
      [["false"], false],
      [["true"], false],
      [["(missing)"], false],
      [["Total"], true],
    ]);
    expect(m.rows.map((r) => r.labels[0])).toEqual(["1", "2", "3", "Total"]);
    // Two cells per group: the mean, then the count.
    expect(m.rows[0].values).toEqual(["200", "1", "266.67", "3", "", "", "250", "4"]);
    expect(m.rows[2].values).toEqual(["", "", "220", "1", "120", "1", "170", "2"]);
    expect(m.rows[3]).toEqual({
      labels: ["Total"],
      values: ["233.33", "3", "378.33", "6", "120", "1", "309", "10"],
      total: true,
    });
  });

  it("lays out binned rows with no column dimension, and the corner below them", () => {
    const m = tableModel({
      rows: ["area"],
      columns: [],
      cells: ["median of price"],
      body: {
        columns: ["r0", "r0_end", "n", "v0"],
        rows: [
          [40, 60, 4, 135],
          [60, 80, 4, 275],
        ],
      },
      grand_total: { columns: ["n", "v0"], rows: [[8, 235]] },
      bins: {},
      total: 8,
      truncated: false,
    });
    expect(m.groups).toEqual([{ labels: [], total: false }]);
    expect(m.rows.map((r) => [r.labels, r.values])).toEqual([
      [["40–60"], ["135"]],
      [["60–80"], ["275"]],
      [["Total"], ["235"]],
    ]);
  });

  it("puts the corner in a Total column when there are only column dimensions", () => {
    const m = tableModel({
      rows: [],
      columns: ["sold"],
      cells: ["rows"],
      body: {
        columns: ["c0", "n", "v0"],
        rows: [
          [false, 3, 3],
          [true, 7, 7],
        ],
      },
      grand_total: { columns: ["n", "v0"], rows: [[10, 10]] },
      bins: {},
      total: 10,
      truncated: false,
    });
    expect(m.groups.map((g) => g.labels[0])).toEqual(["false", "true", "Total"]);
    expect(m.rows).toEqual([{ labels: [], values: ["3", "7", "10"], total: false }]);
  });
});
