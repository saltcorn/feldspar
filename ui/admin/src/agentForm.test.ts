/**
 * The agent form's attributes: roles and budgets saved sparsely, keys the form
 * does not show kept, and the `coding` trait's shell settings grouped and
 * warned about.
 */

import { describe, expect, it } from "vitest";

import {
  AGENT_TABS,
  ANSWER_SETTINGS,
  CREDITS,
  LIMITS,
  agentAttributes,
  agentTab,
  agentTabHref,
  chargesCredits,
  missingRequired,
  readEffort,
  readNumbers,
  readRoles,
  refusalTab,
  shellWarning,
  splitShellSettings,
  traitNeedsAttention,
  traitSummary,
} from "./agentForm";
import type { FieldSpec } from "./settings";

function field(name: string, type = "text"): FieldSpec {
  return { name, label: name, type, required: false, options: [], multiline: false };
}

describe("the agent's attributes", () => {
  const stored = {
    temperature: 0.2,
    max_cost: 1.5,
    strong: { provider: "house", model: "claude-opus-5" },
    max_identical_calls: 4,
  };

  it("reads the roles and the numbers the form shows", () => {
    expect(readRoles(stored)).toEqual({
      strong: { provider: "house", model: "claude-opus-5" },
      cheap: { provider: "", model: "" },
    });
    expect(readNumbers(stored)).toEqual({ temperature: "0.2", max_cost: "1.5" });
    expect(readRoles(null).strong).toEqual({ provider: "", model: "" });
  });

  it("keeps what the form does not show, and saves only what was set", () => {
    const saved = agentAttributes(
      stored,
      { temperature: "", max_cost: "2", max_wall_seconds: " 600 " },
      { strong: { provider: "house", model: "" }, cheap: { provider: "", model: "x" } },
    );
    expect(saved).toEqual({
      // A loop-control limit this form has no box for survives the save.
      max_identical_calls: 4,
      max_cost: 2,
      max_wall_seconds: 600,
      // A role with no model is the provider's default model; a role with no
      // provider is no role, whatever its model box says.
      strong: { provider: "house" },
    });
    // A cleared box removes the attribute rather than saving a zero.
    expect("temperature" in saved).toBe(false);
  });
  it("reads and saves the LLM credits rates like any other number", () => {
    const metered = { credits_per_dollar: 20, credits_min_balance: 1 };
    expect(readNumbers(metered)).toEqual({ credits_per_dollar: "20", credits_min_balance: "1" });
    const saved = agentAttributes(
      metered,
      { credits_per_dollar: "", credits_per_1k_output: "0.5", credits_min_balance: "1" },
      { strong: { provider: "", model: "" }, cheap: { provider: "", model: "" } },
    );
    expect(saved).toEqual({ credits_per_1k_output: 0.5, credits_min_balance: 1 });
  });
  it("reads and saves the reasoning effort, and blank removes it", () => {
    const none = { strong: { provider: "", model: "" }, cheap: { provider: "", model: "" } };
    expect(readEffort({ reasoning_effort: "low" })).toBe("low");
    expect(readEffort({})).toBe("");
    expect(agentAttributes({}, {}, none, "none")).toEqual({ reasoning_effort: "none" });
    expect(agentAttributes({ reasoning_effort: "low" }, {}, none, "")).toEqual({});
    // A caller that does not edit it keeps what was stored.
    expect(agentAttributes({ reasoning_effort: "low" }, {}, none)).toEqual({ reasoning_effort: "low" });
  });
});

describe("the coding trait's shell settings", () => {
  const spec = [
    field("store"),
    field("may_edit", "bool"),
    field("may_use_shell", "bool"),
    field("shell_timeout", "int"),
    field("shell_sandbox"),
  ];

  it("are grouped: the grant and everything declared after it", () => {
    const { own, shell } = splitShellSettings("coding", spec);
    expect(own.map((f) => f.name)).toEqual(["store", "may_edit"]);
    expect(shell.map((f) => f.name)).toEqual(["may_use_shell", "shell_timeout", "shell_sandbox"]);
    // Another trait's settings are its own, even one that happens to share a name.
    expect(splitShellSettings("subagent", spec).shell).toEqual([]);
  });

  it("warn when the shell is on with no sandbox, including the untouched default", () => {
    expect(shellWarning("coding", { may_use_shell: "true" })).toContain("no sandbox");
    expect(shellWarning("coding", { may_use_shell: "true", shell_sandbox: "none" })).not.toBeNull();
    expect(shellWarning("coding", { may_use_shell: "true", shell_sandbox: "container" })).toBeNull();
    expect(shellWarning("coding", { may_use_shell: "false" })).toBeNull();
    expect(shellWarning("other", { may_use_shell: "true" })).toBeNull();
  });
});

