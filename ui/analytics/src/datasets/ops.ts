// A dataset in the Analytics UI: its definition as the server stores it, the
// report the server compiles it into, and the pure logic the Dataset editor
// is built on (analytics TODO A1.16, A1.17) — the operation kinds and their
// defaults, what an operation says in the side panel, moving operations
// about, the columns each stage has, and what a formula may name.
//
// The shapes mirror `sc-dataset`'s serde types exactly: an operation is
// `{ id, enabled, kind, params }`, and `params` is the kind's own struct.

/** Where a dataset's rows start. */
export type Base = { kind: "table"; table: string } | { kind: "dataset"; dataset: string };

/** The operation kinds: A1's, and A5's Spatial join. */
export type OpKind =
  | "calculated"
  | "filter"
  | "select"
  | "sort"
  | "window"
  | "aggregate"
  | "limit"
  | "stack"
  | "split"
  | "complete"
  | "join"
  | "union"
  | "spatial_join";

/** One operation of a dataset. */
export type Operation = {
  id: string;
  enabled: boolean;
  kind: OpKind;
  params: Record<string, unknown>;
};

/** A dataset's definition. */
export type DatasetDef = {
  id: string;
  name: string;
  description: string;
  base: Base;
  operations: Operation[];
};

/** A foreign key: where a column points. */
export type ForeignKey = { table: string; field: string };

/** One column of a stage. */
export type StageColumn = { name: string; type: string; key?: ForeignKey | null };

/** What one row of a stage represents. */
export type Grain =
  | { kind: "table"; table: string; key: string }
  | { kind: "group"; keys: string[] }
  | { kind: "derived" };

/** A stage's columns and grain. */
export type StageShape = { columns: StageColumn[]; grain: Grain };

/** What became of one operation. */
export type OpStatus = "ok" | "disabled" | "invalid" | "not_reached";

/** The server's report on one operation. */
export type OpReport = {
  id: string;
  kind: string;
  status: OpStatus;
  shape?: StageShape | null;
  error?: string | null;
};

/** The server's report on a whole definition (`datasetShapes`). */
export type Report = {
  base: { shape?: StageShape | null; error?: string | null };
  operations: OpReport[];
  /** Every table's columns, for the formulas' join paths. */
  tables: Record<string, StageColumn[]>;
  /** For each table, the child tables whose keys point at it. */
  children: Record<string, { table: string; key: string }[]>;
};

/** A message translator, as `useT().t` is. */
export type Translate = (text: string, args?: Record<string, string | number>) => string;

// --- the kinds ------------------------------------------------------------------

/** How the goals document groups the operations. */
export type OpGroup = "keep" | "change" | "combine";

/** One kind, as the Add menu offers it. */
export type KindInfo = { kind: OpKind; label: string; group: OpGroup; about: string };

/** The operations, in the Add menu's order. */
export const OP_KINDS: KindInfo[] = [
  { kind: "calculated", label: "Calculated column", group: "keep", about: "Add or replace a column computed by a formula." },
  { kind: "filter", label: "Filter", group: "keep", about: "Keep the rows a condition holds for." },
  { kind: "select", label: "Select columns", group: "keep", about: "Keep, drop, rename and reorder columns." },
  { kind: "sort", label: "Sort", group: "keep", about: "Order the rows." },
  { kind: "window", label: "Window column", group: "keep", about: "A column computed over the ordered rows of a group: lag, running total, rank, share of the group." },
  { kind: "aggregate", label: "Aggregate", group: "change", about: "One row per group, with summaries: count, mean, median…" },
  { kind: "limit", label: "Limit", group: "change", about: "The first rows, a random sample, or the top rows of each group." },
  { kind: "stack", label: "Stack", group: "change", about: "Turn columns into rows of name/value pairs." },
  { kind: "split", label: "Split", group: "change", about: "Turn the values of a column into columns of their own." },
  { kind: "complete", label: "Complete", group: "change", about: "Add rows for missing combinations of values." },
  { kind: "join", label: "Join", group: "combine", about: "Join another table or dataset on key columns." },
  { kind: "union", label: "Union", group: "combine", about: "Append the rows of another table or dataset." },
  {
    kind: "spatial_join",
    label: "Spatial join",
    group: "combine",
    about: "Join another table or dataset where the geometries meet, or to the nearest.",
  },
];

/** How two geometries must be placed for a Spatial join to match them, and
 * whether the relation takes a distance (required, or an optional limit). */
