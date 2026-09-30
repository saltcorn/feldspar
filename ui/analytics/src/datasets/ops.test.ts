import { describe, expect, it } from "vitest";

import {
  applyCompletion,
  cellText,
  defaultParams,
  describeGrain,
  describeOperation,
  formulaCompletions,
  insertOperation,
  matchCompletions,
  moveOperation,
  newOperation,
  newOpId,
  removeOperation,
  rowsOf,
  stageOf,
  stageShape,
  toggleOperation,
  tokenAt,
  uniqueName,
  type Operation,
  type Report,
  type StageShape,
} from "./ops";

const t = (text: string, args: Record<string, string | number> = {}) =>
  text.replace(/\{(\w+)\}/g, (_, k: string) => String(args[k] ?? `{${k}}`));

const op = (id: string, kind: Operation["kind"], params: Record<string, unknown>): Operation => ({
  id,
  enabled: true,
  kind,
  params,
});

const houses: StageShape = {
  columns: [
    { name: "id", type: "int" },
    { name: "price", type: "float" },
    { name: "neighbourhood", type: "int", key: { table: "neighbourhoods", field: "id" } },
  ],
  grain: { kind: "table", table: "houses", key: "id" },
};

const report: Report = {
  base: { shape: houses },
  operations: [
    { id: "a", kind: "calculated", status: "ok", shape: houses },
    { id: "b", kind: "filter", status: "invalid", error: "no" },
  ],
  tables: {
    neighbourhoods: [
      { name: "id", type: "int" },
      { name: "name", type: "text" },
    ],
    viewings: [
      { name: "id", type: "int" },
      { name: "house", type: "int", key: { table: "houses", field: "id" } },
      { name: "offer", type: "float" },
    ],
  },
  children: { houses: [{ table: "viewings", key: "house" }] },
};

describe("the operation list", () => {
  const ops = [op("op1", "filter", {}), op("op2", "sort", {}), op("op3", "select", {})];

  it("inserts, moves, switches off and removes by id", () => {
    const added = insertOperation(ops, 1, op("x", "calculated", {}));
    expect(added.map((o) => o.id)).toEqual(["op1", "x", "op2", "op3"]);
    // Dragging the first to the end, and the last to the front.
    expect(moveOperation(ops, 0, 2).map((o) => o.id)).toEqual(["op2", "op3", "op1"]);
    expect(moveOperation(ops, 2, 0).map((o) => o.id)).toEqual(["op3", "op1", "op2"]);
    expect(moveOperation(ops, 5, 0)).toBe(ops);
    expect(toggleOperation(ops, "op2")[1].enabled).toBe(false);
    expect(removeOperation(ops, "op2").map((o) => o.id)).toEqual(["op1", "op3"]);
  });

  it("gives a new operation an id nothing has", () => {
    expect(newOpId(ops)).toBe("op4");
    expect(newOpId([op("op2", "filter", {})])).toBe("op3");
    const made = newOperation("calculated", ops, houses.columns);
    expect(made.enabled).toBe(true);
    expect(made.params).toEqual({ name: "new_column", formula: "" });
  });

  it("fills a new operation in from the columns it reads", () => {
    expect(defaultParams("select", houses.columns)).toEqual({
      columns: [{ column: "id" }, { column: "price" }, { column: "neighbourhood" }],
    });
    expect(defaultParams("window", houses.columns)).toMatchObject({
      function: "lag",
      column: "id",
    });
    expect(uniqueName("price", ["price", "price_2"])).toBe("price_3");
  });
});

describe("stages", () => {
  it("reads the base and each operation's shape, and the stage an operation makes", () => {
    expect(stageShape(report, 0)).toBe(houses);
    expect(stageShape(report, 1)).toBe(houses);
    expect(stageShape(report, 2)).toBeNull();
    const ops = [op("a", "filter", {}), op("b", "sort", {})];
    expect(stageOf(ops, "")).toBe(0);
    expect(stageOf(ops, "a")).toBe(1);
    expect(stageOf(ops, null)).toBe(2);
    expect(stageOf(ops, "gone")).toBe(2);
  });

  it("says what a row is", () => {
    expect(describeGrain(houses.grain, t)).toBe("one row per houses");
    expect(describeGrain({ kind: "group", keys: ["region", "month"] }, t)).toBe(
      "one row per region × month",
    );
    expect(describeGrain({ kind: "group", keys: [] }, t)).toBe("one row in all");
  });
});

