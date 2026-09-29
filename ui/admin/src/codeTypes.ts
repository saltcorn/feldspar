// The **type declarations** the code editor reads: `db`, its fluent chain, and
// the event's own bindings, as TypeScript.
//
// A `run_js_code` body is edited in Monaco (`CodeEditor.tsx`), and an editor
// with no types is a text area with brackets: completions are the point. So the
// editor is handed an ambient `.d.ts` describing exactly what a code body can
// reach — `db.invoices.where(…).rows()`, `row`, `old`, `user`, `payload` — with
// this server's own tables and columns in it.
//
// **The declarations are built in the browser, from the catalog the admin UI
// already reads.** The alternative was a server endpoint emitting the same text,
// which is the better home for it the moment a second consumer appears (a
// non-JavaScript adapter, §15). Today there is one consumer, and building it here
// costs no endpoint, no schema and no generated-client churn — while `listTables`
// and `listFields` are already the API this screen uses.
//
// The shape of the chain is the other half, and it is a **transcription** of
// `sc-expr`'s `DB_PRELUDE` (the JavaScript that implements `db`), the operators
// `sc-api`'s filter module accepts, and the results `sc-api::code_host` returns.
// Those live in Rust and this is a copy of what they do, so it can be *close* to
// correct and not provably so; the comments below name what each part is a copy
// of. Nothing type-checks against it — the editor reports no diagnostics on a
// body (see `CodeEditor.tsx`) — so a declaration that drifts costs a wrong
// completion, never a refused save.

import { api } from "./api";

/** One column of a table, as the declarations need it. */
export type ColumnInfo = {
  name: string;
  /** The catalog's type name (`text`, `int`, a rich type's name). */
  type: string;
  /** The SQL type, which a rich type falls back to. */
  sqlType: string;
  /** Whether a write must supply it — what makes it non-optional on an insert. */
  required: boolean;
  /** The table a key field points at, for the `Ⱶ` join columns. */
  keyTo?: string;
};

/** One table: its name and its columns. */
export type TableInfo = { name: string; columns: ColumnInfo[] };

/** One function a module supplies, as `listModules` describes it. */
export type ModuleFunctionInfo = {
  /** The package that supplies it. */
  module: string;
  /** The function's own name, v1's and unqualified. */
  name: string;
  /** The module's one-line description, or "". */
  description: string;
  /** v1's own `isAsync`. It decides nothing about the call — everything crosses
   * the host seam awaited — but it is what the module said, and a signature
   * that says otherwise would be a lie about the module. */
  isAsync: boolean;
  /** The declared signature, when the module declared one. */
  arguments: { name: string; type: string | null }[];
};

/** What the screen knows about the event a body will run in.
 *
 * `table` is the trigger's table (a row event) and `undefined` for every kind
 * that has no row — which is exactly the difference between `row` being declared
 * and not being declared at all, because that is the difference in the sandbox
 * (`run_js_code`'s `bindings`: naming `row` in a `login` body is a
 * `ReferenceError`, not a null).
 *
 * `run` says the body is a **workflow step**, which is the same difference for
 * `context`: a step is bound the run so far and a trigger's own body is bound
 * nothing at all.
 *
 * `request` says the body is an application's **custom query** (§13.4): it has
 * no event, so no `payload`, and is bound the request as `body` and `query`. */
export type CodeScope = { table?: string; event?: string; run?: boolean; request?: boolean };

/** The join separator between a key field and a column of the table it points
 * at (`customerⱵemail`) — `sc_expr::JOIN`. */
const JOIN = "Ⱶ";

/** The TypeScript type a column's values arrive as, over the JSON boundary.
 *
 * Rich types are not enumerated: they are stored as one of the basic types and
 * the SQL type is what says which, so an unknown type name falls through to the
 * column's `sql_type` and then to `unknown` — a wrong-but-quiet `unknown` being
 * better in an editor than a confidently wrong `string`. */
export function columnType(column: ColumnInfo): string {
  const scalar = (name: string): string | null => {
    switch (name) {
      case "bool":
      case "boolean":
        return "boolean";
      case "int":
      case "integer":
      case "bigint":
      case "smallint":
      case "float":
      case "double precision":
      case "real":
        return "number";
      // A decimal crosses as a string: it is exact, and a JavaScript number is
      // not. Same for the date/time family, which arrive as ISO strings.
      case "decimal":
      case "numeric":
      case "text":
      case "uuid":
      case "date":
      case "time":
      case "timestamp":
      case "timestamptz":
      case "character varying":
        return "string";
      case "json":
      case "jsonb":
        return "unknown";
      default:
        return null;
    }
  };
  const type = scalar(column.type) ?? scalar(column.sqlType) ?? "unknown";
  return column.required ? type : `${type} | null`;
}

/** A TypeScript identifier for a table's generated interfaces (`order_lines` →
 * `OrderLines`). Two tables cannot collide: a name that is not already unique
 * keeps enough of itself to stay so, because non-identifier characters become
 * `_` rather than being dropped. */
export function typeName(table: string): string {
  const cleaned = table.replace(/[^A-Za-z0-9_]/g, "_");
  const camel = cleaned
    .split("_")
    .map((part) => (part === "" ? "_" : part[0].toUpperCase() + part.slice(1)))
    .join("");
  return /^[A-Za-z]/.test(camel) ? camel : `T${camel}`;
}

/** How a property name is written in an interface: bare when it is an
 * identifier, quoted otherwise — which every `Ⱶ` join column is. */
function propertyKey(name: string): string {
  return /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name) ? name : JSON.stringify(name);
}

/** A string literal, for the column unions. */
function literal(value: string): string {
  return JSON.stringify(value);
}

