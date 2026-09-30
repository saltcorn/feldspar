// The model screens' model: the JSON the model API carries, and the handful of
// pure functions three screens would otherwise each invent (TODO "Predictive
// models", task 6.6).
//
// Four of the endpoints' fields are declared `json` in the endpoint set and
// therefore arrive as `unknown` in the generated client — the dataset, the
// outcome, the metrics, the parameter blocks. That is right for the *wire*: they
// are a provider's vocabulary, not the endpoint's, and giving them a
// `TypeSchema` would mean the API declaring what a coefficient table is. It is
// wrong for a screen, which has to render them. So the shapes are written out
// here once, as the discriminated unions their Rust originals serialise to, and
// the screens read them through the narrowing functions below rather than
// casting at each use.
//
// The rest is arithmetic with an opinion:
//
//   - a **hyperparameter** is a value or a list of values (§11), and the form
//     edits both as one text box — so the parse and the print are here, with the
//     equivalence they hold up to stated in the tests;
//   - a **metric set** is chosen by the outcome, so nothing has to ask whether
//     an accuracy on a regression means anything;
//   - a **p-value** in a coefficient table must not print as `1.2e-16`, because
//     a column of those is unreadable and the only question anybody asks of one
//     is which side of a threshold it falls;
//   - and the **instance order** is the active fit first, then newest, because
//     the active one is what everything outside this screen means by "the model".

import type {
  GetModelInstanceResponse,
  GetModelResponse,
  ListModelInstancesResponse,
  ListModelProvidersResponse,
} from "./client";
import type { FieldSpec } from "./settings";

// --- the JSON the API carries -----------------------------------------------

/** One column of a named dataset's last stage, as the server compiled it. */
export type DatasetColumnInfo = {
  name: string;
  type: string;
  /** The formula that computed it, where a Calculated column did. */
  expr?: string;
  key?: { table: string; field: string } | null;
};

/** A model's dataset: a **named dataset** (analytics TODO A1.8), which the
 * model refers to by id and the server resolves — its name, the table its rows
 * start from, its columns, and why it does not read when it does not. */
export type ModelDataset = {
  dataset_id: string;
  name: string;
  table: string;
  columns: DatasetColumnInfo[];
  error: string | null;
};

/** The fractions a fit divides its rows by, and the seed its hash is salted
 * with (§5). */
export type Split = { train: number; validation: number; test: number; seed: number };

/** What a fit of a given configuration produces (§10) — what the UI switches on. */
export type Outcome =
  | { outcome: "regression"; label: string }
  | { outcome: "classification"; label: string; classes?: string[] }
  | { outcome: "cluster" }
  | { outcome: "embedding"; dimensions: number }
  | { outcome: "test" }
  | { outcome: "posterior"; prediction?: string | null };

/** One class's precision, recall and F1, and how many rows actually were it. */
export type ClassMetrics = {
  class: string;
  precision: number | null;
  recall: number | null;
  f1: number | null;
  support: number;
};

/** What one split's rows scored (§7). The variant is the outcome's. */
export type Metrics =
  | { metrics: "regression"; r2: number | null; rmse: number | null; mae: number | null; rows: number }
  | {
      metrics: "classification";
      accuracy: number | null;
      classes: ClassMetrics[];
      confusion: number[][];
      rows: number;
    }
  | { metrics: "clustering"; sizes: number[]; wcss: number | null; rows: number }
  | { metrics: "embedding"; explained_variance: (number | null)[]; rows: number }
  | { metrics: "none" }
  | ({ metrics: "posterior" } & PosteriorMetrics)
  | {
      metrics: "posterior_mode";
      log_density: number | null;
      iterations?: number | null;
      wall_seconds: (number | null)[];
    }
  | {
      metrics: "posterior_approximation";
      draws: number;
      min_ess_bulk: number | null;
      min_ess_tail: number | null;
      wall_seconds: (number | null)[];
    };

/** A posterior's sampler diagnostics (Stan TODO §15), computed by the host from
 * the draws and stored under the `train` split. */
export type PosteriorMetrics = {
  chains: number;
  draws_per_chain: number;
  divergent: number;
  divergent_per_chain: number[];
  max_treedepth_hits: number;
  ebfmi: (number | null)[];
  max_rhat: number | null;
  min_ess_bulk: number | null;
  min_ess_tail: number | null;
  wall_seconds: (number | null)[];
};

/** The metrics of each split a fit had rows for. */
export type SplitMetrics = {
  train?: Metrics | null;
  validation?: Metrics | null;
  test?: Metrics | null;
};

/** A fitted parameter, in the shape the screen renders it in (§7). */
export type ParameterBlock =
  | { block: "scalar"; name: string; value: number | null }
  | { block: "table"; name: string; columns: string[]; rows: { cells: unknown[] }[] }
  | { block: "text"; name: string; body: string };

/** Where the rows went: what was selected, what each split got, what the
 * encoding could not represent. */
export type RowCounts = {
  selected: number;
  train: number;
  validation: number;
  test: number;
  dropped: number;
};

/** One point of the hyperparameter grid and what it scored (§11). A point that
 * failed carries its sentence rather than being dropped. */
export type GridPoint = {
  hyperparameters: Record<string, unknown>;
  score?: number | null;
  error?: string | null;
};

/** How one dataset column becomes one or more matrix columns (§6) — everything
 * the fit learned about it, which is what makes applying it a lookup and never a
 * re-derivation. */
export type ColumnEncoding =
  | { encoding: "passthrough"; column: string }
  | { encoding: "standardised"; column: string; mean: number; sd: number }
  | { encoding: "one_hot"; column: string; categories: string[] }
  | { encoding: "epoch"; column: string };

/** The encoding fitted with an instance. */
export type Encoding = { columns: ColumnEncoding[] };

/** One prediction, as `predictRows` answers it. */
export type Prediction =
  | { prediction: "number"; value: number }
  | { prediction: "class"; class: string; probability?: number | null }
  | { prediction: "cluster"; cluster: number }
  | { prediction: "vector"; values: number[] };

/** One model, as the list and the form see it. */
export type ModelItem = GetModelResponse;
/** One fit, in full. */
export type InstanceDetail = GetModelInstanceResponse;
/** One fit, as a list sees it. */
export type InstanceItem = ListModelInstancesResponse[number];
/** One model provider the picker offers. */
export type ProviderItem = ListModelProvidersResponse["providers"][number];

/** The three states a fit is in (`_fd_model_instances.status`). */
export type FitStatus = "fitting" | "fitted" | "failed";

// --- reading the `unknown`s -------------------------------------------------
//
// One narrowing function per blob, each answering `null` for a value that is not
// the shape it should be. `null` rather than a thrown error because these come
// off the wire into a *screen*: an instance whose metrics could not be read
// should still show its parameters and its status, which is more than an empty
// page saying nothing.

/** A model's dataset off the wire, or `null` when it names none. */
export function readModelDataset(raw: unknown): ModelDataset | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as Partial<ModelDataset>;
  if (typeof value.dataset_id !== "string") return null;
  return {
    dataset_id: value.dataset_id,
    name: typeof value.name === "string" ? value.name : value.dataset_id,
    table: typeof value.table === "string" ? value.table : "",
    columns: readColumns(value.columns),
    error: typeof value.error === "string" ? value.error : null,
  };
}

/** A list of dataset columns off the wire. */
export function readColumns(raw: unknown): DatasetColumnInfo[] {
  if (!Array.isArray(raw)) return [];
  return raw.filter(
    (c): c is DatasetColumnInfo =>
      Boolean(c) && typeof (c as { name?: unknown }).name === "string",
  );
}