describe("what an operation says in the side panel", () => {
  it("shows its formulas as written", () => {
    expect(describeOperation(op("a", "calculated", { name: "ppm", formula: "price / area" }), t)).toBe(
      "ppm = price / area",
    );
    expect(describeOperation(op("a", "filter", { formula: "price > 100000" }), t)).toBe(
      "price > 100000",
    );
    expect(
      describeOperation(
        op("a", "aggregate", {
          group_by: [{ name: "neighbourhood", formula: "neighbourhood" }],
          summaries: [
            { name: "mean_ppm", function: "mean", column: "ppm" },
            { name: "n", function: "count" },
          ],
        }),
        t,
      ),
    ).toBe("by neighbourhood: mean_ppm = mean(ppm), n = count()");
    expect(
      describeOperation(
        op("a", "select", { columns: [{ column: "a" }, { column: "b", rename: "c" }] }),
        t,
      ),
    ).toBe("a, b → c");
    expect(
      describeOperation(
        op("a", "join", {
          with: { kind: "dataset", dataset: "d1" },
          kind: "left",
          on: [{ left: "id", right: "house" }],
        }),
        t,
        (id) => (id === "d1" ? "viewing counts" : id),
      ),
    ).toBe("left join viewing counts on id = house");
    expect(describeOperation(op("a", "limit", { mode: "sample", n: 10, seed: 3 }), t)).toBe(
      "a sample of 10 (seed 3)",
    );
  });
});

describe("what a formula may name", () => {
  it("offers the columns, one step along each key, and the child tables' counts", () => {
    const all = formulaCompletions(houses, report).map((c) => c.text);
    expect(all).toContain("price");
    expect(all).toContain("neighbourhoodⱵname");
    expect(all).toContain("viewingsↃhouse.length");
    expect(all).toContain('viewingsↃhouse.sum("offer")');
    // The key back to the parent is not a value to add up.
    expect(all).not.toContain('viewingsↃhouse.sum("house")');
  });

  it("offers child tables only while rows are a table's rows", () => {
    const byMonth: StageShape = {
      columns: [{ name: "month", type: "date" }],
      grain: { kind: "group", keys: ["month"] },
    };
    expect(rowsOf(byMonth)).toBeNull();
    expect(formulaCompletions(byMonth, report).map((c) => c.text)).toEqual(["month"]);
    const byHouse: StageShape = {
      columns: [{ name: "house", type: "int", key: { table: "houses", field: "id" } }],
      grain: { kind: "group", keys: ["house"] },
    };
    expect(rowsOf(byHouse)).toBe("houses");
    expect(formulaCompletions(byHouse, report).map((c) => c.text)).toContain(
      "viewingsↃhouse.length",
    );
  });

  it("completes the identifier at the cursor, join paths included", () => {
    const text = "price / neighbourhoodⱵna + 1";
    const cursor = "price / neighbourhoodⱵna".length;
    expect(tokenAt(text, cursor)).toEqual({ start: 8, end: cursor, prefix: "neighbourhoodⱵna" });
    const all = formulaCompletions(houses, report);
    const offered = matchCompletions(all, "neighbourhoodⱵna").map((c) => c.text);
    expect(offered).toEqual(["neighbourhoodⱵname"]);
    expect(applyCompletion(text, cursor, "neighbourhoodⱵname")).toEqual({
      text: "price / neighbourhoodⱵname + 1",
      cursor: "price / neighbourhoodⱵname".length,
    });
    // Nothing typed: nothing offered; the full name typed: nothing either.
    expect(matchCompletions(all, "")).toEqual([]);
    expect(matchCompletions(all, "price").map((c) => c.text)).toEqual([]);
  });
});

describe("a cell's text", () => {
  it("rounds a fractional number to four decimals and leaves the rest as it is", () => {
    expect(cellText(3213.175036865414, "float")).toBe("3213.175");
    expect(cellText(2.5, "float")).toBe("2.5");
    expect(cellText(42, "int")).toBe("42");
    expect(cellText(0.123456, "text")).toBe("0.123456");
    expect(cellText({ a: 1 }, "json")).toBe('{"a":1}');
    expect(cellText(null, "float")).toBe("");
  });
});
