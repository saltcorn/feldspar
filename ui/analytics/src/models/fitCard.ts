// The arithmetic of the model editor's workbench (analytics design A1): the
// fits as numbered cards, what an open card says about a fit — its errors by
// split, a posterior's checks against their thresholds and the verdict built
// from them — and what the setup column says about itself: the split drawn as
// a bar, the tuning boxes against the fit they were last fitted with.
//
// Pure, so it is tested without a screen (`fitCard.test.ts`).

import {
  formatNumber,
  printGridValue,
  type Metrics,
  type PosteriorMetrics,
  type SplitMetrics,
} from "./models";

// --- the fits ----------------------------------------------------------------

/**
 * Each fit's number, `#1` the oldest: the order they were started in, which is
 * how the list is talked about ("#14 was better than #12"). Fits carry no
 * number of their own, so removing one renumbers the newer ones; ties in the
 * created time are broken by id so two screens agree.
 */
export function fitNumbers(fits: { id: string; created: string }[]): Map<string, number> {
  const sorted = [...fits].sort((a, b) =>
    a.created === b.created ? (a.id < b.id ? -1 : a.id > b.id ? 1 : 0) : a.created < b.created ? -1 : 1,
  );
  return new Map(sorted.map((f, i) => [f.id, i + 1]));
}

/** The hyperparameter point a fit used, on one line: `trees=200 max_depth=0`. */
export function hyperparameterText(raw: unknown): string {
  if (!raw || typeof raw !== "object") return "";
  return Object.entries(raw as Record<string, unknown>)
    .map(([key, value]) => `${key}=${printGridValue(value)}`)
    .join(" ");
}

/** A short form of a fit's id, for "Fit id": its first eight characters. */
export function shortId(id: string): string {
  return id.replace(/-/g, "").slice(0, 8);
}

/** One row of the errors-by-split table. */
export type SplitErrorRow = { split: "train" | "validation" | "test"; cells: string[] };

/** The metrics of each split side by side: the columns the outcome's metric
 * set has, and a row per split the fit had rows for. A posterior's metrics are
 * not errors and answer `null` here (they are its checks, `posteriorChecks`). */
export function splitErrors(metrics: SplitMetrics): { columns: string[]; rows: SplitErrorRow[] } | null {
  const rows: SplitErrorRow[] = [];
  let columns: string[] | null = null;
  for (const split of ["train", "validation", "test"] as const) {
    const m = metrics[split];
    if (!m) continue;
    const cells = errorCells(m);
    if (!cells) continue;
    columns ??= cells.columns;
    rows.push({ split, cells: cells.values });
  }
  return columns && rows.length > 0 ? { columns, rows } : null;
}

function errorCells(m: Metrics): { columns: string[]; values: string[] } | null {
  switch (m.metrics) {
    case "regression":
      return {
        columns: ["Rows", "R²", "RMSE", "MAE"],
        values: [String(m.rows), formatNumber(m.r2, 3), formatNumber(m.rmse, 3), formatNumber(m.mae, 3)],
      };
    case "classification":
      return { columns: ["Rows", "Accuracy", "Classes"], values: [String(m.rows), formatNumber(m.accuracy, 3), String(m.classes.length)] };
    case "clustering":
      return { columns: ["Rows", "WCSS", "Clusters"], values: [String(m.rows), formatNumber(m.wcss), String(m.sizes.length)] };
    case "embedding":
      return {
        columns: ["Rows", "Explained variance"],
        values: [String(m.rows), formatNumber(m.explained_variance.reduce((s: number, v) => s + (v ?? 0), 0), 3)],
      };
    default:
      return null;
  }
}

// --- a posterior's trust -----------------------------------------------------

/** The host's thresholds (`sc_model::diagnose`). */
export const RHAT_THRESHOLD = 1.01;
export const ESS_PER_CHAIN_THRESHOLD = 100;
export const EBFMI_THRESHOLD = 0.3;

/** One sampler check: what it is, what it must be, what it was. */
export type PosteriorCheck = {
  key: "rhat" | "ess_bulk" | "ess_tail" | "divergent" | "treedepth" | "ebfmi";
  label: string;
  threshold: string;
  value: string;
  /** `null` when there is no value to judge. */
  ok: boolean | null;
};

/** A posterior's diagnostics as checks against the thresholds the host warns
 * at. ESS is judged per chain, as the host judges it. */