export const SPATIAL_RELATIONS: { value: string; label: string; distance: "none" | "required" | "optional" }[] = [
  { value: "within", label: "is within", distance: "none" },
  { value: "contains", label: "contains", distance: "none" },
  { value: "intersects", label: "intersects", distance: "none" },
  { value: "within_distance", label: "is within a distance of", distance: "required" },
  { value: "nearest", label: "is nearest to", distance: "optional" },
];

/** The kind's label. */
export function kindLabel(kind: string): string {
  return OP_KINDS.find((k) => k.kind === kind)?.label ?? kind;
}

/** The functions a Window column computes, and whether each reads a column. */
export const WINDOW_FUNCTIONS: { value: string; label: string; column: boolean }[] = [
  { value: "lag", label: "Previous value (lag)", column: true },
  { value: "lead", label: "Next value (lead)", column: true },
  { value: "difference", label: "Difference from previous", column: true },
  { value: "cumulative_sum", label: "Running total", column: true },
  { value: "cumulative_mean", label: "Running mean", column: true },
  { value: "rank", label: "Rank", column: false },
  { value: "row_number", label: "Row number", column: false },
  { value: "group_sum", label: "Group total", column: true },
  { value: "group_mean", label: "Group mean", column: true },
  { value: "group_count", label: "Group count", column: true },
  { value: "group_min", label: "Group minimum", column: true },
  { value: "group_max", label: "Group maximum", column: true },
  { value: "share", label: "Share of group total", column: true },
  { value: "fill", label: "Last value that was not missing", column: true },
];

/** The summaries of an Aggregate. */
export const SUMMARY_FUNCTIONS: { value: string; label: string }[] = [
  { value: "count", label: "Count" },
  { value: "count_distinct", label: "Count distinct" },
  { value: "sum", label: "Sum" },
  { value: "mean", label: "Mean" },
  { value: "median", label: "Median" },
  { value: "min", label: "Minimum" },
  { value: "max", label: "Maximum" },
  { value: "sd", label: "Standard deviation" },
  { value: "first", label: "First" },
  { value: "last", label: "Last" },
  { value: "union", label: "Union of geometries" },
];

/** A cell's text: a fractional number to at most four decimals (the value
 * itself is kept, and shown whole in the cell's tooltip), anything else as the
 * admin grid shows it. */
export function cellText(value: unknown, type: string): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "number" && !Number.isInteger(value) && (type === "float" || type === "decimal")) {
    return String(Number(value.toFixed(4)));
  }
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

/** Whether a column type is a number. */
export function isNumeric(type: string): boolean {
  return type === "int" || type === "float" || type === "decimal";
}

/** A new operation's parameters, filled in from the stage it reads. */
export function defaultParams(kind: OpKind, columns: StageColumn[]): Record<string, unknown> {
  const first = columns[0]?.name ?? "";
  const numeric = columns.find((c) => isNumeric(c.type))?.name ?? first;
  switch (kind) {
    case "calculated":
      return { name: uniqueName("new_column", columns.map((c) => c.name)), formula: "" };
    case "filter":
      return { formula: "" };
    case "select":
      return { columns: columns.map((c) => ({ column: c.name })) };
    case "sort":
      return { keys: [{ formula: first, descending: false }] };
    case "window":
      return {
        name: uniqueName(`${numeric}_previous`, columns.map((c) => c.name)),
        function: "lag",
        column: numeric,
        partition: [],
        order: [],
      };
    case "aggregate":
      return { group_by: [], summaries: [{ name: "n", function: "count" }] };
    case "limit":
      return { mode: "first", n: 100, seed: 0, group_by: [], order: [] };
    case "stack":
      return { columns: [], names_to: "name", values_to: "value" };
    case "split":
      return { names_from: "", values_from: "", id_columns: [], values: [], summary: "first" };
    case "complete":
      return { columns: [], fill: [] };
    case "join":
      return {
        with: { kind: "table", table: "" },
        kind: "left",
        on: [],
        suffix: "_right",
      };
    case "union":
      return { with: { kind: "table", table: "" } };
    case "spatial_join":
      return {
        with: { kind: "table", table: "" },
        kind: "left",
        relation: "within",
        left: columns.find((c) => c.type === "geometry")?.name ?? "",
        right: "",
        suffix: "_right",
      };
  }
}

