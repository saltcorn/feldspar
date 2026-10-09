import { describe, expect, it } from "vitest";

import { makePanel } from "../panels/panel";
import { inputKind, rangeCondition } from "./FilterBar";
import {
  conditionsFor,
  datasetsOf,
  describe as describeCondition,
  featurePicks,
  readFilters,
  readRefresh,
  select,
  selectionOf,
  wire,
  type Selected,
} from "./filters";

const t = (s: string, args?: Record<string, string | number>) =>
  s.replace(/\{(\w+)\}/g, (_, k: string) => String(args?.[k] ?? `{${k}}`));

const click = (source: string, column: string, values: unknown[]): Selected[] =>
  selectionOf(source, "d1", [{ field: column, values }], "click");

describe("a dashboard's filters", () => {
  it("makes one selection per column picked, named by its tile and column", () => {
    const made = selectionOf(
      "bar",
      "d1",
      [
        { field: "category", values: ["theft"] },
        { field: "price", range: { min: 1, max: 2, max_exclusive: true } },
        { values: [3] },
        { field: "nothing" },
      ],
      "click",
    );
    expect(made.map((m) => m.id)).toEqual(["sel:bar:category", "sel:bar:price", "sel:bar:*"]);
    expect(made[2]).toEqual({ id: "sel:bar:*", dataset: "d1", source: "bar", by: "click", values: [3] });
    expect(wire(made[0])).toEqual({ id: "sel:bar:category", dataset: "d1", column: "category", values: ["theft"] });
  });

  it("replaces a tile's selection, lets the same click go, and adds with a modifier", () => {
    const map = click("map", "district", [3]);
    let s = select([], "map", map);
    s = select(s, "bar", click("bar", "category", ["theft"]));
    expect(s.map((x) => x.id)).toEqual(["sel:map:district", "sel:bar:category"]);
    // Another bar replaces the first.
    s = select(s, "bar", click("bar", "category", ["burglary"]));
    expect(s.find((x) => x.source === "bar")?.values).toEqual(["burglary"]);
    // Shift-clicking adds a value, and takes it away again.
    s = select(s, "bar", click("bar", "category", ["theft"]), true);
    expect(s.find((x) => x.source === "bar")?.values).toEqual(["burglary", "theft"]);
    s = select(s, "bar", click("bar", "category", ["burglary"]), true);
    expect(s.find((x) => x.source === "bar")?.values).toEqual(["theft"]);
    // Clicking the one selected value again lets it go.
    s = select(s, "bar", click("bar", "category", ["theft"]));
    expect(s.map((x) => x.id)).toEqual(["sel:map:district"]);
  });

  it("lets a tile's brushed range go when its brush is cleared, but not its clicks", () => {
    const brushed = selectionOf("line", "d1", [{ field: "on", range: { min: "2025-01-01", max: "2025-02-01" } }], "brush");
    let s = select([], "line", brushed);
    expect(s).toHaveLength(1);
    s = select(s, "line", []);
    expect(s).toEqual([]);
    s = select([], "map", click("map", "district", [3]));
    expect(select(s, "map", [])).toEqual(s);
  });

  it("draws a tile with every condition but its own selections", () => {
    const filters = [{ id: "f", dataset: "d1", column: "year", values: [2025] }];
    const selections = [...click("bar", "category", ["theft"]), ...click("map", "district", [3])];
    const drill = [{ id: "drill:bar:0", dataset: "d1", column: "district", values: [1] }];
    expect(conditionsFor("bar", { filters, selections, drill }).map((c) => c.id)).toEqual(["f", "sel:map:district", "drill:bar:0"]);
    // Sent as the server reads them.
    expect(conditionsFor("card", { filters: [], selections })).toEqual([
      { id: "sel:bar:category", dataset: "d1", column: "category", values: ["theft"] },
      { id: "sel:map:district", dataset: "d1", column: "district", values: [3] },
    ]);
  });

  it("reads stored filters and the refresh interval leniently", () => {
    expect(
      readFilters([
        { id: "a", dataset: "d1", column: "x", values: [1, "b", null] },
        { id: "b", dataset: "d1", column: "y", range: { min: 1 } },
        { id: "a", dataset: "d1", values: [2] },
        { id: "c", dataset: "d1", range: {} },
        { id: "d", dataset: "d1", values: [[1]] },
        "nonsense",
      ]).map((c) => c.id),
    ).toEqual(["a", "b"]);
    expect(readFilters(undefined)).toEqual([]);
    expect(readRefresh(300)).toBe(300);
    expect(readRefresh(5)).toBe(0);
    expect(readRefresh("60")).toBe(0);
  });

  it("says what a condition keeps", () => {
    expect(describeCondition({ id: "a", dataset: "d", column: "category", values: ["theft", null] }, t)).toEqual({
      on: "category",
      keeps: "theft, (missing)",
    });
    expect(describeCondition({ id: "a", dataset: "d", column: "n", values: [1, 2, 3, 4, 5] }, t).keeps).toBe("1, 2, 3 and 2 more");
    expect(
      describeCondition({ id: "a", dataset: "d", column: "on", range: { min: "2025-02-01T00:00:00.000Z", max: "2025-03-01" } }, t)
        .keeps,
    ).toBe("2025-02-01 – 2025-03-01");
    expect(describeCondition({ id: "a", dataset: "d", column: "at", range: { min: "2025-02-01T10:30:00.000Z" } }, t).keeps).toBe(
      "from 2025-02-01 10:30:00",
    );
    expect(describeCondition({ id: "a", dataset: "d", column: "p", range: { max: 12500 } }, t).keeps).toBe("up to 12,500");
    // The rows themselves, by their dataset's name.
    expect(describeCondition({ id: "a", dataset: "d", values: [3] }, t, "Districts")).toEqual({ on: "Districts", keeps: "3" });
  });

  it("picks a map's feature by the key its geometry is found by, or by its row's key", () => {
    const byKey = { geometry: { kind: "key", column: "district", geometry: "outline" } };
    expect(featurePicks(byKey, false, { id: 7, properties: { district: 3, n: 12 } })).toEqual([{ field: "district", values: [3] }]);
    const own = { geometry: { kind: "column", column: "outline" } };
    expect(featurePicks(own, true, { id: 3, properties: {} })).toEqual([{ values: [3] }]);
    // Told apart only by their place: nothing to filter by.
    expect(featurePicks(own, false, { id: 3, properties: {} })).toEqual([]);
  });

  it("lists the datasets a panel reads", () => {
    const card = makePanel({ kind: "stat_card", content: { dataset: "d2", value: { function: "count" } } });
    const plotPanel = makePanel({ kind: "plot", content: { spec: { data: { kind: "dataset", dataset: "d1" }, layers: [] } } });
    const fit = makePanel({ kind: "plot", content: { spec: { data: { kind: "fit_output", instance: "i", name: "rows" }, layers: [] } } });
    expect(datasetsOf(card)).toEqual(["d2"]);
    expect(datasetsOf(plotPanel)).toEqual(["d1"]);
    expect(datasetsOf(fit)).toEqual([]);
  });

  it("makes a filter's range from what was typed", () => {
    expect(rangeCondition("f", "d", "price", "number", "100", "")).toEqual({ id: "f", dataset: "d", column: "price", range: { min: 100 } });
    expect(rangeCondition("f", "d", "at", "timestamp", "2025-02-01T10:30", "")?.range).toEqual({ min: "2025-02-01T10:30:00Z" });
    expect(rangeCondition("f", "d", "on", "date", "", "2025-03-01")?.range).toEqual({ max: "2025-03-01" });
    expect(rangeCondition("f", "d", "price", "number", "x", " ")).toBeNull();
    expect(inputKind({ name: "district", type: "int", key: { table: "districts", field: "id" } })).toBe("values");
    expect(inputKind({ name: "price", type: "float" })).toBe("number");
    expect(inputKind({ name: "on", type: "date" })).toBe("date");
    expect(inputKind({ name: "category", type: "text" })).toBe("values");
    expect(inputKind({ name: "outline", type: "geometry" })).toBeNull();
  });
});