/** Where a dataset is edited: the Analytics UI's Dataset editor (A1.17). */
export function analyticsDatasetUrl(datasetId: string): string {
  return `/analytics/#/datasets/${encodeURIComponent(datasetId)}`;
}

/** Where a new dataset is made — over `table`, when one is given. */
export function newDatasetUrl(table?: string): string {
  return table ? `/analytics/#/datasets/new?table=${encodeURIComponent(table)}` : "/analytics/#/datasets/new";
}

/** The default split: four fifths fitted, one fifth held out, no validation
 * rows — the shape of a fit with no hyperparameter search (`Split::default`). */
export const DEFAULT_SPLIT: Split = { train: 0.8, validation: 0.0, test: 0.2, seed: 0 };

/** A split off the wire, falling back to the default it was created with. */
export function readSplit(raw: unknown): Split {
  if (raw && typeof raw === "object") {
    const value = raw as Partial<Split>;
    if (
      typeof value.train === "number" &&
      typeof value.validation === "number" &&
      typeof value.test === "number"
    ) {
      return {
        train: value.train,
        validation: value.validation,
        test: value.test,
        seed: typeof value.seed === "number" ? value.seed : 0,
      };
    }
  }
  return { ...DEFAULT_SPLIT };
}

/** An outcome off the wire, or `null` for one that is absent or unrecognised. */
export function readOutcome(raw: unknown): Outcome | null {
  if (!raw || typeof raw !== "object") return null;
  const tag = (raw as { outcome?: unknown }).outcome;
  if (typeof tag !== "string") return null;
  if (!["regression", "classification", "cluster", "embedding", "test", "posterior"].includes(tag)) {
    return null;
  }
  return raw as Outcome;
}

/** The metrics of each split, from an instance's `metrics` column. */
export function readMetrics(raw: unknown): SplitMetrics {
  if (!raw || typeof raw !== "object") return {};
  return raw as SplitMetrics;
}

/** The parameter blocks of an instance, dropping anything that is not one of
 * the three renderings. */
export function readParameters(raw: unknown[]): ParameterBlock[] {
  return raw.filter((block): block is ParameterBlock => {
    if (!block || typeof block !== "object") return false;
    const tag = (block as { block?: unknown }).block;
    return tag === "scalar" || tag === "table" || tag === "text";
  });
}

/** The grid points an instance recorded, in the order they were tried. */
export function readSearch(raw: unknown[]): GridPoint[] {
  return raw.filter((point): point is GridPoint => {
    if (!point || typeof point !== "object") return false;
    const hyper = (point as { hyperparameters?: unknown }).hyperparameters;
    return Boolean(hyper) && typeof hyper === "object";
  });
}

/** The row counts of a fit, or `null` for an instance that has not got that far. */
export function readRowCounts(raw: unknown): RowCounts | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as Partial<RowCounts>;
  return typeof value.selected === "number"
    ? {
        selected: value.selected,
        train: value.train ?? 0,
        validation: value.validation ?? 0,
        test: value.test ?? 0,
        dropped: value.dropped ?? 0,
      }
    : null;
}

/** The encoding an instance was fitted with, or `null` for a fit that has none
 * (a hypothesis test, or one that has not finished). */
export function readEncoding(raw: unknown): Encoding | null {
  if (!raw || typeof raw !== "object") return null;
  const columns = (raw as { columns?: unknown }).columns;
  if (!Array.isArray(columns)) return null;
  return {
    columns: columns.filter((column): column is ColumnEncoding => {
      if (!column || typeof column !== "object") return false;
      const tag = (column as { encoding?: unknown }).encoding;
      return (
        tag === "passthrough" || tag === "standardised" || tag === "one_hot" || tag === "epoch"
      );
    }),
  };
}

/** A prediction off the wire, or `null` for one that is not a shape we render. */
export function readPrediction(raw: unknown): Prediction | null {
  if (!raw || typeof raw !== "object") return null;
  const tag = (raw as { prediction?: unknown }).prediction;
  if (tag === "number" || tag === "class" || tag === "cluster" || tag === "vector") {
    return raw as Prediction;
  }
  return null;
}

// --- the hyperparameter grid ------------------------------------------------

/** The most grid points a fit will run (`sc_model::MAX_GRID_POINTS`). Duplicated
 * here to warn *on the form*; the server is what refuses. */
export const MAX_GRID_POINTS = 200;

/**
 * One hyperparameter box read as what it means: a value, a list of values, or
 * nothing at all.
 *
 * The box is one control for both because §11 says they are one thing — "a list
 * of one and a scalar are the same search" — and asking the admin to tick "this
 * is a search" before typing a second number would be a second way to say it.
 * Commas separate; a blank box is `undefined` and is *not sent*, so the
 * provider's own default applies rather than a zero this form invented.
 *
 * A value that is not of the declared type is passed through as the text that
 * was typed, for the reason `buildConfig` does the same: the server validates
 * against the same declaration and its message names the hyperparameter, which
 * is a better error than anything this could invent.
 */
export function parseGridValue(text: string, type: string): unknown {
  const parts = text
    .split(",")
    .map((part) => part.trim())
    .filter((part) => part !== "");
  if (parts.length === 0) return undefined;
  const values = parts.map((part) => coerceValue(part, type));
  // A trailing comma is how a one-element *list* is written, and it is worth
  // keeping: `[8]` and `8` fit identically, but the box the admin comes back to
  // should say what they typed.
  return values.length === 1 && !text.includes(",") ? values[0] : values;
}

/** One typed hyperparameter value from the text of it. */
function coerceValue(text: string, type: string): unknown {
  if (type === "bool") return text === "true" || text === "yes" || text === "1";
  if (type === "int" || type === "float") {
    const number = Number(text);
    return Number.isFinite(number) ? number : text;
  }
  return text;
}

/**
 * A stored hyperparameter back as the text of the box it is edited in.
 *
 * The inverse of [`parseGridValue`] **up to §11's equivalence**: a one-element
 * list prints as the bare value, because that is the same search and the shorter
 * thing to read.
 */
export function printGridValue(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (Array.isArray(value)) return value.map((one) => printGridValue(one)).join(", ");
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

/** Every hyperparameter box read into the object `saveModel` takes. */
export function buildHyperparameters(
  spec: FieldSpec[],
  values: Record<string, string>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const field of spec) {
    const parsed = parseGridValue(values[field.name] ?? "", field.type);
    if (parsed !== undefined) out[field.name] = parsed;
  }
  return out;
}

/** A stored hyperparameter object back into the boxes that edit it. */
export function readHyperparameters(raw: unknown): Record<string, string> {
  const out: Record<string, string> = {};
  if (raw && typeof raw === "object") {
    for (const [key, value] of Object.entries(raw as Record<string, unknown>)) {
      out[key] = printGridValue(value);
    }
  }
  return out;
}

/**
 * How many fits this hyperparameter space comes to: the product of the lists,
 * with the scalars held fixed.
 *
 * `1` is "no search", which is the common case and the one that must not pay for
 * the uncommon one. `0` means some list is empty — a search over nothing, which
 * the server refuses, and which the form should say so about while it is still
 * being typed.
 */
export function gridPoints(hyperparameters: Record<string, unknown>): number {
  let points = 1;
  for (const value of Object.values(hyperparameters)) {
    if (Array.isArray(value)) points *= value.length;
  }
  return points;
}

// --- the outcome, and the metrics it chooses --------------------------------

