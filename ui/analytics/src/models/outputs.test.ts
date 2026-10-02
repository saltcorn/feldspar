import { describe, expect, it } from "vitest";

import { compareRows, includeParam, moreOutputs, readOutputs, readOutputsFit, shownOutputs } from "./outputs";

const wire = [
  { name: "metrics", label: "Metrics", optional: false, kind: "table", table: { columns: ["metric", "test"], rows: [["r2", 0.9]], truncated: false } },
  { name: "coefficients", label: "Coefficients", optional: false, kind: "table", table: { columns: ["term"], rows: [], truncated: false } },
  { name: "residuals_fitted", label: "Residuals", optional: false, kind: "plot", spec: { data: {}, layers: [] }, plot: { layers: [], domains: {}, facets: {}, bins: {}, warnings: [] } },
  { name: "qq", label: "Q-Q", optional: true, kind: "plot", spec: { data: {}, layers: [] } },
  { name: "residual_histogram", label: "Histogram", optional: true, kind: "plot", spec: { data: {}, layers: [] } },
  { name: "broken", label: "Broken", optional: false, kind: "plot", plot: { error: "no column", problems: [] } },
  { label: "not an output" },
];

describe("a fit's outputs", () => {
  it("reads what the server answers, a refused plot as its sentence", () => {
    const outputs = readOutputs(wire);
    expect(outputs.map((o) => o.name)).toEqual([
      "metrics",
      "coefficients",
      "residuals_fitted",
      "qq",
      "residual_histogram",
      "broken",
    ]);
    expect(outputs[2].plot?.layers).toEqual([]);
    expect(outputs[3].plot).toBeUndefined();
    expect(outputs[5].error).toBe("no column");
    expect(outputs[5].plot).toBeUndefined();
  });

  it("shows the outputs that are not optional, then the plots chosen in their order", () => {
    const outputs = readOutputs(wire);
    expect(shownOutputs(outputs, ["residual_histogram", "qq", "gone"]).map((o) => o.name)).toEqual([
      "metrics",
      "coefficients",
      "residuals_fitted",
      "broken",
      "residual_histogram",
      "qq",
    ]);
    expect(moreOutputs(outputs, ["qq"]).map((o) => o.name)).toEqual(["residual_histogram"]);
    expect(includeParam([])).toBeUndefined();
    expect(includeParam(["qq", "residual_histogram"])).toBe("qq,residual_histogram");
  });

  it("reads the fit they are of", () => {
    expect(
      readOutputsFit({ id: "f1", name: "", status: "fitted", created: "2026-10-02", active: true, dataset_changed: true }),
    ).toEqual({ id: "f1", name: "", status: "fitted", created: "2026-10-02", active: true, error: null, dataset_changed: true });
    expect(readOutputsFit(null)).toBeNull();
  });

  it("lines several models' outputs up by name for a comparison", () => {
    const a = readOutputs(wire);
    const b = readOutputs([
      { name: "metrics", label: "Metrics", optional: false, kind: "table" },
      { name: "clusters", label: "Clusters", optional: false, kind: "plot" },
    ]);
    expect(compareRows([a, b]).map((r) => r.name)).toEqual([
      "metrics",
      "coefficients",
      "residuals_fitted",
      "broken",
      "clusters",
    ]);
  });
});
