import { describe, expect, it } from "vitest";

import {
  clearRow,
  coefText,
  completionsFor,
  parsePrior,
  priorProblem,
  readRows,
  retarget,
  retargets,
  setRow,
  sourceText,
  wordAt,
  writeRows,
  type FormulaCheck,
  type TableRow,
} from "./formula";

const row = (over: Partial<TableRow>): TableRow => ({
  class: "b",
  coef: "",
  group: "",
  dpar: "",
  prior: "flat",
  source: { kind: "default" },
  whole_class: false,
  structural: false,
  ...over,
});

describe("the prior menu as the admin types", () => {
  it("reads a prior and its numbers", () => {
    expect(parsePrior("normal(0, 5)")).toEqual({ name: "normal", args: [0, 5] });
    expect(parsePrior("")).toEqual({ name: "flat", args: [] });
    expect(parsePrior("lkj_corr_cholesky(2)")).toEqual({ name: "lkj", args: [2] });
  });

  it("says each constraint the server says", () => {
    expect(priorProblem("normal(0, -1)", "b")).toBe("σ must be positive (it is -1)");
    expect(priorProblem("student_t(0, 0, 1)", "b")).toBe("ν must be positive (it is 0)");
    expect(priorProblem("uniform(1, 0)", "b")).toBe("a must be less than b (they are 1 and 0)");
    expect(priorProblem("normal(0)", "b")).toBe("`normal` takes 2 arguments (μ, σ)");
    expect(priorProblem("normal(0, sigma = 1)", "b")).toContain("without names");
    expect(priorProblem("wishart(1)", "b")).toContain("not a prior on the menu");
    expect(priorProblem("horseshoe(1)", "b")).toContain("later release");
  });

  it("allows a prior on its classes only", () => {
    expect(priorProblem("gamma(1, 1)", "b")).toContain("positive numbers only");
    expect(priorProblem("gamma(1, 1)", "sd")).toBeNull();
    expect(priorProblem("normal(0, 1)", "cor")).toContain("lkj");
    expect(priorProblem("lkj(2)", "cor")).toBeNull();
    expect(priorProblem("lkj(2)", "sd")).toContain("correlations");
    expect(priorProblem("flat", "sigma")).toContain("`b` only");
    expect(priorProblem("flat", "Intercept")).toContain("centred intercept");
    expect(priorProblem("flat", "Intercept", false)).toBeNull();
  });
});

describe("editing the table", () => {
  it("makes an inherited row explicit with its own target, and clears it back", () => {
    const area = row({ coef: "area", source: { kind: "inherited", from: "the prior row `b`" } });
    const rows = setRow([{ class: "b", prior: "normal(0, 2)" }], area, { prior: "normal(0, 10)" });
    expect(rows).toEqual([
      { class: "b", prior: "normal(0, 2)" },
      { class: "b", coef: "area", group: undefined, dpar: undefined, prior: "normal(0, 10)" },
    ]);
    expect(clearRow(rows, area)).toEqual([{ class: "b", prior: "normal(0, 2)" }]);
    // Emptied, the row goes.
    expect(setRow(rows, area, { prior: "" })).toEqual([{ class: "b", prior: "normal(0, 2)" }]);
  });

  it("keeps only what was set when it writes the rows", () => {
    const text = writeRows([{ class: "sd", group: "county", coef: "", dpar: "", prior: "normal(0, 1)" }]);
    expect(readRows(text)).toEqual([{ class: "sd", group: "county", prior: "normal(0, 1)" }]);
    expect(writeRows([])).toBe("");
    expect(readRows("not json")).toEqual([]);
  });

  it("points an unmatched row at a current coefficient", () => {
    const table = [row({ coef: "regionSouth" }), row({ coef: "", whole_class: true }), row({ class: "sd", coef: "Intercept" })];
    const gone = { class: "b", coef: "regionWest", prior: "normal(0, 1)" };
    expect(retargets(table, gone)).toEqual(["regionSouth"]);
    expect(retarget([gone], gone, "regionSouth")).toEqual([{ ...gone, coef: "regionSouth" }]);
  });

  it("reads a whole class as (all) and says where a prior came from", () => {
    expect(coefText(row({ whole_class: true }))).toBe("(all)");
    expect(coefText(row({ class: "sigma", whole_class: true }))).toBe("");
    expect(sourceText(row({ source: { kind: "inherited", from: "the prior row `b`" } }))).toBe("from `b`");
    expect(sourceText(row({ source: { kind: "default_from_data" }, rule: "student_t(3, 0, scale) of `y`" }))).toBe(
      "default, from the data",
    );
  });
});

describe("completion", () => {
  const check = {
    completions: { columns: ["floor", "county"], dpars: ["mu", "sigma"], families: [], priors: ["normal"], links: {} },
  } as unknown as FormulaCheck;

  it("offers the columns and parameters the word could be", () => {
    const text = "log_radon ~ fl";
    const { word, start } = wordAt(text, text.length);
    expect(word).toBe("fl");
    expect(start).toBe(12);
    expect(completionsFor(word, check)).toEqual(["floor"]);
    expect(completionsFor("s", check)).toEqual(["sigma", "se"]);
    expect(completionsFor("", check)).toEqual([]);
  });
});
