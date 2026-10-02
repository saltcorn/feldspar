import { describe, expect, it } from "vitest";

import { format } from "../i18n";
import { effectName, estimateName, testName } from "../labels";
import { clear, drop, readState, setTests } from "./state";
import {
  differingPairs,
  estimateText,
  formatP,
  isAnalysis,
  levelLabel,
  notes,
  num,
  pValue,
  sectionHeading,
  sentence,
  testSpecOf,
  type Analysis,
  type Entry,
  type Section,
  type TestKind,
  type TestResult,
} from "./tests";

// The source translator: English, with the placeholders filled in — and the
// no-break spaces of a p-value written as spaces, to compare with.
const t = (text: string, args?: Record<string, string | number>) => format(text, args).replace(/\u00a0/g, " ");
const L = "en-US";

function result(test: TestKind, p: number, more: Partial<TestResult> = {}): TestResult {
  return { test, p_value: p, n: 10, sampled: false, ...more };
}

function entry(test: TestKind, role: Entry["role"], r?: TestResult, error?: string): Entry {
  return { test, role, result: r, error };
}

function section(more: Partial<Section>): Section {
  return { n: 10, levels: [], tests: [], checks: [], ...more };
}

function analysis(design: Analysis["design"], s: Section, more: Partial<Analysis> = {}): Analysis {
  return { design, y: ["price"], mu: 0, level: 0.95, sections: [s], ...more };
}

describe("what the explorer asks the server", () => {
  it("asks nothing without a dataset or a column on Y", () => {
    expect(testSpecOf(readState({}))).toBeNull();
    expect(testSpecOf(readState({ dataset: "d1" }))).toBeNull();
  });

  it("asks for Y, X and Wrap, paired only with two columns on Y", () => {
    let s = readState({ dataset: "d1" });
    s = drop(s, "y", "price");
    s = drop(s, "x", "neighbourhood");
    s = drop(s, "wrap", "year_built");
    s = drop(s, "color", "sold");
    expect(testSpecOf(s)).toEqual({
      data: { kind: "dataset", dataset: "d1" },
      y: [{ field: "price" }],
      x: { field: "neighbourhood" },
      by: { field: "year_built" },
      paired: false,
      mu: 0,
    });
    s = setTests(s, { paired: true, mu: 3 });
    expect(testSpecOf(s)?.paired).toBe(false);
    s = drop(s, "y", "area", true);
    expect(testSpecOf(s)?.paired).toBe(true);
    expect(testSpecOf(s)?.mu).toBe(3);
  });

  it("keeps the tests' settings in the workspace, and Clear resets them", () => {
    expect(readState({}).tests).toEqual({ show: true, paired: false, mu: 0 });
    const stored = readState({ tests: { show: false, paired: true, mu: 12.5 } });
    expect(stored.tests).toEqual({ show: false, paired: true, mu: 12.5 });
    expect(readState({ tests: { mu: "x" } }).tests.mu).toBe(0);
    expect(clear(stored).tests).toEqual({ show: false, paired: false, mu: 0 });
  });

  it("tells an analysis from a sentence", () => {
    expect(isAnalysis({ design: "paired", sections: [] })).toBe(true);
    expect(isAnalysis({ error: "put a column on Y to test it" })).toBe(false);
  });
});

describe("numbers as the panel writes them", () => {
  it("writes p-values as a reader expects", () => {
    expect(formatP(0.0004, L)).toBe("p\u00a0<\u00a00.001");
    expect(formatP(0.0034, L)).toBe("p\u00a0=\u00a00.003");
    expect(formatP(0.38255, L)).toBe("p\u00a0=\u00a00.38");
    expect(pValue(0.0004, L)).toBe("< 0.001");
    expect(pValue(0.049, L)).toBe("0.049");
  });

  it("writes numbers to about four significant digits", () => {
    expect(num(-4000, L)).toBe("-4,000");
    expect(num(0.87988269, L)).toBe("0.8799");
    expect(num(0.00001234, L)).toBe("1.23E-5");
    expect(num(null, L)).toBe("–");
  });

  it("writes an interval, an infinite end as ∞", () => {
    const r = result("fisher_exact", 0.5, { estimate: { of: "odds_ratio", value: 6.4, lower: 0.21, level: 0.95 } });
    expect(estimateText(r, t, L)).toBe("6.4 (0.21 to ∞)");
    const w = result("welch_t", 0.3, { estimate: { of: "mean_difference", value: -4000, lower: -13100, upper: 5100, level: 0.95 } });
    expect(estimateText(w, t, L)).toBe("-4,000 (-13,100 to 5,100)");
  });

  it("labels a bin as its range and a missing value as missing", () => {
    expect(levelLabel({ value: 1990, end: 2000 }, t, L)).toBe("1990–2000");
    expect(levelLabel({ value: "North" }, t, L)).toBe("North");
    expect(levelLabel({ value: null }, t, L)).toBe("(missing)");
  });
});

