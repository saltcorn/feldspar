/**
 * The posterior half of the model screens' helpers (Stan TODO 8.3): the
 * binding editor and the instance screen.
 *
 * Each is here because getting it wrong is quiet on the screen:
 *
 *   - a kind picker offering a kind the server refuses for that declaration is
 *     a form that lets the admin pick a mistake and learn of it on save;
 *   - a binding row whose print and parse are not inverses changes the
 *     configuration merely by being opened and saved;
 *   - warnings in the wrong order bury "the chains disagree" under "the tree
 *     depth was hit", which are not the same kind of news;
 *   - the forest plot's sort is the plot's meaning — by mean it is the spread
 *     of the groups, by label it is a lookup;
 *   - and an element picked by key or label must be the element the server
 *     means by that position, or the trace plot is of somebody else's county.
 */

import { describe, expect, it } from "vitest";

import {
  BINDING_KINDS,
  DIMENSION_KINDS,
  applySuggestions,
  buildPolicies,
  buildPosteriorWrite,
  chainPercent,
  chainTraces,
  dimensionNames,
  elementAt,
  elementName,
  elementSelection,
  forestRows,
  forestable,
  headlineMetric,
  histogram,
  kindsFor,
  matchElements,
  metricRows,
  orderWarnings,
  outcomeSummary,
  isPosteriorStage,
  parseDraft,
  parseDrafts,
  printDraft,
  printDrafts,
  readInterface,
  readOutcome,
  readPolicies,
  readProgress,
  readRelated,
  readVariables,
  shapeText,
  summaryTable,
  updateTarget,
  type Declaration,
  type Metrics,
} from "./models";

/** A declaration of `element` with the given size expressions. */
function decl(element: Declaration["element"], ...dims: string[]): Declaration {
  return { name: "v", element, dims: dims.map((text) => ({ text })), stan_type: "" };
}

describe("which binding kinds fit a declaration", () => {
  it("offers a scalar int the counts and sizes, and never a per-row kind", () => {
    const kinds = kindsFor(decl("int"));
    for (const k of ["value", "count", "size", "width", "count_present", "edge_count", "components"]) {
      expect(kinds).toContain(k);
    }
    for (const k of ["column", "index", "columns", "design"]) expect(kinds).not.toContain(k);
    // A scaling factor is a real, so an `int` cannot hold one.
    expect(kinds).not.toContain("icar_scale");
    expect(kindsFor(decl("real"))).toContain("icar_scale");
  });

  it("offers a one-axis variable the per-row kinds, an index included", () => {
    const ints = kindsFor(decl("int", "N"));
    expect(ints).toEqual(expect.arrayContaining(["column", "index", "present", "segment_start", "series", "edge_from", "component"]));
    expect(ints).not.toContain("count");
    const reals = kindsFor(decl("real", "N"));
    // An int binding fits a real variable (Stan reads ints as reals); only the
    // other way round is refused.
    expect(reals).toContain("index");
    expect(reals).toContain("column");
  });

  it("offers a matrix the two-axis kinds, and keeps the real-only ones from an int array", () => {
    expect(kindsFor(decl("real", "N", "K"))).toEqual(
      expect.arrayContaining(["columns", "design", "cells", "adjacency", "points", "distances"]),
    );
    const ints = kindsFor(decl("int", "N", "K"));
    expect(ints).toEqual(expect.arrayContaining(["columns", "cells_present", "adjacency"]));
    for (const k of ["design", "points", "distances"]) expect(ints).not.toContain(k);
  });

  it("offers a value to any rank, and nothing to what the data block refuses", () => {
    expect(kindsFor(decl("real", "3", "3"))).toContain("value");
    expect(kindsFor(decl("complex"))).toEqual([]);
    expect(kindsFor(decl("tuple", "N"))).toEqual([]);
  });

  it("knows every kind the server does", () => {
    // `sc_model::Binding::kind`, in its own order.
    expect(BINDING_KINDS.map((k) => k.kind)).toEqual([
      "value", "count", "size", "column", "columns", "design", "width", "index", "present",
      "absent", "count_present", "count_absent", "present_values", "segment_start",
      "segment_size", "series", "series_present", "cells", "cells_present", "edge_count",
      "edge_from", "edge_to", "adjacency", "components", "component", "icar_scale", "points",
      "distances",
    ]);
  });
});