/** An outcome as a sentence: what a fit of this model answers, per row. */
export function outcomeSummary(outcome: Outcome | null): string {
  if (!outcome) return "—";
  switch (outcome.outcome) {
    case "regression":
      return `Regression on ${outcome.label}`;
    case "classification":
      return `Classification of ${outcome.label}`;
    case "cluster":
      return "Clustering";
    case "embedding":
      return `Embedding (${outcome.dimensions} components)`;
    case "test":
      return "Hypothesis test";
    case "posterior":
      return "Posterior";
  }
}

/** One row of the metrics table: what it is called, and what it came to. */
export type MetricRow = { label: string; value: string };

/**
 * One split's metrics as labelled numbers, chosen by the metric set's own
 * variant.
 *
 * This is the outcome-to-metric-set mapping the screen renders, and it is a
 * mapping and not a merge: a regression has no accuracy and a clustering has no
 * R², so there is no row for one. A hypothesis test scores nothing at all — its
 * parameters are the answer — and answers the empty list.
 */
export function metricRows(metrics: Metrics | null | undefined): MetricRow[] {
  if (!metrics) return [];
  switch (metrics.metrics) {
    case "regression":
      return [
        { label: "R²", value: formatNumber(metrics.r2) },
        { label: "RMSE", value: formatNumber(metrics.rmse) },
        { label: "MAE", value: formatNumber(metrics.mae) },
        { label: "Rows", value: String(metrics.rows) },
      ];
    case "classification":
      return [
        { label: "Accuracy", value: formatNumber(metrics.accuracy) },
        { label: "Classes", value: String(metrics.classes.length) },
        { label: "Rows", value: String(metrics.rows) },
      ];
    case "clustering":
      return [
        { label: "Within-cluster sum of squares", value: formatNumber(metrics.wcss) },
        { label: "Clusters", value: String(metrics.sizes.length) },
        { label: "Cluster sizes", value: metrics.sizes.join(", ") },
        { label: "Rows", value: String(metrics.rows) },
      ];
    case "embedding":
      return [
        {
          label: "Explained variance",
          value: metrics.explained_variance.map((v) => formatNumber(v)).join(", "),
        },
        {
          label: "Total explained",
          value: formatNumber(
            metrics.explained_variance.reduce((sum: number, v) => sum + (v ?? 0), 0),
          ),
        },
        { label: "Rows", value: String(metrics.rows) },
      ];
    case "none":
      return [];
    case "posterior":
      return [
        { label: "Chains", value: String(metrics.chains) },
        { label: "Draws per chain", value: String(metrics.draws_per_chain) },
        {
          label: "Divergent transitions",
          value:
            metrics.divergent > 0 && metrics.divergent_per_chain.length > 0
              ? `${metrics.divergent} (${metrics.divergent_per_chain.join(" / ")})`
              : String(metrics.divergent),
        },
        { label: "Iterations at the maximum tree depth", value: String(metrics.max_treedepth_hits) },
        { label: "E-BFMI per chain", value: listOf(metrics.ebfmi, 2) },
        { label: "Largest R̂", value: formatNumber(metrics.max_rhat) },
        { label: "Smallest bulk ESS", value: formatNumber(metrics.min_ess_bulk, 3) },
        { label: "Smallest tail ESS", value: formatNumber(metrics.min_ess_tail, 3) },
        { label: "Wall time per chain (s)", value: listOf(metrics.wall_seconds, 3) },
      ];
    case "posterior_mode":
      return [
        { label: "Log density at the mode", value: formatNumber(metrics.log_density) },
        ...(metrics.iterations == null
          ? []
          : [{ label: "Iterations", value: String(metrics.iterations) }]),
        { label: "Wall time (s)", value: listOf(metrics.wall_seconds, 3) },
      ];
    case "posterior_approximation":
      return [
        { label: "Draws", value: String(metrics.draws) },
        { label: "Smallest bulk ESS", value: formatNumber(metrics.min_ess_bulk, 3) },
        { label: "Smallest tail ESS", value: formatNumber(metrics.min_ess_tail, 3) },
        { label: "Wall time (s)", value: listOf(metrics.wall_seconds, 3) },
      ];
  }
}

/** Numbers as one cell: `0.91 / 1.02 / 0.87 / 0.95`. */
function listOf(values: (number | null)[], digits: number): string {
  return values.length === 0 ? "—" : values.map((v) => formatNumber(v, digits)).join(" / ");
}

/** The one number a fit is judged by, for the list: "R² 0.94", "accuracy 0.81".
 * The **test** split's, because a metric measured on the rows the fit was
 * computed from is not a claim about anything. */
export function headlineMetric(metrics: SplitMetrics): string | null {
  const set = metrics.test ?? metrics.validation ?? metrics.train;
  if (!set) return null;
  switch (set.metrics) {
    case "regression":
      return `R² ${formatNumber(set.r2)}`;
    case "classification":
      return `accuracy ${formatNumber(set.accuracy)}`;
    case "clustering":
      return `WCSS ${formatNumber(set.wcss)}`;
    case "embedding":
      return `explained ${formatNumber(
        set.explained_variance.reduce((sum: number, v) => sum + (v ?? 0), 0),
      )}`;
    case "none":
      return null;
    case "posterior":
      return `R̂ ≤ ${formatNumber(set.max_rhat)}`;
    case "posterior_mode":
      return `log density ${formatNumber(set.log_density)}`;
    case "posterior_approximation":
      return `${set.draws} draws`;
  }
}

// --- numbers on a screen ----------------------------------------------------

/**
 * A number as a table cell: enough digits to be worth reading and not so many
 * that a column of them cannot be scanned.
 *
 * A null is an em dash rather than a `0`: the metrics serialise a NaN as null
 * (an R² over one row is not zero, it is undefined), and printing it as a number
 * would be a wrong answer rather than a missing one.
 */
export function formatNumber(value: unknown, digits = 4): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "boolean") return value ? "yes" : "no";
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return typeof value === "string" ? value : "—";
  }
  if (Number.isInteger(value) && Math.abs(value) < 1e15) return String(value);
  const magnitude = Math.abs(value);
  if (magnitude >= 1e7 || magnitude < 1e-4) return value.toExponential(2);
  return String(Number(value.toPrecision(digits)));
}

/**
 * A p-value, which is the one number in a coefficient table nobody wants in
 * full.
 *
 * `1.2e-16` is what a regression on well-separated data actually produces, and
 * a column of those is unreadable — while the question asked of a p-value is
 * almost always which side of a threshold it falls, which `< 0.001` answers
 * better than any number of digits. Above the floor it prints to three decimals,
 * so `0.049` and `0.051` are still distinguishable, which is the one place the
 * exact value earns its space.
 */
export function formatPValue(value: unknown): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return formatNumber(value);
  if (value < 0.001) return "< 0.001";
  return value.toFixed(3);
}

/** The conventional significance marks, for the column beside a p-value. */
export function significanceStars(value: unknown): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return "";
  if (value < 0.001) return "***";
  if (value < 0.01) return "**";
  if (value < 0.05) return "*";
  if (value < 0.1) return ".";
  return "";
}

/** Whether a parameter table's column holds p-values, by the names providers
 * give it (`p` here, `p-value` on a scalar, and the two spellings a provider
 * from a module is likely to use). */
export function isPValueColumn(column: string): boolean {
  const name = column.trim().toLowerCase();
  return (
    name === "p" ||
    name === "p-value" ||
    name === "p value" ||
    name === "p_value" ||
    name === "pr(>|t|)"
  );
}

/** One cell of a parameter table, formatted by what its column holds. */
export function formatParameterCell(column: string, value: unknown): string {
  if (isPValueColumn(column)) return formatPValue(value);
  if (typeof value === "string") return value;
  if (value === null || value === undefined) return "—";
  if (typeof value === "object") return JSON.stringify(value);
  return formatNumber(value);
}

