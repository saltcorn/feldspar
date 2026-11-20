import { describe, expect, it } from "vitest";

import { PANEL_MIME, readPanelDrag, setPanelDrag, type Panel, type Transfer } from "../panels/panel";
import { readSession, readSessionFrame } from "./session";
import {
  addCell,
  completionsAt,
  datasetsNamed,
  deleteCell,
  moveCell,
  nameInline,
  ranInSession,
  readNotebook,
  runnable,
  setKind,
  setSource,
  withRun,
  type Cell,
  type NotebookState,
  type SessionStatus,
} from "./state";

const code = (id: string, source = "", extra: Partial<Cell> = {}): Cell => ({ id, kind: "code", source, ...extra });
const text = (id: string, source = ""): Cell => ({ id, kind: "text", source });
const nb = (...cells: Cell[]): NotebookState => ({ language: "javascript", cells });
const ids = (n: NotebookState) => n.cells.map((c) => c.id);

const inline = {
  kind: "inline",
  definition: { id: "6d1f6a52-0000-4000-8000-000000000001", name: "pricey", base: { kind: "table", table: "houses" } },
} as const;

const plot: Panel = {
  id: "p1",
  kind: "plot",
  content: { spec: { data: inline, layers: [{ mark: "bar", encoding: {} }] } as never },
};

describe("reading a notebook", () => {
  it("keeps the language and the cells, and drops what is not a cell", () => {
    const read = readNotebook({
      language: "python",
      cells: [code("a", "x = 1"), { id: 3 }, { id: "b", kind: "pie" }, text("c", "# Hi")],
    });
    expect(read.language).toBe("python");
    expect(ids(read)).toEqual(["a", "c"]);
  });

  it("gives a text cell neither outputs nor a run", () => {
    const read = readNotebook({
      language: "javascript",
      cells: [{ id: "t", kind: "text", source: "x", outputs: [{ kind: "text", text: "no" }], run: { count: 1, at: "" } }],
    });
    expect(read.cells[0]).toEqual({ id: "t", kind: "text", source: "x" });
  });
});

describe("editing cells", () => {
  it("adds a cell after another, or at the end, with an id of its own", () => {
    const start = nb(code("a"), code("b"));
    const after = addCell(start, "text", "a");
    expect(ids(after.nb)).toEqual(["a", after.id, "b"]);
    expect(after.nb.cells[1]).toEqual({ id: after.id, kind: "text", source: "" });
    const end = addCell(start, "code");
    expect(ids(end.nb)).toEqual(["a", "b", end.id]);
    expect(["a", "b"]).not.toContain(end.id);
  });

  it("moves a cell up and down, never past either end", () => {
    const start = nb(code("a"), code("b"), code("c"));
    expect(ids(moveCell(start, "c", -1))).toEqual(["a", "c", "b"]);
    expect(ids(moveCell(start, "a", 1))).toEqual(["b", "a", "c"]);
    expect(moveCell(start, "a", -1)).toBe(start);
    expect(moveCell(start, "c", 1)).toBe(start);
  });

  it("deletes a cell and edits a source", () => {
    const start = nb(code("a", "1"), code("b", "2"));
    expect(ids(deleteCell(start, "a"))).toEqual(["b"]);
    expect(setSource(start, "b", "3").cells[1].source).toBe("3");
  });

  it("switching a code cell to text drops its outputs and its run, which a text cell cannot have", () => {
    const ran = code("a", "# Title", { outputs: [{ kind: "value", value: 1 }], run: { count: 1, at: "2026-01-01T00:00:00Z" } });
    const switched = setKind(nb(ran), "a", "text");
    expect(switched.cells[0]).toEqual({ id: "a", kind: "text", source: "# Title" });
    expect(setKind(switched, "a", "code").cells[0]).toEqual({ id: "a", kind: "code", source: "# Title" });
  });

  it("runs the code cells with code in them", () => {
    expect(runnable(nb(text("t", "x"), code("a", " "), code("b", "1"))).map((c) => c.id)).toEqual(["b"]);
  });
});

describe("a run's answer", () => {
  const answer = {
    id: "a",
    kind: "code",
    source: "what was run",
    outputs: [{ kind: "panel", panel: plot }],
    run: { count: 4, at: "2026-11-20T10:00:00Z", ms: 12 },
  };

  it("brings the outputs and the run, and keeps what was typed meanwhile", () => {
    const after = withRun(nb(code("a", "typed since"), code("b")), answer);
    expect(after.cells[0].source).toBe("typed since");
    expect(after.cells[0].outputs).toEqual(answer.outputs);
    expect(after.cells[0].run?.count).toBe(4);
    expect(after.cells[1]).toEqual(code("b"));
  });

  it("does not bring back a cell deleted while it ran, nor give a text cell outputs", () => {
    expect(ids(withRun(nb(code("b")), answer))).toEqual(["b"]);
    expect(withRun(nb(text("a", "x")), answer).cells[0]).toEqual(text("a", "x"));
  });
});

