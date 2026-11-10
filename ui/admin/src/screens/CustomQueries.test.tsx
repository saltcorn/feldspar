// The custom queries card: a table of the queries, each row opening that
// query's editor.

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { blankQueryRow, type QueryRow } from "../customQueries";
import { CustomQueries, QueryEditor, QueryList } from "./CustomQueries";

function query(extra: Partial<QueryRow>): QueryRow {
  return { ...blankQueryRow(), ...extra };
}

const queries = [
  query({
    name: "logSet",
    description: "Log one set.",
    method: "POST",
    path: "/sets",
    language: "javascript",
    minRole: 80,
    params: [
      { name: "machine", type: "int", required: true },
      { name: "weight", type: "float", required: false },
    ],
  }),
  query({ name: "machines", path: "/machines", minRole: 100 }),
];

const roles = [
  { role: 1, name: "admin" },
  { role: 80, name: "user" },
  { role: 100, name: "public" },
] as never;

describe("the custom queries card", () => {
  it("lists the queries, not every query's editor", () => {
    const html = renderToStaticMarkup(
      <CustomQueries queries={queries} tables={[]} idPrefix="api-0" onChange={() => {}} />,
    );
    expect(html).toContain("logSet");
    expect(html).toMatch(/POST<\/span>.*\/sets/);
    expect(html).toContain("Add query");
    // No editor is open: no code box, no Check button.
    expect(html).not.toContain('id="api-0-q0-name"');
    expect(html).not.toContain(">Check<");
  });

  it("shows each query's endpoint, language, role and parameters on its row", () => {
    const html = renderToStaticMarkup(
      <QueryList queries={queries} roles={roles} status={{}} onOpen={() => {}} />,
    );
    expect(html).toContain("Log one set.");
    // The method is a badge coloured by what it does, beside the path.
    expect(html).toMatch(/bg-blue-lt[^>]*>POST<\/span>/);
    expect(html).toMatch(/bg-green-lt[^>]*>GET<\/span>.*\/machines/);
    expect(html).toMatch(/bg-purple-lt[^>]*>JavaScript</);
    expect(html).toMatch(/>user</);
    expect(html).toContain("Public");
    // A parameter each, the optional one marked.
    expect(html).toMatch(/>machine</);
    expect(html).toMatch(/>weight\?</);
  });

  it("puts a failed check on the query's row", () => {
    const html = renderToStaticMarkup(
      <QueryList
        queries={queries}
        roles={roles}
        status={{ 1: { kind: "error", message: "column `titel` does not exist" } }}
        onOpen={() => {}}
      />,
    );
    expect(html).toContain("column `titel` does not exist");
  });

  it("says when there is none", () => {
    const html = renderToStaticMarkup(
      <QueryList queries={[]} roles={roles} status={{}} onOpen={() => {}} />,
    );
    expect(html).toContain("None.");
  });
});

describe("a query's editor", () => {
  const sql = query({
    name: "topAuthors",
    method: "DELETE",
    path: "/authors",
    code: "select 1",
    params: [{ name: "since", type: "int", required: false }],
  });
  const html = renderToStaticMarkup(
    <QueryEditor
      query={sql}
      id="api-0-q0"
      python={false}
      roles={roles}
      status={{ kind: "idle" }}
      onChange={() => {}}
      onCheck={() => {}}
      onResetCheck={() => {}}
      onRemove={() => {}}
    />,
  );

  it("puts the method and path on one request line, the method coloured", () => {
    expect(html).toMatch(/query-request-line.*<select class="[^"]*text-red[^"]*" id="api-0-q0-method".*id="api-0-q0-path"/s);
  });

  it("frames the code and the parameters, a row per parameter with a switch", () => {
    expect(html).toContain('id="api-0-q0-code"');
    expect(html).toMatch(/query-params.*value="since"/s);
    expect(html).toMatch(/form-switch[^>]*>.*id="api-0-q0-p0-req"/s);
  });

  it("keeps how it is served in a settings panel beside it", () => {
    expect(html).toMatch(/query-settings.*id="api-0-q0-language".*id="api-0-q0-role".*id="api-0-q0-rate"/s);
    expect(html).toContain("Remove query");
  });
});
