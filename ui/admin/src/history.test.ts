// Row history's words and filters (TODO-history H2.3).

import { describe, expect, it } from "vitest";

import {
  EMPTY_CHANGESET_FILTER,
  canRevert,
  changesetQuery,
  conflictPlace,
  dropTableQuestion,
  droppedNote,
  historyValue,
  opLabel,
  rowSummary,
  sourceLabel,
  whoLabel,
  type ChangesetInfo,
  type HistoryEntry,
} from "./history";

const changeset = (over: Partial<ChangesetInfo>): ChangesetInfo => ({
  id: "c1",
  at: "2026-10-22T09:00:00Z",
  source: "request",
  ...over,
});

describe("row history", () => {
  it("says what an entry did and where it came from", () => {
    expect(["i", "u", "d"].map(opLabel)).toEqual(["Created", "Changed", "Deleted"]);
    expect(sourceLabel("sql")).toBe("SQL");
    expect(sourceLabel("something new")).toBe("something new");
  });

  it("names who by email, then id, and says when nobody did", () => {
    expect(whoLabel(changeset({ user_id: "u1", user_email: "ann@example.com" }))).toBe(
      "ann@example.com",
    );
    expect(whoLabel(changeset({ user_id: "u1" }))).toBe("u1");
    expect(whoLabel(changeset({ source: "sql" }))).toBe("Outside Saltcorn");
    expect(whoLabel(changeset({}))).toBe("No user");
  });

  it("shows a value as text, and null as nothing", () => {
    expect(historyValue(null)).toBe("");
    expect(historyValue("milk")).toBe("milk");
    expect(historyValue(3)).toBe("3");
    expect(historyValue({ a: 1 })).toBe('{"a":1}');
  });

  it("summarises a row without its key or its empty fields", () => {
    expect(rowSummary({ id: 4, title: "milk", done: false, note: null }, "id")).toBe(
      "title: milk, done: false",
    );
    expect(rowSummary(null)).toBe("");
  });

  it("sends only the filters that are filled in", () => {
    const page = { limit: 50, offset: 0 };
    expect(changesetQuery(EMPTY_CHANGESET_FILTER, page)).toEqual(page);
    const query = changesetQuery(
      { user: " u1 ", table: "todo", source: "sql", since: "2026-10-01" },
      page,
    );
    expect(query.user).toBe("u1");
    expect(query.table).toBe("todo");
    expect(query.source).toBe("sql");
    // The start of the day where the admin is, as an instant.
    expect(new Date(query.since as string).getTime()).toBe(
      new Date("2026-10-01T00:00:00").getTime(),
    );
  });

  it("offers a revert to an older version that is not a deletion", () => {
    const entry = (seq: number, op: string): HistoryEntry => ({
      seq,
      version: seq,
      table: "t",
      key: "1",
      op,
      at: "2026-10-22T09:00:00Z",
      changes: [],
      changeset: changeset({}),
    });
    const newest = entry(3, "u");
    expect(canRevert(entry(1, "i"), newest)).toBe(true);
    expect(canRevert(newest, newest)).toBe(false);
    expect(canRevert(entry(2, "d"), newest)).toBe(false);
  });

  it("says which dropped fields were left out, and where a conflict is", () => {
    expect(droppedNote([])).toBeNull();
    expect(droppedNote(["budget"])).toContain("field budget no longer exists");
    expect(droppedNote(["a", "b"])).toContain("fields a, b no longer exist");
    const conflict = { table: "t", key: "1", expected: 1, actual: 2, reason: "r" };
    expect(conflictPlace({ ...conflict, field: "x" })).toBe("t 1 · x");
    expect(conflictPlace({ ...conflict, field: null })).toBe("t 1");
  });
});

describe("dropTableQuestion", () => {
  it("says the row history goes too when the table records its changes", () => {
    expect(dropTableQuestion("notes", true)).toContain("row history is deleted too");
    expect(dropTableQuestion("notes", false)).not.toContain("history");
    expect(dropTableQuestion("notes", false)).toContain('"notes"');
  });
});
