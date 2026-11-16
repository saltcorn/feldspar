// The formula provider's editor logic (TODO-bayesian-regression §7, R1.8),
// apart from React so it is tested on its own: the prior menu and its
// constraints (checked as the admin types, and again on the server), the
// explicit prior rows the configuration stores and how an edit of the table
// changes them, and how the resolved table reads.
//
// The server is the authority — `checkFormula` and `checkPriors` resolve the
// table and name every mistake — and this mirrors only what must answer at
// keystroke speed: "σ must be positive" under the cell being typed in.

/** The provider's name. */
export const FORMULA_PROVIDER = "bayesian_regression";

/** The configuration keys the formula section edits (the sampler's settings
 * are rendered as ordinary settings). */
export const FORMULA_KEYS = ["formula", "family", "links", "priors", "center"];

/** One mistake in the formula, located (`sc_bayesreg::ParseError`). */
export type FormulaError = {
  message: string;
  span: { start: number; end: number };
  /** 1-based; 0 when the mistake is about no place in the text. */
  line: number;
  column: number;
  end_column: number;
};

/** One explicit prior row, as the configuration stores it (§3.7). */
export type PriorRow = {
  class: string;
  coef?: string;
  group?: string;
  dpar?: string;
  prior: string;
  lb?: number;
  ub?: number;
};

/** Where a resolved prior came from. */
export type PriorSource =
  | { kind: "explicit" }
  | { kind: "inherited"; from: string }
  | { kind: "default" }
  | { kind: "default_from_data" };

/** One row of the resolved table (`sc_bayesreg::prior::TableRow`). */
export type TableRow = {
  class: string;
  coef: string;
  group: string;
  dpar: string;
  prior: string;
  rule?: string;
  source: PriorSource;
  whole_class: boolean;
  structural: boolean;
  lb?: number;
  ub?: number;
};

/** An explicit row the formula no longer has a target for. */
export type Unmatched = { row: PriorRow; reason: string };

/** What `checkFormula` / `checkPriors` answer. */
export type FormulaCheck = {
  errors: FormulaError[];
  problems?: string[];
  notes?: string[];
  formula: string;
  priors: PriorRow[];
  replaced?: PriorRow[];
  pasted: number;
  table?: { rows: TableRow[]; unmatched: Unmatched[] } | null;
  from_data: boolean;
  program?: string | null;
  structure?: unknown;
  data?: unknown;
  completions: {
    columns: string[];
    dpars: string[];
    families: string[];
    priors: string[];
    links: Record<string, string[]>;
  };
};

/** The menu (§3.7): each prior's argument names and which must be positive. */
const MENU: Record<string, { args: string[]; positive: number[] }> = {
  flat: { args: [], positive: [] },
  normal: { args: ["μ", "σ"], positive: [1] },
  student_t: { args: ["ν", "μ", "σ"], positive: [0, 2] },
  cauchy: { args: ["μ", "σ"], positive: [1] },
  double_exponential: { args: ["μ", "σ"], positive: [1] },
  logistic: { args: ["μ", "σ"], positive: [1] },
  exponential: { args: ["λ"], positive: [0] },
  gamma: { args: ["α", "β"], positive: [0, 1] },
  inv_gamma: { args: ["α", "β"], positive: [0, 1] },
  lognormal: { args: ["μ", "σ"], positive: [1] },
  uniform: { args: ["a", "b"], positive: [] },
  beta: { args: ["α", "β"], positive: [0, 1] },
  lkj: { args: ["η"], positive: [0] },
  constant: { args: ["x"], positive: [] },
};

/** The priors defined on the positive numbers only. */
const POSITIVE_ONLY = ["exponential", "gamma", "inv_gamma", "lognormal"];

/** What values a class's parameters take. */
export function classKind(cls: string): "real" | "positive" | "above_one" | "correlation" {
  if (cls === "cor") return "correlation";
  if (cls === "nu") return "above_one";
  if (["sd", "sigma", "shape", "phi", "kappa"].includes(cls)) return "positive";
  return "real";
}

/** Whether a class has coefficients, so its whole-class row reads `(all)`. */
export function hasCoefs(cls: string): boolean {
  return cls === "b" || cls === "sd";
}