export function posteriorChecks(m: PosteriorMetrics): PosteriorCheck[] {
  const ess = ESS_PER_CHAIN_THRESHOLD * Math.max(1, m.chains);
  const ebfmi = m.ebfmi.filter((v): v is number => typeof v === "number");
  const minEbfmi = ebfmi.length > 0 ? Math.min(...ebfmi) : null;
  const judged = (value: number | null, ok: (v: number) => boolean) => (value === null ? null : ok(value));
  return [
    { key: "rhat", label: "Max R̂", threshold: `≤ ${RHAT_THRESHOLD}`, value: formatNumber(m.max_rhat, 4), ok: judged(m.max_rhat, (v) => v <= RHAT_THRESHOLD) },
    { key: "ess_bulk", label: "Min bulk ESS", threshold: `≥ ${ess}`, value: formatNumber(round(m.min_ess_bulk)), ok: judged(m.min_ess_bulk, (v) => v >= ess) },
    { key: "ess_tail", label: "Min tail ESS", threshold: `≥ ${ess}`, value: formatNumber(round(m.min_ess_tail)), ok: judged(m.min_ess_tail, (v) => v >= ess) },
    { key: "divergent", label: "Divergences", threshold: "0", value: String(m.divergent), ok: m.divergent === 0 },
    { key: "treedepth", label: "Tree-depth hits", threshold: "0", value: String(m.max_treedepth_hits), ok: m.max_treedepth_hits === 0 },
    { key: "ebfmi", label: "E-BFMI", threshold: `≥ ${EBFMI_THRESHOLD}`, value: formatNumber(minEbfmi, 2), ok: judged(minEbfmi, (v) => v >= EBFMI_THRESHOLD) },
  ];
}

function round(v: number | null): number | null {
  return v === null ? null : Math.round(v);
}

/** The Trust column: one verdict from the checks, the worst first — chains
 * that disagree, then divergences, then the rest — or "trustworthy". */
export type Trust = { tone: "green" | "yellow"; text: string };

export function trustVerdict(m: PosteriorMetrics): Trust {
  const failed = new Set(posteriorChecks(m).filter((c) => c.ok === false).map((c) => c.key));
  if (failed.has("rhat")) return { tone: "yellow", text: "chains disagree" };
  if (failed.has("divergent")) {
    return { tone: "yellow", text: m.divergent === 1 ? "1 divergence" : `${m.divergent} divergences` };
  }
  if (failed.has("ebfmi")) return { tone: "yellow", text: "low E-BFMI" };
  if (failed.has("ess_bulk") || failed.has("ess_tail")) return { tone: "yellow", text: "few effective draws" };
  if (failed.has("treedepth")) return { tone: "yellow", text: "tree depth hit" };
  return { tone: "green", text: "trustworthy" };
}

/** A posterior's metrics, when the fit's `train` split holds them. */
export function readPosteriorMetrics(metrics: SplitMetrics): PosteriorMetrics | null {
  const m = metrics.train;
  return m && m.metrics === "posterior" ? m : null;
}

/** The Sampler column: `4×1000`. */
export function samplerText(m: PosteriorMetrics): string {
  return `${m.chains}×${m.draws_per_chain}`;
}

// --- the setup column --------------------------------------------------------

/** The split as the form edits it. */
export type SplitForm = { train: string; validation: string; test: string; seed: string };

/** The split as the bar draws it: each part's share, in percent, of the three
 * fractions as typed (half-typed boxes count as nothing). */
export function splitBar(split: SplitForm): { part: "train" | "validation" | "test"; percent: number }[] {
  const parts = (["train", "validation", "test"] as const).map((part) => {
    const n = Number(split[part]);
    return { part, value: Number.isFinite(n) && n > 0 ? n : 0 };
  });
  const total = parts.reduce((s, p) => s + p.value, 0);
  return parts
    .filter((p) => p.value > 0)
    .map((p) => ({ part: p.part, percent: total > 0 ? Math.round((1000 * p.value) / total) / 10 : 0 }));
}

/** "Use 10% for validation": a tenth of the rows moved to validation, taken
 * from training, so the three still sum to 1. */
export function withValidation(split: SplitForm, share = 0.1): SplitForm {
  const train = Number(split.train);
  const validation = Number(split.validation) || 0;
  const take = Math.min(share, Number.isFinite(train) ? train : 0);
  return { ...split, train: fixed(train - take), validation: fixed(validation + take) };
}

/** A fraction without floating-point dust: `0.7`, not `0.7000000000000001`. */
function fixed(n: number): string {
  return String(Math.round(n * 1e6) / 1e6);
}

/**
 * Which tuning boxes say something other than the fit they are compared with,
 * and what that fit had: `{ trees: "200" }`. A blank box is the provider's
 * default, which the form cannot compare, and is never reported.
 */
export function tuningChanges(
  boxes: Record<string, string>,
  fitted: unknown,
): Record<string, string> {
  const was = fitted && typeof fitted === "object" ? (fitted as Record<string, unknown>) : {};
  const out: Record<string, string> = {};
  for (const [name, text] of Object.entries(boxes)) {
    const typed = text
      .split(",")
      .map((s) => s.trim())
      .filter((s) => s !== "");
    if (typed.length === 0) continue;
    const before = printGridValue(was[name]);
    if (before === "") continue;
    if (typed.join(", ") !== before) out[name] = before;
  }
  return out;
}