describe("the binding editor's print and parse", () => {
  // Bindings as the configuration writes them — the radon program's, and one of
  // each shape of field.
  const bindings: Record<string, unknown>[] = [
    { kind: "count", dataset: "main" },
    { kind: "size", dimension: "counties" },
    { kind: "index", dataset: "main", column: "fips", dimension: "counties", match: "fips" },
    { kind: "column", dataset: "main", column: "at", time: { unit: "hours", origin: "min" } },
    { kind: "design", dataset: "main", columns: ["floor", "basement"], standardise: true },
    { kind: "value", value: [[1, 0], [0, 1]] },
    { kind: "width", of: "X" },
    {
      kind: "series", dataset: "main", column: "amount",
      over: { dimension: "day" }, aggregate: "sum", fill: 0,
    },
    { kind: "series", dataset: "main", column: "temp", over: { dimension: "day", column: "at" }, fill: "NaN" },
    {
      kind: "cells", dataset: "main", column: "value",
      rows: { dimension: "sensors", column: "sensor" }, cols: { dimension: "hour", column: "at" },
    },
    { kind: "edge_from", dataset: "adjacency", from: "a", to: "b", dimension: "regions", symmetric: "keep" },
    { kind: "points", dataset: "sites", lat: "lat", lon: "lon", project: true },
  ];

  it("round-trips every binding the form can write", () => {
    for (const binding of bindings) {
      const draft = printDraft(BINDING_KINDS, binding);
      expect(parseDraft(BINDING_KINDS, draft), JSON.stringify(binding)).toEqual({
        value: binding,
        problems: [],
      });
    }
  });

  it("prints a list comma-separated, a literal as JSON and a flag as true", () => {
    expect(printDraft(BINDING_KINDS, bindings[4]).fields).toEqual({
      dataset: "main",
      columns: "floor, basement",
      standardise: "true",
    });
    expect(printDraft(BINDING_KINDS, bindings[5]).fields).toEqual({ value: "[[1,0],[0,1]]" });
    expect(printDraft(BINDING_KINDS, bindings[7]).fields["over.dimension"]).toBe("day");
  });

  it("leaves out what is blank, and says early what the server would refuse", () => {
    const parsed = parseDraft(BINDING_KINDS, {
      kind: "index",
      fields: { dataset: "main", column: " county ", dimension: "", match: "" },
    });
    expect(parsed.value).toEqual({ kind: "index", dataset: "main", column: "county" });
    expect(parsed.problems).toEqual(["`dimension` is required"]);
    // A literal that is not JSON goes as its text, for the server to name.
    const literal = parseDraft(BINDING_KINDS, { kind: "value", fields: { value: "[1, 2" } });
    expect(literal).toEqual({ value: { kind: "value", value: "[1, 2" }, problems: ["`value` is not a JSON value"] });
    // An unticked flag is absent, which is what the server's default means.
    expect(parseDraft(BINDING_KINDS, { kind: "design", fields: { dataset: "main", columns: "x", standardise: "" } }).value)
      .toEqual({ kind: "design", dataset: "main", columns: ["x"] });
  });

  it("keeps a kind it does not know whole, so opening and saving loses nothing", () => {
    const future = { kind: "splines", dataset: "main", knots: 5 };
    const draft = printDraft(BINDING_KINDS, future);
    expect(draft.kind).toBe("splines");
    expect(parseDraft(BINDING_KINDS, draft).value).toEqual(future);
  });

  it("reads and writes the whole map, leaving the rows with no kind unbound", () => {
    const stored = { N: bindings[0], J: bindings[1] };
    const drafts = printDrafts(BINDING_KINDS, stored);
    expect(parseDrafts(BINDING_KINDS, { ...drafts, y: { kind: "", fields: {} } })).toEqual(stored);
  });

  it("does the same for the declared dimensions", () => {
    const grid = { kind: "time_grid", dataset: "main", column: "day", step: "1 day", horizon: 14 };
    expect(parseDraft(DIMENSION_KINDS, printDraft(DIMENSION_KINDS, grid)).value).toEqual(grid);
    expect(dimensionNames(["main", "counties"], printDrafts(DIMENSION_KINDS, { day: grid, region: { kind: "values", dataset: "main", column: "region" } })))
      .toEqual(["main", "counties", "day", "day.future", "region"]);
  });
});