/** The menu's names, for completion. */
export const PRIOR_NAMES = Object.keys(MENU);

/** `normal(0, 5)` as its name and numbers, or the sentence saying why not. */
export function parsePrior(text: string): { name: string; args: number[] } | string {
  const trimmed = text.trim();
  if (trimmed === "") return { name: "flat", args: [] };
  const m = /^([A-Za-z_][A-Za-z0-9_]*)\s*(?:\((.*)\))?$/s.exec(trimmed);
  if (!m) return "a prior is a distribution: `normal(0, 5)`";
  const name = m[1] === "lkj_corr_cholesky" || m[1] === "lkj_corr" ? "lkj" : m[1];
  const entry = MENU[name];
  if (!entry) {
    return ["horseshoe", "R2D2", "dirichlet", "lasso"].includes(name)
      ? `\`${name}()\` arrives in a later release (R2)`
      : `\`${name}\` is not a prior on the menu`;
  }
  const inner = (m[2] ?? "").trim();
  const parts = inner === "" ? [] : inner.split(",").map((s) => s.trim());
  const args: number[] = [];
  for (const p of parts) {
    if (/^[A-Za-z_.][\w.]*\s*=/.test(p)) return `write the arguments of \`${name}\` in order, without names`;
    const x = Number(p);
    if (p === "" || !Number.isFinite(x)) return `the arguments of \`${name}\` are numbers`;
    args.push(x);
  }
  if (args.length !== entry.args.length) {
    if (entry.args.length === 0) return `\`${name}\` takes no arguments`;
    const n = entry.args.length;
    return `\`${name}\` takes ${n} argument${n === 1 ? "" : "s"} (${entry.args.join(", ")})`;
  }
  for (const i of entry.positive) {
    if (!(args[i] > 0)) return `${entry.args[i]} must be positive (it is ${args[i]})`;
  }
  if (name === "uniform" && !(args[0] < args[1])) {
    return `a must be less than b (they are ${args[0]} and ${args[1]})`;
  }
  return { name, args };
}

/** The sentence about `text` as the prior of a row of `cls`, or `null` when
 * it is one the server will take (`center`: whether the intercept is centred). */
export function priorProblem(text: string, cls: string, center = true): string | null {
  const parsed = parsePrior(text);
  if (typeof parsed === "string") return parsed;
  const kind = classKind(cls);
  const { name } = parsed;
  if (kind === "correlation") return name === "lkj" ? null : "a correlation takes `lkj(η)`";
  if (name === "lkj") return "`lkj` is for correlations (`cor`) only";
  if (name === "flat" && !(cls === "b" || (cls === "Intercept" && !center))) {
    return cls === "Intercept"
      ? "`flat` cannot go on a centred intercept"
      : "`flat` is allowed on `b` only";
  }
  if (name === "uniform" && cls !== "b") return "`uniform` is allowed on `b` only";
  if (name === "beta") return "`beta` is for a parameter bounded to (0, 1)";
  if (POSITIVE_ONLY.includes(name) && kind === "real") {
    return `\`${name}\` is defined on the positive numbers only, and \`${cls}\` can be negative`;
  }
  return null;
}

/** Whether two rows set the same parameter (or the same whole class). */
export function sameTarget(a: PriorRow | TableRow, b: PriorRow | TableRow): boolean {
  return (
    a.class === b.class &&
    (a.coef ?? "") === (b.coef ?? "") &&
    (a.group ?? "") === (b.group ?? "") &&
    (a.dpar ?? "") === (b.dpar ?? "")
  );
}

/** The configuration's `priors` text as rows; anything unreadable as none
 * (the server names the mistake). */
export function readRows(text: string | undefined): PriorRow[] {
  if (!text || text.trim() === "") return [];
  try {
    const parsed: unknown = JSON.parse(text);
    return Array.isArray(parsed) ? (parsed as PriorRow[]) : [];
  } catch {
    return [];
  }
}

