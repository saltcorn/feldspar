// The hypothesis tests beside the Data explorer's plot (analytics TODO
// A2.12–A2.14): what is asked of the server, and the plain-language sentence
// and notes made of its answer.
//
// The server chooses the tests from the Y, X and Wrap drop zones and runs
// them (`runTests`); it answers numbers and the names of things (a test, an
// effect size, an assumption check), never English, so that every sentence
// the person reads is in the `analytics` domain and translated. Every
// function here is pure: the panel calls them, and the tests do too.

import type { Translate } from "../datasets/ops";
import type { DataRef, FieldDef } from "../plot/spec";
import type { ExplorerState } from "./state";

/** The roles a test reads. */
export type TestSpec = {
  data: DataRef;
  y: FieldDef[];
  x?: FieldDef;
  by?: FieldDef;
  paired: boolean;
  mu: number;
};

export type TestKind =
  | "one_sample_t"
  | "shapiro_wilk"
  | "signed_rank"
  | "chi_square_fit"
  | "binomial"
  | "welch_t"
  | "mann_whitney"
  | "anova"
  | "kruskal_wallis"
  | "levene"
  | "chi_square_independence"
  | "fisher_exact"
  | "pearson"
  | "spearman"
  | "linear_regression"
  | "logistic_regression"
  | "paired_t"
  | "paired_signed_rank";

export type Design =
  | "one_number"
  | "one_category"
  | "number_by_groups"
  | "two_categories"
  | "two_numbers"
  | "category_by_number"
  | "paired";

export type TestResult = {
  test: TestKind;
  statistic?: { symbol: string; value: number };
  df?: number[];
  p_value: number;
  /** An absent `upper` with `lower` present is infinite. */
  estimate?: { of: string; value: number | null; lower?: number; upper?: number; level: number };
  effect?: { kind: string; value: number };
  details?: { name: string; value: number }[];
  n: number;
  method?: string;
  sampled: boolean;
};

export type Entry = { test: TestKind; role: "main" | "alternative"; result?: TestResult; error?: string };

export type Level = { value: unknown; end?: unknown; n: number; mean?: number; sd?: number };

export type Comparison = { a: number; b: number; difference: number; lower: number; upper: number; p_value: number };

export type Check = { check: string; ok: boolean; of?: unknown; n?: number; p_value?: number; value?: number };

export type Section = {
  by?: unknown;
  by_end?: unknown;
  n: number;
  levels: Level[];
  categories?: Level[];
  event?: number;
  tests: Entry[];
  comparisons?: Comparison[];
  checks: Check[];
  preferred?: TestKind;
  sampled?: number;
  error?: string;
};

export type Analysis = {
  design: Design;
  y: string[];
  x?: string;
  by?: string;
  mu: number;
  level: number;
  sections: Section[];
};

/** The p-value below which a result is called a difference. */
export const ALPHA = 0.05;

/** What to ask the server for the explorer's drop zones: Y, X and Wrap, or
 * nothing while there is no dataset or nothing on Y. */
export function testSpecOf(state: ExplorerState): TestSpec | null {
  const ys = state.assignment.y ?? [];
  if (!state.dataset || ys.length === 0) return null;
  const spec: TestSpec = {
    data: { kind: "dataset", dataset: state.dataset },
    y: ys,
    paired: state.tests.paired && ys.length === 2,
    mu: state.tests.mu,
  };
  if (state.assignment.x) spec.x = state.assignment.x;
  if (state.assignment.wrap) spec.by = state.assignment.wrap;
  return spec;
}

/** Whether the server answered an analysis rather than a sentence. */
export function isAnalysis(answer: unknown): answer is Analysis {
  return Boolean(answer) && Array.isArray((answer as { sections?: unknown }).sections);
}

/** A number to about four significant digits, in the reader's locale. */
export function num(x: number | null | undefined, locale?: string): string {
  if (x === null || x === undefined || !Number.isFinite(x)) return "–";
  const abs = Math.abs(x);
  const options: Intl.NumberFormatOptions =
    abs !== 0 && (abs < 0.001 || abs >= 1e9)
      ? { notation: "scientific", maximumSignificantDigits: 3 }
      : abs >= 1000
        ? { maximumFractionDigits: 0 }
        : { maximumSignificantDigits: 4 };
  return new Intl.NumberFormat(locale, options).format(x);
}

