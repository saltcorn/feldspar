import { describe, expect, it } from "vitest";

import { compareCells, selectedOnly, tableOrder, type LayerRows } from "./table";

const answer = (sorted: boolean): LayerRows => ({
  columns: [
    { name: "name", type: "text" },
    { name: "count", type: "int" },
  ],
  rows: [
    ["b", 2],
    ["a", null],
    ["c", 10],
    ["a2", 2],
  ],
  ids: [11, 12, 13, 14],
  total: 4,
  keyed: !sorted,
  sorted,
});

describe("the attribute table's order", () => {
  it("keeps what the server sorted, and sorts the rest here with ids alongside", () => {
    expect(tableOrder(answer(true), { column: "count", descending: true })).toEqual([0, 1, 2, 3]);
    const unsorted = answer(false);
    // Numbers as numbers, ties in the dataset's order, missing last either way.
    expect(tableOrder(unsorted, { column: "count", descending: false })).toEqual([0, 3, 2, 1]);
    expect(tableOrder(unsorted, { column: "count", descending: true })).toEqual([2, 0, 3, 1]);
    expect(tableOrder(unsorted, { column: "name", descending: false }).map((i) => unsorted.ids[i])).toEqual([12, 14, 11, 13]);
    expect(tableOrder(unsorted, undefined)).toEqual([0, 1, 2, 3]);
    expect(tableOrder(unsorted, { column: "gone", descending: false })).toEqual([0, 1, 2, 3]);
  });

  it("keeps the selection only, and compares text as a reader would", () => {
    expect(selectedOnly(answer(false), [3, 2, 1, 0], [11, 13])).toEqual([2, 0]);
    expect(compareCells("item 2", "item 10")).toBeLessThan(0);
    expect(compareCells(true, false)).toBeGreaterThan(0);
  });
});
