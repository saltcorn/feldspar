// The plot spec and the data `renderPlot` and `renderTable` answer, as the
// Analytics UI reads them (analytics TODO A2.7; the server's types are
// `sc_analytics::plot`'s, and `crates/sc-analytics/src/plot/spec.rs` says what
// each field means).
//
// The generated client types these as JSON: a spec is stored in a workspace's
// state and sent back as it is, so the browser only needs to read it, and
// these types are what it reads.

/** A visual channel, or a facet. */
export type Channel = "x" | "y" | "color" | "size" | "shape" | "label" | "row" | "column" | "wrap";

/** The channels a layer encodes, in order. */
export const ENCODED: Channel[] = ["x", "y", "color", "size", "shape", "label"];

/** The facet channels. */
export const FACETS: Channel[] = ["row", "column", "wrap"];

/** What a layer draws. */
export type Mark =
  | "point"
  | "line"
  | "bar"
  | "area"
  | "box"
  | "rect"
  | "text"
  | "band"
  | "errorbar"
  | "mosaic";

/** Every mark, in the mark palette's order. */
export const MARKS: Mark[] = [
  "point",
  "line",
  "bar",
  "area",
  "box",
  "rect",
  "text",
  "band",
  "errorbar",
  "mosaic",
];

/** How a column is binned: neither set means the Freedman–Diaconis rule. */
export type Bin = { width?: number; bins?: number };

/** A column on a channel. */
export type FieldDef = { field: string; bin?: Bin };

/** Which column each channel of a layer shows. */
export type Encoding = Partial<Record<"x" | "y" | "color" | "size" | "shape" | "label", FieldDef>>;

/** How a summary is computed. */
export type AggregateFn = "count" | "sum" | "mean" | "median" | "min" | "max" | "sd";

/** Every summary, in the order menus offer them. */
export const AGGREGATES: AggregateFn[] = ["mean", "median", "sum", "count", "min", "max", "sd"];

/** What a layer's rows go through first. */
export type Stat =
  | { kind: "identity" }
  | { kind: "count" }
  | { kind: "aggregate"; function: AggregateFn; channel?: Channel }
  | { kind: "quantiles"; probabilities: number[] }
  | { kind: "boxplot"; coef?: number }
  | { kind: "summary"; level?: number }
  | { kind: "density"; bandwidth?: number; adjust?: number }
  | { kind: "smooth"; method?: "linear" | "loess"; span?: number; se?: boolean; level?: number }
  | { kind: "correlation"; x: string; y: string };

/** One layer. */
export type Layer = {
  mark: Mark;
  encoding: Encoding;
  /** Absent means the identity. */
  stat?: Stat;
  sample?: number;
};

/** A channel's scale. */
export type Scale = {
  kind?: "linear" | "log" | "sqrt";
  zero?: boolean;
  domain?: unknown[];
  scheme?: string;
  reverse?: boolean;
};

/** The coordinate system. */
export type Coord = "cartesian" | "flipped" | "polar" | "parallel";

/** Small multiples. */
export type Facet = {
  row?: FieldDef;
  column?: FieldDef;
  wrap?: FieldDef;
  columns?: number;
  scales?: "fixed" | "free";
};

/** Several columns compared as one variable, or paired. */
export type Fold = {
  columns: string[];
  key?: string;
  value?: string;
  pairs?: { diagonal?: boolean };
};

/** A line at a fixed value of X or Y. */
export type Reference = { channel: "x" | "y"; value: number | string; label?: string };

/** Where a plot's rows come from: a dataset's last stage, or a fit's output
 * data (analytics TODO A3.1), read from the instance rather than through SQL. */
export type DataRef =
  | { kind: "dataset"; dataset: string }
  | { kind: "fit_output"; instance: string; name: string };

/** A plot. */
export type PlotSpec = {
  data: DataRef;
  fold?: Fold;
  layers: Layer[];
  scales?: Partial<Record<Channel, Scale>>;
  coord?: Coord;
  facet?: Facet;
  references?: Reference[];
  selections?: unknown[];
};

/** The stat a layer has, the identity when it has none. */
export function statOf(layer: Layer): Stat {
  return layer.stat ?? { kind: "identity" };
}

/** The name of the column holding a fold's column names. */
export function foldKey(fold: Fold): string {
  return (fold.key ?? "variable").trim();
}

/** The name of the column holding a fold's values. */
export function foldValue(fold: Fold): string {
  return (fold.value ?? "value").trim();
}

/** The columns a fold makes whose values are the folded columns' names:
 * `variable`, or `variable_x` and `variable_y` for pairs. */
export function foldNameColumns(fold: Fold): string[] {
  const key = foldKey(fold);
  return fold.pairs ? [`${key}_x`, `${key}_y`] : [key];
}

// --- what the server draws ---------------------------------------------------

/** The values one channel spans. */
export type Domain = {
  kind: "continuous" | "discrete";
  min?: unknown;
  max?: unknown;
  values?: unknown[];
};

/** A small table. */
export type DataTable = { columns: string[]; rows: unknown[][] };

/** One layer's data, its columns named by channel (`x`, `x_end`, `y_lower`…). */
export type LayerData = DataTable & {
  mark: Mark;
  stat: string;
  outliers?: DataTable;
  sampled: boolean;
  total: number;
  truncated: boolean;
  info?: Record<string, unknown>;
};

/** A drawn plot's data. */
export type PlotData = {
  layers: LayerData[];
  domains: Record<string, Domain>;
  facets: Record<string, unknown[]>;
  bins: Record<string, { origin: number; width: number }>;
  warnings: string[];
};

/** What `renderPlot` answers. */
export type Rendered = PlotData | { error: string; problems: string[] };

/** Whether the server refused to draw. */
export function isRefused(r: unknown): r is { error: string; problems?: string[] } {
  return Boolean(r) && typeof (r as { error?: unknown }).error === "string";
}

// --- summary tables -------------------------------------------------------------

/** One summary in each cell. */
export type Cell = { field?: string; function: AggregateFn };

/** A summary table. */
export type TableSpec = {
  data: DataRef;
  fold?: Fold;
  rows: FieldDef[];
  columns: FieldDef[];
  cells: Cell[];
  totals?: boolean;
};

/** A summary table's data: each part's columns are `r0`…, `c0`…, `n`, `v0`…. */
export type TableData = {
  rows: string[];
  columns: string[];
  cells: string[];
  body: DataTable;
  row_totals?: DataTable;
  column_totals?: DataTable;
  grand_total?: DataTable;
  bins: Record<string, { origin: number; width: number }>;
  total: number;
  truncated: boolean;
};
