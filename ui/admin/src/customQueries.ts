// The application form's **custom query** model (§13.4): the records an admin
// edits, the request they are checked with, and what the check reports back.
// A query is SQL, JavaScript or Python (`language`); the source is stored under
// `code` whichever it is.
//
// A module rather than state inside the screen, for the reason `apiRows.ts` is
// one: what has to be right is a conversion. A stored `queries` array becomes
// rows an admin can edit and comes back as the same array, and a row becomes the
// body `describeCustomQuery` prepares. Both are testable without a browser
// (`customQueries.test.ts`); what is left in the screen is controls.
//
// **Nothing here decides whether a query is valid.** The name, the path, the
// parameters and the SQL are all judged by the server — the same call a save
// makes, so there is one authority for what a valid query is and the admin never
// sees the form and the server disagree. This module carries the answer to the
// screen; it does not invent one.

/** One declared input parameter, as the form edits it. */
export type QueryParamRow = {
  name: string;
  /** A `ValueType` name (`text`, `int`, `date`, …), as the wire spells it. */
  type: string;
  required: boolean;
};

/** One result column, as the **database** described it. Never edited: the admin
 * declares the parameters and Postgres types the result (decision 5). */
export type QueryColumn = { name: string; type: string };

/** What a query's source is written in — `sc_api::QueryLanguage`'s names. */
export type QueryLanguage = "sql" | "javascript" | "python";

/** The languages, in the order the drop-down lists them. */
export const QUERY_LANGUAGES: readonly QueryLanguage[] = ["sql", "javascript", "python"];

/** A language's name as the drop-down shows it. Product names, so not
 * translated. */
export function languageLabel(language: QueryLanguage): string {
  switch (language) {
    case "sql":
      return "SQL";
    case "javascript":
      return "JavaScript";
    case "python":
      return "Python";
  }
}

/** Whether the server can run Python, from `getPythonStatus`'s `state`: built
 * with it, and not started with `--python off`. The interpreter itself starts
 * with the first body that needs it, so `not_initialised` counts. */
export function pythonAvailable(state: string | null | undefined): boolean {
  return state === "running" || state === "not_initialised";
}

/** The languages the drop-down offers a query. Python only where the server
 * runs it — except for a query that is already Python, which has to be shown
 * as what it is so it can be changed to something that runs. */
export function languageOptions(current: QueryLanguage, python: boolean): QueryLanguage[] {
  return QUERY_LANGUAGES.filter((l) => l !== "python" || python || current === "python");
}

/** Whether the source is a JavaScript or Python body rather than SQL. */
export function isCode(language: QueryLanguage): boolean {
  return language !== "sql";
}

/** Whether a code query with no declared parameters should carry the note
 * that its body is untyped. The generated client method still sends a body
 * (`GET`/`DELETE`: a query string) typed `Record<string, unknown>`, and the
 * code reads all of it; declaring parameters types and checks them. The same
 * rule as the server's `undeclared_params_note`. */
export function needsParamsNote(row: Pick<QueryRow, "language" | "params">): boolean {
  return isCode(row.language) && row.params.length === 0;
}

function languageFrom(value: unknown): QueryLanguage {
  return QUERY_LANGUAGES.includes(value as QueryLanguage) ? (value as QueryLanguage) : "sql";
}

/** What a JavaScript query's `body` is, and what it answers with (TODO
 * F2.9/F2.10): JSON, or bytes — a raw body in, a `Blob` (bytes or a file) out. */
export type PayloadType = "json" | "bytes";

function payloadFrom(value: unknown): PayloadType {
  return value === "bytes" ? "bytes" : "json";
}

/** Whether the body-type choice applies: a JavaScript query on a method that
 * has a body. */
export function bodyTypeApplies(row: Pick<QueryRow, "language" | "method">): boolean {
  return row.language === "javascript" && !["GET", "DELETE"].includes(row.method);
}

/** Whether the response-type choice applies: a JavaScript query. */
export function responseTypeApplies(row: Pick<QueryRow, "language">): boolean {
  return row.language === "javascript";
}

/** One custom query, as the form edits it. */
export type QueryRow = {
  name: string;
  description: string;
  method: string;
  path: string;
  language: QueryLanguage;
  /** The source: SQL, or a JavaScript or Python body. */
  code: string;
  minRole: number;
  params: QueryParamRow[];
  /** `bytes`: `body` is a Uint8Array of the raw request body. */
  bodyType: PayloadType;
  /** `bytes`: the client method resolves to a `Blob`. */
  responseType: PayloadType;
  /** The columns last described — from the stored query when the form opened,
   * or from the check button. Shown as the preview of what the generated client
   * method will return. */
  columns: QueryColumn[];
};

/** What the check button is doing, and what it found. */
export type CheckStatus =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "ok"; columns: QueryColumn[] }
  /** A JavaScript or Python query passed every check but its body, which only
   * runs when it is called. */
  | { kind: "checked" }
  | { kind: "error"; message: string };

/** The parameter types an admin may declare — the wire's `ValueType` names, in
 * `ValueType::ALL`'s order.
 *
 * A list rather than a free-text box: the type is what the caller's argument is
 * coerced to before it is bound, so a misspelt one is a query that refuses every
 * call it gets. The copy is the same arrangement every field name in this file
 * lives under — these are protocol names, and the server refuses one it does not
 * know. */
export const PARAM_TYPES = [
  "text",
  "int",
  "float",
  "decimal",
  "bool",
  "date",
  "timestamp",
  "time",
  "uuid",
  "json",
  "bytes",
] as const;