/** Every column name a query on `table` may name: its own columns, plus one
 * `keyⱵcolumn` per column of each table its key fields point at.
 *
 * One level deep on purpose. The server resolves a path of any length
 * (`join_path_expr`), but the completions that matter are the first hop, and
 * every further hop multiplies the union by another table's width. */
export function columnNames(table: TableInfo, tables: TableInfo[]): string[] {
  const names = table.columns.map((c) => c.name);
  for (const column of table.columns) {
    const target = tables.find((t) => t.name === column.keyTo);
    if (!target) continue;
    for (const far of target.columns) names.push(`${column.name}${JOIN}${far.name}`);
  }
  return names;
}

/** The part of the declarations that is the same on every server: the value
 * types, the filter DSL, and the chain itself.
 *
 * Transcribed from `sc-expr`'s `DB_PRELUDE` (which methods exist and what they
 * return), `sc_api::filter::OPERATORS` (the comparisons), and
 * `sc_api::code_host` (the shape a write answers with). */
export function chainDeclarations(): string {
  return `/** A value a column can hold, as it crosses into the sandbox. */
type ScValue = string | number | boolean | null;

/** One row, when its columns are not known ahead of time.
 *
 * \`any\` rather than \`unknown\` wherever a value is genuinely undeclared —
 * here, in the open end of a row interface, in \`user\`'s other columns and in
 * \`payload\`. Nothing declares what a sender put in a payload or what an alias
 * in a \`.select()\` produced, and \`unknown\` would state that by making every
 * ordinary use of it (\`payload.today\` inside a comparison) an error. The
 * editor reports no errors, so what this really decides is whether hovering one
 * says something useful; "not declared" is the true answer either way. */
type ScRow = Record<string, any>;

/** A comparison on one column. The operators are the ones every Saltcorn
 * surface speaks — a REST query string, an agent's tool, this chain. */
interface ScCompare {
  eq?: ScValue;
  ne?: ScValue;
  gt?: ScValue;
  gte?: ScValue;
  lt?: ScValue;
  lte?: ScValue;
  in?: ScValue[];
  nin?: ScValue[];
  /** SQL \`LIKE\` pattern: \`%\` is any run of characters. */
  like?: string;
  /** \`LIKE\`, ignoring case. */
  ilike?: string;
  is_null?: boolean;
}

/** What \`.where()\` takes: the object DSL, or a formula written as a string
 * (\`"pages > 200 && !paid"\`). Both mean the same predicate. */
type ScWhere<Col extends string> =
  | string
  | ({ [K in Col]?: ScValue | ScCompare } & {
      and?: ScWhere<Col>[];
      or?: ScWhere<Col>[];
      not?: ScWhere<Col>;
      /** The formula spelling, inside an object. */
      formula?: string;
    });

/** What \`.select()\` takes: a column name (or \`keyⱵcolumn\` join path), or an
 * object of alias → formula, which is how a computed or aggregated column
 * (\`{ chased: "remindersↃinvoice.length" }\`) is asked for. */
type ScProjection<Col extends string> = Col | Record<string, string>;

/** What a bulk \`.update()\` or \`.delete()\` answers: how many rows, and which. */
interface ScWriteResult {
  updated?: number;
  deleted?: number;
  ids: ScValue[];
}

/** A query over one table. Every chain method is pure, synchronous and returns
 * a new query; the **terminals** below execute, each one sending a single
 * statement and answering a **promise** — so \`await\` goes at the front of a
 * whole chain, never inside one, and \`.iter()\` is walked with \`for await\`.
 *
 * Bounded, and the bounds are named errors rather than truncations: 1000 rows
 * per read, 200 database calls per run, and the trigger's \`timeout_ms\` wall
 * clock. */
interface ScQuery<Row, Col extends string> {
  /** Narrow the rows. Repeated calls are ANDed. */
  where(condition: ScWhere<Col>): ScQuery<Row, Col>;
  /** Choose the columns, instead of the whole row. */
  select(...columns: ScProjection<Col>[]): ScQuery<Row, Col>;
  /** Order by a column, ascending unless told otherwise. */
  orderBy(field: Col, direction?: "asc" | "desc"): ScQuery<Row, Col>;
  groupBy(...fields: Col[]): ScQuery<Row, Col>;
  limit(n: number): ScQuery<Row, Col>;
  offset(n: number): ScQuery<Row, Col>;
  /** Run what follows as the person who caused the event: their ownership rule
   * decides every row, and a write they may not make is a catchable error. */
  asUser(): ScQuery<Row, Col>;
  /** Run what follows as the server (the default). */
  asAdmin(): ScQuery<Row, Col>;

  /** The matching rows. Every terminal answers a promise: \`await\` it. */
  rows(): Promise<Row[]>;
  /** The matching rows, **streamed**: one batch is read at a time, so a body can
   * walk a table far larger than the 1000 rows \`.rows()\` may answer.
   *
   * \`\`\`js
   * for await (const invoice of db.invoices.where({ paid: false }).iter()) { … }
   * \`\`\`
   *
   * Each batch is one database call and counts against the run's budget, and
   * stopping early (a \`break\`, a \`return\`) reads nothing further. The primary
   * key is added to whatever this query orders by, so no batch boundary can skip
   * or repeat a row — which means \`.orderBy()\` must name a column or a
   * \`keyⱵcolumn\` path, never an expression, and that batches are separate
   * statements rather than one snapshot: a row whose **sort key** the loop
   * changes may be seen twice or not at all. A \`.limit()\` bounds the iteration;
   * \`iter(n)\` sets how many rows a batch reads. */
  iter(batchSize?: number): AsyncIterableIterator<Row>;
  /** The first matching row, or null. */
  first(): Promise<Row | null>;
  /** The row with this primary key, or null. */
  get(pk: ScValue): Promise<Row | null>;
  exists(): Promise<boolean>;
  count(): Promise<number>;
  sum(field: Col): Promise<number | null>;
  avg(field: Col): Promise<number | null>;
  min(field: Col): Promise<ScValue>;
  max(field: Col): Promise<ScValue>;

  /** Insert a row and return it as stored — coerced, calculated columns filled
   * in, and the table's own triggers fired. */
  insert(values: Partial<Row>): Promise<Row>;
  insert(values: Partial<Row>[]): Promise<Row[]>;
  /** Update every row the \`.where()\` matched. A \`.where()\` is required: an
   * omitted one would rewrite the table. */
  update(values: Partial<Row>): Promise<ScWriteResult>;
  /** Delete every row the \`.where()\` matched. A \`.where()\` is required. */
  delete(): Promise<ScWriteResult>;
}

/** A query over a table named at runtime, whose columns are therefore not
 * known here. */
type ScAnyQuery = ScQuery<ScRow, string>;

/** \`db.sql()\`'s third argument. An object rather than a flag so what it can
 * say may grow without the call changing shape. */
interface ScSqlOptions {
  /** Run the statement as the person who caused the event, rather than as the
   * server: the caller's role and user are what row-level security reads. */
  asUser?: boolean;
}

/** The signed-in person who caused the event, or null.
 *
 * \`id\` and \`role\` are always there; every other column of the users table
 * comes with them. */
interface ScUser {
  id: string;
  role: number;
  email?: string;
  [column: string]: any;
}

/** Request and response headers, as the web API has them. */
declare class ScHeaders {
  constructor(init?: Record<string, string> | [string, string][] | ScHeaders);
  /** Every value under this name, joined with \`", "\` — or null. */
  get(name: string): string | null;
  has(name: string): boolean;
  /** Replace whatever this name had. */
  set(name: string, value: string): void;
  /** Add another value under this name, leaving any others. */
  append(name: string, value: string): void;
  delete(name: string): void;
  forEach(each: (value: string, name: string, headers: ScHeaders) => void, thisArg?: any): void;
  entries(): IterableIterator<[string, string]>;
  keys(): IterableIterator<string>;
  values(): IterableIterator<string>;
  [Symbol.iterator](): IterableIterator<[string, string]>;
}

/** What \`fetch\` answers with.
 *
 * A status the endpoint did not like is **not** an error: \`ok\` is false and
 * nothing throws, exactly as in a browser. The body is read once — \`clone()\`
 * first if two readers need it. */
declare class ScResponse {
  readonly ok: boolean;
  readonly status: number;
  readonly statusText: string;
  /** The URL that answered, which differs from the one asked for after a
   * redirect. */
  readonly url: string;
  readonly redirected: boolean;
  readonly headers: ScHeaders;
  readonly bodyUsed: boolean;
  readonly type: string;
  text(): Promise<string>;
  json(): Promise<any>;
  bytes(): Promise<Uint8Array>;
  arrayBuffer(): Promise<ArrayBuffer>;
  /** A second reader of the same body. */
  clone(): ScResponse;
}

/** \`fetch\`'s options — the web's, minus what a server has no use for. */
interface ScFetchOptions {
  /** Default \`"GET"\`. */
  method?: "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS";
  headers?: Record<string, string> | [string, string][] | ScHeaders;
  /** A string is sent as written; an object is sent as JSON (with the content
   * type to match), which is this sandbox's one difference from the browser;
   * bytes are sent as they are. */
  body?: string | Record<string, any> | any[] | Uint8Array | ArrayBuffer;
  /** How long this one request may take. Always clamped to what is left of the
   * code's own \`timeout_ms\`, so it can shorten a request but never lengthen
   * the trigger. There is no \`AbortSignal\` here — no timers in the sandbox to
   * drive one. */
  timeout_ms?: number;
  /** Redirects are followed; nothing else is supported. */
  redirect?: "follow";
  /** Accepted and ignored: a server has no use for them. */
  mode?: string;
  credentials?: string;
  cache?: string;
  referrer?: string;
  referrerPolicy?: string;
  integrity?: string;
  keepalive?: boolean;
}
`;
}