/** A p-value for a table's column: `< 0.001`, `0.003`, `0.049`, `0.38`. */
export function pValue(p: number, locale?: string): string {
  if (!Number.isFinite(p)) return "–";
  if (p < 0.001) return `< ${new Intl.NumberFormat(locale).format(0.001)}`;
  // Three decimals below 0.1, so that 0.049 is not written as 0.05.
  const digits = p < 0.1 ? 3 : 2;
  return new Intl.NumberFormat(locale, { minimumFractionDigits: digits, maximumFractionDigits: digits }).format(p);
}

/** A p-value as a sentence writes it: `p < 0.001`, `p = 0.003`, `p = 0.38`,
 * with no-break spaces so that a line never ends inside it. */
export function formatP(p: number, locale?: string): string {
  const text = pValue(p, locale).replace(" ", "\u00a0");
  return text.startsWith("<") ? `p\u00a0${text}` : `p\u00a0=\u00a0${text}`;
}

/** A column's value, rather than a statistic: as `num` writes it, but with
 * no separator below 10,000, so that years read as years (as the plot's axes
 * write them). */
export function valueNum(x: number, locale?: string): string {
  const abs = Math.abs(x);
  if (!Number.isFinite(x) || abs >= 10000 || (abs !== 0 && abs < 0.001)) return num(x, locale);
  return new Intl.NumberFormat(locale, {
    maximumSignificantDigits: abs >= 1000 ? undefined : 4,
    maximumFractionDigits: abs >= 1000 ? 0 : undefined,
    useGrouping: false,
  }).format(x);
}

/** A group's or a category's value as a reader sees it: a bin as its range. */
export function levelLabel(level: { value: unknown; end?: unknown } | undefined, t: Translate, locale?: string): string {
  if (!level) return "";
  const show = (v: unknown) =>
    v === null || v === undefined ? t("(missing)") : typeof v === "number" ? valueNum(v, locale) : String(v);
  return level.end === undefined || level.end === null ? show(level.value) : `${show(level.value)}–${show(level.end)}`;
}

/** The section's heading under Wrap: `year_built 1990–2000`. */
export function sectionHeading(a: Analysis, s: Section, t: Translate, locale?: string): string | null {
  if (!a.by) return null;
  return t("{column}: {value}", { column: a.by, value: levelLabel({ value: s.by, end: s.by_end }, t, locale) });
}

/** The result the sentence reports. */
export function preferredResult(s: Section): TestResult | undefined {
  return s.tests.find((e) => e.test === s.preferred)?.result;
}

/** The plain-language sentence for a section: what the preferred test says,
 * with its p-value — "The mean of weight differs between A and B (p = 0.003)." */
