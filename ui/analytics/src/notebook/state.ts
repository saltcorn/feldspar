// The Notebook workspace's state, as the browser edits it (analytics TODO
// A10.7; the server's is `sc_analytics::notebook`).
//
// A notebook is `{ language, cells }`: the language fixed when it was made,
// each cell `{ id, kind: text | code, source, outputs?, run? }`. The browser
// adds, moves, deletes and edits cells and saves the state as every workspace
// does; running a cell is the server's (`runNotebookCell`), which stores the
// run's outputs into the state itself and answers the cell — merged here with
// `withRun`, which takes the outputs and the run and keeps what was typed
// meanwhile.
//
// Pure functions over plain JSON, so the rules are tested without a browser.

import type { Panel } from "../panels/panel";
import type { DataRef } from "../plot/spec";

export type Language = "javascript" | "python" | "llm";
export const LANGUAGES: Language[] = ["javascript", "python", "llm"];

export type CellKind = "text" | "code";

/** One `nb` function an LLM prompt cell's agent called. */
export type RecordedCall = { function: string; arguments?: unknown };

/** One thing a cell's run made. */
export type Output =
  | { kind: "panel"; panel: Panel }
  | { kind: "text"; text: string }
  | { kind: "value"; value: unknown }
  | { kind: "dataset"; data: DataRef; columns: string[]; rows: unknown[][]; total: number }
  | { kind: "error"; message: string; line?: number }
  | { kind: "calls"; calls: RecordedCall[]; prompt?: string };

/** When a cell was last run. */
export type CellRun = { count: number; at: string; ms?: number };

export type Cell = { id: string; kind: CellKind; source: string; outputs?: Output[]; run?: CellRun };

export type NotebookState = { language: Language; cells: Cell[] };

/** A notebook's session, as `getNotebookSession` answers it. */
export type SessionStatus = {
  state: string;
  language?: string | null;
  cell?: string | null;
  runs: number;
  started?: string | null;
  fit?: string | null;
};

export const NO_SESSION: SessionStatus = { state: "none", runs: 0 };

function isObject(v: unknown): v is Record<string, unknown> {
  return Boolean(v) && typeof v === "object" && !Array.isArray(v);
}

/** A stored state read leniently: what is not a cell is dropped. A state with
 * no language (none can be made) is read as JavaScript's. */
export function readNotebook(raw: Record<string, unknown>): NotebookState {
  const language = LANGUAGES.includes(raw.language as Language) ? (raw.language as Language) : "javascript";
  const cells = Array.isArray(raw.cells) ? raw.cells.flatMap((c) => (readCell(c) ? [readCell(c) as Cell] : [])) : [];
  return { language, cells };
}

/** One cell read from anything, or `null`. */
export function readCell(raw: unknown): Cell | null {
  if (!isObject(raw) || typeof raw.id !== "string" || (raw.kind !== "text" && raw.kind !== "code")) return null;
  const cell: Cell = { id: raw.id, kind: raw.kind, source: typeof raw.source === "string" ? raw.source : "" };
  if (raw.kind === "code") {
    if (Array.isArray(raw.outputs)) cell.outputs = raw.outputs.filter(isObject) as Output[];
    if (isObject(raw.run) && typeof raw.run.count === "number") cell.run = raw.run as CellRun;
  }
  return cell;
}

/** The state as the workspace stores it. */
export function toState(nb: NotebookState): Record<string, unknown> {
  return { language: nb.language, cells: nb.cells };
}

/** A new cell's id: short, and not one of `taken`. */
export function newCellId(taken: Iterable<string> = []): string {
  const used = new Set(taken);
  for (;;) {
    const id = `c${Math.random().toString(36).slice(2, 10)}`;
    if (!used.has(id)) return id;
  }
}

/** `nb` with a new empty cell of `kind` after `after` — at the end without one.
 * Answers the notebook and the new cell's id. */
export function addCell(nb: NotebookState, kind: CellKind, after?: string): { nb: NotebookState; id: string } {
  const id = newCellId(nb.cells.map((c) => c.id));
  const cell: Cell = { id, kind, source: "" };
  const at = after === undefined ? -1 : nb.cells.findIndex((c) => c.id === after);
  const cells = [...nb.cells];
  cells.splice(at < 0 ? cells.length : at + 1, 0, cell);
  return { nb: { ...nb, cells }, id };
}