describe("not run in this session", () => {
  const ran = code("a", "1", { run: { count: 1, at: "2026-11-20T10:00:00Z" } });
  const session = (started: string | null, state = "idle"): SessionStatus => ({ state, runs: 1, started });

  it("marks outputs from before the session started, or with no session", () => {
    expect(ranInSession(ran, session("2026-11-20T09:00:00Z"))).toBe(true);
    expect(ranInSession(ran, session("2026-11-20T11:00:00Z"))).toBe(false);
    expect(ranInSession(ran, session(null, "none"))).toBe(false);
    expect(ranInSession(code("b"), session("2026-11-20T09:00:00Z"))).toBe(false);
  });

  it("reads the session's socket frames", () => {
    const frame = JSON.stringify({ type: "status", session: { state: "running", cell: "a", runs: 2, started: "x", fit: "f" } });
    expect(readSessionFrame(frame)).toEqual({ state: "running", language: null, cell: "a", runs: 2, started: "x", fit: "f" });
    expect(readSessionFrame("{}")).toBeNull();
    expect(readSessionFrame("not json")).toBeNull();
    expect(readSession(null).state).toBe("none");
  });
});

describe("save as dataset", () => {
  it("points every reference to the non-persisted dataset at the stored one, and no other", () => {
    const other = { kind: "inline", definition: { ...inline.definition, id: "other" } };
    const state = {
      language: "javascript",
      cells: [
        code("a", "", {
          outputs: [
            { kind: "panel", panel: plot },
            { kind: "dataset", data: inline, columns: ["x"], rows: [], total: 0 },
            { kind: "dataset", data: other as never, columns: ["x"], rows: [], total: 0 },
          ],
        }),
      ],
    };
    const named = nameInline(state, inline.definition.id);
    const outputs = named.cells[0].outputs ?? [];
    const stored = { kind: "dataset", dataset: inline.definition.id };
    expect((outputs[0] as { panel: { content: { spec: { data: unknown } } } }).panel.content.spec.data).toEqual(stored);
    expect((outputs[1] as { data: unknown }).data).toEqual(stored);
    expect((outputs[2] as { data: unknown }).data).toEqual(other);
    // The state it was handed is left as it was.
    expect((state.cells[0].outputs?.[1] as { data: unknown }).data).toBe(inline);
  });
});

describe("dragging an output (A10.8)", () => {
  it("is a copy that carries its non-persisted dataset", () => {
    const store = new Map<string, string>();
    const transfer: Transfer = {
      get types() {
        return [...store.keys()];
      },
      setData: (type, data) => void store.set(type, data),
      getData: (type) => store.get(type) ?? "",
    };
    setPanelDrag(transfer, plot);
    expect(transfer.types).toContain(PANEL_MIME);
    const dropped = readPanelDrag(transfer);
    expect(dropped?.id).not.toBe(plot.id);
    expect(dropped?.kind).toBe("plot");
    expect((dropped?.content as { spec: { data: unknown } }).spec.data).toEqual(inline);
  });
});

describe("completions", () => {
  const names = { datasets: ["houses", "sales"], tables: ["houses"], columns: ["price", "neighbourhood"] };

  it("offers nb's functions after `nb.`, spelled for the language", () => {
    const js = completionsAt("const d = nb.", "javascript", names);
    expect(js.map((c) => c.label)).toEqual(expect.arrayContaining(["dataset", "plot", "table", "test", "card", "fit", "show"]));
    const plotJs = js.find((c) => c.label === "plot");
    const plotPy = completionsAt("nb.pl", "python", names).find((c) => c.label === "plot");
    expect(plotJs?.insert).toContain("{ data:");
    expect(plotPy?.insert).toContain("data=");
  });

  it("offers datasets and tables inside nb.dataset(\"", () => {
    expect(completionsAt('nb.dataset("ho', "javascript", names).map((c) => c.label)).toEqual(["houses", "sales"]);
  });

  it("offers db's functions and the tables after `db.`", () => {
    expect(completionsAt("await db.", "javascript", names).map((c) => c.label)).toEqual(["table", "sql", "houses"]);
  });

  it("offers a dataset's methods after a dot, and columns inside other strings", () => {
    expect(completionsAt('nb.dataset("houses").', "javascript", names).map((c) => c.label)).toContain("aggregate");
    expect(completionsAt('d.filter("pri', "javascript", names).map((c) => c.label)).toEqual(["price", "neighbourhood"]);
    expect(completionsAt("const x = ", "javascript", names)).toEqual([]);
  });

  it("finds the datasets a cell names", () => {
    expect(datasetsNamed('nb.dataset("houses").filter("x")\nnb.dataset(\'sales\')\nnb.dataset("houses")')).toEqual([
      "houses",
      "sales",
    ]);
  });
});