/** `name`, or `name_2`, `name_3`… — whichever is not taken. */
export function uniqueName(name: string, taken: string[]): string {
  if (!taken.includes(name)) return name;
  let n = 2;
  while (taken.includes(`${name}_${n}`)) n += 1;
  return `${name}_${n}`;
}

/** An id no operation of `ops` has. */
export function newOpId(ops: Operation[]): string {
  const taken = new Set(ops.map((o) => o.id));
  let n = ops.length + 1;
  while (taken.has(`op${n}`)) n += 1;
  return `op${n}`;
}

/** A new, enabled operation of `kind` over `columns`, with an id `ops` does
 * not use. */
export function newOperation(
  kind: OpKind,
  ops: Operation[],
  columns: StageColumn[],
  params?: Record<string, unknown>,
): Operation {
  return { id: newOpId(ops), enabled: true, kind, params: params ?? defaultParams(kind, columns) };
}

// --- editing the list -------------------------------------------------------------

/** `ops` with `op` inserted at `index`. */
export function insertOperation(ops: Operation[], index: number, op: Operation): Operation[] {
  const at = Math.max(0, Math.min(index, ops.length));
  return [...ops.slice(0, at), op, ...ops.slice(at)];
}

/** `ops` with the operation at `from` moved to `to` — dragging it there. */
export function moveOperation(ops: Operation[], from: number, to: number): Operation[] {
  if (from === to || from < 0 || from >= ops.length) return ops;
  const out = [...ops];
  const [moved] = out.splice(from, 1);
  out.splice(Math.max(0, Math.min(to, out.length)), 0, moved);
  return out;
}

/** `ops` with `op` in place of the operation with its id. */
export function replaceOperation(ops: Operation[], op: Operation): Operation[] {
  return ops.map((o) => (o.id === op.id ? op : o));
}

/** `ops` with the operation `id` switched on or off. */
export function toggleOperation(ops: Operation[], id: string): Operation[] {
  return ops.map((o) => (o.id === id ? { ...o, enabled: !o.enabled } : o));
}

/** `ops` without the operation `id`. */
export function removeOperation(ops: Operation[], id: string): Operation[] {
  return ops.filter((o) => o.id !== id);
}

// --- stages -----------------------------------------------------------------------

/** The stage after the first `upto` operations (0 is the base). */
export function stageShape(report: Report | null, upto: number): StageShape | null {
  if (!report) return null;
  if (upto <= 0) return report.base.shape ?? null;
  return report.operations[upto - 1]?.shape ?? null;
}

/** The index of the stage an operation's result is — one past its position. */
export function stageOf(ops: Operation[], id: string | null): number {
  if (id === null) return ops.length;
  if (id === "") return 0;
  const i = ops.findIndex((o) => o.id === id);
  return i === -1 ? ops.length : i + 1;
}

/** What a grain says in a sentence. */
export function describeGrain(grain: Grain | undefined | null, t: Translate): string {
  if (!grain) return "";
  switch (grain.kind) {
    case "table":
      return t("one row per {table}", { table: grain.table });
    case "group":
      return grain.keys.length === 0
        ? t("one row in all")
        : t("one row per {keys}", { keys: grain.keys.join(" × ") });
    case "derived":
      return t("derived rows");
  }
}

// --- what an operation says ---------------------------------------------------

const str = (v: unknown): string => (typeof v === "string" ? v : "");
const arr = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
const rec = (v: unknown): Record<string, unknown> =>
  v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {};

/** The table or dataset a Join or a Union reads, named. */
export function otherName(
  other: unknown,
  datasetName: (id: string) => string = (id) => id,
): string {
  const o = rec(other);
  if (o.kind === "dataset") return datasetName(str(o.dataset));
  return str(o.table);
}

/**
 * What an operation says in the side panel: one short line, its formulas as
 * written.
 */
