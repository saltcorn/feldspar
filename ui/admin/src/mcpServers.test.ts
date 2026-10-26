/**
 * The MCP servers screen's form (TODO-agent-skills AS4.3): what a save sends,
 * and the name rule shown before saving.
 */

import { describe, expect, it } from "vitest";

import { bodyFromForm, emptyForm, formFromServer, nameProblem, toolSummary } from "./mcpServers";

describe("the MCP server form", () => {
  it("sends the headers it has, the sentinel included, and a blank timeout as the default", () => {
    const form = formFromServer({
      id: "s1",
      name: "docs",
      description: "",
      url: "https://mcp.example.com/mcp",
      transport: "sse",
      headers: [{ name: "Authorization", value: "••••••••" }],
      allow_private_network: false,
      timeout_secs: 0,
    });
    form.headers.push({ name: "  ", value: "dropped" });
    expect(bodyFromForm(form)).toEqual({
      name: "docs",
      description: "",
      url: "https://mcp.example.com/mcp",
      transport: "sse",
      headers: [{ name: "Authorization", value: "••••••••" }],
      allow_private_network: false,
      timeout_secs: 0,
    });
    expect(bodyFromForm({ ...emptyForm(), timeoutSecs: "30" }).timeout_secs).toBe(30);
    // A blank Authorization row is dropped only if its name is blank.
    expect(bodyFromForm(emptyForm()).headers).toEqual([{ name: "Authorization", value: "" }]);
  });

  it("says why a name will be refused", () => {
    expect(nameProblem("docs")).toBeNull();
    expect(nameProblem("")).toMatch(/required/);
    expect(nameProblem("Docs")).toMatch(/Lowercase/);
    expect(nameProblem("a__b")).toMatch(/double underscore/);
    expect(nameProblem("a".repeat(25))).toMatch(/24/);
  });

  it("marks the read-only tools", () => {
    expect(
      toolSummary([
        { name: "search", description: "", input_schema: {}, read_only: true },
        { name: "write", description: "", input_schema: {}, read_only: false },
      ]),
    ).toBe("search (read-only), write");
    expect(toolSummary([])).toBe("no tools");
  });
});