describe("Bind automatically", () => {
  it("fills only the empty rows", () => {
    const drafts = {
      N: { kind: "", fields: {} },
      y: printDraft(BINDING_KINDS, { kind: "column", dataset: "main", column: "log_radon" }),
    };
    const { drafts: out, filled } = applySuggestions(drafts, {
      N: { kind: "count", dataset: "main" },
      // A row the admin filled while the request was out is theirs.
      y: { kind: "column", dataset: "main", column: "y" },
      J: { kind: "size", dimension: "counties" },
    });
    expect(filled).toEqual(["N", "J"]);
    expect(out.y.fields.column).toBe("log_radon");
    expect(out.N).toEqual({ kind: "count", fields: { dataset: "main" } });
  });
});

describe("the policies", () => {
  it("writes only what is not the default", () => {
    const read = readPolicies({ main: { nulls: "drop" }, counties: {} });
    expect(read).toEqual({
      main: { nulls: "drop", unknown: "refuse" },
      counties: { nulls: "refuse", unknown: "refuse" },
    });
    expect(buildPolicies(read)).toEqual({ main: { nulls: "drop" } });
  });
});

describe("reading what the API carries about a posterior", () => {
  it("reads an interface and related datasets", () => {
    const iface = readInterface({ data: [{ name: "N", element: "int", dims: [], stan_type: "int" }, { junk: 1 }] });
    expect(iface?.data.map((d) => d.name)).toEqual(["N"]);
    expect(iface?.parameters).toEqual([]);
    expect(readInterface(null)).toBeNull();
    expect(
      readRelated([
        { name: "counties", dataset_id: "d2", dataset_name: "Radon — counties", table: "counties", label: "name" },
        { nope: 1 },
      ]),
    ).toEqual([
      {
        name: "counties",
        dataset_id: "d2",
        label: "name",
        dataset_name: "Radon — counties",
        table: "counties",
        columns: [],
        error: null,
      },
    ]);
  });

  it("reads a posterior outcome and its metrics", () => {
    expect(outcomeSummary(readOutcome({ outcome: "posterior" }))).toBe("Posterior");
    const metrics: Metrics = {
      metrics: "posterior", chains: 4, draws_per_chain: 1000, divergent: 3,
      divergent_per_chain: [0, 3, 0, 0], max_treedepth_hits: 0, ebfmi: [0.9, 1.1, null, 0.8],
      max_rhat: 1.0042, min_ess_bulk: 812.4, min_ess_tail: 950.1, wall_seconds: [1.5, 1.6, 1.4, 1.5],
    };
    const rows = metricRows(metrics);
    expect(rows.find((r) => r.label === "Divergent transitions")?.value).toBe("3 (0 / 3 / 0 / 0)");
    expect(rows.find((r) => r.label === "E-BFMI per chain")?.value).toBe("0.9 / 1.1 / — / 0.8");
    expect(headlineMetric({ train: metrics })).toBe("R̂ ≤ 1.004");
  });

  it("reads progress chain by chain", () => {
    const progress = readProgress({
      stage: "sampling",
      chains: [
        { chain: 2, iteration: 500, total: 2000, phase: "warmup" },
        { chain: 1, iteration: 1999, total: 2000, phase: "sampling" },
      ],
    });
    expect(progress?.chains.map((c) => c.chain)).toEqual([1, 2]);
    expect(progress?.chains.map(chainPercent)).toEqual([100, 25]);
    expect(readProgress({ stage: "queued" })).toEqual({ stage: "queued", chains: [] });
    expect(readProgress(null)).toBeNull();
  });

  it("tells a posterior's stages from the ones every fit reports", () => {
    // Every fit reports where it is now, so progress alone no longer means a
    // posterior: only its own four stages do.
    expect(["queued", "compiling", "sampling", "summarising"].every(isPosteriorStage)).toBe(true);
    expect(["reading", "fitting", "scoring"].some(isPosteriorStage)).toBe(false);
    expect(isPosteriorStage(undefined)).toBe(false);
  });

  it("leaves the sampler's own variables out of the list", () => {
    expect(
      Object.keys(readVariables({ alpha: { dims: [3], dimensions: ["counties"] }, lp__: { dims: [], dimensions: [] }, beta: { dims: [] } })),
    ).toEqual(["alpha", "beta"]);
  });

  it("says a shape the way it reads", () => {
    expect(shapeText([919])).toBe("919");
    expect(shapeText([85, 3])).toBe("85 × 3");
    expect(shapeText([])).toBe("scalar");
  });
});