/** `nb` with cell `id` moved `by` places (−1 up, 1 down), kept within it. */
export function moveCell(nb: NotebookState, id: string, by: number): NotebookState {
  const from = nb.cells.findIndex((c) => c.id === id);
  if (from < 0) return nb;
  const to = Math.max(0, Math.min(nb.cells.length - 1, from + by));
  if (to === from) return nb;
  const cells = [...nb.cells];
  const [cell] = cells.splice(from, 1);
  cells.splice(to, 0, cell);
  return { ...nb, cells };
}

export function deleteCell(nb: NotebookState, id: string): NotebookState {
  return { ...nb, cells: nb.cells.filter((c) => c.id !== id) };
}

export function setSource(nb: NotebookState, id: string, source: string): NotebookState {
  return { ...nb, cells: nb.cells.map((c) => (c.id === id ? { ...c, source } : c)) };
}

/** `nb` with cell `id` switched to `kind`. A text cell is never run, so one
 * switched to text loses its outputs and its run: the server refuses a text
 * cell that has them. */
export function setKind(nb: NotebookState, id: string, kind: CellKind): NotebookState {
  return {
    ...nb,
    cells: nb.cells.map((c) => (c.id !== id || c.kind === kind ? c : { id: c.id, kind, source: c.source })),
  };
}

/** `nb` with what the server's run of a cell made: its outputs and its run,
 * the source kept as it is here — it may have been typed into meanwhile. A
 * cell deleted while it ran stays deleted. */
export function withRun(nb: NotebookState, ran: unknown): NotebookState {
  const cell = readCell(ran);
  if (!cell || cell.kind !== "code") return nb;
  return {
    ...nb,
    cells: nb.cells.map((c) =>
      c.id === cell.id && c.kind === "code" ? { ...c, outputs: cell.outputs ?? [], run: cell.run } : c,
    ),
  };
}

/** The code cells `runNotebook` runs, in order: those with code in them. */
export function runnable(nb: NotebookState): Cell[] {
  return nb.cells.filter((c) => c.kind === "code" && c.source.trim() !== "");
}

/**
 * Whether a cell's outputs are from this session. A notebook reopens with its
 * outputs, but its session — the globals one cell left for the next — is gone
 * after a restart, an idle timeout or a server restart: those outputs are
 * marked "not run in this session".
 */
export function ranInSession(cell: Cell, session: SessionStatus): boolean {
  if (!cell.run || session.state === "none" || !session.started) return false;
  return Date.parse(cell.run.at) >= Date.parse(session.started);
}

/** The cell's first error, if its last run stopped on one. */
export function cellError(cell: Cell): Extract<Output, { kind: "error" }> | undefined {
  return cell.outputs?.find((o): o is Extract<Output, { kind: "error" }> => o.kind === "error");
}

/** An LLM prompt cell's recorded calls, if it has any. */
export function cellCalls(cell: Cell): Extract<Output, { kind: "calls" }> | undefined {
  return cell.outputs?.find((o): o is Extract<Output, { kind: "calls" }> => o.kind === "calls");
}

/** The id of the non-persisted dataset `data` is, if it is one. */
export function inlineId(data: DataRef): string | undefined {
  return data.kind === "inline" ? data.definition.id : undefined;
}

/**
 * `state` with every reference to the non-persisted dataset `id` replaced by
 * one to the stored dataset of the same id: what **Save as dataset** does to
 * the workspace (the server's `name_inline_dataset`), done here as well so
 * what is on the screen — and saved next — agrees with what was stored.
 */
export function nameInline<T>(state: T, id: string): T {
  const walk = (v: unknown): unknown => {
    if (Array.isArray(v)) return v.map(walk);
    if (!isObject(v)) return v;
    if (v.kind === "inline" && isObject(v.definition) && v.definition.id === id) {
      return { kind: "dataset", dataset: id };
    }
    return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, walk(x)]));
  };
  return walk(state) as T;
}

// --- completions ------------------------------------------------------------------

/** One completion the code editor offers. */
export type Completion = { label: string; insert: string; detail: string; kind: "function" | "method" | "dataset" | "column" };