/** The declarations for `fs`: a store, a file, a directory, and the shapes they
 * exchange.
 *
 * A transcription of `sc-expr`'s `FILES_PRELUDE` and what `sc-api`'s
 * `FileStoreHost` answers, on the same terms as {@link chainDeclarations}: close
 * to correct rather than provably so, and a drift costs a wrong completion. */
export function fileDeclarations(): string {
  return `
/** What one entry's own facts are, from \`await file.stat()\`.
 *
 * This is where \`size\` and \`type\` live, rather than being properties of the
 * file: there is no synchronous I/O in the sandbox, and a property that had to
 * lie about a file it has not looked at would be worse than an await. */
interface ScFileStat {
  /** Bytes; \`0\` for a directory. */
  size: number;
  isDirectory: boolean;
  /** When it last changed (RFC 3339), where the backend records one. */
  modified: string | null;
  /** Guessed from the path, as a \`File\` field's MIME rule guesses it. */
  mimeType: string | null;
}

/** A file's metadata: the store's own per-file record, which is where the
 * \`min_role\` access rule is set. */
interface ScFileMeta {
  /** The rule set on this entry, if any. \`1\` is admin, \`100\` is public. */
  minRole: number | null;
  /** The rule that actually applies, given the store's floor and every
   * directory above this entry — read-only, and the one to believe. */
  effectiveMinRole: number | null;
  attributes: Record<string, string>;
}

/** What a file can be written from.
 *
 * A string is written as it is; bytes as they are; a \`Response\` is its body
 * (so \`await file.write(await fetch(url))\` saves a download); another file is
 * copied host-side, so the bytes never enter the sandbox; anything else is
 * stored as JSON. */
type ScWritable = string | Uint8Array | ArrayBuffer | ScResponse | ScFile | Record<string, any> | any[];

/** One file in a store — a **reference** to a path, which need not exist.
 *
 * \`fs("s").open("a.txt")\` touches nothing: every method that does is
 * awaited. */
declare class ScFile {
  /** Store-relative, \`/\`-separated. */
  readonly path: string;
  /** The last component of the path. */
  readonly name: string;
  readonly isDirectory: false;
  readonly store: ScFileStore;
  readonly parent: ScDir;
  /** Whether a **file** is there. A directory of the same name is not one, and
   * this is the question to ask instead of catching a failed read. */
  exists(): Promise<boolean>;
  /** The entry's facts, or null when nothing is there. */
  stat(): Promise<ScFileStat | null>;
  text(): Promise<string>;
  json(): Promise<any>;
  bytes(): Promise<Uint8Array>;
  arrayBuffer(): Promise<ArrayBuffer>;
  /** Create or replace, making the parent directories on the way. Answers the
   * number of bytes written. */
  write(data: ScWritable): Promise<number>;
  /** The same, but refuses to replace a file that is already there. */
  create(data: ScWritable): Promise<number>;
  /** Whether there was anything to delete. */
  delete(): Promise<boolean>;
  /** Move it, within this store or to a file in another, and answer where it
   * went. Refuses to replace an existing destination. */
  moveTo(dest: ScFile | string): Promise<ScFile>;
  /** Copy it, on the same terms. The bytes never enter the sandbox. */
  copyTo(dest: ScFile | string): Promise<ScFile>;
  meta(): Promise<ScFileMeta>;
  /** Replace this entry's metadata — read it first if what you want is a change
   * to one attribute. A delegated body may tighten a rule, never loosen one. */
  setMeta(meta: { minRole?: number | null; attributes?: Record<string, string> }): Promise<ScFile>;
}

/** One directory in a store — a reference, like a file. */
declare class ScDir {
  readonly path: string;
  readonly name: string;
  readonly isDirectory: true;
  readonly store: ScFileStore;
  /** Null at the store's root. */
  readonly parent: ScDir | null;
  file(name: string): ScFile;
  dir(name: string): ScDir;
  /** The direct children, as the same objects everything else takes — so a
   * listing is walked and acted on rather than re-opened by name. Under
   * \`asUser()\` it is filtered to what that caller may see. */
  list(): Promise<(ScFile | ScDir)[]>;
  exists(): Promise<boolean>;
  stat(): Promise<ScFileStat | null>;
  /** Make it, parents included. Not an error if it is already there. */
  create(): Promise<ScDir>;
  /** It and everything in it. */
  delete(): Promise<boolean>;
  meta(): Promise<ScFileMeta>;
  setMeta(meta: { minRole?: number | null; attributes?: Record<string, string> }): Promise<ScFile>;
}

/** One file store, as a code body reaches it. */
declare class ScFileStore {
  readonly name: string;
  /** A file in this store. No I/O: the path need not exist, and writing to it
   * is how it comes to. */
  open(path: string): ScFile;
  dir(path: string): ScDir;
  readonly root: ScDir;
  /** Delegate everything that follows to the person who caused the event, at
   * which point the store's floor and every directory's \`min_role\` decide. */
  asUser(): ScFileStore;
  /** Act as the server (the default). */
  asAdmin(): ScFileStore;
}

/** The file stores, by name. */
interface ScFs {
  (store: string): ScFileStore;
  /** The stores this server has connected. */
  readonly stores: readonly string[];
}
`;
}

