import { describe, expect, it } from "vitest";

import { readPanel, readPanelDrag, setPanelDrag, type Transfer } from "../panels/panel";
import {
  MAX_SCENARIOS,
  applyScenario,
  currentValue,
  freeName,
  intervalText,
  isMoved,
  nextScenarioName,
  parseControl,
  pickModel,
  profilerPanel,
  readSimState,
  removeScenario,
  saveScenario,
  scenariosPanel,
  scenariosToCompare,
  scoreDefinition,
  setValue,
  simulatable,
  sliderStep,
  type Input,
  type SimulationState,
} from "./simulation";

const t = (text: string, args?: Record<string, string | number>) =>
  text.replace(/\{(\w+)\}/g, (_, k: string) => String(args?.[k] ?? ""));

const area: Input = { name: "area", kind: "number", typical: 120, min: 40, max: 300 };
const rooms: Input = { name: "bedrooms", kind: "integer", typical: 3, min: 1, max: 6 };
const hood: Input = { name: "neighbourhood", kind: "category", typical: 2, levels: [1, 2, 3] };
const sold: Input = { name: "sold", kind: "boolean", typical: false, levels: [false, true] };
const built: Input = { name: "built", kind: "date", typical: "1990-06-01", min: "1950-01-01", max: "2020-01-01" };

describe("the state", () => {
  it("reads what was stored, dropping what is not a scenario", () => {
    const s = readSimState({
      model: "m1",
      level: 0.8,
      values: { area: 150 },
      scenarios: [{ name: " baseline ", values: { area: 100 } }, { name: "" }, "junk", { values: {} }],
    });
    expect(s).toEqual({ model: "m1", level: 0.8, values: { area: 150 }, scenarios: [{ name: "baseline", values: { area: 100 } }] });
    expect(readSimState(null)).toEqual({ values: {}, scenarios: [] });
    expect(readSimState({ level: 2 }).level).toBeUndefined();
  });

  it("forgets the inputs and scenarios when another model is picked, but keeps the level", () => {
    const s: SimulationState = { model: "m1", fit: "f1", level: 0.8, values: { area: 1 }, scenarios: [{ name: "a", values: {} }] };
    expect(pickModel(s, "m1")).toBe(s);
    expect(pickModel(s, "m2")).toEqual({ model: "m2", level: 0.8, values: {}, scenarios: [] });
  });

  it("sets an input, and puts it back at its typical value", () => {
    let s: SimulationState = { model: "m", values: {}, scenarios: [] };
    s = setValue(s, "area", 200);
    expect(currentValue(area, s.values)).toBe(200);
    expect(isMoved(area, s.values)).toBe(true);
    s = setValue(s, "area", undefined);
    expect(s.values).toEqual({});
    expect(currentValue(area, s.values)).toBe(120);
    expect(isMoved(area, { area: 120 })).toBe(false);
  });

  it("saves, replaces, applies and removes scenarios", () => {
    let s: SimulationState = { model: "m", values: { area: 100 }, scenarios: [] };
    s = saveScenario(s, "baseline", t).state;
    s = saveScenario(setValue(s, "neighbourhood", 3), "north side", t).state;
    expect(s.scenarios.map((x) => x.name)).toEqual(["baseline", "north side"]);
    expect(s.scenarios[1].values).toEqual({ area: 100, neighbourhood: 3 });
    // The same name takes the new values in its place.
    s = saveScenario(setValue(s, "area", 90), "baseline", t).state;
    expect(s.scenarios.map((x) => x.name)).toEqual(["baseline", "north side"]);
    expect(s.scenarios[0].values).toEqual({ area: 90, neighbourhood: 3 });
    // A saved scenario does not change with the inputs afterwards.
    s = setValue(s, "area", 1);
    expect(s.scenarios[0].values.area).toBe(90);
    expect(applyScenario(s, "north side").values).toEqual({ area: 100, neighbourhood: 3 });
    expect(removeScenario(s, "baseline").scenarios.map((x) => x.name)).toEqual(["north side"]);
    expect(saveScenario(s, "  ", t).error).toContain("name");
  });

  it("refuses a scenario past the most a simulation keeps", () => {
    const full: SimulationState = {
      model: "m",
      values: {},
      scenarios: Array.from({ length: MAX_SCENARIOS }, (_, i) => ({ name: `s${i}`, values: {} })),
    };
    const { state, error } = saveScenario(full, "one more", t);
    expect(state).toBe(full);
    expect(error).toContain(String(MAX_SCENARIOS));
    // Replacing one is still fine.
    expect(saveScenario(full, "s3", t).error).toBeUndefined();
  });

  it("names a new scenario with a name no scenario has", () => {
    const s: SimulationState = { values: {}, scenarios: [{ name: "Scenario 1", values: {} }] };
    expect(nextScenarioName(s, t)).toBe("Scenario 2");
  });

  it("compares the saved scenarios, with the current inputs when they are not one of them", () => {
    const s: SimulationState = { model: "m", values: { area: 100 }, scenarios: [{ name: "a", values: { area: 100 } }] };
    expect(scenariosToCompare(s, t).map((x) => x.name)).toEqual(["a"]);
    const moved = setValue(s, "area", 150);
    expect(scenariosToCompare(moved, t)).toEqual([
      { name: "a", values: { area: 100 } },
      { name: "Current", values: { area: 150 } },
    ]);
    const clash = { ...moved, scenarios: [{ name: "Current", values: {} }] };
    expect(scenariosToCompare(clash, t)[1].name).toBe("Current (2)");
  });
});