export function describeOperation(
  op: Operation,
  t: Translate,
  datasetName?: (id: string) => string,
): string {
  const p = op.params;
  switch (op.kind) {
    case "calculated":
      return `${str(p.name)} = ${str(p.formula)}`;
    case "filter":
      return str(p.formula);
    case "select":
      return arr(p.columns)
        .map((c) => {
          const col = rec(c);
          const rename = str(col.rename);
          return rename && rename !== str(col.column) ? `${str(col.column)} → ${rename}` : str(col.column);
        })
        .join(", ");
    case "sort":
      return arr(p.keys)
        .map((k) => `${str(rec(k).formula)}${rec(k).descending ? " ↓" : " ↑"}`)
        .join(", ");
    case "window": {
      const fn = str(p.function);
      const column = str(p.column);
      const by = arr(p.partition).map(str);
      const call = column ? `${fn}(${column})` : `${fn}()`;
      return by.length > 0
        ? t("{name} = {call} by {groups}", { name: str(p.name), call, groups: by.join(", ") })
        : `${str(p.name)} = ${call}`;
    }
    case "aggregate": {
      const keys = arr(p.group_by).map((g) => str(rec(g).name));
      const summaries = arr(p.summaries).map((s) => {
        const sm = rec(s);
        const fn = str(sm.function);
        const column = str(sm.column);
        return column ? `${str(sm.name)} = ${fn}(${column})` : `${str(sm.name)} = ${fn}()`;
      });
      if (keys.length === 0) return summaries.join(", ");
      return summaries.length === 0
        ? t("distinct {keys}", { keys: keys.join(", ") })
        : t("by {keys}: {summaries}", { keys: keys.join(", "), summaries: summaries.join(", ") });
    }
    case "limit": {
      const n = Number(p.n ?? 0);
      if (p.mode === "sample") return t("a sample of {n} (seed {seed})", { n, seed: Number(p.seed ?? 0) });
      if (p.mode === "top") {
        return t("the first {n} of each {groups}", {
          n,
          groups: arr(p.group_by).map(str).join(", ") || t("group"),
        });
      }
      return t("the first {n}", { n });
    }
    case "stack":
      return `${arr(p.columns).map(str).join(", ")} → ${str(p.names_to)}, ${str(p.values_to)}`;
    case "split":
      return t("{names} → {values} (from {column})", {
        names: str(p.names_from),
        values: arr(p.values).map(str).join(", "),
        column: str(p.values_from),
      });
    case "complete":
      return arr(p.columns)
        .map((c) => str(rec(c).column))
        .join(" × ");
    case "join": {
      const on = arr(p.on).map((k) => `${str(rec(k).left)} = ${str(rec(k).right)}`);
      const asof = rec(p.asof);
      if (str(asof.left)) on.push(`${str(asof.left)} ≥ ${str(asof.right)}`);
      return t("{kind} join {other} on {keys}", {
        kind: str(p.kind),
        other: otherName(p.with, datasetName),
        keys: on.join(", "),
      });
    }
    case "union":
      return t("with {other}", { other: otherName(p.with, datasetName) });
    case "spatial_join": {
      const other = otherName(p.with, datasetName);
      const left = str(p.left);
      const right = str(p.right);
      const distance = typeof p.distance === "number" ? p.distance : null;
      switch (p.relation) {
        case "contains":
          return t("{left} contains {other}.{right}", { left, other, right });
        case "intersects":
          return t("{left} intersects {other}.{right}", { left, other, right });
        case "within_distance":
          return t("{left} within {metres} m of {other}.{right}", { left, other, right, metres: distance ?? 0 });
        case "nearest":
          return distance === null
            ? t("nearest {other}.{right} to {left}", { left, other, right })
            : t("nearest {other}.{right} to {left}, within {metres} m", { left, other, right, metres: distance });
        default:
          return t("{left} within {other}.{right}", { left, other, right });
      }
    }
  }
}

// --- what a formula may name ----------------------------------------------------

/** One completion a formula input offers. */
export type Completion = {
  /** What is inserted. */
  text: string;
  /** Why it is offered: a column, a join path, an aggregation, a function. */
  kind: "column" | "join" | "aggregation" | "function";
  /** A column's type, where it has one. */
  detail?: string;
};

/**
 * Whose rows a stage's rows are — what an aggregation over a child table
 * needs — mirroring the server's rule: a table's while the grain is the
 * table's, or the referenced table's after an Aggregate by one foreign key.
 */
export function rowsOf(shape: StageShape): string | null {
  if (shape.grain.kind === "table") return shape.grain.table;
  if (shape.grain.kind === "group" && shape.grain.keys.length === 1) {
    const key = shape.columns.find((c) => c.name === (shape.grain as { keys: string[] }).keys[0]);
    return key?.key?.table ?? null;
  }
  return null;
}

/**
 * The geometry functions (analytics TODO A5.3), as `sc-expr`'s `GEO_FUNCTIONS`
 * lists them: the call's opening, and what it returns. Computed by PostGIS, so
 * offered only over a stage that has a geometry column.
 */
