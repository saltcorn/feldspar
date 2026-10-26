/**
 * The Test run button's pure part.
 *
 * What is pinned here is what an admin would otherwise be misled by:
 *
 *   - a **failing** action is a 200 with `ok: false`, and it has to read as a
 *     failure rather than as a run that returned nothing;
 *   - the **console lines survive the failure** — they are the reason an admin
 *     pressed the button a second time with a `console.log` added;
 *   - and the **row** a table trigger was given is named, including when there
 *     was none to give, because a result computed from a row nobody can
 *     identify is a result nobody can check.
 */

import { describe, expect, it } from "vitest";

import {
  isTableEvent,
  rowSummary,
  testRunFailure,
  testRunOutcome,
} from "./triggerTestRun";

describe("a test run that succeeded", () => {
  it("shows the result as JSON and says which row it ran on", () => {
    const out = testRunOutcome(
      { name: "archive done", when: "update" },
      {
        ok: true,
        result: { archived: 3 },
        error: null,
        console: [],
        row: { id: 7, title: "Milk" },
      },
    );
    expect(out.ok).toBe(true);
    expect(out.title).toBe("archive done ran");
    expect(out.text).toBe('{\n  "archived": 3\n}');
    expect(out.row).toBe("id 7");
    expect(out.console).toEqual([]);
  });

  it("says nothing about a row for an event that never had one", () => {
    const out = testRunOutcome(
      { name: "nightly", when: "daily" },
      { ok: true, result: null, error: null, console: [], row: null },
    );
    expect(out.row).toBeNull();
    expect(out.text).toBe("null");
  });

  it("says so when the table a trigger fires on is empty", () => {
    const out = testRunOutcome(
      { name: "on insert", when: "insert" },
      { ok: true, result: null, error: null, console: [], row: null },
    );
    expect(out.row).toBe("no row — the table is empty");
  });
});

describe("a test run that failed", () => {
  it("reads as a failure and keeps what the body printed", () => {
    const out = testRunOutcome(
      { name: "chase invoices", when: "none" },
      {
        ok: false,
        result: null,
        error: "trigger `chase invoices`: `code`: JavaScript code failed: boom",
        console: [
          { level: "log", text: "starting" },
          { level: "error", text: "no rate for GBP" },
        ],
        row: null,
      },
    );
    expect(out.ok).toBe(false);
    expect(out.title).toBe("chase invoices failed");
    expect(out.text).toContain("boom");
    expect(out.console.map((l) => l.level)).toEqual(["log", "error"]);
  });

  it("says something even when the failure said nothing", () => {
    const out = testRunOutcome(
      { name: "quiet", when: "none" },
      { ok: false, result: null, error: null, console: [], row: null },
    );
    expect(out.text).toMatch(/failed/);
  });

  it("shows a request that never reached the action the same way", () => {
    const out = testRunFailure({ name: "gone" }, "no trigger with id 42");
    expect(out.ok).toBe(false);
    expect(out.title).toBe("gone could not be run");
    expect(out.text).toBe("no trigger with id 42");
    expect(out.console).toEqual([]);
  });
});

describe("the row summary", () => {
  it("prefers the id, falls back to the row, and bounds what it shows", () => {
    expect(rowSummary({ id: "a-b-c", n: 1 })).toBe("id a-b-c");
    expect(rowSummary({ name: "Ada" })).toBe('{"name":"Ada"}');
    expect(rowSummary(null)).toBeNull();
    const long = rowSummary({ note: "x".repeat(500) });
    expect(long?.length).toBe(121);
    expect(long?.endsWith("…")).toBe(true);
  });
});

describe("which events carry a row", () => {
  it("is the table events and nothing else", () => {
    for (const when of ["insert", "update", "insert_or_update", "delete"]) {
      expect(isTableEvent(when)).toBe(true);
    }
    for (const when of ["none", "login", "daily", "stream", "error"]) {
      expect(isTableEvent(when)).toBe(false);
    }
  });
});