/** The declarations for `trigger`: a handle over one of this server's triggers,
 * and the function that answers one.
 *
 * A transcription of `sc-expr`'s `TRIGGERS_PRELUDE`, on the same terms as
 * {@link fileDeclarations}. The name is left as a plain `string` rather than a
 * union of this server's triggers: a trigger's name is the admin's own sentence
 * and may be anything, and the run-time check (the names travel into the run)
 * is the one that can be exact. */
export function triggerDeclarations(): string {
  return `
/** One of this server's triggers, ready to run. Getting the handle does
 * nothing — \`run()\` is what runs it. */
interface ScTrigger {
  /** The trigger's name. */
  readonly name: string;
  /** Run it, with \`payload\` as the event's payload — the same call the Run
   * button and \`POST {mount}/actions/{name}\` make. Answers what its action
   * returned, or \`null\` when its \`only if\` declined. Nothing passed is an
   * empty payload. */
  run(payload?: any): Promise<any>;
  /** Run it on behalf of whoever caused this event, at which point the
   * trigger's own \`min_role\` decides and a refusal is a catchable error. */
  asUser(): ScTrigger;
  /** Run it as the server (the default): a trigger is server-side
   * configuration, so no floor is consulted. */
  asAdmin(): ScTrigger;
}

/** This server's triggers, by name. */
interface ScTriggers {
  (name: string): ScTrigger;
  /** The triggers this server has. */
  readonly names: readonly string[];
}
`;
}

/** The declarations for `models`: `models.get(name)` and the handle it
 * answers, over the run's own `db` (milestone 31 §3).
 *
 * A transcription of `sc-expr`'s `__scMakeModels`, on the same terms as
 * {@link triggerDeclarations}. The posterior's four are optional members,
 * because which handle a name answers is decided by the fit, at run time. The
 * draws and summary answers are the admin API's `getModelDraws` and
 * `getPosteriorSummary`, typed as far as their shape is fixed; the labels and
 * keys are whatever the database's are. */