describe("the warnings", () => {
  // The host's sentences (`sc_model::diagnose`), in the host's order.
  const treedepth = "4 iterations hit the maximum tree depth of 10: …";
  const divergent = "12 divergent transitions after warmup (in chain 2): …";
  const ess = "the bulk effective sample size is 212 for `sigma_a`, below 400 (100 per chain): …";
  const rhat = "R̂ is 1.052 for `alpha[Aitkin]` (and 3 other elements), above 1.01: …";
  const ebfmi = "E-BFMI is 0.21 in chain 3 (below 0.3): …";
  const other = "the draws of `y_rep` were dropped: they would have been 2 GB";

  it("puts the chains disagreeing first and a saturated tree depth last", () => {
    const ordered = orderWarnings([divergent, treedepth, ess, other, rhat, ebfmi]);
    expect(ordered.map((w) => w.kind)).toEqual(["rhat", "divergent", "ebfmi", "other", "ess", "treedepth"]);
    expect(ordered.map((w) => w.text)).toEqual([rhat, divergent, ebfmi, other, ess, treedepth]);
    expect(ordered.filter((w) => w.serious).map((w) => w.kind)).toEqual(["rhat", "divergent"]);
  });

  it("keeps the host's order within a kind", () => {
    const tail = ess.replace("bulk", "tail");
    expect(orderWarnings([tail, ess]).map((w) => w.text)).toEqual([tail, ess]);
  });
});

describe("a summary, and the element an admin means", () => {
  // `alpha` over three counties, as the stored table has it: the label column,
  // then the statistics.
  const columns = ["counties", "mean", "sd", "mcse", "q5", "q50", "q95", "rhat", "ess_bulk", "ess_tail"];
  const rows = [
    ["Aitkin", 0.9, 0.3, 0.01, 0.4, 0.9, 1.4, 1.0, 900, 800],
    ["Anoka", 1.9, 0.1, 0.01, 1.7, 1.9, 2.1, 1.0, 950, 870],
    ["Becker", 1.5, 0.6, 0.02, 0.5, 1.5, 2.5, 1.0, 700, 650],
  ];
  const table = summaryTable(columns, rows, 1, [["27001"], ["27003"], ["27005"]]);

  it("splits the label columns from the statistics", () => {
    expect(table.labelColumns).toEqual(["counties"]);
    expect(table.statColumns[0]).toBe("mean");
    expect(table.rows[1].labels).toEqual(["Anoka"]);
  });

  it("sorts the forest plot by label, by mean, or as the dimension numbers it", () => {
    expect(forestRows(table, "position").map((r) => r.label)).toEqual(["Aitkin", "Anoka", "Becker"]);
    expect(forestRows(table, "mean").map((r) => r.label)).toEqual(["Aitkin", "Becker", "Anoka"]);
    const numbered = summaryTable(["index", "mean", "q5", "q95"], [["10", 1, 0, 2], ["9", 2, 1, 3], ["1", 0, -1, 1]], 1);
    // Numbers as numbers: `9` before `10`.
    expect(forestRows(numbered, "label").map((r) => r.label)).toEqual(["1", "9", "10"]);
    expect(forestRows(table, "mean")[0]).toEqual({ row: 0, label: "Aitkin", low: 0.4, centre: 0.9, high: 1.4 });
  });

  it("draws a forest only of a labelled axis with something to compare", () => {
    expect(forestable({ dims: [85], dimensions: ["counties"] })).toBe(true);
    expect(forestable({ dims: [85], dimensions: [null] })).toBe(false);
    expect(forestable({ dims: [1], dimensions: ["counties"] })).toBe(false);
    expect(forestable({ dims: [3, 3], dimensions: ["regions", "regions"] })).toBe(false);
  });

  it("picks an element by key, by label, by part of a label, or by position", () => {
    expect(matchElements(table, "27003")).toEqual([1]);
    expect(matchElements(table, "Becker")).toEqual([2]);
    expect(matchElements(table, "ai")).toEqual([0]);
    expect(matchElements(table, "#3")).toEqual([2]);
    expect(matchElements(table, "#9")).toEqual([]);
    expect(matchElements(table, "")).toEqual([0, 1, 2]);
  });

  it("finds an element's position the way the host orders them, last axis fastest", () => {
    expect(elementAt(0, [])).toEqual([]);
    expect(elementAt(2, [85])).toEqual([3]);
    // `Sigma` is 2 × 3: row 4 is Sigma[2, 2].
    expect(elementAt(4, [2, 3])).toEqual([2, 2]);
    expect(elementSelection([2, 2])).toBe("[[2,2]]");
    expect(elementName("alpha", ["Aitkin"], [1])).toBe("alpha[Aitkin]");
    expect(elementName("Sigma", ["", ""], [2, 1])).toBe("Sigma[2, 1]");
    expect(elementName("beta", [], [])).toBe("beta");
  });
});

