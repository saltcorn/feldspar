/**
 * The custom-query editor's model (TODO "API improvements" Phase 5): a stored
 * `queries` array in, rows an admin can edit out, and the same array back —
 * plus the body a check is made with and the preview a check produces.
 *
 * The claims worth asserting without a browser are all conversions, and each
 * one has a failure it prevents: a query that loses its parameters on the way
 * back is an endpoint whose client method takes no arguments; a query silently
 * dropped by a form that only knows about settings is an endpoint that
 * disappears when somebody edits the mount; and echoing stale result columns
 * back to the server is a second source of truth travelling in the same request
 * as the first.
 */

import { describe, expect, it } from "vitest";

import {
  ADMIN_ROLE,
  blankQueryRow,
  columnsSummary,
  describeBody,
  isCode,
  languageOptions,
  needsParamsNote,
  pythonAvailable,
  queryRowsFromConfig,
  queryRowsToConfig,
  statusSummary,
} from "./customQueries";
import { apiRowsFromApp, apiRowsToRequest, supportsCustomQueries } from "./apiRows";
import type { FieldSpec } from "./settings";

/** One stored query, as the server writes it: parameters declared by the admin,
 * columns written by the database. */
const stored = {
  name: "topAuthors",
  description: "Most published authors since a year",
  method: "GET",
  path: "/reports/top-authors",
  code: "select author, count(*) as n from books where year > :since group by author",
  params: [{ name: "since", type: "int", required: true }],
  min_role: 40,
  columns: [
    { name: "author", type: "text" },
    { name: "n", type: "int" },
  ],
};

const field = (name: string, type: string, dflt: unknown): FieldSpec => ({
  name,
  label: name,
  type,
  required: false,
  default: dflt,
  options: [],
  multiline: false,
});

const providers = [
  { name: "rest", config_spec: [field("row_cap", "int", 500)], supports_custom_queries: true },
  {
    name: "graphql",
    config_spec: [field("aggregates", "bool", false)],
    supports_custom_queries: false,
  },
];

describe("reading the stored queries", () => {
  it("reads a query, its parameters and the columns the database described", () => {
    const [row] = queryRowsFromConfig({ queries: [stored] });
    expect(row.name).toBe("topAuthors");
    expect(row.method).toBe("GET");
    expect(row.minRole).toBe(40);
    expect(row.params).toEqual([{ name: "since", type: "int", required: true }]);
    expect(row.columns).toEqual(stored.columns);
  });

  it("reads an absent role floor as admin and an absent `required` as required", () => {
    // Both are the stored form's serde defaults, and both fail *open* if read
    // the other way: a public endpoint nobody asked for, and an argument the
    // client may omit into a query that needs it.
    const [row] = queryRowsFromConfig({
      queries: [{ name: "r", method: "GET", path: "/r", code: "select 1", params: [{ name: "a" }] }],
    });
    expect(row.minRole).toBe(ADMIN_ROLE);
    expect(row.params[0].required).toBe(true);
  });

  it("reads a config with no queries, and one that is not a list, as none", () => {
    expect(queryRowsFromConfig({ row_cap: 100 })).toEqual([]);
    expect(queryRowsFromConfig({ queries: "nonsense" })).toEqual([]);
    expect(queryRowsFromConfig(null)).toEqual([]);
  });
});