export function modelDeclarations(): string {
  return `
/** Which elements: \`keys\` picks positions of the first axis by key or
 * label; \`elements\` is the general form — index arrays, or per axis
 * (by its name) the keys or labels wanted. */
interface ScModelSelection {
  keys?: unknown[];
  elements?: number[][] | Record<string, unknown[]>;
}

interface ScDrawsOptions extends ScModelSelection {
  /** Only these chains, from 1. */
  chains?: number[];
  /** The warmup draws too, as chains of their own (when the fit kept them). */
  warmup?: boolean;
  /** Keep every n-th draw. */
  thin?: number;
}

/** One variable's draws, labelled by the database. */
interface ScDraws {
  variable: string;
  /** Positions per axis. */
  dims: number[];
  /** Each axis's name: its dimension's, or \`index\`. */
  axes: string[];
  /** Per axis, every position's label. */
  labels: unknown[][];
  /** Per axis, every position's key. */
  keys: unknown[][];
  /** The selected elements as 1-based index arrays, and by name. */
  elements: number[][];
  names: string[];
  thin: number;
  /** Per chain, one array of draws per selected element. */
  chains: { chain: number; warmup: boolean; draws: (number | null)[][] }[];
}

/** A variable's posterior summary, one row per selected element: its labels,
 * then mean, sd, mcse, q5, q50, q95, rhat, ess_bulk, ess_tail. */
interface ScSummary {
  variable: string;
  source: "draws" | "stored";
  columns: string[];
  elements: number[][];
  names: string[];
  keys: unknown[][];
  rows: unknown[][];
}

/** A fit, as \`m.fit\` holds it. */
interface ScModelFit {
  id: string;
  name: string;
  status: "fitting" | "fitted" | "failed";
  active: boolean;
  created: string;
  error: string | null;
  warnings: string[];
  metrics: any;
  parameters: any[];
}

/** What a fit's outcome is, as it was recorded when it was fitted. */
type ScModelOutcome =
  | { outcome: "regression"; label: string }
  | { outcome: "classification"; label: string; classes?: string[] }
  | { outcome: "cluster" }
  | { outcome: "embedding"; dimensions: number }
  | { outcome: "test" }
  | { outcome: "posterior"; prediction?: string };

/** One prediction with \`{ detail: true }\`: the value, and the class's
 * probability where there is one. */
interface ScPrediction {
  value: any;
  probability?: number;
}

/** What \`m.writePosterior\` writes: statistics of a variable into fields —
 * into the rows the variable is about (\`update\`, the default), or as new rows
 * of \`table\` (\`insert\`), with each element's coordinates written too. */
interface ScPosteriorWrite {
  variable: string;
  mode?: "update" | "insert";
  /** Statistic → field: \`{ mean: "alpha_mean", sd: "alpha_sd" }\`. */
  statistics: Record<string, string>;
  table?: string;
  coordinates?: { axis: string; field: string; value?: "key" | "label" | "position" }[];
  instance_field?: string;
  elements?: ScModelSelection["elements"];
}

/** A model, and the fit \`models.get\` resolved — which every call on the
 * handle keeps using, even if another fit is activated meanwhile.
 *
 * \`draws\`, \`summary\`, \`variables\` and \`writePosterior\` exist on a
 * posterior's handle only; on any other, reaching one throws a sentence
 * saying what the model is. */
interface ScModel {
  readonly name: string;
  readonly provider: string;
  /** The table whose rows it predicts. */
  readonly table: string;
  readonly outcome: ScModelOutcome | null;
  readonly fit: ScModelFit;
  /** One value for a row; one per row, in order, for an array — one request
   * either way. A row with the table's primary key is read through the
   * model's dataset; any other must supply every feature. */
  predict(row: ScRow): Promise<any>;
  predict(rows: ScRow[]): Promise<any[]>;
  predict(row: ScRow, options: { detail: true }): Promise<ScPrediction>;
  predict(rows: ScRow[], options: { detail: true }): Promise<ScPrediction[]>;
  predict(row: ScRow | ScRow[], options?: { detail?: boolean }): Promise<any>;
  /** A posterior's draws of one variable, labelled by the database. */
  draws?(variable: string, options?: ScDrawsOptions): Promise<ScDraws>;
  /** A posterior's summary of one variable. */
  summary?(variable: string, options?: ScModelSelection): Promise<ScSummary>;
  /** What a posterior's fit drew, its \`__\` internals left out. */
  readonly variables?: readonly string[];
  /** Write a posterior's summary into rows, under this handle's authority:
   * ownership is checked and the target table's triggers fire. */
  writePosterior?(write: ScPosteriorWrite): Promise<{
    variable: string;
    mode: "update" | "insert";
    table: string;
    instance: string;
    written: number;
  }>;
  /** The same handle, writing back as the event's caller. */
  asUser(): ScModel;
  /** The same handle, writing back as the trigger — the default. */
  asAdmin(): ScModel;
}

/** The models, by name. */
interface ScModels {
  /** A handle on the model's active fit, or on the fit \`fit\` names. */
  get(model: string, options?: { fit?: string }): Promise<ScModel>;
}
`;
}

/** The TypeScript type one of v1's declared argument types arrives as.
 *
 * v1's own type names, which is the vocabulary `sc_module::spec` already
 * translates for a setting. An undeclared or unrecognised one is `unknown`
 * rather than a confident `string`, for {@link columnType}'s reason. */
function moduleArgType(declared: string | null): string {
  switch (declared) {
    case "String":
    case "string":
      return "string";
    case "Integer":
    case "Float":
    case "Number":
      return "number";
    case "Bool":
    case "Boolean":
      return "boolean";
    default:
      return "unknown";
  }
}

/** One function's parameter list, as TypeScript.
 *
 * A function that declared nothing takes anything: v1 does not require
 * `arguments`, and an empty list here would refuse a call the sandbox accepts. */
function moduleParams(fn: ModuleFunctionInfo): string {
  if (fn.arguments.length === 0) return "...args: unknown[]";
  return fn.arguments
    .map((arg, index) => {
      const name = /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(arg.name) ? arg.name : `arg${index + 1}`;
      return `${name}?: ${moduleArgType(arg.type)}`;
    })
    .join(", ");
}

