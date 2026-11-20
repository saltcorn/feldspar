import { describe, expect, it } from "vitest";

import {
  fitNumbers,
  hyperparameterText,
  posteriorChecks,
  splitBar,
  splitErrors,
  trustVerdict,
  tuningChanges,
  withValidation,
} from "./fitCard";
import type { PosteriorMetrics } from "./models";

const posterior = (over: Partial<PosteriorMetrics> = {}): PosteriorMetrics => ({
  chains: 4,
  draws_per_chain: 1000,
  divergent: 0,
  divergent_per_chain: [],
  max_treedepth_hits: 0,
  ebfmi: [0.9, 0.71, 0.8, 1.1],
  max_rhat: 1.003,
  min_ess_bulk: 612.4,
  min_ess_tail: 701,
  wall_seconds: [],
  ...over,
});

describe("fitNumbers", () => {
  it("numbers the oldest fit #1, whatever order they are listed in", () => {
    const numbers = fitNumbers([
      { id: "c", created: "2026-11-14T10:00:00Z" },
      { id: "a", created: "2026-11-10T10:00:00Z" },
      { id: "b", created: "2026-11-12T10:00:00Z" },
    ]);
    expect([numbers.get("a"), numbers.get("b"), numbers.get("c")]).toEqual([1, 2, 3]);
  });

  it("breaks a tie in the created time by id", () => {
    const numbers = fitNumbers([
      { id: "y", created: "2026-11-10T10:00:00Z" },
      { id: "x", created: "2026-11-10T10:00:00Z" },
    ]);
    expect(numbers.get("x")).toBe(1);
  });
});

describe("splitErrors", () => {
  it("lines a regression's splits up as R², RMSE and MAE", () => {
    const table = splitErrors({
      train: { metrics: "regression", r2: 0.97, rmse: 11900, mae: 7400, rows: 1001 },
      test: { metrics: "regression", r2: 0.91, rmse: 24300, mae: 15800, rows: 246 },
    });
    expect(table?.columns).toEqual(["Rows", "R²", "RMSE", "MAE"]);
    expect(table?.rows.map((r) => r.split)).toEqual(["train", "test"]);
    expect(table?.rows[1].cells).toEqual(["246", "0.91", "24300", "15800"]);
  });

  it("has no errors for a posterior", () => {
    expect(splitErrors({ train: { metrics: "posterior", ...posterior() } })).toBeNull();
  });
});

describe("posterior trust", () => {
  it("passes a clean run", () => {
    expect(posteriorChecks(posterior()).every((c) => c.ok)).toBe(true);
    expect(trustVerdict(posterior())).toEqual({ tone: "green", text: "trustworthy" });
  });

  it("judges ESS per chain and E-BFMI by its worst chain", () => {
    const checks = posteriorChecks(posterior({ chains: 2, min_ess_bulk: 250, ebfmi: [0.2, 0.9] }));
    expect(checks.find((c) => c.key === "ess_bulk")).toMatchObject({ threshold: "≥ 200", ok: true });
    expect(checks.find((c) => c.key === "ebfmi")).toMatchObject({ value: "0.2", ok: false });
  });

  it("names the worst problem: chains that disagree before divergences", () => {
    expect(trustVerdict(posterior({ divergent: 3 })).text).toBe("3 divergences");
    expect(trustVerdict(posterior({ divergent: 3, max_rhat: 1.2 })).text).toBe("chains disagree");
  });
});

describe("the setup column", () => {
  it("draws the split as shares", () => {
    expect(splitBar({ train: "0.8", validation: "0", test: "0.2", seed: "0" })).toEqual([
      { part: "train", percent: 80 },
      { part: "test", percent: 20 },
    ]);
  });

  it("takes validation rows from training", () => {
    expect(withValidation({ train: "0.8", validation: "0", test: "0.2", seed: "3" })).toEqual({
      train: "0.7",
      validation: "0.1",
      test: "0.2",
      seed: "3",
    });
  });

  it("says which tuning boxes differ from the fit, ignoring blank boxes", () => {
    expect(
      tuningChanges({ trees: "100, 200, 400", max_depth: "", leaf: "1" }, { trees: 200, max_depth: 0, leaf: 1 }),
    ).toEqual({ trees: "200" });
    expect(hyperparameterText({ trees: 200, leaf: 1 })).toBe("trees=200 leaf=1");
  });
});