/** A prediction as one line: the number, the class and its probability, the
 * cluster, or the vector. */
export function predictionSummary(prediction: Prediction | null): string {
  if (!prediction) return "—";
  switch (prediction.prediction) {
    case "number":
      return formatNumber(prediction.value);
    case "class":
      return prediction.probability == null
        ? prediction.class
        : `${prediction.class} (p ${formatNumber(prediction.probability)})`;
    case "cluster":
      return `cluster ${prediction.cluster}`;
    case "vector":
      return `[${prediction.values.map((v) => formatNumber(v)).join(", ")}]`;
  }
}

// --- the instance list ------------------------------------------------------

/**
 * The instances of a model, in the order the screen shows them: the **active**
 * fit first, then newest first.
 *
 * Active first because it is what everything outside this screen means by "the
 * model" — a trigger names the model and gets that fit — so it is the row the
 * admin came to check. Newest next because the list is a history and a refit is
 * the reason to look at one. The sort is stable in the created time, so two fits
 * started in the same millisecond keep the order the server sent them in rather
 * than swapping about between polls.
 */
export function orderInstances<T extends { active: boolean; created: string }>(
  instances: T[],
): T[] {
  return instances
    .map((instance, index) => ({ instance, index }))
    .sort((a, b) => {
      if (a.instance.active !== b.instance.active) return a.instance.active ? -1 : 1;
      if (a.instance.created !== b.instance.created) {
        return a.instance.created < b.instance.created ? 1 : -1;
      }
      return a.index - b.index;
    })
    .map((entry) => entry.instance);
}

/** What an instance is called on the screen: its name, else when it was fitted —
 * because a fit that was not named is still addressed by *when* it happened. */
export function instanceLabel(instance: { name: string; created: string }): string {
  return instance.name.trim() !== "" ? instance.name : formatTimestamp(instance.created);
}

/** A timestamp as the admin's own locale writes it, or the raw string when it is
 * not a time at all. */
export function formatTimestamp(value: string): string {
  const when = new Date(value);
  return Number.isNaN(when.getTime()) ? value : when.toLocaleString();
}

// --- the dataset builder's picker -------------------------------------------

/** One box of the "try a row" form: which feature, how it is typed, and — for a
 * category — the values this fit actually saw. */
export type FeatureInput = {
  name: string;
  /** What the value has to be for the fit to accept it. */
  kind: "number" | "category" | "date";
  /** The categories the fit was shown, for a one-hot column. A value not in this
   * list is refused **by name** at predict time rather than encoded as a row of
   * zeros, so offering the list is the difference between a form that works and
   * one that produces a confident refusal. */
  categories?: string[];
  /** The dataset formula behind it, as a hint under the box. */
  expr?: string;
};

/**
 * The boxes to ask for, from the encoding this instance was fitted with.
 *
 * The **encoding** and not the dataset, because the encoding is what a
 * prediction is applied through: it names exactly the feature columns, in the
 * order they were fitted, and it never includes the label — which is the thing
 * being predicted and is usually absent from the row being asked about.
 */
export function featureInputs(
  encoding: Encoding | null,
  dataset: ModelDataset | null,
): FeatureInput[] {
  if (!encoding) return [];
  const formulas = new Map(
    (dataset?.columns ?? []).filter((c) => c.expr && c.expr !== c.name).map((c) => [c.name, c.expr]),
  );
  return encoding.columns.map((column) => ({
    name: column.column,
    kind:
      column.encoding === "one_hot"
        ? ("category" as const)
        : column.encoding === "epoch"
          ? ("date" as const)
          : ("number" as const),
    categories: column.encoding === "one_hot" ? column.categories : undefined,
    expr: formulas.get(column.column),
  }));
}

/**
 * What a typed box sends: the value **as the fit's frame is typed**, not as
 * text.
 *
 * A numeric feature wants a JSON number and refuses `"100"` by name, which is
 * right — the row being predicted is encoded the way the fit was or it fails —
 * and it means the coercion belongs here, where the box is. `true`/`false` in a
 * numeric column become 1 and 0, which is what a boolean feature was
 * passed through as at fit time.
 *
 * A value that will not coerce is sent **as it was typed**, so the server's
 * refusal names the column and the value rather than this form inventing a
 * number nobody entered.
 */
export function typedFeatureValue(text: string, kind: FeatureInput["kind"]): unknown {
  const trimmed = text.trim();
  if (kind === "category") return text;
  if (kind === "number") {
    if (trimmed === "true") return 1;
    if (trimmed === "false") return 0;
    const number = Number(trimmed);
    return Number.isFinite(number) ? number : text;
  }
  // A date crosses as epoch seconds or as a string the server parses.
  const epoch = Number(trimmed);
  return Number.isFinite(epoch) && trimmed !== "" ? epoch : text;
}


// --- a program's interface (Stan TODO §5) -----------------------------------
//
// Nothing below names Stan. A provider that binds data (`binds_data`) declares
// an interface and receives bound data; the binding keys are the host's
// (`sc_model::BINDINGS_KEY` and its siblings), so the form built from them is
// the same for any such provider.

/** One size expression of a declaration: `N`, `J + 1`, `num_elements(y)`. */
export type SizeExpr = { text: string };

/** What one scalar of a declared variable is. */
export type Element = "int" | "real" | "complex" | "tuple";

/** One top-level declaration of a program block (`sc_model::Declaration`). */
export type Declaration = {
  name: string;
  element: Element;
  /** The full shape, outer to inner; empty for a scalar. */
  dims: SizeExpr[];
  /** The declared type as written: `array[N] int<lower=1, upper=J>`. */
  stan_type: string;
  lower?: string | null;
  upper?: string | null;
};

/** What a program declares, block by block. */
export type Interface = {
  data: Declaration[];
  parameters: Declaration[];
  transformed: Declaration[];
  generated: Declaration[];
};

/** An interface off the wire, or `null` for one that is absent. */
export function readInterface(raw: unknown): Interface | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as Partial<Record<keyof Interface, unknown>>;
  const block = (list: unknown): Declaration[] =>
    Array.isArray(list)
      ? list.filter(
          (d): d is Declaration =>
            Boolean(d) &&
            typeof (d as Declaration).name === "string" &&
            Array.isArray((d as Declaration).dims),
        )
      : [];
  return {
    data: block(value.data),
    parameters: block(value.parameters),
    transformed: block(value.transformed),
    generated: block(value.generated),
  };
}

/** A related dataset: a dataset over another table, under the name bindings
 * address it by (Stan TODO §7). */
export type NamedDataset = {
  name: string;
  /** The named dataset's id; empty while none is picked. */
  dataset_id: string;
  label?: string | null;
  /** What the server resolved it to, when it did. */
  dataset_name?: string;
  /** The table its rows start from. */
  table?: string;
  columns?: DatasetColumnInfo[];
  error?: string | null;
};

/** The name bindings give the model's own dataset. */
export const MAIN_DATASET = "main";

/** A model's related datasets off the wire. */
export function readRelated(raw: unknown): NamedDataset[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .filter(
      (r): r is Record<string, unknown> & { name: string } =>
        Boolean(r) && typeof (r as { name?: unknown }).name === "string",
    )
    .map((r) => ({
      name: r.name,
      dataset_id: typeof r.dataset_id === "string" ? r.dataset_id : "",
      label: typeof r.label === "string" && r.label.trim() !== "" ? r.label : null,
      dataset_name: typeof r.dataset_name === "string" ? r.dataset_name : undefined,
      table: typeof r.table === "string" ? r.table : undefined,
      columns: readColumns(r.columns),
      error: typeof r.error === "string" ? r.error : null,
    }));
}

