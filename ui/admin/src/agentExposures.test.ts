/**
 * The application form's exposed agents (§11.7): the drop-downs an admin picks
 * from become the record's flags, and a stored exposure comes back as the same
 * picks.
 */

import { describe, expect, it } from "vitest";

import {
  agentOptions,
  agentRowsFromApp,
  agentRowsToRequest,
  blankAgentRow,
} from "./agentExposures";

describe("agentRowsToRequest", () => {
  it("turns the picks into the record's flags and drops an empty row", () => {
    const rows = [
      { ...blankAgentRow(), name: " helpdesk " },
      {
        name: "greeter",
        use: "prompt" as const,
        anonymous: "yes" as const,
        tools: "none" as const,
        prompt_template: "Greet {{name}}",
      },
      blankAgentRow(),
    ];
    expect(agentRowsToRequest(rows)).toEqual([
      {
        name: "helpdesk",
        chat: true,
        prompt: true,
        anonymous: false,
        tools: "names",
        prompt_template: null,
      },
      {
        name: "greeter",
        chat: false,
        prompt: true,
        anonymous: true,
        tools: "none",
        prompt_template: "Greet {{name}}",
      },
    ]);
  });

  it("round-trips through the stored shape", () => {
    const rows = [
      { ...blankAgentRow(), name: "a", use: "chat" as const, tools: "full" as const },
      { ...blankAgentRow(), name: "b", use: "prompt" as const, prompt_template: "x {{y}}" },
    ];
    expect(agentRowsFromApp(agentRowsToRequest(rows))).toEqual(rows);
  });
});

describe("agentOptions", () => {
  it("offers every agent, and keeps a stored name that is gone", () => {
    expect(agentOptions(["a", "b"], "")).toEqual([
      { value: "a", known: true },
      { value: "b", known: true },
    ]);
    expect(agentOptions(["a"], "deleted")).toEqual([
      { value: "a", known: true },
      { value: "deleted", known: false },
    ]);
  });
});
