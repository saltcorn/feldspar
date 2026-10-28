/**
 * The Checklists screens' rules: status tones, authors, what is pending, step
 * order, proposals, and the step form built from a type's `params` spec.
 */

import { describe, expect, it } from "vitest";

import {
  attributesWith,
  authorLabel,
  buildParams,
  checklistPath,
  checklistRunPath,
  checklistsPath,
  controlKind,
  initialValues,
  lastResultFor,
  moveStep,
  orderedSteps,
  outcomeTone,
  parseAuthor,
  pendingCount,
  pendingSteps,
  proposalSummary,
  proposedStep,
  runTone,
  selectOptions,
  settingsFrom,
  type ChecklistRun,
  type ChecklistStep,
  type ParamSpec,
} from "./checklists";

const HUMAN = "11111111-1111-1111-1111-111111111111";
const users = [{ id: HUMAN, email: "ann@example.com" }];

function step(id: string, position: number, author: string, accepted: boolean): ChecklistStep {
  return {
    id,
    checklist: "c1",
    position,
    type: "data.expect_rows",
    params: {},
    label: `step ${id}`,
    as: null,
    author,
    accepted,
    superseded_by: null,
    deleted: false,
    created_at: "2026-10-01T00:00:00Z",
  };
}

function spec(over: Partial<ParamSpec>): ParamSpec {
  return {
    name: "p",
    label: "P",
    type: "Text",
    required: false,
    default: null,
    options: [],
    multiline: false,
    sublabel: null,
    ...over,
  };
}

describe("status tones", () => {
  it("never shows an incomplete run as green", () => {
    expect(runTone("passed")).toBe("green");
    expect(runTone("failed")).toBe("red");
    expect(runTone("error")).toBe("red");
    expect(runTone("incomplete")).toBe("yellow");
    expect(runTone(null)).toBe("secondary");
    expect(outcomeTone("pass")).toBe("green");
    expect(outcomeTone("fail")).toBe("red");
    expect(outcomeTone("skipped")).toBe("yellow");
  });
});

describe("authors", () => {
  it("names a person by email and an agent by its run", () => {
    expect(parseAuthor(`human:${HUMAN}`)).toEqual({ kind: "human", id: HUMAN });
    expect(authorLabel(`human:${HUMAN}`, users).label).toBe("ann@example.com");
    const agent = authorLabel("agent:abcdef0123456789", users);
    expect(agent.kind).toBe("agent");
    expect(agent.label).toBe("abcdef01");
    // A person since deleted is still somebody: the id.
    expect(authorLabel("human:gone", users).label).toBe("gone");
  });
});

describe("what is pending", () => {
  it("is an agent's unaccepted steps and the proposals", () => {
    const steps = [
      step("a", 0, `human:${HUMAN}`, true),
      step("b", 1, "agent:r1", false),
      step("c", 2, "agent:r1", true),
    ];
    expect(pendingSteps(steps).map((s) => s.id)).toEqual(["b"]);
    const proposals = [
      { id: "p", checklist: "c1", kind: "reorder", change: {}, reason: "", author: "agent:r1", created_at: "" },
    ];
    expect(pendingCount({ steps, proposals })).toBe(2);
  });
});

describe("step order", () => {
  it("sorts by position and moves a step within the ends", () => {
    const steps = [step("b", 2, "x", true), step("a", 1, "x", true)];
    expect(orderedSteps(steps).map((s) => s.id)).toEqual(["a", "b"]);
    expect(moveStep(["a", "b", "c"], 2, -1)).toEqual(["a", "c", "b"]);
    expect(moveStep(["a", "b", "c"], 0, 1)).toEqual(["b", "a", "c"]);
    expect(moveStep(["a", "b", "c"], 0, -1)).toBeNull();
    expect(moveStep(["a", "b", "c"], 2, 1)).toBeNull();
  });
});

describe("proposals", () => {
  const steps = [step("a", 0, "x", true), step("b", 1, "x", true)];

  it("says which step an edit is about, and whom a run_as names", () => {
    expect(proposalSummary({ kind: "edit", step: "b", change: {} }, steps, users)).toEqual({
      kind: "edit",
      step: 2,
    });
    expect(
      proposalSummary({ kind: "run_as", step: null, change: { run_as: HUMAN } }, steps, users),
    ).toEqual({ kind: "run_as", user: "ann@example.com" });
    expect(
      proposalSummary({ kind: "run_as", step: null, change: { run_as: null } }, steps, users),
    ).toEqual({ kind: "run_as", user: null });
  });

  it("reads the step an edit would write", () => {
    const change = { step: { type: "browser.click", params: { selector: "#save" }, label: "Save" } };
    expect(proposedStep({ change })).toEqual({
      type: "browser.click",
      params: { selector: "#save" },
      label: "Save",
      as: null,
    });
    expect(proposedStep({ change: { order: ["a"] } })).toBeNull();
  });

  it("finds the newest run's result for the step", () => {
    const result = {
      step: "b",
      type: "t",
      label: "",
      outcome: "fail",
      message: "no rows",
      duration_ms: 3,
      console_errors: [],
      failed_requests: [],
    };
    const run = { steps: [result] } as unknown as ChecklistRun;
    expect(lastResultFor("b", [run])).toBe(result);
    expect(lastResultFor("a", [run])).toBeNull();
    expect(lastResultFor("b", [])).toBeNull();
  });
});