/** The `nb` functions, as JavaScript and Python spell them. */
const NB_FUNCTIONS: { name: string; js: string; py: string; detail: string }[] = [
  { name: "dataset", js: 'dataset("$1")', py: 'dataset("$1")', detail: "a dataset or a table, to filter, aggregate, …" },
  { name: "plot", js: "plot({ data: $1, mark: \"point\", x: \"\", y: \"\" })", py: 'plot(data=$1, mark="point", x="", y="")', detail: "a plot panel" },
  { name: "table", js: "table($1)", py: "table($1)", detail: "a summary table panel" },
  { name: "test", js: "test($1, { y: \"\", x: \"\" })", py: 'test($1, y="", x="")', detail: "hypothesis tests, as a panel" },
  { name: "card", js: "card($1, { value: \"count()\" })", py: 'card($1, value="count()")', detail: "a stat card panel" },
  { name: "map", js: "map($1)", py: "map($1)", detail: "a map panel" },
  { name: "markdown", js: "markdown($1)", py: "markdown($1)", detail: "a text panel" },
  { name: "fit", js: 'fit("$1")', py: 'fit("$1")', detail: "fit a model; its outputs as panels" },
  { name: "show", js: "show($1)", py: "show($1)", detail: "add to the cell's output" },
];

/** A dataset's chain methods. */
const DATASET_METHODS: { name: string; js: string; py: string }[] = [
  { name: "filter", js: 'filter("$1")', py: 'filter("$1")' },
  { name: "mutate", js: "mutate({ $1: \"\" })", py: 'mutate($1="")' },
  { name: "select", js: 'select("$1")', py: 'select("$1")' },
  { name: "sort", js: 'sort("$1")', py: 'sort("$1")' },
  { name: "aggregate", js: 'aggregate(["$1"], { n: "count()" })', py: 'aggregate(["$1"], n="count()")' },
  { name: "limit", js: "limit($1)", py: "limit($1)" },
  { name: "sample", js: "sample($1)", py: "sample($1)" },
  { name: "named", js: 'named("$1")', py: 'named("$1")' },
  { name: "rows", js: "rows()", py: "rows()" },
  { name: "columns", js: "columns()", py: "columns()" },
];

/** What the database surface offers, besides a table by its name. */
const DB_FUNCTIONS = ["table", "sql"];

/** The names a cell refers to with `nb.dataset("…")`, in order, once each. */
export function datasetsNamed(source: string): string[] {
  const out: string[] = [];
  for (const m of source.matchAll(/\bnb\.dataset\(\s*["'`]([^"'`]+)["'`]/g)) {
    if (!out.includes(m[1])) out.push(m[1]);
  }
  return out;
}

/**
 * What to offer at the point the cursor is, given the line up to it: after
 * `nb.`, the `nb` functions; after `db.`, the database's functions and its tables; inside
 * `nb.dataset("`, the datasets' and tables' names; after another `.`, a
 * dataset's methods; inside any other string, the columns of the datasets the
 * cell names.
 */
export function completionsAt(
  before: string,
  language: Exclude<Language, "llm">,
  names: { datasets: string[]; tables: string[]; columns: string[] },
): Completion[] {
  const spell = (f: { js: string; py: string }) => (language === "python" ? f.py : f.js);
  if (/\bnb\.dataset\(\s*["'`][^"'`]*$/.test(before)) {
    return names.datasets.map((d) => ({ label: d, insert: d, detail: "dataset or table", kind: "dataset" }));
  }
  if (/\bnb\.\w*$/.test(before)) {
    return NB_FUNCTIONS.map((f) => ({ label: f.name, insert: spell(f), detail: f.detail, kind: "function" }));
  }
  if (/\bdb\.\w*$/.test(before)) {
    return [
      ...DB_FUNCTIONS.map((f): Completion => ({ label: f, insert: `${f}($1)`, detail: "database", kind: "function" })),
      ...names.tables.map((t): Completion => ({ label: t, insert: t, detail: "table", kind: "dataset" })),
    ];
  }
  // Inside a string: a column, most likely.
  const quotes = (before.match(/["'`]/g) ?? []).length;
  if (quotes % 2 === 1) {
    return names.columns.map((c) => ({ label: c, insert: c, detail: "column", kind: "column" }));
  }
  if (/[\w)\]]\.\w*$/.test(before)) {
    return DATASET_METHODS.map((m) => ({ label: m.name, insert: spell(m), detail: "dataset", kind: "method" }));
  }
  return [];
}