export function sentence(a: Analysis, s: Section, t: Translate, locale?: string): string | null {
  if (s.error) return null;
  const r = preferredResult(s);
  if (!r) return null;
  const p = formatP(r.p_value, locale);
  const differs = r.p_value < ALPHA;
  const y = a.y[0] ?? "";
  const x = a.x ?? "";
  const label = (i: number) => levelLabel(s.levels[i], t, locale);
  const est = r.estimate?.value ?? null;
  switch (a.design) {
    case "one_number": {
      const mu = num(a.mu, locale);
      if (r.test === "one_sample_t") {
        return differs
          ? t("The mean of {y} is {mean}, which differs from {mu} ({p}).", { y, mean: num(est, locale), mu, p })
          : t("The mean of {y} is {mean}, which is not clearly different from {mu} ({p}).", {
              y,
              mean: num(est, locale),
              mu,
              p,
            });
      }
      return differs
        ? t("The typical value of {y} is about {value}, which differs from {mu} ({p}).", { y, value: num(est, locale), mu, p })
        : t("The typical value of {y} is about {value}, which is not clearly different from {mu} ({p}).", {
            y,
            value: num(est, locale),
            mu,
            p,
          });
    }
    case "one_category": {
      if (r.test === "binomial" && s.event !== undefined) {
        const share = new Intl.NumberFormat(locale, { style: "percent", maximumFractionDigits: 1 }).format(est ?? 0);
        const args = { share, y, value: label(s.event), p };
        return differs
          ? t("{share} of the rows have {y} = {value}, which differs from half ({p}).", args)
          : t("{share} of the rows have {y} = {value}, which is not clearly different from half ({p}).", args);
      }
      return differs
        ? t("The values of {y} are not equally common ({p}).", { y, p })
        : t("The values of {y} could be equally common ({p}).", { y, p });
    }
    case "number_by_groups": {
      const rank = r.test === "mann_whitney" || r.test === "kruskal_wallis";
      if (s.levels.length === 2) {
        const args = { y, a: label(0), b: label(1), p };
        if (rank) {
          return differs
            ? t("{y} tends to differ between {a} and {b} ({p}).", args)
            : t("No clear difference in {y} between {a} and {b} ({p}).", args);
        }
        return differs
          ? t("The mean of {y} differs between {a} and {b} ({p}).", args)
          : t("No clear difference in the mean of {y} between {a} and {b} ({p}).", args);
      }
      const args = { y, x, p };
      if (rank) {
        return differs
          ? t("{y} tends to differ between the groups of {x} ({p}).", args)
          : t("No clear difference in {y} between the groups of {x} ({p}).", args);
      }
      return differs
        ? t("The mean of {y} differs between the groups of {x} ({p}).", args)
        : t("No clear difference in the mean of {y} between the groups of {x} ({p}).", args);
    }
    case "two_categories":
      return differs
        ? t("{y} depends on {x} ({p}).", { y, x, p })
        : t("No clear association between {y} and {x} ({p}).", { y, x, p });
    case "two_numbers": {
      const value = num(est, locale);
      if (r.test === "spearman") {
        if (!differs) return t("No clear relation between {y} and {x}: ρ = {value} ({p}).", { y, x, value, p });
        return (est ?? 0) > 0
          ? t("{y} tends to rise with {x}: ρ = {value} ({p}).", { y, x, value, p })
          : t("{y} tends to fall as {x} rises: ρ = {value} ({p}).", { y, x, value, p });
      }
      if (!differs) return t("No clear linear relation between {y} and {x}: r = {value} ({p}).", { y, x, value, p });
      return (est ?? 0) > 0
        ? t("{y} rises with {x}: r = {value} ({p}).", { y, x, value, p })
        : t("{y} falls as {x} rises: r = {value} ({p}).", { y, x, value, p });
    }
    case "category_by_number": {
      const args = { y, x, value: label(s.event ?? 1), ratio: num(est, locale), p };
      if (!differs) return t("No clear relation between {x} and the chance that {y} is {value} ({p}).", args);
      return (est ?? 1) > 1
        ? t("The chance that {y} is {value} rises with {x}: the odds multiply by {ratio} per unit ({p}).", args)
        : t("The chance that {y} is {value} falls as {x} rises: the odds multiply by {ratio} per unit ({p}).", args);
    }
    case "paired": {
      const args = { a: label(0), b: label(1), d: num(est, locale), p };
      if (r.test === "paired_signed_rank") {
        return differs
          ? t("{a} and {b} tend to differ, by about {d} ({p}).", args)
          : t("No clear difference between {a} and {b} ({p}).", args);
      }
      return differs
        ? t("{a} and {b} differ by {d} on average ({p}).", args)
        : t("No clear difference between {a} and {b} ({p}).", args);
    }
    default:
      return null;
  }
}

/** The pairs of groups whose means differ, by Tukey's comparisons:
 * "North and South, North and East". */
export function differingPairs(s: Section, t: Translate, locale?: string): string[] {
  return (s.comparisons ?? [])
    .filter((c) => c.p_value < ALPHA)
    .map((c) =>
      t("{a} and {b}", { a: levelLabel(s.levels[c.a], t, locale), b: levelLabel(s.levels[c.b], t, locale) }),
    );
}