/** The related datasets as `saveModel` takes them. */
export function relatedBody(related: NamedDataset[]) {
  return related.map((r) => ({
    name: r.name.trim(),
    dataset_id: r.dataset_id,
    ...(r.label && r.label.trim() !== "" ? { label: r.label.trim() } : {}),
  }));
}

// --- the binding editor (Stan TODO §§9, 18) ---------------------------------

/** The configuration keys the binding editor owns. They are the host's, not a
 * provider's, so every provider that binds data spells them the same way. */
export const BINDINGS_KEY = "bindings";
export const DIMENSIONS_KEY = "dimensions";
export const POLICIES_KEY = "policies";

/** The two settings that say where a binding provider's program is: a file
 * store and a path in it — what `getProgramInterface` takes (§6). */
export const PROGRAM_STORE_KEY = "program_store";
export const PROGRAM_KEY = "program";

/** Every configuration key the model form edits with its own controls rather
 * than as a generic setting, for a provider that binds data. */
export const BINDING_FORM_KEYS = [
  PROGRAM_STORE_KEY,
  PROGRAM_KEY,
  BINDINGS_KEY,
  DIMENSIONS_KEY,
  POLICIES_KEY,
];

/** What one field of a binding (or a dimension) holds, which decides the
 * control it is edited with. */
export type FieldKind =
  | "dataset"
  | "column"
  | "columns"
  | "dimension"
  | "variable"
  | "text"
  | "json"
  | "bool"
  | "choice";

/** One field of a kind: where it sits in the JSON (a dotted path for the
 * nested `over`, `rows`, `cols` and `time`), how it is edited, and whether the
 * kind needs it. */
export type KindField = {
  path: string;
  kind: FieldKind;
  required: boolean;
  /** For a `choice`. */
  options?: string[];
  /** For a `column` field: the path of the field naming its dataset, when it
   * is not the binding's own `dataset` (a series' axis column is over the
   * binding's dataset; nothing here is over another). */
  of?: string;
};

/** One binding kind: its wire name, the rank it produces (`null` when the
 * literal decides — a `value`), whether it can only produce reals, and its
 * fields. */
export type KindSpec = {
  kind: string;
  rank: number | null;
  real: boolean;
  fields: KindField[];
};

const f = (
  path: string,
  kind: FieldKind,
  required = true,
  options?: string[],
): KindField => ({ path, kind, required, ...(options ? { options } : {}) });

const DATASET = f("dataset", "dataset");
const COLUMN = f("column", "column");
const AGGREGATES = ["count", "sum", "mean", "min", "max", "first", "last", "refuse"];
const along = (axis: string): KindField[] => [
  f(`${axis}.dimension`, "dimension"),
  f(`${axis}.column`, "column", false),
  f(`${axis}.match`, "text", false),
];
const EDGES = [
  DATASET,
  f("from", "column"),
  f("to", "column"),
  f("dimension", "dimension"),
  f("match", "text", false),
];
const DEDUPED_EDGES = [...EDGES, f("symmetric", "choice", false, ["dedupe", "keep"])];
const POINTS = [DATASET, f("lat", "column"), f("lon", "column")];

/**
 * Every binding kind (`sc_model::Binding`), with the rank and element type it
 * can produce — the same table the server's save-time check reads (§10) — and
 * the fields its row of the binding table edits.
 */
export const BINDING_KINDS: KindSpec[] = [
  { kind: "value", rank: null, real: false, fields: [f("value", "json")] },
  { kind: "count", rank: 0, real: false, fields: [DATASET] },
  { kind: "size", rank: 0, real: false, fields: [f("dimension", "dimension")] },
  {
    kind: "column",
    rank: 1,
    real: false,
    fields: [DATASET, COLUMN, f("time.unit", "choice", false, ["seconds", "minutes", "hours", "days", "weeks"]), f("time.origin", "text", false)],
  },
  { kind: "columns", rank: 2, real: false, fields: [DATASET, f("columns", "columns")] },
  {
    kind: "design",
    rank: 2,
    real: true,
    fields: [DATASET, f("columns", "columns"), f("standardise", "bool", false)],
  },
  { kind: "width", rank: 0, real: false, fields: [f("of", "variable")] },
  {
    kind: "index",
    rank: 1,
    real: false,
    fields: [DATASET, COLUMN, f("dimension", "dimension"), f("match", "text", false)],
  },
  { kind: "present", rank: 1, real: false, fields: [DATASET, COLUMN] },
  { kind: "absent", rank: 1, real: false, fields: [DATASET, COLUMN] },
  { kind: "count_present", rank: 0, real: false, fields: [DATASET, COLUMN] },
  { kind: "count_absent", rank: 0, real: false, fields: [DATASET, COLUMN] },
  { kind: "present_values", rank: 1, real: false, fields: [DATASET, COLUMN] },
  { kind: "segment_start", rank: 1, real: false, fields: [DATASET, f("index", "variable")] },
  { kind: "segment_size", rank: 1, real: false, fields: [DATASET, f("index", "variable")] },
  {
    kind: "series",
    rank: 1,
    real: false,
    fields: [
      DATASET,
      f("column", "column", false),
      ...along("over"),
      f("aggregate", "choice", false, AGGREGATES),
      f("fill", "json", false),
    ],
  },
  {
    kind: "series_present",
    rank: 1,
    real: false,
    fields: [DATASET, f("column", "column", false), ...along("over")],
  },
  {
    kind: "cells",
    rank: 2,
    real: false,
    fields: [
      DATASET,
      f("column", "column", false),
      ...along("rows"),
      ...along("cols"),
      f("aggregate", "choice", false, AGGREGATES),
      f("fill", "json", false),
    ],
  },
  {
    kind: "cells_present",
    rank: 2,
    real: false,
    fields: [DATASET, f("column", "column", false), ...along("rows"), ...along("cols")],
  },
  { kind: "edge_count", rank: 0, real: false, fields: DEDUPED_EDGES },
  { kind: "edge_from", rank: 1, real: false, fields: DEDUPED_EDGES },
  { kind: "edge_to", rank: 1, real: false, fields: DEDUPED_EDGES },
  { kind: "adjacency", rank: 2, real: false, fields: EDGES },
  { kind: "components", rank: 0, real: false, fields: EDGES },
  { kind: "component", rank: 1, real: false, fields: EDGES },
  { kind: "icar_scale", rank: 0, real: true, fields: EDGES },
  {
    kind: "points",
    rank: 2,
    real: true,
    fields: [...POINTS, f("project", "bool", false)],
  },
  { kind: "distances", rank: 2, real: true, fields: POINTS },
];

/** The kinds of dimension a configuration declares (§8); a dataset's rows are
 * one without being declared. */
export const DIMENSION_KINDS: KindSpec[] = [
  { kind: "values", rank: null, real: false, fields: [DATASET, COLUMN] },
  {
    kind: "time_grid",
    rank: null,
    real: false,
    fields: [
      DATASET,
      COLUMN,
      f("step", "text"),
      f("start", "text", false),
      f("end", "text", false),
      f("horizon", "json", false),
    ],
  },
];

/** The spec of binding kind `kind`, if it is one. */
export function bindingKind(kind: string): KindSpec | undefined {
  return BINDING_KINDS.find((k) => k.kind === kind);
}

/**
 * The binding kinds that can produce `decl` — the kind picker's options.
 *
 * The server's save-time rule (§10), mirrored: the kind's rank must be the
 * declaration's, and a kind that can only produce reals cannot bind an `int`.
 * A `value` fits anything, since its literal decides. A `complex` or a `tuple`
 * in the data block is refused by the server by name, so nothing fits it.
 */
