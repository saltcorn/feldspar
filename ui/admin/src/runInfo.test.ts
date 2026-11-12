/**
 * A coding run read from the API: its totals with its sessions', its plan's
 * checklist, and its diff as lines.
 */

import { describe, expect, it } from "vitest";

import {
  compactCount,
  diffLines,
  elapsedLabel,
  featureMark,
  planProgress,
  runTotals,
  scopeLabel,
  totalsLabel,
} from "./runInfo";

const usage = (input: number, output: number, cached = 0) => ({
  input_tokens: input,
  output_tokens: output,
  cached_input_tokens: cached,
});

describe("a run's totals", () => {
  it("add its steps, its side calls and its sessions", () => {
    const context = {
      ledger: {
        steps: [
          { step: 1, role: "strong", usage: usage(1000, 100, 800), cost: 0.01 },
          { step: 2, role: "strong", usage: usage(1200, 50), cost: 0.02 },
        ],
        asides: [{ step: 2, role: "cheap", usage: usage(300, 20), cost: 0.001 }],
        children: [
          {
            run: "c1",
            agent: "builder",
            totals: { executor: { steps: 5, usage: usage(9000, 900), cost: 0.1, elapsed_ms: 1 } },
          },
        ],
      },
    };
    const totals = runTotals(context);
    expect(totals.steps).toBe(7);
    expect(totals.inputTokens).toBe(11_500);
    expect(totals.cachedTokens).toBe(800);
    expect(totals.outputTokens).toBe(1070);
    expect(totals.cost).toBeCloseTo(0.131);
    expect(totalsLabel(totals)).toBe("cost 0.1310 · 7 steps · 12k in, 800 cached, 1070 out");
  });

  it("is of unknown cost when any call was unpriced, and zero for an empty run", () => {
    const unpriced = runTotals({
      ledger: { steps: [{ usage: usage(1, 1), cost: 0.5 }, { usage: usage(1, 1), cost: null }] },
    });
    expect(unpriced.cost).toBeNull();
    expect(totalsLabel(unpriced)).toContain("cost unknown");
    expect(runTotals(null)).toEqual({
      steps: 0,
      inputTokens: 0,
      cachedTokens: 0,
      outputTokens: 0,
      cost: 0,
    });
  });
});

describe("a plan's checklist", () => {
  it("marks each status and counts what is done", () => {
    expect(featureMark("done")).toEqual({ mark: "✓", label: "done", done: true });
    expect(featureMark("in_progress").label).toBe("in progress");
    expect(featureMark("todo").done).toBe(false);
    const feature = (id: string, status: string) => ({
      id,
      title: id,
      description: "",
      kind: "feature",
      acceptance: [],
      files: [],
      pages: [],
      status,
      attempts: 0,
      runs: [],
    });
    expect(
      planProgress({ features: [feature("a", "done"), feature("b", "failed")], progress: [] }),
    ).toBe("1 of 2 done");
  });
});

describe("a run's diff", () => {
  it("is drawn line by line, headers apart from changes", () => {
    const unified = "--- a/src/App.tsx\n+++ b/src/App.tsx\n@@ -1,2 +1,2 @@\n-old\n+new\n same\n";
    expect(diffLines(unified).map((l) => l.kind)).toEqual([
      "file",
      "file",
      "hunk",
      "remove",
      "add",
      "context",
    ]);
    expect(scopeLabel({ store: "apps", root: "todo", files: [], moves: [], stat: "", unified: "" })).toBe(
      "apps/todo",
    );
  });
});

describe("a run's stat tiles", () => {
  it("shortens token counts", () => {
    expect(compactCount(812)).toBe("812");
    expect(compactCount(3824)).toBe("3.8k");
    expect(compactCount(20_000)).toBe("20k");
    expect(compactCount(219_080)).toBe("219k");
    expect(compactCount(1_250_000)).toBe("1.3M");
  });

  it("says how long a run took in its two largest units", () => {
    const at = (s: number) => new Date(Date.UTC(2026, 0, 1) + s * 1000).toISOString();
    expect(elapsedLabel(at(0), at(42))).toBe("42 s");
    expect(elapsedLabel(at(0), at(185))).toBe("3 min 5 s");
    expect(elapsedLabel(at(0), at(180))).toBe("3 min");
    expect(elapsedLabel(at(0), at(4320))).toBe("1 h 12 min");
    expect(elapsedLabel(at(10), at(0))).toBe("");
    expect(elapsedLabel("never", at(0))).toBe("");
  });
});
