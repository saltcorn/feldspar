import { describe, expect, it } from "vitest";

import { format } from "../i18n";
import { parseMarkdown } from "../panels/markdown";
import { buildFitReport, fitReportName, mdTable, mdText } from "./fitReport";
import type { InstanceDetail } from "./models";
import type { ModelOutput } from "./outputs";

const t = (text: string, args?: Record<string, string | number>) => format(text, args);

const FIT = "6f1c0d6e-1d1e-4a57-9a1c-1b0b9d4c2a11";

function instance(over: Partial<InstanceDetail> = {}): InstanceDetail {
  return {
    id: FIT,
    model: "m1",
    name: "",
    description: "",
    status: "fitted",
    created: "2026-11-19T10:00:00Z",
    active: true,
    error: null,
    hyperparameters: { alpha: 0.5 },
    outcome: { outcome: "regression", label: "price" },
    metrics: {},
    rows: { selected: 100, train: 70, validation: 10, test: 20, dropped: 3 },
    progress: null,
    cancel_requested: false,
    warnings: [],
    dataset_changed: true,
    report: null,
    parameters: [],
    encoding: { columns: [{ encoding: "passthrough", column: "area" }, { encoding: "passthrough", column: "rooms" }] },
    search: [
      { hyperparameters: { alpha: 0.5 }, score: 0.81 },
      { hyperparameters: { alpha: 1 }, score: 0.7 },
    ],
    variables: {},
    ...over,
  } as InstanceDetail;
}

const outputs: ModelOutput[] = [
  { name: "coefficients", label: "Coefficients", optional: false, kind: "table", table: { columns: [], rows: [], truncated: false } },
  {
    name: "residuals",
    label: "Residuals",
    optional: false,
    kind: "plot",
    spec: { data: { kind: "fit_output", instance: FIT, name: "rows" } } as unknown as ModelOutput["spec"],
  },
  { name: "broken", label: "Broken", optional: false, kind: "plot", error: "cannot draw this" },
];

const model = { name: "House prices", description: "Prices | by area", provider: "linear_regression" };

describe("a fit's report", () => {
  const report = buildFitReport({
    model,
    providerDescription: "Ordinary least squares",
    datasetName: "Sales",
    instance: instance(),
    outputs,
    t,
  });

  it("opens with what was fitted, to which data, when and how", () => {
    expect(report.blocks[0]).toMatchObject({ kind: "heading", level: 1 });
    const facts = report.blocks.find((b) => b.kind === "text" && b.markdown.includes("Hyperparameters"));
    if (facts?.kind !== "text") throw new Error("no facts");
    const table = parseMarkdown(facts.markdown)[0];
    expect(table.kind).toBe("table");
    for (const fact of ["House prices", "linear\\_regression", "Ordinary least squares", "Sales", "price", "area", "rooms", "alpha", FIT]) {
      expect(facts.markdown).toContain(fact);
    }
    // The model's description is text, not markup: its pipe is escaped.
    expect(report.blocks.some((b) => b.kind === "text" && b.markdown === "Prices \\| by area")).toBe(true);
  });

  it("says when the dataset has changed since, and how many rows were dropped", () => {
    const all = report.blocks.map((b) => (b.kind === "text" ? b.markdown : "")).join("\n");
    expect(all).toContain("The dataset has changed since this fit");
    expect(all).toContain("3 rows could not be represented");
  });

  it("makes each output that draws a panel of the fit, and names the ones that do not", () => {
    const panels = report.blocks.flatMap((b) => (b.kind === "panel" ? [b.panel] : []));
    expect(panels.map((p) => [p.kind, p.title])).toEqual([
      ["fit_table", "Coefficients"],
      ["plot", "Residuals"],
    ]);
    expect(panels[0]).toMatchObject({ content: { fit: FIT, output: "coefficients" } });
    const all = report.blocks.map((b) => (b.kind === "text" ? b.markdown : "")).join("\n");
    expect(all).toContain("**Broken:** cannot draw this");
  });

  it("lists the search with the chosen point marked", () => {
    const search = report.blocks.find((b) => b.kind === "text" && b.markdown.includes("Validation score"));
    if (search?.kind !== "text") throw new Error("no search");
    const table = parseMarkdown(search.markdown)[0];
    if (table.kind !== "table") throw new Error("not a table");
    expect(table.rows).toHaveLength(2);
    expect(search.markdown).toContain("chosen");
  });

  it("puts a posterior's warnings first, and its diagnostics, rather than its rows", () => {
    const posterior = buildFitReport({
      model,
      instance: instance({
        outcome: { outcome: "posterior" },
        rows: null,
        search: [],
        warnings: ["5 of 4000 draws ended with a divergent transition."],
        metrics: {
          train: {
            metrics: "posterior",
            chains: 4,
            draws_per_chain: 1000,
            divergent: 5,
            divergent_per_chain: [5, 0, 0, 0],
            max_treedepth_hits: 0,
            ebfmi: [0.9, 0.8, 0.85, 0.9],
            max_rhat: 1.001,
            min_ess_bulk: 900,
            min_ess_tail: 800,
            wall_seconds: [1, 1, 1, 1],
          },
        },
      }),
      outputs: [],
      t,
    });
    const headings = posterior.blocks.flatMap((b) => (b.kind === "heading" ? [b.text] : []));
    expect(headings.slice(0, 3)).toEqual([headings[0], "Warnings", "Diagnostics"]);
    expect(headings).not.toContain("Rows");
    expect(headings).not.toContain("Hyperparameter search");
    const warnings = posterior.blocks.find((b) => b.kind === "text" && b.markdown.includes("divergent"));
    expect(warnings).toMatchObject({ markdown: expect.stringContaining("**Divergent transitions.**") });
  });

  it("is a report the workspace reads back as it was built", async () => {
    const { readReport } = await import("../report/state");
    expect(readReport(JSON.parse(JSON.stringify(report)) as Record<string, unknown>)).toEqual(report);
  });

  it("is offered under the model's and the fit's names", () => {
    expect(fitReportName("House prices", { name: "baseline", created: "2026-11-19T10:00:00Z" }, t)).toBe(
      "House prices — baseline report",
    );
  });
});

describe("Markdown written into a report", () => {
  it("escapes markup and keeps a line", () => {
    expect(mdText("a*b* | `c`\nd")).toBe("a\\*b\\* \\| \\`c\\` d");
    expect(parseMarkdown(mdTable(["x", "y"], [[mdText("a|b"), "1"]]))[0]).toMatchObject({ kind: "table" });
  });
});