export function kindsFor(decl: Pick<Declaration, "element" | "dims">): string[] {
  if (decl.element !== "int" && decl.element !== "real") return [];
  const rank = decl.dims.length;
  return BINDING_KINDS.filter(
    (k) => k.rank === null || (k.rank === rank && !(k.real && decl.element === "int")),
  ).map((k) => k.kind);
}

/** A binding (or a dimension) as its row edits it: the kind, and every field
 * as the text of its control. */
export type KindDraft = {
  kind: string;
  fields: Record<string, string>;
  /** A stored value of a kind this form does not know, kept whole so saving
   * the form does not lose it. */
  raw?: unknown;
};

/** An empty row. */
export const EMPTY_DRAFT: KindDraft = { kind: "", fields: {} };

function getPath(value: Record<string, unknown>, path: string): unknown {
  let at: unknown = value;
  for (const part of path.split(".")) {
    if (!at || typeof at !== "object") return undefined;
    at = (at as Record<string, unknown>)[part];
  }
  return at;
}

function setPath(value: Record<string, unknown>, path: string, field: unknown): void {
  const parts = path.split(".");
  let at = value;
  for (const part of parts.slice(0, -1)) {
    if (!at[part] || typeof at[part] !== "object") at[part] = {};
    at = at[part] as Record<string, unknown>;
  }
  at[parts[parts.length - 1]] = field;
}

/**
 * A stored binding (or dimension) as the text of its row's controls — the
 * inverse of [`parseDraft`] for every binding that parse can produce.
 *
 * A list of columns prints comma-separated; a literal (`value`, `fill`,
 * `horizon`) prints as JSON; a flag prints as `true` or nothing.
 */
export function printDraft(specs: KindSpec[], raw: unknown): KindDraft {
  if (!raw || typeof raw !== "object") return { ...EMPTY_DRAFT, fields: {} };
  const value = raw as Record<string, unknown>;
  const kind = typeof value.kind === "string" ? value.kind : "";
  const spec = specs.find((k) => k.kind === kind);
  if (!spec) return { kind, fields: {}, raw };
  const fields: Record<string, string> = {};
  for (const field of spec.fields) {
    const at = getPath(value, field.path);
    if (at === undefined || at === null) continue;
    if (field.kind === "columns" && Array.isArray(at)) fields[field.path] = at.join(", ");
    else if (field.kind === "json") fields[field.path] = JSON.stringify(at);
    else if (field.kind === "bool") fields[field.path] = at === true ? "true" : "";
    else fields[field.path] = String(at);
  }
  return { kind, fields };
}

/** What a row's controls come to: the binding as the configuration writes it,
 * and the mistakes this form can see without the data. */
export type ParsedDraft = { value: Record<string, unknown> | null; problems: string[] };

/**
 * A row's controls read back into a binding (or a dimension).
 *
 * The binding is always produced when there is a kind, even with a problem:
 * a required field left empty is left out, and a literal that is not JSON is
 * sent as its text — so the server, which checks the same thing against the
 * program, names the mistake in its own sentence, and the problem here is
 * only said early, on the row.
 */
export function parseDraft(specs: KindSpec[], draft: KindDraft): ParsedDraft {
  if (draft.kind === "") return { value: null, problems: [] };
  const spec = specs.find((k) => k.kind === draft.kind);
  if (!spec) {
    return draft.raw && typeof draft.raw === "object"
      ? { value: draft.raw as Record<string, unknown>, problems: [] }
      : { value: { kind: draft.kind }, problems: [`\`${draft.kind}\` is not a kind this form knows`] };
  }
  const value: Record<string, unknown> = { kind: draft.kind };
  const problems: string[] = [];
  for (const field of spec.fields) {
    const text = (draft.fields[field.path] ?? "").trim();
    if (text === "") {
      if (field.required) problems.push(`\`${field.path}\` is required`);
      continue;
    }
    switch (field.kind) {
      case "columns": {
        const list = text.split(",").map((c) => c.trim()).filter((c) => c !== "");
        setPath(value, field.path, list);
        break;
      }
      case "bool":
        if (text === "true") setPath(value, field.path, true);
        break;
      case "json":
        try {
          setPath(value, field.path, JSON.parse(text));
        } catch {
          problems.push(`\`${field.path}\` is not a JSON value`);
          setPath(value, field.path, text);
        }
        break;
      default:
        setPath(value, field.path, text);
    }
  }
  return { value, problems };
}

/** A map of stored bindings (or dimensions) as drafts, keyed as stored. */
export function printDrafts(specs: KindSpec[], raw: unknown): Record<string, KindDraft> {
  const out: Record<string, KindDraft> = {};
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    for (const [name, value] of Object.entries(raw as Record<string, unknown>)) {
      out[name] = printDraft(specs, value);
    }
  }
  return out;
}

/** Every draft with a kind, parsed, as the configuration object; the rows with
 * no kind yet are left out, which the server then names as unbound. */
export function parseDrafts(
  specs: KindSpec[],
  drafts: Record<string, KindDraft>,
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [name, draft] of Object.entries(drafts)) {
    const parsed = parseDraft(specs, draft);
    if (parsed.value) out[name] = parsed.value;
  }
  return out;
}

/**
 * Bind automatically (`suggestBindings`, §18) folded into the table: a
 * suggestion fills a row **only if it is empty**. A variable the admin bound is
 * never second-guessed — the server already proposes nothing for it, and this
 * holds that true of a row the admin filled after the request was sent.
 * Answers the drafts and the names that were filled.
 */
export function applySuggestions(
  drafts: Record<string, KindDraft>,
  suggested: unknown,
): { drafts: Record<string, KindDraft>; filled: string[] } {
  const out = { ...drafts };
  const filled: string[] = [];
  if (suggested && typeof suggested === "object") {
    for (const [name, binding] of Object.entries(suggested as Record<string, unknown>)) {
      if ((out[name]?.kind ?? "") !== "") continue;
      out[name] = printDraft(BINDING_KINDS, binding);
      filled.push(name);
    }
  }
  return { drafts: out, filled };
}

/**
 * The dimensions a binding may name: every dataset (its rows), every declared
 * dimension, and a time grid's `name.future` — the horizon alone (§8).
 */
export function dimensionNames(
  datasets: string[],
  dimensions: Record<string, KindDraft>,
): string[] {
  const names = [...datasets];
  for (const [name, draft] of Object.entries(dimensions)) {
    if (name.trim() === "") continue;
    names.push(name);
    if (draft.kind === "time_grid") names.push(`${name}.future`);
  }
  return names;
}

/** One dataset's two policies (§10): what a null in a bound column does, and
 * what an index value that is not in its dimension does. */
export type Policies = { nulls: "refuse" | "drop"; unknown: "refuse" | "drop" };

/** Stored policies, per dataset; `refuse` wherever nothing is said. */
export function readPolicies(raw: unknown): Record<string, Policies> {
  const out: Record<string, Policies> = {};
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    for (const [name, value] of Object.entries(raw as Record<string, unknown>)) {
      const v = (value ?? {}) as Record<string, unknown>;
      out[name] = {
        nulls: v.nulls === "drop" ? "drop" : "refuse",
        unknown: v.unknown === "drop" ? "drop" : "refuse",
      };
    }
  }
  return out;
}

/** Policies as the configuration writes them: only the datasets that say
 * `drop` somewhere, since `refuse` is what absence means. */