describe("the plots' arithmetic", () => {
  it("reads one element's chains, leaving warmup out", () => {
    expect(
      chainTraces({
        chains: [
          { chain: 2, warmup: false, draws: [[1, null, 3]] },
          { chain: 1, warmup: true, draws: [[9]] },
          { chain: 1, warmup: false, draws: [[4, 5]] },
        ],
      }),
    ).toEqual([
      { chain: 1, values: [4, 5] },
      { chain: 2, values: [1, Number.NaN, 3] },
    ]);
  });

  it("bins every finite draw exactly once", () => {
    const values = Array.from({ length: 1000 }, (_, i) => Math.sin(i) * 3);
    const bins = histogram([...values, Number.NaN]);
    expect(bins.reduce((n, b) => n + b.count, 0)).toBe(1000);
    expect(bins.length).toBeGreaterThanOrEqual(10);
    expect(bins.length).toBeLessThanOrEqual(60);
    expect(bins[0].x0).toBe(Math.min(...values));
    expect(bins[bins.length - 1].x1).toBe(Math.max(...values));
    expect(histogram([2, 2, 2])).toEqual([{ x0: 2, x1: 2, count: 3 }]);
    expect(histogram([])).toEqual([]);
  });

});

describe("the write-back", () => {
  const datasets = [
    { name: "main", table: "homes" },
    { name: "counties", table: "counties" },
  ];

  it("updates only the table a one-axis variable's rows dimension is over", () => {
    expect(updateTarget({ dims: [85], dimensions: ["counties"] }, datasets)).toBe("counties");
    expect(updateTarget({ dims: [30], dimensions: ["day.future"] }, datasets)).toBeNull();
    expect(updateTarget({ dims: [3, 3], dimensions: ["counties", "counties"] }, datasets)).toBeNull();
    expect(updateTarget({ dims: [], dimensions: [] }, datasets)).toBeNull();
  });

  it("sends an update with no table, and an insert with its coordinates", () => {
    const form = {
      mode: "update" as const,
      statistics: { mean: "alpha_mean", sd: " alpha_sd ", q5: "" },
      table: "ignored",
      coordinates: [],
      instanceField: "",
    };
    expect(buildPosteriorWrite("alpha", form)).toEqual({
      variable: "alpha",
      mode: "update",
      statistics: { mean: "alpha_mean", sd: "alpha_sd" },
    });
    expect(
      buildPosteriorWrite("y_future", {
        mode: "insert",
        statistics: { q5: "lower", q95: "upper" },
        table: "forecasts",
        coordinates: [
          { axis: "day.future", field: "day", value: "key" },
          { axis: "other", field: "", value: "label" },
        ],
        instanceField: "fit",
      }),
    ).toEqual({
      variable: "y_future",
      mode: "insert",
      statistics: { q5: "lower", q95: "upper" },
      table: "forecasts",
      coordinates: [{ axis: "day.future", field: "day", value: "key" }],
      instance_field: "fit",
    });
  });
});