describe("the controls", () => {
  it("steps a slider by about a hundredth of the range", () => {
    expect(sliderStep(area)).toBe(3);
    expect(sliderStep(rooms)).toBe(1);
    expect(sliderStep({ ...area, min: 0, max: 0.5 })).toBe(0.005);
    expect(sliderStep({ ...area, min: 5, max: 5 })).toBe(1);
  });

  it("reads a control's text as the input's kind", () => {
    expect(parseControl(area, "12.5")).toBe(12.5);
    expect(parseControl(area, "")).toBeUndefined();
    expect(parseControl(area, "abc")).toBeUndefined();
    expect(parseControl(rooms, "2.6")).toBe(3);
    expect(parseControl(hood, "3")).toBe(3);
    expect(parseControl(hood, "9")).toBeUndefined();
    expect(parseControl({ ...hood, levels: ["north", "south"] }, "south")).toBe("south");
    expect(parseControl(sold, "true")).toBe(true);
    expect(parseControl(built, "2001-02-03")).toBe("2001-02-03");
    expect(parseControl(built, "soon")).toBeUndefined();
  });

  it("says the interval in words", () => {
    expect(intervalText({ value: 5, lower: 4, upper: 6.5, level: 0.8 }, t, "en")).toBe("80 % interval 4 to 6.5");
    expect(intervalText({ value: 5 }, t, "en")).toBeNull();
  });

  it("offers the models that have a fit to predict with", () => {
    const models = [
      { id: "b", name: "B", active_instance: { status: "fitted" } },
      { id: "c", name: "C", last_fit: { status: "failed" } },
      { id: "a", name: "A", last_fit: { status: "fitted" } },
      { id: "d", name: "D" },
    ];
    expect(simulatable(models).map((m) => m.id)).toEqual(["a", "b"]);
  });
});

describe("scoring", () => {
  it("makes a dataset based on the source with a Model predictions operation", () => {
    const def = scoreDefinition(
      { id: "d1", name: "Unsold houses", columns: [{ name: "price" }, { name: "predicted" }] },
      { name: "House prices" },
      { level: 0.8 },
      t,
    );
    expect(def).toEqual({
      name: "Unsold houses scored by House prices",
      base: { kind: "dataset", dataset: "d1" },
      operations: [
        {
          id: "score",
          enabled: true,
          kind: "model_predictions",
          params: { model: "House prices", name: "predicted_2", interval: true, level: 0.8 },
        },
      ],
    });
    const plain = scoreDefinition({ id: "d1", name: "U", columns: [] }, { name: "M" }, { fit: "f1", level: 0.9 }, t);
    expect(plain.operations[0].params).toEqual({ model: "M", name: "predicted", interval: true, fit: "f1" });
  });

  it("names it so that no dataset has the name", () => {
    expect(freeName("X", ["Y"])).toBe("X");
    expect(freeName("X", ["X", "X (2)"])).toBe("X (3)");
  });
});

describe("panels", () => {
  const state: SimulationState = {
    model: "m1",
    level: 0.8,
    values: { area: 150 },
    scenarios: [{ name: "baseline", values: { area: 100 } }],
  };

  it("makes the profiler and the comparison as panels that travel by drag", () => {
    const profiler = profilerPanel(state, "House prices", t)!;
    expect(profiler.kind).toBe("profiler");
    expect(profiler.title).toBe("Profiler — House prices");
    expect(profiler.content).toEqual({ model: "m1", level: 0.8, values: { area: 150 }, scenarios: state.scenarios });
    // A copy: changing the workspace afterwards does not reach the panel.
    state.values.area = 1;
    expect((profiler.content as { values: { area: number } }).values.area).toBe(150);
    state.values.area = 150;

    const data = new Map<string, string>();
    const transfer: Transfer = { types: [], setData: (k, v) => data.set(k, v), getData: (k) => data.get(k) ?? "" };
    setPanelDrag(transfer, profiler);
    const dropped = readPanelDrag(transfer)!;
    expect(dropped.kind).toBe("profiler");
    expect(dropped.id).not.toBe(profiler.id);

    const compared = scenariosPanel(state, undefined, t)!;
    expect(compared.kind).toBe("scenarios");
    expect(compared.title).toBe("Scenarios");
    expect(readPanel(compared)).not.toBeNull();
  });

  it("makes no panel without a model, and no comparison without a scenario", () => {
    expect(profilerPanel({ values: {}, scenarios: [] }, undefined, t)).toBeNull();
    expect(scenariosPanel({ ...state, scenarios: [] }, undefined, t)).toBeNull();
    expect(readPanel({ id: "p", kind: "scenarios", content: { model: "m", scenarios: [] } })).toBeNull();
    expect(readPanel({ id: "p", kind: "profiler", content: { values: {} } })).toBeNull();
  });
});