describe("the plain-language sentence", () => {
  const north = { value: "North", n: 30, mean: 81000 };
  const south = { value: "South", n: 30, mean: 85000 };

  it("says whether two groups' means differ, by the preferred test", () => {
    const s = section({
      levels: [north, south],
      tests: [entry("welch_t", "main", result("welch_t", 0.003)), entry("mann_whitney", "alternative", result("mann_whitney", 0.01))],
      preferred: "welch_t",
    });
    const a = analysis("number_by_groups", s, { x: "neighbourhood" });
    expect(sentence(a, s, t, L)).toBe("The mean of price differs between North and South (p = 0.003).");
    s.preferred = "mann_whitney";
    expect(sentence(a, s, t, L)).toBe("price tends to differ between North and South (p = 0.010).");
    s.tests[1].result = result("mann_whitney", 0.4);
    expect(sentence(a, s, t, L)).toBe("No clear difference in price between North and South (p = 0.40).");
  });

  it("speaks of the groups of X when there are more than two, and names the pairs that differ", () => {
    const s = section({
      levels: [north, south, { value: "East", n: 10 }],
      tests: [entry("anova", "main", result("anova", 0.0002))],
      comparisons: [
        { a: 0, b: 1, difference: 4000, lower: -1, upper: 9, p_value: 0.2 },
        { a: 0, b: 2, difference: 9000, lower: 1, upper: 20, p_value: 0.01 },
      ],
      preferred: "anova",
    });
    const a = analysis("number_by_groups", s, { x: "neighbourhood" });
    expect(sentence(a, s, t, L)).toBe("The mean of price differs between the groups of neighbourhood (p < 0.001).");
    expect(differingPairs(s, t, L)).toEqual(["North and East"]);
  });

  it("has a sentence for every design", () => {
    const one = section({ tests: [entry("one_sample_t", "main", result("one_sample_t", 0.2, { estimate: { of: "mean", value: 309, level: 0.95 } }))], preferred: "one_sample_t" });
    expect(sentence(analysis("one_number", one, { mu: 300 }), one, t, L)).toBe(
      "The mean of price is 309, which is not clearly different from 300 (p = 0.20).",
    );
    const share = section({
      levels: [{ value: false, n: 3 }, { value: true, n: 6 }],
      event: 1,
      tests: [entry("binomial", "main", result("binomial", 0.51, { estimate: { of: "proportion", value: 2 / 3, level: 0.95 } }))],
      preferred: "binomial",
    });
    expect(sentence(analysis("one_category", share, { y: ["sold"] }), share, t, L)).toBe(
      "66.7% of the rows have sold = true, which is not clearly different from half (p = 0.51).",
    );
    const table = section({ tests: [entry("fisher_exact", "alternative", result("fisher_exact", 0.02))], preferred: "fisher_exact" });
    expect(sentence(analysis("two_categories", table, { y: ["sold"], x: "neighbourhood" }), table, t, L)).toBe(
      "sold depends on neighbourhood (p = 0.020).",
    );
    const corr = section({ tests: [entry("pearson", "main", result("pearson", 0.004, { estimate: { of: "correlation", value: -0.816, level: 0.95 } }))], preferred: "pearson" });
    expect(sentence(analysis("two_numbers", corr, { x: "age" }), corr, t, L)).toBe(
      "price falls as age rises: r = -0.816 (p = 0.004).",
    );
    const logit = section({
      levels: [{ value: false, n: 3 }, { value: true, n: 6 }],
      event: 1,
      tests: [entry("logistic_regression", "main", result("logistic_regression", 0.001, { estimate: { of: "odds_ratio", value: 1.5, level: 0.95 } }))],
      preferred: "logistic_regression",
    });
    expect(sentence(analysis("category_by_number", logit, { y: ["sold"], x: "area" }), logit, t, L)).toBe(
      "The chance that sold is true rises with area: the odds multiply by 1.5 per unit (p = 0.001).",
    );
    const paired = section({
      levels: [{ value: "after", n: 20 }, { value: "before", n: 20 }],
      tests: [entry("paired_t", "main", result("paired_t", 0.012, { estimate: { of: "mean_difference", value: 3.2, level: 0.95 } }))],
      preferred: "paired_t",
    });
    expect(sentence(analysis("paired", paired, { y: ["after", "before"] }), paired, t, L)).toBe(
      "after and before differ by 3.2 on average (p = 0.012).",
    );
  });

  it("says nothing for a group with no test", () => {
    const s = section({ error: "the tests compare groups of `sold`, and this group has only one" });
    expect(sentence(analysis("number_by_groups", s), s, t, L)).toBeNull();
  });

  it("heads each Wrap group with its value", () => {
    const s = section({ by: 1990, by_end: 2000 });
    // Years as years, with no thousands separator, as the plot's axes have them.
    expect(sectionHeading(analysis("one_number", s, { by: "year_built" }), s, t, L)).toBe("year_built: 1990–2000");
    const big = section({ by: 100000, by_end: 150000.5 });
    expect(sectionHeading(analysis("one_number", big, { by: "price" }), big, t, L)).toBe("price: 100,000–150,001");
    expect(sectionHeading(analysis("one_number", s), s, t, L)).toBeNull();
  });
});