/** The methods a query may answer on. The admin's choice, with no inference from
 * the SQL (GOALS) — what is inferred is the transaction, and `GET` gets a
 * read-only one. */
export const QUERY_METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE"] as const;

/** The role floor a query starts at: **admin**, and stated rather than defaulted
 * quietly (§10.2). */
export const ADMIN_ROLE = 1;

/** An empty query — what "Add" produces. */
export function blankQueryRow(): QueryRow {
  return {
    name: "",
    description: "",
    method: "GET",
    path: "",
    language: "sql",
    code: "",
    minRole: ADMIN_ROLE,
    params: [],
    bodyType: "json",
    responseType: "json",
    columns: [],
  };
}

/** An empty parameter row. */
export function blankParamRow(): QueryParamRow {
  return { name: "", type: "text", required: true };
}

/** The key custom queries are stored under inside an API's config object
 * (`sc_api::REST_CFG_QUERIES`). A protocol constant, so the client's copy having
 * to match is the arrangement every other field name here lives under. */
export const QUERIES_KEY = "queries";

/** Read the `queries` of a stored API config into editable rows.
 *
 * Tolerant of anything: a config bag arrives as `unknown`, and a value that is
 * not a list of query records reads as no queries rather than throwing on a
 * screen the admin came to fix something else on. What it will not do is
 * *invent* — a record missing its SQL keeps an empty one, and the server refuses
 * it on save. */
export function queryRowsFromConfig(config: unknown): QueryRow[] {
  const raw = (config as Record<string, unknown> | null | undefined)?.[QUERIES_KEY];
  if (!Array.isArray(raw)) return [];
  return raw.filter((q) => q && typeof q === "object").map(queryRowFrom);
}

function queryRowFrom(stored: unknown): QueryRow {
  const q = stored as Record<string, unknown>;
  const params = Array.isArray(q.params) ? q.params : [];
  const columns = Array.isArray(q.columns) ? q.columns : [];
  return {
    name: text(q.name),
    description: text(q.description),
    method: text(q.method) || "GET",
    path: text(q.path),
    language: languageFrom(q.language),
    code: text(q.code),
    minRole: typeof q.min_role === "number" ? q.min_role : ADMIN_ROLE,
    bodyType: payloadFrom(q.body_type),
    responseType: payloadFrom(q.response_type),
    params: params.map((p) => {
      const o = (p ?? {}) as Record<string, unknown>;
      return {
        name: text(o.name),
        type: text(o.type) || "text",
        // Absent means required, as the stored form's serde default does.
        required: o.required !== false,
      };
    }),
    columns: columns.map((c) => {
      const o = (c ?? {}) as Record<string, unknown>;
      return { name: text(o.name), type: text(o.type) };
    }),
  };
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/** One query as it is stored and sent — the wire shape, whose field names are
 * the server's (`min_role`, `type`) rather than the form's. */
export type StoredQuery = {
  name: string;
  description: string;
  method: string;
  path: string;
  language: QueryLanguage;
  code: string;
  min_role: number;
  params: { name: string; type: string; required: boolean }[];
  /** Sent only when not `json`, as the server stores it. */
  body_type?: PayloadType;
  response_type?: PayloadType;
};

/** The rows as the `queries` value of an API's config object.
 *
 * The described **columns are not sent back**: they are the database's answer,
 * rewritten on every save from the SQL that arrives with them, and echoing a
 * stale copy would be a second source of truth travelling in the same request as
 * the first. */
export function queryRowsToConfig(rows: QueryRow[]): StoredQuery[] {
  return rows.map((row) => {
    const stored: StoredQuery = {
      name: row.name.trim(),
      description: row.description.trim(),
      method: row.method,
      path: row.path.trim(),
      language: row.language,
      code: row.code,
      min_role: row.minRole,
      params: row.params.map((p) => ({
        name: p.name.trim(),
        type: p.type,
        required: p.required,
      })),
    };
    // A choice that no longer applies (the language or method changed) is
    // dropped rather than sent for the server to refuse.
    if (row.bodyType === "bytes" && bodyTypeApplies(row)) stored.body_type = "bytes";
    if (row.responseType === "bytes" && responseTypeApplies(row)) stored.response_type = "bytes";
    return stored;
  });
}

/** The body `describeCustomQuery` takes: the row, plus the tables the
 * application declares — so the check answers the *whole* question a save would,
 * including the name and path rules that are about this app's tables. */
export function describeBody(row: QueryRow, tables: string[]): StoredQuery & {
  tables: string[];
} {
  const [query] = queryRowsToConfig([row]);
  return { ...query, tables };
}

/** The preview line: what the generated client method returns, as the database
 * described it. Empty when nothing has been described yet, so the screen shows
 * the invitation to check rather than an empty shape it might be believed. */
export function columnsSummary(columns: QueryColumn[]): string {
  return columns.map((c) => `${c.name}: ${c.type}`).join(", ");
}

/** The one-line status a check leaves behind. */
export function statusSummary(status: CheckStatus): string {
  switch (status.kind) {
    case "idle":
      return "";
    case "checking":
      return "Checking…";
    case "ok":
      return status.columns.length
        ? `Returns ${columnsSummary(status.columns)}`
        : "Prepared. It returns no columns.";
    case "checked":
      return "Checked. It returns whatever its body returns, as JSON.";
    case "error":
      return status.message;
  }
}