/** One function's doc comment: what the module said, and which module said it. */
function moduleFnDoc(fn: ModuleFunctionInfo): string {
  const said = fn.description.trim() === "" ? "" : `${fn.description.trim()}\n *\n * `;
  const sync = fn.isAsync
    ? ""
    : " It is a synchronous function in the module, and awaited here because it\n * runs on the module's own isolate.";
  return `/** ${said}From \`${fn.module}\`.${sync} */`;
}

/** The declarations for `modfn`: the functions this server's modules supply.
 *
 * A transcription of `sc-expr`'s `MODULE_FNS_PRELUDE`, on the same terms as
 * {@link triggerDeclarations} — and the one place in this file where the
 * declarations are exact rather than approximate, because the function list is
 * the *same list* the run is handed.
 *
 * Two spellings, as the prelude has: `modfn(module).fn(…)` always works, and
 * `modfn.fn(…)` is the short form for a name only one module supplies. A name
 * two modules supply is deliberately **left out of the short form**, because
 * calling it there throws: completing it would be offering a mistake. */
export function moduleFunctionDeclarations(functions: ModuleFunctionInfo[]): string {
  if (functions.length === 0) return "";
  const modules = [...new Set(functions.map((f) => f.module))].sort();
  const parts: string[] = [];
  const overloads: string[] = [];
  modules.forEach((module, index) => {
    const iface = `ScModuleFns${index}`;
    const members = functions
      .filter((f) => f.module === module)
      .map((f) => `  ${moduleFnDoc(f)}\n  ${propertyKey(f.name)}(${moduleParams(f)}): Promise<any>;`)
      .join("\n");
    parts.push(`/** The functions \`${module}\` supplies. */\ninterface ${iface} {\n${members}\n}`);
    overloads.push(`  (module: ${literal(module)}): ${iface};`);
  });

  // The short form: one entry per name exactly one module supplies.
  const byName = new Map<string, ModuleFunctionInfo[]>();
  for (const fn of functions) {
    const supplying = byName.get(fn.name) ?? [];
    supplying.push(fn);
    byName.set(fn.name, supplying);
  }
  const short = [...byName.entries()]
    .filter(([, supplying]) => supplying.length === 1)
    .map(
      ([, [fn]]) =>
        `  ${moduleFnDoc(fn)}\n  ${propertyKey(fn.name)}(${moduleParams(fn)}): Promise<any>;`,
    )
    .join("\n");

  parts.push(
    `/** This server's module functions. */\ninterface ScModuleFns {\n${overloads.join("\n")}\n` +
      `${short}\n` +
      `  /** Every function this server's modules supply. */\n` +
      `  readonly functions: readonly {\n` +
      `    readonly module: string;\n    readonly name: string;\n` +
      `    readonly isAsync: boolean;\n    readonly description: string;\n` +
      `  }[];\n}`,
  );
  return `\n${parts.join("\n\n")}\n`;
}

/** The declarations for this server's tables: a row interface and a column union
 * per table, and the `db` handle carrying one property per table. */
export function tableDeclarations(tables: TableInfo[]): string {
  const parts: string[] = [];
  for (const table of tables) {
    const name = typeName(table.name);
    const fields = table.columns
      .map((c) => `  ${propertyKey(c.name)}: ${columnType(c)};`)
      .join("\n");
    parts.push(
      `/** A row of \`${table.name}\`. */\ninterface ${name}Row {\n${fields}\n` +
        // A `.select()` with an alias, or a Ⱶ join column, puts keys here that
        // the table itself does not have. Left open rather than enumerated, so
        // reading one is quiet instead of wrong.
        `  [column: string]: any;\n}\n`,
    );
    const columns = columnNames(table, tables).map(literal).join(" | ");
    parts.push(
      `/** A column of \`${table.name}\`, or a \`keyⱵcolumn\` path from it. */\n` +
        `type ${name}Column = ${columns || "never"};\n`,
    );
  }

  const properties = tables
    .map(
      (t) =>
        `  /** The \`${t.name}\` table. */\n` +
        `  ${propertyKey(t.name)}: ScQuery<${typeName(t.name)}Row, ${typeName(t.name)}Column>;`,
    )
    .join("\n");
  // `(string & {})` keeps the literal completions while still accepting a name
  // computed at runtime — `db.table(payload.which)` is legitimate.
  const names = tables.map((t) => literal(t.name)).join(" | ");
  const tableName = names === "" ? "string" : `${names} | (string & {})`;

  parts.push(
    `/** The tables, read and written from a code body. */\ninterface ScDb {\n` +
      `  /** Any table, by name — the general form of \`db.<table>\`. */\n` +
      `  table(name: ${tableName}): ScAnyQuery;\n` +
      // The escape hatch, declared with the same warning the host carries: the
      // text is the author's, so nothing the chain guarantees applies to it.
      `  /** Run SQL this body wrote, and return its rows.\n` +
      `   *\n` +
      `   * The escape hatch for what the chain does not express — a window\n` +
      `   * function, a recursive CTE, an \`ON CONFLICT\`. Values go in \`params\`\n` +
      `   * and are **bound**, never written into the text: \`await db.sql("select\n` +
      `   * * from books where pages > $1", [200])\`.\n` +
      `   *\n` +
      `   * It does not go through the row layer, so no ownership formula filters\n` +
      `   * it, no rich type coerces it, and a write inside one raises **no table\n` +
      `   * event**. \`{ asUser: true }\` (or \`db.asUser().sql(…)\`) runs it at the\n` +
      `   * caller's role and user, which is what row-level security reads. */\n` +
      `  sql(sql: string, params?: ScValue[], options?: ScSqlOptions): Promise<ScRow[]>;\n` +
      `  /** Delegate everything that follows to the person who caused the event. */\n` +
      `  asUser(): ScDb;\n` +
      `  /** Act as the server (the default). */\n` +
      `  asAdmin(): ScDb;\n${properties}\n}\n`,
  );
  return parts.join("\n");
}