export function buildPolicies(policies: Record<string, Policies>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [name, p] of Object.entries(policies)) {
    if (p.nulls === "refuse" && p.unknown === "refuse") continue;
    out[name] = {
      ...(p.nulls === "drop" ? { nulls: "drop" } : {}),
      ...(p.unknown === "drop" ? { unknown: "drop" } : {}),
    };
  }
  return out;
}

/** One `data` variable's row of Preview data. */
export type VariablePreview = {
  name: string;
  stan_type: string;
  binding?: string | null;
  shape?: number[] | null;
  first: unknown[];
  error?: string | null;
};

/** A bound value's shape as it reads: `919`, `919 × 3`, or `scalar`. */
export function shapeText(shape: number[] | null | undefined): string {
  if (!shape) return "";
  return shape.length === 0 ? "scalar" : shape.join(" × ");
}

// --- a running fit (Stan TODO §13) ------------------------------------------

/** One chain's progress: CmdStan's `Iteration: 400 / 2000 (Warmup)`, read. */
export type ChainProgress = {
  chain: number;
  iteration: number;
  total: number;
  phase: "warmup" | "sampling";
};

/** Where a running posterior fit has got to. */
export type FitProgress = {
  stage: "queued" | "compiling" | "sampling" | "summarising";
  chains: ChainProgress[];
};

/** An instance's `progress`, or `null` when there is none. */
export function readProgress(raw: unknown): FitProgress | null {
  if (!raw || typeof raw !== "object") return null;
  const value = raw as { stage?: unknown; chains?: unknown };
  if (typeof value.stage !== "string") return null;
  const chains = Array.isArray(value.chains)
    ? value.chains
        .filter(
          (c): c is ChainProgress =>
            Boolean(c) && typeof (c as ChainProgress).chain === "number",
        )
        .sort((a, b) => a.chain - b.chain)
    : [];
  return { stage: value.stage as FitProgress["stage"], chains };
}

/** A chain's progress as a percentage, whole. */
export function chainPercent(chain: ChainProgress): number {
  if (chain.total <= 0) return 0;
  return Math.max(0, Math.min(100, Math.round((100 * chain.iteration) / chain.total)));
}

// --- the warnings (Stan TODO §15) -------------------------------------------

/** What a posterior warning is about, as the screen heads it. */
export type WarningKind = "rhat" | "divergent" | "ebfmi" | "other" | "ess" | "treedepth";

/** The order the kinds are read in: the ones that say no summary can be
 * trusted first, then the biased, then the slow. */
const WARNING_ORDER: WarningKind[] = ["rhat", "divergent", "ebfmi", "other", "ess", "treedepth"];

/** One warning, classified. `serious` is the ones that make the posterior
 * wrong rather than imprecise or slow. */
export type ClassifiedWarning = { kind: WarningKind; serious: boolean; text: string };

/** A warning sentence's kind, by what the host's sentences say. */
export function warningKind(text: string): WarningKind {
  if (/^R̂ is /.test(text)) return "rhat";
  if (/divergent transition/.test(text)) return "divergent";
  if (/^E-BFMI /.test(text)) return "ebfmi";
  if (/effective sample size/.test(text)) return "ess";
  if (/maximum tree depth/.test(text)) return "treedepth";
  return "other";
}

/**
 * An instance's warnings in the order to read them: chains that disagree
 * first (no summary of them can be trusted), then divergences (biased draws),
 * then E-BFMI, then anything else, then too few effective draws (imprecise),
 * and a saturated tree depth last (slow, not wrong). Stable within a kind, so
 * the host's own order is kept where it has one.
 */
export function orderWarnings(warnings: string[]): ClassifiedWarning[] {
  return warnings
    .map((text, index) => ({ text, index, kind: warningKind(text) }))
    .sort(
      (a, b) =>
        WARNING_ORDER.indexOf(a.kind) - WARNING_ORDER.indexOf(b.kind) || a.index - b.index,
    )
    .map(({ text, kind }) => ({ text, kind, serious: kind === "rhat" || kind === "divergent" }));
}

// --- reading a posterior (Stan TODO §§15–16) --------------------------------

/** One output variable of a fit: its shape, and the dimension labelling each
 * axis (`null` for a numbered one) — the instance's `variables`. */
export type RecordedAxes = { dims: number[]; dimensions: (string | null)[] };

/** An instance's `variables`, with the sampler's own (`lp__`, `divergent__`)
 * left out: they are diagnostics, and the metrics already say what they say. */
export function readVariables(raw: unknown): Record<string, RecordedAxes> {
  const out: Record<string, RecordedAxes> = {};
  if (raw && typeof raw === "object" && !Array.isArray(raw)) {
    for (const [name, value] of Object.entries(raw as Record<string, unknown>)) {
      if (name.endsWith("__")) continue;
      const v = (value ?? {}) as Partial<RecordedAxes>;
      if (!Array.isArray(v.dims)) continue;
      out[name] = {
        dims: v.dims,
        dimensions: Array.isArray(v.dimensions) ? v.dimensions : v.dims.map(() => null),
      };
    }
  }
  return out;
}

/** A variable's size in elements: 1 for a scalar. */
export function elementCount(axes: RecordedAxes): number {
  return axes.dims.reduce((n, d) => n * d, 1);
}

/** A posterior summary as the screen reads it: which columns are labels, which
 * are statistics, and one row per element in element order. */
export type SummaryTable = {
  labelColumns: string[];
  statColumns: string[];
  rows: { labels: string[]; stats: (number | null)[]; key?: unknown[] }[];
};

/**
 * A summary table from its columns and rows, given how many axes the variable
 * has — its label columns come first (§15). The stored `ParameterBlock::Table`
 * and `getPosteriorSummary`'s answer are both read through this.
 */
export function summaryTable(
  columns: string[],
  rows: unknown[][],
  rank: number,
  keys?: unknown[][],
): SummaryTable {
  const labels = Math.min(rank, columns.length);
  return {
    labelColumns: columns.slice(0, labels),
    statColumns: columns.slice(labels),
    rows: rows.map((cells, i) => ({
      labels: cells.slice(0, labels).map((c) => (c === null || c === undefined ? "" : String(c))),
      stats: cells.slice(labels).map((c) => (typeof c === "number" ? c : null)),
      ...(keys?.[i] ? { key: keys[i] } : {}),
    })),
  };
}

/** A statistic of a summary row, by its column name. */
export function stat(table: SummaryTable, row: number, name: string): number | null {
  const k = table.statColumns.indexOf(name);
  return k < 0 ? null : (table.rows[row]?.stats[k] ?? null);
}

/**
 * The 1-based index array of the `row`-th element of a variable shaped
 * `dims` — the summary's rows are in element order, which is row-major (the
 * last axis fastest), the order the host sorts index arrays in.
 */
export function elementAt(row: number, dims: number[]): number[] {
  const index: number[] = new Array(dims.length).fill(1);
  let rest = row;
  for (let k = dims.length - 1; k >= 0; k -= 1) {
    const size = Math.max(1, dims[k]);
    index[k] = (rest % size) + 1;
    rest = Math.floor(rest / size);
  }
  return index;
}

/** What a draws request's `elements` carries for one element: its position,
 * as `[[1, 3]]`, which every variable has whether or not it is labelled. */
export function elementSelection(element: number[]): string {
  return JSON.stringify([element]);
}

/** An element's name as the server writes it: `alpha[Aitkin]`, `beta`,
 * `Sigma[2, 1]`. */
export function elementName(variable: string, labels: string[], element: number[]): string {
  if (element.length === 0) return variable;
  const parts = element.map((i, k) => (labels[k] && labels[k] !== "" ? labels[k] : String(i)));
  return `${variable}[${parts.join(", ")}]`;
}