/** Rows as the configuration stores them: empty fields left out. */
export function writeRows(rows: PriorRow[]): string {
  const tidy = rows.map((r) => {
    const out: PriorRow = { class: r.class, prior: r.prior };
    if (r.coef) out.coef = r.coef;
    if (r.group) out.group = r.group;
    if (r.dpar) out.dpar = r.dpar;
    if (r.lb !== undefined && r.lb !== null && Number.isFinite(r.lb)) out.lb = r.lb;
    if (r.ub !== undefined && r.ub !== null && Number.isFinite(r.ub)) out.ub = r.ub;
    return out;
  });
  return tidy.length === 0 ? "" : JSON.stringify(tidy, null, 2);
}

/** The explicit row of a table row's target, made explicit with `change` —
 * editing any row of the table makes it explicit, with that row's exact
 * target (§7). An edit that leaves it empty with no bounds removes it. */
export function setRow(rows: PriorRow[], target: TableRow, change: Partial<PriorRow>): PriorRow[] {
  const existing = rows.find((r) => sameTarget(r, target));
  const base: PriorRow = existing ?? {
    class: target.class,
    coef: target.coef || undefined,
    group: target.group || undefined,
    dpar: target.dpar || undefined,
    prior: "",
  };
  const next: PriorRow = { ...base, ...change };
  const empty = next.prior.trim() === "" && next.lb === undefined && next.ub === undefined;
  const others = rows.filter((r) => !sameTarget(r, target));
  if (empty) return others;
  return existing ? rows.map((r) => (sameTarget(r, target) ? next : r)) : [...rows, next];
}

/** The rows without `target`'s — the ✕ that clears a row back to its default. */
export function clearRow(rows: PriorRow[], target: PriorRow | TableRow): PriorRow[] {
  return rows.filter((r) => !sameTarget(r, target));
}

/** An unmatched row pointed at another coefficient. */
export function retarget(rows: PriorRow[], row: PriorRow, coef: string): PriorRow[] {
  return rows.map((r) => (sameTarget(r, row) ? { ...r, coef } : r));
}

/** How a table row's coefficient cell reads: `(all)` for a whole class with
 * coefficients. */
export function coefText(row: TableRow): string {
  if (row.whole_class && hasCoefs(row.class)) return "(all)";
  return row.coef;
}

/** Whether a table row is shown greyed: a default, or inherited from a
 * whole-class row. */
export function greyed(row: TableRow): boolean {
  return row.source.kind !== "explicit";
}

/** Where a row's prior came from, in a few words. */
export function sourceText(row: TableRow): string {
  switch (row.source.kind) {
    case "explicit":
      return "set here";
    case "inherited":
      return `from ${row.source.from.replace(/^the prior row /, "")}`;
    case "default":
      return "default";
    case "default_from_data":
      return row.rule ? "default, from the data" : "default, computed from the data";
  }
}

/** The coefficients a class's unmatched row could be pointed at instead. */
export function retargets(rows: TableRow[], row: PriorRow): string[] {
  return rows
    .filter(
      (r) =>
        !r.whole_class &&
        r.class === row.class &&
        r.dpar === (row.dpar ?? "") &&
        r.group === (row.group ?? "") &&
        r.coef !== "",
    )
    .map((r) => r.coef);
}

/** The word being typed at `offset` in `text`, and where it starts — what a
 * completion replaces. */
export function wordAt(text: string, offset: number): { word: string; start: number } {
  let start = offset;
  while (start > 0 && /[\w.]/.test(text[start - 1])) start--;
  return { word: text.slice(start, offset), start };
}

/** The completions for `word`: columns, then distributional parameters, then
 * the formula's own words. */
export function completionsFor(
  word: string,
  check: Pick<FormulaCheck, "completions"> | null,
): string[] {
  if (!check || word === "") return [];
  const pool = [
    ...check.completions.columns,
    ...check.completions.dpars,
    "trials",
    "weights",
    "cens",
    "trunc",
    "se",
    "rate",
    "offset",
    "gr",
    ...check.completions.priors,
  ];
  const seen = new Set<string>();
  return pool.filter((c) => {
    if (seen.has(c) || !c.startsWith(word) || c === word) return false;
    seen.add(c);
    return true;
  });
}