/** The declarations for the event's own bindings, which depend on the trigger.
 *
 * Presence is the whole point: a binding this event does not have is left out,
 * because naming it in the sandbox is a `ReferenceError` and an editor that
 * completed it would be promising something the run refuses. */
export function scopeDeclarations(
  scope: CodeScope,
  tables: TableInfo[],
  functions: ModuleFunctionInfo[] = [],
): string {
  const parts: string[] = [];
  const table = tables.find((t) => t.name === scope.table);
  if (table) {
    const row = `${typeName(table.name)}Row`;
    parts.push(`/** The \`${table.name}\` row the event is about. */\ndeclare const row: ${row};`);
    parts.push(
      `/** The row as it was before this event — null on an insert. */\n` +
        `declare const old: ${row} | null;`,
    );
  } else if (scope.table !== undefined) {
    // A table event whose table has no declarations (it was dropped, or the
    // fetch for it failed): the bindings still exist, so declare them loosely
    // rather than leaving them undeclared and completing nothing.
    parts.push(`/** The row the event is about. */\ndeclare const row: ScRow;`);
    parts.push(`/** The row as it was before this event — null on an insert. */\ndeclare const old: ScRow | null;`);
  }
  parts.push(
    `/** Whoever caused the event, or null for the server's own events. */\n` +
      `declare const user: ScUser | null;`,
  );
  if (scope.request) {
    parts.push(
      `/** The request's JSON body — \`{}\` when there was none. A declared\n` +
        ` * parameter arrives here, converted to its type, for a method with a\n` +
        ` * body. */\ndeclare const body: any;`,
    );
    parts.push(
      `/** The request's query string, one value per key. A declared parameter\n` +
        ` * arrives here, converted to its type, for \`GET\` and \`DELETE\`. */\n` +
        `declare const query: Record<string, any>;`,
    );
  } else {
    parts.push(
      `/** What the trigger was called with: the body posted to a directly-run\n` +
        ` * trigger, or what the event carried. */\n` +
        `declare const payload: Record<string, any>;`,
    );
  }
  if (scope.run) {
    // Only a workflow step has it, so it is declared only for one — naming it
    // in a trigger's own body is a `ReferenceError`, and completing it would be
    // promising something the run refuses.
    parts.push(
      `/** The run so far: what the steps before this one returned, under their\n` +
        ` * own names, plus whatever a \`Set\` step wrote. Only a workflow step\n` +
        ` * has it. */\ndeclare const context: Record<string, any>;`,
    );
  }
  parts.push(
    `/** The tables. Only a code body has this — a formula (an \`only if\`, an\n` +
      ` * ownership rule) evaluates without it. */\ndeclare const db: ScDb;`,
  );
  parts.push(
    `/** The models, by name. \`models.get\` answers a handle on a model's\n` +
      ` * active fit (or \`{ fit: id }\`'s):\n` +
      ` *\n` +
      ` * \`\`\`js\n` +
      ` * const m = await models.get("House prices");\n` +
      ` * const price = await m.predict(row);\n` +
      ` * const r = await models.get("Radon");\n` +
      ` * const alpha = await r.draws("alpha", { keys: [27001] });\n` +
      ` * await r.writePosterior({ variable: "alpha", statistics: { mean: "alpha_mean" } });\n` +
      ` * \`\`\`\n` +
      ` *\n` +
      ` * Each call is a database call of this run, on its budget; a draws answer\n` +
      ` * is at most 500 000 numbers — \`thin\` and \`chains\` keep it under. */\n` +
      `declare const models: ScModels;`,
  );
  parts.push(
    `/** Call an HTTP endpoint. The web's \`fetch\`, with the web's rules: a\n` +
      ` * non-2xx status is an answer rather than a throw, and only a transport\n` +
      ` * failure rejects (with a \`TypeError\`).\n` +
      ` *\n` +
      ` * \`\`\`js\n` +
      ` * const res = await fetch("https://api.example.com/rates", {\n` +
      ` *   headers: { authorization: "Bearer " + payload.token },\n` +
      ` * });\n` +
      ` * if (!res.ok) throw new Error("rates: " + res.status);\n` +
      ` * const { usd } = await res.json();\n` +
      ` * \`\`\`\n` +
      ` *\n` +
      ` * Bounded like everything else a body reaches: 50 requests per run, each\n` +
      ` * clamped to what is left of this code's \`timeout_ms\`, and a response of\n` +
      ` * at most 8 MB. Only a code body has it — a formula evaluates without\n` +
      ` * it. */\ndeclare function fetch(\n` +
      `  url: string,\n` +
      `  options?: ScFetchOptions,\n` +
      `): Promise<ScResponse>;\n` +
      `declare const Headers: typeof ScHeaders;\n` +
      `declare const Response: typeof ScResponse;`,
  );
  parts.push(
    `/** The file stores. \`fs(name)\` is one store, \`open\` is a reference to a\n` +
      ` * path in it — no I/O, and the path need not exist — and everything that\n` +
      ` * touches bytes is awaited:\n` +
      ` *\n` +
      ` * \`\`\`js\n` +
      ` * const theFile = fs("uploads").open("the_file.txt");\n` +
      ` * if (await theFile.exists()) {\n` +
      ` *   const theString = await theFile.text();\n` +
      ` * }\n` +
      ` * await fs("uploads").open("reports/summary.json").write({ rows: 12 });\n` +
      ` * \`\`\`\n` +
      ` *\n` +
      ` * Bounded like everything else a body reaches: 100 operations per run and\n` +
      ` * 8 MB across the boundary per read or write. Only a code body has it — a\n` +
      ` * formula evaluates without it. */\ndeclare const fs: ScFs;`,
  );
  parts.push(
    `/** This server's other triggers. \`trigger(name)\` is a handle — nothing\n` +
      ` * happens until \`run()\`:\n` +
      ` *\n` +
      ` * \`\`\`js\n` +
      ` * const archived = await trigger("archive_done").run({ before: payload.today });\n` +
      ` * await trigger("send_invoice").asUser().run({ id: row.id });\n` +
      ` * \`\`\`\n` +
      ` *\n` +
      ` * It runs the trigger the admin configured, through the same path every\n` +
      ` * other event takes: its \`only if\` runs, a disabled one stays disabled,\n` +
      ` * and the cascade is bounded — a chain five deep is refused, naming it.\n` +
      ` * Bounded like everything else a body reaches: 20 runs per body, each\n` +
      ` * clamped to what is left of this code's \`timeout_ms\`. Only a code body\n` +
      ` * has it — a formula evaluates without it. */\ndeclare const trigger: ScTriggers;`,
  );
  // Declared only when this server's modules supply something: a handle with
  // nothing on it completes nothing, and would only suggest that a module
  // function exists somewhere.
  if (functions.length > 0) {
    parts.push(
      `/** The functions this server's installed modules supply.\n` +
        ` *\n` +
        ` * \`\`\`js\n` +
        ` * const html = await modfn.md_to_html(row.notes);\n` +
        ` * const lat = await modfn("@saltcorn/nominatim-geocode").geocode_lat(q);\n` +
        ` * \`\`\`\n` +
        ` *\n` +
        ` * **Everything is awaited**, including the functions that are\n` +
        ` * synchronous inside the module: the call crosses to the isolate that\n` +
        ` * module was loaded on, which is where its state is. A name two\n` +
        ` * modules supply has no short form — say which with \`modfn(name)\`.\n` +
        ` * Bounded like everything else a body reaches: 100 calls per run, each\n` +
        ` * clamped to what is left of this code's \`timeout_ms\`. A formula may\n` +
        ` * call one too, but only with columns, Ⱶ-join values and literals as\n` +
        ` * arguments. */\ndeclare const modfn: ScModuleFns;`,
    );
  }
  return `${parts.join("\n\n")}\n`;
}