describe("the step form", () => {
  it("picks a control per parameter type", () => {
    expect(controlKind(spec({ options: ["a"] }))).toBe("select");
    expect(controlKind(spec({ type: "Bool" }))).toBe("checkbox");
    expect(controlKind(spec({ type: "Int" }))).toBe("number");
    expect(controlKind(spec({ type: "Json" }))).toBe("json");
    expect(controlKind(spec({ multiline: true }))).toBe("textarea");
    expect(controlKind(spec({}))).toBe("text");
  });

  it("fills the boxes from params, then from defaults", () => {
    const specs = [
      spec({ name: "table" }),
      spec({ name: "count", type: "Int", default: 1 }),
      spec({ name: "where", type: "Json" }),
      spec({ name: "exact", type: "Bool" }),
    ];
    expect(initialValues(specs, { table: "orders", where: { id: 3 } })).toEqual({
      table: "orders",
      count: "1",
      where: JSON.stringify({ id: 3 }, null, 2),
      exact: false,
    });
  });

  it("leaves empty boxes out and converts the rest", () => {
    const specs = [
      spec({ name: "table", required: true }),
      spec({ name: "note" }),
      spec({ name: "count", type: "Int" }),
      spec({ name: "where", type: "Json" }),
      spec({ name: "exact", type: "Bool" }),
      spec({ name: "mode", options: [{ label: "Any", value: "any" }, 2] }),
    ];
    const built = buildParams(specs, {
      table: "orders",
      note: "  ",
      count: "3",
      where: '{"id": 3}',
      exact: true,
      mode: JSON.stringify(2),
    });
    expect(built).toEqual({
      ok: true,
      params: { table: "orders", count: 3, where: { id: 3 }, exact: true, mode: 2 },
    });
    expect(selectOptions(specs[5]).map((o) => o.label)).toEqual(["Any", "2"]);
  });

  it("reports a missing required box, a bad number and bad JSON", () => {
    expect(buildParams([spec({ name: "t", label: "Table", required: true })], { t: "" })).toEqual({
      ok: false,
      problem: "required",
      label: "Table",
    });
    expect(buildParams([spec({ type: "Int" })], { p: "1.5" })).toMatchObject({
      ok: false,
      problem: "number",
    });
    const json = buildParams([spec({ type: "Json" })], { p: "{nope" });
    expect(json).toMatchObject({ ok: false, problem: "json", label: "P" });
    expect(json.ok === false && json.detail).toBeTruthy();
  });
});

describe("checklist settings", () => {
  it("defaults stop_on_failure on and keeps other attributes", () => {
    const settings = settingsFrom({ tags: ["x"] });
    expect(settings).toEqual({
      stopOnFailure: true,
      implicitClean: true,
      timeout: "",
      allowOnProduction: false,
    });
    expect(
      attributesWith({ tags: ["x"], timeout: 5 }, { ...settings, allowOnProduction: true }),
    ).toEqual({
      tags: ["x"],
      stop_on_failure: true,
      implicit_clean: true,
      allow_on_production: true,
    });
    expect(settingsFrom({ implicit_clean: false }).implicitClean).toBe(false);
    expect(attributesWith({}, { ...settings, timeout: "30" })).toMatchObject({ timeout: 30 });
    expect(attributesWith({}, { ...settings, timeout: "-1" })).toBeNull();
    expect(settingsFrom({ stop_on_failure: false, timeout: 9 })).toMatchObject({
      stopOnFailure: false,
      timeout: "9",
    });
  });
});

describe("where things are", () => {
  it("keeps an application's checklists under the application", () => {
    expect(checklistsPath("a1")).toBe("/applications/a1/checklists");
    expect(checklistPath("c1", "a1")).toBe("/applications/a1/checklists/c1");
    expect(checklistRunPath("r1", "a1")).toBe("/applications/a1/checklists/runs/r1");
  });

  it("puts a data-only checklist under the Data Layer", () => {
    expect(checklistsPath(null)).toBe("/checklists");
    expect(checklistPath("c1", null)).toBe("/checklists/c1");
    expect(checklistRunPath("r1")).toBe("/checklist-runs/r1");
  });
});