describe("the form's groups", () => {
  it("shows each numeric attribute in exactly one group, the step limit with the limits", () => {
    const keys = [...ANSWER_SETTINGS, ...LIMITS, ...CREDITS].map((a) => a.key);
    expect(new Set(keys).size).toBe(keys.length);
    expect(LIMITS.map((a) => a.key)).toContain("max_steps");
    expect(ANSWER_SETTINGS.map((a) => a.key)).toEqual(["temperature", "max_tokens"]);
  });

  it("opens the credits only for an agent that charges", () => {
    expect(chargesCredits({})).toBe(false);
    expect(chargesCredits({ max_steps: "40", credits_per_dollar: " " })).toBe(false);
    expect(chargesCredits({ credits_min_balance: "5" })).toBe(true);
  });
});

describe("the form's tabs", () => {
  it("opens the tab the URL names, and the first for none or an unknown one", () => {
    expect(agentTab("traits")).toBe("traits");
    expect(agentTab(undefined)).toBe("general");
    expect(agentTab("chat")).toBe("general");
    expect(AGENT_TABS[0].id).toBe("general");
  });

  it("links each tab, the first being the form's own URL", () => {
    expect(agentTabHref("a b", "general")).toBe("#/agents/a%20b/edit");
    expect(agentTabHref("a b", "limits")).toBe("#/agents/a%20b/edit/limits");
    expect(agentTabHref(undefined, "general")).toBe("#/agents/new");
    expect(agentTabHref(undefined, "traits")).toBe("#/agents/new/traits");
  });

  it("names what is missing before a save, on the tab that has it", () => {
    expect(missingRequired("helper", "house")).toBeNull();
    expect(missingRequired("  ", "house")?.tab).toBe("general");
    expect(missingRequired("helper", "")?.message).toContain("LLM provider");
  });

  it("sends a refusal about a trait or a tool to the traits tab, and others nowhere", () => {
    expect(refusalTab("its tool `query_books` has the same name as one from trait 1")).toBe("traits");
    expect(refusalTab("no LLM provider named `house`")).toBeNull();
  });
});

describe("a folded trait card", () => {
  const spec: FieldSpec[] = [
    { ...field("table"), label: "Table (the one it reads)" },
    { ...field("writable", "bool"), label: "Writable" },
    { ...field("read_only", "bool"), label: "Read only" },
    { ...field("api_key"), label: "API key", secret: true },
    { ...field("filter", "json"), label: "Filter" },
    { ...field("notes"), label: "Notes", multiline: true },
  ];

  it("sums up the values that are set, by their labels", () => {
    expect(
      traitSummary(spec, {
        table: "books",
        writable: "true",
        read_only: "false",
        api_key: "sk-secret",
        filter: '{"a":1}',
        notes: "",
      }),
    ).toBe("Table: books · Writable");
  });

  it("cuts long values and keeps the first line of many", () => {
    const summary = traitSummary(spec, { table: "x".repeat(60), notes: "one\ntwo" });
    expect(summary).toBe(`Table: ${"x".repeat(39)}… · Notes: one…`);
  });

  it("is empty for a trait with nothing set", () => {
    expect(traitSummary(spec, {})).toBe("");
  });

  it("opens for a required setting with no value, or a shell with no sandbox", () => {
    const required: FieldSpec[] = [{ ...field("table"), required: true }];
    expect(traitNeedsAttention("query_table", required, {})).toBe(true);
    expect(traitNeedsAttention("query_table", required, { table: "books" })).toBe(false);
    expect(traitNeedsAttention("coding", [], { may_use_shell: "true" })).toBe(true);
  });
});