/** The whole ambient library handed to the editor. */
export function codeLibrary(
  tables: TableInfo[],
  scope: CodeScope,
  functions: ModuleFunctionInfo[] = [],
): string {
  return [
    "// The Saltcorn code sandbox, as types. Generated by the admin UI from this",
    "// server's tables; not a file in any project.",
    "",
    chainDeclarations(),
    fileDeclarations(),
    triggerDeclarations(),
    modelDeclarations(),
    moduleFunctionDeclarations(functions),
    tableDeclarations(tables),
    scopeDeclarations(scope, tables, functions),
  ].join("\n");
}

/** The catalog the declarations are built from, read once per page.
 *
 * Cached as the *promise*, so two editors opening at once make one round of
 * requests. Not invalidated: a table added in another tab changes what a body
 * can reach, and the admin reloads to see it — the cost of being wrong is a
 * missing completion, and the cost of re-reading the whole catalog on every
 * keystroke-adjacent event is worse. */
let catalogCache: Promise<TableInfo[]> | null = null;

/** Read every table and its fields.
 *
 * One request per table, in parallel, because that is the API the admin UI has
 * (`listFields` is per-table). A table whose fields cannot be read is kept with
 * no columns rather than dropped: `db.<name>` still exists in the sandbox, so it
 * should still exist in the completions. */
export async function loadCatalog(): Promise<TableInfo[]> {
  const tables = await api.listTables();
  return await Promise.all(
    tables.map(async (table): Promise<TableInfo> => {
      try {
        const fields = await api.listFields(table.name);
        return {
          name: table.name,
          columns: fields.map((field) => {
            const kind = field.kind as { type?: string; target_table?: string } | null;
            return {
              name: field.name,
              type: field.type,
              sqlType: field.sql_type,
              required: field.required,
              keyTo: kind?.type === "key" ? kind.target_table : undefined,
            };
          }),
        };
      } catch {
        return { name: table.name, columns: [] };
      }
    }),
  );
}

/** [`loadCatalog`] once per page. */
export function catalog(): Promise<TableInfo[]> {
  catalogCache ??= loadCatalog();
  return catalogCache;
}

/** The module functions, cached like the catalog and for its reasons.
 *
 * Read from `listModules`, which is the API this server already has for the
 * Modules tab: a module that loaded reports what it supplies, and one that did
 * not supplies nothing — which is exactly what should be completed. */
let moduleFunctionCache: Promise<ModuleFunctionInfo[]> | null = null;

/** Every function every loaded module supplies. */
export async function loadModuleFunctions(): Promise<ModuleFunctionInfo[]> {
  try {
    const listed = await api.listModules();
    return listed.modules.flatMap((module) =>
      module.functions.map((fn) => ({
        module: module.name,
        name: fn.name,
        description: fn.description,
        isAsync: fn.is_async,
        arguments: fn.arguments.map((argument) => ({
          name: argument.name,
          type: argument.type ?? null,
        })),
      })),
    );
  } catch {
    // A server built without module support answers this with a configuration
    // error, and an admin editing a body should still get their completions.
    return [];
  }
}

/** [`loadModuleFunctions`] once per page. */
export function moduleFunctions(): Promise<ModuleFunctionInfo[]> {
  moduleFunctionCache ??= loadModuleFunctions();
  return moduleFunctionCache;
}