/**
 * The rows of a summary an admin means by `query`: an element picked by a
 * **key** (`27001`) or a **label** (`Aitkin`), exactly first; failing that,
 * every row whose label contains it, ignoring case. A 1-based position
 * (`#3`) picks that row. Positions are this instance's private business
 * (§8), so the box speaks keys and labels and a position only when asked.
 */
export function matchElements(table: SummaryTable, query: string): number[] {
  const q = query.trim();
  if (q === "") return table.rows.map((_, i) => i);
  const position = /^#(\d+)$/.exec(q);
  if (position) {
    const i = Number(position[1]) - 1;
    return i >= 0 && i < table.rows.length ? [i] : [];
  }
  const exact = table.rows
    .map((row, i) => ({ row, i }))
    .filter(
      ({ row }) =>
        row.labels.join(", ") === q ||
        row.labels.includes(q) ||
        (row.key ?? []).some((k) => String(k) === q),
    )
    .map(({ i }) => i);
  if (exact.length > 0) return exact;
  const lower = q.toLowerCase();
  return table.rows
    .map((row, i) => ({ row, i }))
    .filter(({ row }) => row.labels.some((l) => l.toLowerCase().includes(lower)))
    .map(({ i }) => i);
}

/** Whether a variable gets a forest plot: one axis, labelled by a dimension,
 * and more than one element — the plot a hierarchical model is read by. */
export function forestable(axes: RecordedAxes): boolean {
  return axes.dims.length === 1 && axes.dims[0] > 1 && Boolean(axes.dimensions[0]);
}

/** How a forest plot's rows are ordered. */
export type ForestSort = "position" | "label" | "mean";

/** One row of a forest plot: its label, and its interval and centre. */
export type ForestRow = {
  row: number;
  label: string;
  low: number;
  centre: number;
  high: number;
};

/**
 * A forest plot's rows from a summary: the 90 % interval (`q5`–`q95`) about
 * the mean, one per element with all three defined, sorted as asked — by
 * label (in the admin's own collation, numbers as numbers), by mean
 * (smallest first, the caterpillar that shows the spread of the groups), or
 * as the dimension numbers them.
 */
export function forestRows(table: SummaryTable, sort: ForestSort): ForestRow[] {
  const rows: ForestRow[] = [];
  table.rows.forEach((row, i) => {
    const low = stat(table, i, "q5");
    const centre = stat(table, i, "mean") ?? stat(table, i, "estimate");
    const high = stat(table, i, "q95");
    if (low === null || centre === null || high === null) return;
    rows.push({ row: i, label: row.labels.join(", ") || `#${i + 1}`, low, centre, high });
  });
  if (sort === "label") {
    const collator = new Intl.Collator(undefined, { numeric: true });
    rows.sort((a, b) => collator.compare(a.label, b.label) || a.row - b.row);
  } else if (sort === "mean") {
    rows.sort((a, b) => a.centre - b.centre || a.row - b.row);
  }
  return rows;
}

/** One chain's draws of one element. */
export type ChainTrace = { chain: number; values: number[] };

/** The first selected element's draws, chain by chain, from a
 * `getModelDraws` answer — post-warmup only, NaN (`null` on the wire) as a
 * gap. */
export function chainTraces(answer: { chains: unknown[] }): ChainTrace[] {
  const out: ChainTrace[] = [];
  for (const raw of answer.chains) {
    const c = raw as { chain?: unknown; warmup?: unknown; draws?: unknown };
    if (typeof c.chain !== "number" || c.warmup === true || !Array.isArray(c.draws)) continue;
    const first = c.draws[0];
    if (!Array.isArray(first)) continue;
    out.push({
      chain: c.chain,
      values: first.map((v) => (typeof v === "number" ? v : Number.NaN)),
    });
  }
  return out.sort((a, b) => a.chain - b.chain);
}

/** One bar of a histogram: its bin's edges and how many draws fell in it. */
export type Bin = { x0: number; x1: number; count: number };

/**
 * The pooled draws as a histogram. The bin width is Freedman–Diaconis's
 * (twice the interquartile range over the cube root of the count), clamped
 * to between 10 and 60 bins so a heavy tail neither makes one bar nor a
 * comb. A constant draw (an optimiser's one point) is one bin.
 */
export function histogram(values: number[]): Bin[] {
  const xs = values.filter(Number.isFinite).sort((a, b) => a - b);
  if (xs.length === 0) return [];
  const min = xs[0];
  const max = xs[xs.length - 1];
  if (max === min) return [{ x0: min, x1: max, count: xs.length }];
  const q = (p: number) => xs[Math.min(xs.length - 1, Math.floor(p * (xs.length - 1)))];
  const iqr = q(0.75) - q(0.25);
  const width = (2 * iqr) / Math.cbrt(xs.length);
  const bins = Math.max(10, Math.min(60, width > 0 ? Math.ceil((max - min) / width) : 10));
  const step = (max - min) / bins;
  const out: Bin[] = Array.from({ length: bins }, (_, i) => ({
    x0: min + i * step,
    x1: i === bins - 1 ? max : min + (i + 1) * step,
    count: 0,
  }));
  for (const x of xs) out[Math.min(bins - 1, Math.floor((x - min) / step))].count += 1;
  return out;
}

/** Round axis ticks spanning `[min, max]`: about `count` of them, each a 1, 2
 * or 5 times a power of ten. */
export function niceTicks(min: number, max: number, count = 5): number[] {
  if (!Number.isFinite(min) || !Number.isFinite(max)) return [];
  if (min === max) return [min];
  const raw = (max - min) / Math.max(1, count);
  const power = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 5, 10].map((m) => m * power).find((s) => s >= raw) ?? 10 * power;
  const out: number[] = [];
  for (let t = Math.ceil(min / step) * step; t <= max + step * 1e-9; t += step) {
    out.push(Number(t.toPrecision(12)));
  }
  return out;
}

// --- writing a posterior back (Stan TODO §16) -------------------------------

/**
 * Where an **update** of `axes` writes: the table of the dataset its one axis
 * is the rows of, matched by key — or `null` when the variable cannot be
 * updated into anything (more than one axis, a numbered axis, or a values
 * dimension or time grid, which have no rows of their own).
 */
export function updateTarget(
  axes: RecordedAxes,
  datasets: { name: string; table: string }[],
): string | null {
  if (axes.dims.length !== 1) return null;
  const dimension = axes.dimensions[0];
  if (!dimension) return null;
  return datasets.find((d) => d.name === dimension)?.table ?? null;
}

/** The write-back dialog's state. */
export type WriteBackForm = {
  mode: "update" | "insert";
  /** Statistic → target field, for the statistics that are ticked. */
  statistics: Record<string, string>;
  table: string;
  /** Per axis heading, the field its coordinate goes into and which part. */
  coordinates: { axis: string; field: string; value: "key" | "label" | "position" }[];
  instanceField: string;
};

/** The dialog's state as `writePosterior` takes it: empty fields left out, an
 * update sending no table (the server refuses one). */
export function buildPosteriorWrite(variable: string, form: WriteBackForm) {
  const statistics: Record<string, string> = {};
  for (const [stat, field] of Object.entries(form.statistics)) {
    if (field.trim() !== "") statistics[stat] = field.trim();
  }
  if (form.mode === "update") return { variable, mode: "update", statistics };
  return {
    variable,
    mode: "insert",
    statistics,
    table: form.table.trim(),
    coordinates: form.coordinates
      .filter((c) => c.field.trim() !== "")
      .map((c) => ({ axis: c.axis, field: c.field.trim(), value: c.value })),
    instance_field: form.instanceField.trim() === "" ? null : form.instanceField.trim(),
  };
}