export const GEO_FUNCTIONS: Array<{ name: string; params: string; returns: string }> = [
  { name: "point", params: "longitude, latitude", returns: "geometry" },
  { name: "buffer", params: "geometry, metres", returns: "geometry" },
  { name: "centroid", params: "geometry", returns: "geometry" },
  { name: "area", params: "geometry", returns: "float" },
  { name: "length", params: "geometry", returns: "float" },
  { name: "distance", params: "a, b", returns: "float" },
  { name: "intersects", params: "a, b", returns: "bool" },
  { name: "contains", params: "a, b", returns: "bool" },
  { name: "within", params: "a, b", returns: "bool" },
  { name: "squareCell", params: "geometry, metres", returns: "geometry" },
  { name: "hexCell", params: "geometry, metres", returns: "geometry" },
  { name: "intersection", params: "a, b", returns: "geometry" },
  { name: "fromGeoJSON", params: "text", returns: "geometry" },
];

/** Everything a formula over `shape` may name: its columns, one step along each
 * foreign key, the counts and totals of the child tables its rows have, and the
 * geometry functions where there is geometry. */
export function formulaCompletions(shape: StageShape | null, report: Report | null): Completion[] {
  if (!shape) return [];
  const out: Completion[] = shape.columns.map((c) => ({
    text: c.name,
    kind: "column" as const,
    detail: c.type,
  }));
  for (const column of shape.columns) {
    const target = column.key ? report?.tables[column.key.table] : undefined;
    for (const field of target ?? []) {
      out.push({ text: `${column.name}Ⱶ${field.name}`, kind: "join", detail: field.type });
    }
  }
  const parent = rowsOf(shape);
  for (const child of parent ? (report?.children[parent] ?? []) : []) {
    const relation = `${child.table}Ↄ${child.key}`;
    out.push({ text: `${relation}.length`, kind: "aggregation", detail: "int" });
    for (const field of report?.tables[child.table] ?? []) {
      if (!isNumeric(field.type) || field.name === child.key) continue;
      out.push({ text: `${relation}.sum("${field.name}")`, kind: "aggregation", detail: field.type });
      out.push({ text: `${relation}.avg("${field.name}")`, kind: "aggregation", detail: "float" });
    }
  }
  if (shape.columns.some((c) => c.type === "geometry")) {
    for (const f of GEO_FUNCTIONS) {
      out.push({ text: `Geo.${f.name}(`, kind: "function", detail: `(${f.params}) → ${f.returns}` });
    }
  }
  return out;
}

/** The identifier the cursor is in: where it starts and ends, and what has
 * been typed of it. `Ⱶ` and `Ↄ` are letters to a formula, so a join path is
 * one identifier. */
export function tokenAt(text: string, cursor: number): { start: number; end: number; prefix: string } {
  const isWord = (ch: string) => /[\p{L}\p{N}_$]/u.test(ch);
  let start = cursor;
  while (start > 0 && isWord(text[start - 1])) start -= 1;
  let end = cursor;
  while (end < text.length && isWord(text[end])) end += 1;
  return { start, end, prefix: text.slice(start, cursor) };
}

/** The completions for what has been typed at the cursor, best first: those
 * starting with it, then those containing it. None for nothing typed. */
export function matchCompletions(all: Completion[], prefix: string, max = 12): Completion[] {
  if (prefix === "") return [];
  const lower = prefix.toLowerCase();
  const starts = all.filter((c) => c.text.toLowerCase().startsWith(lower));
  const contains = all.filter(
    (c) => !c.text.toLowerCase().startsWith(lower) && c.text.toLowerCase().includes(lower),
  );
  return [...starts, ...contains].filter((c) => c.text !== prefix).slice(0, max);
}

/** `text` with the identifier at the cursor replaced by `insert`, and where the
 * cursor goes after it. */
export function applyCompletion(
  text: string,
  cursor: number,
  insert: string,
): { text: string; cursor: number } {
  let { start } = tokenAt(text, cursor);
  const { end } = tokenAt(text, cursor);
  // `Geo.dis` is completed as a whole: the `Geo.` already typed is part of
  // what `Geo.distance(` replaces.
  const dot = insert.lastIndexOf(".");
  if (dot > 0 && text.slice(0, start).endsWith(insert.slice(0, dot + 1))) start -= dot + 1;
  return { text: text.slice(0, start) + insert + text.slice(end), cursor: start + insert.length };
}