describe("writing the queries back", () => {
  it("round-trips a stored query, without echoing its described columns", () => {
    const rows = queryRowsFromConfig({ queries: [stored] });
    const [out] = queryRowsToConfig(rows);
    const { columns, ...withoutColumns } = stored;
    // A query stored before it had a language is SQL, and says so going back.
    expect(out).toEqual({ ...withoutColumns, language: "sql" });
    expect(columns).toHaveLength(2);
    expect(out).not.toHaveProperty("columns");
  });

  it("trims the name and path but leaves the code exactly as written", () => {
    const row = { ...blankQueryRow(), name: " r ", path: " /r ", code: "  select 1  " };
    const [out] = queryRowsToConfig([row]);
    expect(out.name).toBe("r");
    expect(out.path).toBe("/r");
    expect(out.code).toBe("  select 1  ");
  });

  it("survives an edit to the API row it belongs to", () => {
    // The failure: `buildConfig` writes the provider's declared settings and
    // nothing else, so an admin who opened the application to change its mount
    // would save its custom queries away.
    const rows = apiRowsFromApp([
      { provider: "rest", mount: "/api", config: { row_cap: 100, queries: [stored] } },
    ]);
    expect(rows[0].queries).toHaveLength(1);
    rows[0].mount = "/rest";
    const [api] = apiRowsToRequest(rows, providers);
    expect(api.mount).toBe("/rest");
    expect(api.config.row_cap).toBe(100);
    expect(api.config.queries).toEqual([queryRowsToConfig(rows[0].queries)[0]]);
  });

  it("carries no `queries` key when there are none", () => {
    const rows = apiRowsFromApp([{ provider: "rest", mount: "/api", config: {} }]);
    expect(apiRowsToRequest(rows, providers)[0].config).not.toHaveProperty("queries");
  });

  it("offers the editor only where the provider says it serves queries", () => {
    expect(supportsCustomQueries(providers, "rest")).toBe(true);
    expect(supportsCustomQueries(providers, "graphql")).toBe(false);
    expect(supportsCustomQueries(providers, "grpc")).toBe(false);
  });
});

describe("checking a query", () => {
  it("sends the query and the app's tables, so the check is the save's check", () => {
    const [row] = queryRowsFromConfig({ queries: [stored] });
    const body = describeBody(row, ["books", "authors"]);
    expect(body.name).toBe("topAuthors");
    expect(body.code).toBe(stored.code);
    expect(body.params).toEqual(stored.params);
    expect(body.tables).toEqual(["books", "authors"]);
  });

  it("previews the described columns as the client method's return shape", () => {
    expect(columnsSummary(stored.columns)).toBe("author: text, n: int");
    expect(statusSummary({ kind: "ok", columns: stored.columns })).toContain("author: text");
  });

  it("shows a refusal as the message the server sent", () => {
    // Postgres's own words, unedited: "column `titel` does not exist" is a
    // better error than anything this form could invent from it.
    const message = 'custom SQL query `topAuthors`: column "titel" does not exist';
    expect(statusSummary({ kind: "error", message })).toBe(message);
  });

  it("says nothing at all before a check has run", () => {
    expect(statusSummary({ kind: "idle" })).toBe("");
  });
});

describe("the language a query is written in", () => {
  const js = {
    name: "greet",
    method: "POST",
    path: "/greet",
    language: "javascript",
    code: "return { hello: body.name };",
    params: [],
  };

  it("reads and writes the language, and reads an unknown one as SQL", () => {
    const [row] = queryRowsFromConfig({ queries: [js] });
    expect(row.language).toBe("javascript");
    expect(isCode(row.language)).toBe(true);
    expect(queryRowsToConfig([row])[0].language).toBe("javascript");
    expect(describeBody(row, []).language).toBe("javascript");

    const [odd] = queryRowsFromConfig({ queries: [{ ...js, language: "cobol" }] });
    expect(odd.language).toBe("sql");
    expect(blankQueryRow().language).toBe("sql");
  });

  it("offers Python only where the server runs it, or where the query already is", () => {
    expect(languageOptions("sql", false)).toEqual(["sql", "javascript"]);
    expect(languageOptions("sql", true)).toEqual(["sql", "javascript", "python"]);
    // A stored Python query on a server without Python is still shown as one.
    expect(languageOptions("python", false)).toEqual(["sql", "javascript", "python"]);
  });

  it("counts a Python runtime that has not started yet as available", () => {
    expect(pythonAvailable("running")).toBe(true);
    expect(pythonAvailable("not_initialised")).toBe(true);
    for (const state of ["not_built", "off", "unavailable", undefined]) {
      expect(pythonAvailable(state)).toBe(false);
    }
  });

  it("reports a checked code query without claiming columns", () => {
    expect(statusSummary({ kind: "checked" })).toMatch(/whatever its body returns/);
  });
});

describe("the note on a code query with no parameters (TODO F1.1)", () => {
  it("is shown for a code query that declares none, and only then", () => {
    const row = { ...blankQueryRow(), language: "javascript" as const, params: [] };
    expect(needsParamsNote(row)).toBe(true);
    expect(needsParamsNote({ ...row, params: [{ name: "q", type: "text", required: true }] })).toBe(
      false,
    );
    // SQL declares exactly what it uses, so the note would be noise there.
    expect(needsParamsNote({ ...row, language: "sql" })).toBe(false);
  });
});