/** A note for each assumption check that failed, and for a sample. */
export function notes(a: Analysis, s: Section, t: Translate, testName: (k: TestKind) => string, locale?: string): string[] {
  const out: string[] = [];
  const y = a.y[0] ?? "";
  const failed = s.checks.filter((c) => !c.ok);
  const group = (c: Check) => levelLabel({ value: c.of }, t, locale);
  // Small groups, and groups that are not normal, as one note each.
  const small = failed.filter((c) => c.check === "group_size" && c.of !== undefined);
  if (small.length === 1) {
    out.push(
      t("The group {group} has only {n} rows; a test on so few says little.", {
        group: group(small[0]),
        n: small[0].n ?? 0,
        count: small[0].n ?? 0,
      }),
    );
  } else if (small.length > 1) {
    out.push(
      t("Some groups have only a few rows, and a test on so few says little: {groups}.", {
        groups: small.map((c) => t("{group} ({n})", { group: group(c), n: c.n ?? 0 })).join(", "),
      }),
    );
  }
  const skewed = failed.filter(
    (c) => c.check === "normality" && !["values", "residuals", "differences"].includes(String(c.of)),
  );
  if (skewed.length === 1) {
    out.push(
      t("{y} is clearly not normal in the group {group} (Shapiro-Wilk, {p}).", {
        y,
        group: group(skewed[0]),
        p: formatP(skewed[0].p_value ?? Number.NaN, locale),
      }),
    );
  } else if (skewed.length > 1) {
    out.push(
      t("{y} is clearly not normal in the groups {groups} (Shapiro-Wilk).", {
        y,
        groups: skewed.map(group).join(", "),
      }),
    );
  }
  for (const c of failed) {
    const p = formatP(c.p_value ?? Number.NaN, locale);
    switch (c.check) {
      case "group_size":
        if (c.of === undefined) {
          out.push(t("Only {n} rows have values; a test on so few says little.", { n: c.n ?? 0, count: c.n ?? 0 }));
        }
        break;
      case "normality":
        if (c.of === "values") out.push(t("{y} is clearly not normal (Shapiro-Wilk, {p}).", { y, p }));
        else if (c.of === "residuals") out.push(t("The residuals are clearly not normal (Shapiro-Wilk, {p}).", { p }));
        else if (c.of === "differences") out.push(t("The differences are clearly not normal (Shapiro-Wilk, {p}).", { p }));
        break;
      case "equal_variances":
        out.push(t("The groups' variances differ (Levene's test, {p}).", { p }));
        break;
      case "expected_counts":
        out.push(
          t("Some counts are expected to be below 5 (the smallest, {value}), which the chi-square test needs.", {
            value: num(c.value, locale),
          }),
        );
        break;
      case "events":
        out.push(
          t("Only {n} rows have the rarer value of {y}, which is too few for a confident regression.", {
            n: c.value ?? 0,
            y,
            count: c.value ?? 0,
          }),
        );
        break;
    }
  }
  const main = s.tests.find((e) => e.role === "main" && e.result)?.test;
  if (main && s.preferred && s.preferred !== main) {
    out.push(
      t("So the sentence reports the {alternative} rather than the {main}.", {
        alternative: testName(s.preferred),
        main: testName(main),
      }),
    );
  }
  if (s.sampled !== undefined) {
    out.push(
      t("The rank tests and the checks read a random sample of {shown} of the {total} rows.", {
        shown: new Intl.NumberFormat(locale).format(s.sampled),
        total: new Intl.NumberFormat(locale).format(s.n),
      }),
    );
  }
  return out;
}

/** An estimate with its interval: `−4,000 (−13,100 to 5,100)`. */
export function estimateText(r: TestResult, t: Translate, locale?: string): string {
  const e = r.estimate;
  if (!e || e.value === null) return e ? "∞" : "";
  if (e.lower === undefined) return num(e.value, locale);
  const upper = e.upper === undefined ? "∞" : num(e.upper, locale);
  return t("{value} ({lower} to {upper})", { value: num(e.value, locale), lower: num(e.lower, locale), upper });
}
