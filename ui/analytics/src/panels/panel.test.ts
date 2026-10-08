import { describe, expect, it } from "vitest";

import type { ModelOutput } from "../models/outputs";
import type { PlotSpec, TableSpec } from "../plot/spec";
import type { TestSpec } from "../explorer/tests";
import {
  PANEL_MIME,
  carriesPanel,
  explorerPanel,
  explorerTitle,
  makePanel,
  mapPanel,
  outputPanel,
  readPanel,
  readPanelDrag,
  setPanelDrag,
  type Transfer,
} from "./panel";

const t = (text: string, args?: Record<string, string | number>) =>
  text.replace(/\{(\w+)\}/g, (_, k: string) => String(args?.[k] ?? ""));

const data = { kind: "dataset", dataset: "d1" } as PlotSpec["data"];
const spec = {
  data,
  layers: [{ mark: "point", encoding: { x: { field: "area" }, y: { field: "price" } } }],
} as unknown as PlotSpec;
const table = { data, rows: [{ field: "neighbourhood" }], cells: [{ field: "price", function: "mean" }] } as unknown as TableSpec;
const tests: TestSpec = { data, y: [{ field: "price" }], x: { field: "neighbourhood" }, paired: false, mu: 0 };

/** A browser's `DataTransfer`, as much of it as a drag uses. */
function transfer(): Transfer & { store: Map<string, string> } {
  const store = new Map<string, string>();
  return {
    store,
    get types() {
      return [...store.keys()];
    },
    setData: (type, value) => void store.set(type, value),
    getData: (type) => store.get(type) ?? "",
  };
}

describe("panels (A4.2)", () => {
  it("reads every kind and refuses what is not a panel", () => {
    const panels = [
      makePanel({ kind: "plot", content: { spec } }, "Price by area"),
      makePanel({ kind: "summary_table", content: { spec: table } }),
      makePanel({ kind: "test_result", content: { tests, plot: spec } }),
      makePanel({ kind: "text", content: { markdown: "Notes" } }),
      makePanel({ kind: "fit_table", content: { fit: "f1", output: "coefficients" } }),
      makePanel({ kind: "custom", content: { renderer: "gauge", config: {} } }),
    ];
    for (const p of panels) expect(readPanel(JSON.parse(JSON.stringify(p)))).toEqual(p);
    expect(panels[0].title).toBe("Price by area");
    expect(readPanel({ id: "x", kind: "pie", content: {} })).toBeNull();
    expect(readPanel({ id: "x", kind: "plot", content: { spec: {} } })).toBeNull();
    expect(readPanel({ kind: "text", content: { markdown: "" } })).toBeNull();
    expect(readPanel("plot")).toBeNull();
  });
});

describe("drag and drop (A4.3)", () => {
  it("carries the panel as it was when the drag started, and the drop is a copy", () => {
    const source = makePanel({ kind: "plot", content: { spec: JSON.parse(JSON.stringify(spec)) as PlotSpec } }, "P");
    const dt = transfer();
    setPanelDrag(dt, source);
    expect(carriesPanel(dt)).toBe(true);
    expect(dt.effectAllowed).toBe("copy");
    expect(dt.getData("text/plain")).toBe("P");

    // The explorer changes its plot after the drag began: the copy does not.
    (source.content as { spec: PlotSpec }).spec.layers[0].mark = "line";
    const dropped = readPanelDrag(dt);
    expect(dropped).not.toBeNull();
    expect(dropped?.id).not.toBe(source.id);
    expect((dropped?.content as { spec: PlotSpec }).spec.layers[0].mark).toBe("point");

    // Two drops of one drag are two panels.
    expect(readPanelDrag(dt)?.id).not.toBe(dropped?.id);
  });

  it("ignores drags that are not panels", () => {
    const dt = transfer();
    dt.setData("application/x-feldspar-column", JSON.stringify({ field: "area" }));
    expect(carriesPanel(dt)).toBe(false);
    expect(readPanelDrag(dt)).toBeNull();
    dt.setData(PANEL_MIME, "not json");
    expect(readPanelDrag(dt)).toBeNull();
  });

  it("makes the explorer's output a panel: its plot with its tests as one, or its table", () => {
    const plot = explorerPanel({ view: "plot", spec, table, tests: null }, "price by area");
    expect(plot?.kind).toBe("plot");
    expect(plot?.title).toBe("price by area");
    const withTests = explorerPanel({ view: "plot", spec, table, tests });
    expect(withTests?.kind).toBe("test_result");
    expect(withTests?.content).toEqual({ tests, plot: spec });
    expect(explorerPanel({ view: "table", spec, table, tests })?.kind).toBe("summary_table");
    // Nothing drawn yet is nothing to drag.
    expect(explorerPanel({ view: "plot", spec: null, table, tests })).toBeNull();
    expect(explorerPanel({ view: "table", spec, table: null, tests: null })).toBeNull();

    expect(explorerTitle(["price"], "neighbourhood", "Houses", t)).toBe("price by neighbourhood — Houses");
    expect(explorerTitle(["before", "after"], undefined, undefined, t)).toBe("before, after");
    expect(explorerTitle([], undefined, "Houses", t)).toBe("Houses");
  });

  it("makes a fit's output a panel: a plot by its spec, a table by the fit and its name", () => {
    const residuals: ModelOutput = {
      name: "residuals",
      label: "Residuals against fitted",
      optional: false,
      kind: "plot",
      spec: { ...spec, data: { kind: "fit_output", instance: "f1", name: "rows" } } as PlotSpec,
    };
    const coefficients: ModelOutput = { name: "coefficients", label: "Coefficients", optional: false, kind: "table" };
    const p = outputPanel(residuals, "f1", "Prices", t);
    expect(p?.kind).toBe("plot");
    expect(p?.title).toBe("Residuals against fitted — Prices");
    expect((p?.content as { spec: PlotSpec }).spec.data).toEqual({ kind: "fit_output", instance: "f1", name: "rows" });
    expect(outputPanel(coefficients, "f1", undefined, t)).toMatchObject({
      kind: "fit_table",
      title: "Coefficients",
      content: { fit: "f1", output: "coefficients" },
    });
    expect(outputPanel({ ...coefficients, error: "this fit has no metrics" }, "f1", undefined, t)).toBeNull();
  });
});

describe("a map as a panel", () => {
  const spec = {
    layers: [{ id: "a", dataset: "d1", geometry: { kind: "column" as const, column: "at" }, style: { kind: "categories" as const } }],
    view: { center: [0, 51] as [number, number], zoom: 10 },
  };

  it("is its spec, copied, and reads back as a panel", () => {
    const panel = mapPanel(spec, "Incidents");
    expect(panel?.kind).toBe("map");
    expect(panel?.title).toBe("Incidents");
    const content = (panel as Extract<typeof panel, { kind: "map" }>).content;
    expect(content.spec).toEqual(spec);
    expect(content.spec).not.toBe(spec);
    expect(readPanel(JSON.parse(JSON.stringify(panel)))).toEqual(panel);
    expect(mapPanel({ layers: [] })).toBeNull();
    expect(mapPanel(null)).toBeNull();
    expect(readPanel({ id: "x", kind: "map", content: { spec: { layers: [] } } })).toBeNull();
  });
});