describe("the notes under the results", () => {
  it("says which assumption is doubtful, which test the sentence reports, and that it sampled", () => {
    const s = section({
      n: 1_000_000,
      sampled: 5000,
      levels: [{ value: "a", n: 4 }],
      tests: [entry("welch_t", "main", result("welch_t", 0.2)), entry("mann_whitney", "alternative", result("mann_whitney", 0.3))],
      checks: [
        { check: "group_size", ok: false, of: "a", n: 4 },
        { check: "normality", ok: false, of: "a", n: 4, p_value: 0.004 },
        { check: "normality", ok: true, of: "b", n: 60, p_value: 0.5 },
      ],
      preferred: "mann_whitney",
    });
    const a = analysis("number_by_groups", s, { x: "kind" });
    expect(notes(a, s, t, (k) => testName(k, t), L)).toEqual([
      "The group a has only 4 rows; a test on so few says little.",
      "price is clearly not normal in the group a (Shapiro-Wilk, p = 0.004).",
      "So the sentence reports the Mann-Whitney test rather than the Welch's t-test.",
      "The rank tests and the checks read a random sample of 5,000 of the 1,000,000 rows.",
    ]);
  });

  it("gathers the small and the skewed groups into one note each", () => {
    const s = section({
      tests: [entry("anova", "main", result("anova", 0.4)), entry("kruskal_wallis", "alternative", result("kruskal_wallis", 0.4))],
      checks: [
        { check: "group_size", ok: false, of: 2, n: 2 },
        { check: "group_size", ok: false, of: 3, n: 8 },
        { check: "normality", ok: false, of: 3, n: 8, p_value: 0.01 },
        { check: "normality", ok: false, of: 4, n: 30, p_value: 0.02 },
        { check: "equal_variances", ok: false, p_value: 0.03 },
      ],
      preferred: "kruskal_wallis",
    });
    expect(notes(analysis("number_by_groups", s, { y: ["area"], x: "neighbourhood" }), s, t, (k) => testName(k, t), L)).toEqual([
      "Some groups have only a few rows, and a test on so few says little: 2 (2), 3 (8).",
      "area is clearly not normal in the groups 3, 4 (Shapiro-Wilk).",
      "The groups' variances differ (Levene's test, p = 0.030).",
      "So the sentence reports the Kruskal-Wallis test rather than the One-way ANOVA.",
    ]);
  });

  it("explains small expected counts and rare outcomes", () => {
    const s = section({
      checks: [
        { check: "expected_counts", ok: false, value: 1 / 3 },
        { check: "events", ok: false, value: 3 },
      ],
    });
    expect(notes(analysis("two_categories", s, { y: ["sold"] }), s, t, (k) => k, L)).toEqual([
      "Some counts are expected to be below 5 (the smallest, 0.3333), which the chi-square test needs.",
      "Only 3 rows have the rarer value of sold, which is too few for a confident regression.",
    ]);
  });
});

describe("the names of the tests' things", () => {
  it("names every test, estimate and effect size the server answers", () => {
    const tests: TestKind[] = [
      "one_sample_t", "shapiro_wilk", "signed_rank", "chi_square_fit", "binomial", "welch_t",
      "mann_whitney", "anova", "kruskal_wallis", "levene", "chi_square_independence",
      "fisher_exact", "pearson", "spearman", "linear_regression", "logistic_regression",
      "paired_t", "paired_signed_rank",
    ];
    for (const k of tests) expect(testName(k, t)).not.toBe(k);
    for (const e of ["mean", "mean_difference", "location_shift", "pseudomedian", "pseudomedian_difference", "proportion", "odds_ratio", "correlation", "slope"]) {
      expect(estimateName(e, t)).not.toBe(e);
    }
    for (const e of ["cohens_d", "eta_squared", "epsilon_squared", "rank_biserial", "cohens_w", "cohens_h", "cramers_v", "r_squared", "mcfadden_r_squared"]) {
      expect(effectName(e, t)).not.toBe(e);
    }
  });
});
