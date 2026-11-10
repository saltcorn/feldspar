// The registry editor's model: the tree, the filter, a new entry's path, and
// the save review's headings, ticks and typed confirmation.

import { describe, expect, it } from "vitest";

import {
  NEW_KINDS,
  canConfirm,
  changeLine,
  confirmationWord,
  defaultTicks,
  groupEntries,
  groupOps,
  instanceOnlyKeys,
  newEntryPath,
  parseDraft,
  pathSegment,
  tickable,
  wholeInstanceOnly,
  type PlannedOp,
  type RegistryEntry,
} from "./registry";

const t = (s: string) => s;

const entry = (over: Partial<RegistryEntry>): RegistryEntry => ({
  path: "x.json",
  kind: "table",
  name: "x",
  application: null,
  instance_only: null,
  version: "v",
  ...over,
});

const ENTRIES: RegistryEntry[] = [
  entry({ path: "tables/tickets.json", kind: "table", name: "tickets" }),
  entry({
    path: "applications/desk/views/list.json",
    kind: "view",
    name: "list",
    application: "desk",
  }),
  entry({
    path: "settings/email.json",
    kind: "setting",
    name: "email",
    instance_only: { scope: "whole" },
  }),
  entry({ path: "settings/localisation.json", kind: "setting", name: "localisation" }),
  entry({
    path: "build-hosts/mac.json",
    kind: "build_host",
    name: "mac",
    instance_only: { scope: "whole" },
  }),
  entry({
    path: "file-stores/code.json",
    kind: "file_store",
    name: "code",
    instance_only: { scope: "keys", keys: ["/config/path"] },
  }),
];

describe("the registry tree", () => {
  it("groups by kind, this instance's own first, then in deploy order", () => {
    const groups = groupEntries(ENTRIES, "", t);
    expect(groups.map((g) => g.kind)).toEqual(["setting", "build_host", "table", "file_store", "view"]);
    // A heading is marked when every entry under it is this instance's.
    expect(groups.find((g) => g.kind === "build_host")?.instanceOnly).toBe(true);
    expect(groups.find((g) => g.kind === "setting")?.instanceOnly).toBe(false);
  });

  it("filters by name, application, path or heading", () => {
    const kinds = (filter: string) => groupEntries(ENTRIES, filter, t).map((g) => g.kind);
    expect(kinds("TICK")).toEqual(["table"]);
    expect(kinds("desk")).toEqual(["view"]);
    expect(kinds("build hosts")).toEqual(["build_host"]);
    expect(kinds("nothing like it")).toEqual([]);
  });

  it("tells whole instance-only files from ones with instance-only values", () => {
    const [email, store] = [ENTRIES[2], ENTRIES[5]];
    expect(wholeInstanceOnly(email)).toBe(true);
    expect(instanceOnlyKeys(email)).toEqual([]);
    expect(wholeInstanceOnly(store)).toBe(false);
    expect(instanceOnlyKeys(store)).toEqual(["/config/path"]);
  });
});

describe("a new entry's path", () => {
  const kind = (k: string) => NEW_KINDS.find((n) => n.kind === k)!;

  it("is the kind's directory and the name, written as the server writes it", () => {
    expect(newEntryPath(kind("table"), "tickets", "")).toBe("tables/tickets.json");
    expect(newEntryPath(kind("view"), "list", "desk")).toBe("applications/desk/views/list.json");
    expect(newEntryPath(kind("application"), "desk", "")).toBe("applications/desk/application.json");
    expect(newEntryPath(kind("dataset"), "q1", "")).toBe("analytics/datasets/q1.json");
  });

  it("waits for a name, and for an application for a kind inside one", () => {
    expect(newEntryPath(kind("table"), "  ", "")).toBeNull();
    expect(newEntryPath(kind("page"), "home", "")).toBeNull();
  });

  it("escapes what a file name cannot hold, as `path_segment` does", () => {
    expect(pathSegment("a/b")).toBe("a%2Fb");
    expect(pathSegment(".hidden")).toBe("%2Ehidden");
    expect(pathSegment("50%")).toBe("50%25");
    expect(pathSegment("café")).toBe("café");
    expect(pathSegment("")).toBe("%00");
  });
});

describe("the editor's text", () => {
  it("parses, or says why not", () => {
    expect(parseDraft('{"a": 1}')).toEqual({ ok: true, value: { a: 1 } });
    const bad = parseDraft("{ a: 1 }");
    expect(bad.ok).toBe(false);
  });
});

describe("the save review", () => {
  const op = (id: string, cls: string, ticked: boolean): PlannedOp => ({
    id,
    step: "tables",
    class: cls,
    ticked,
    kind: "table",
    entity: "table:tickets",
    action: "x",
    summary: id,
  });
  const ops = [
    op("update:table:tickets", "safe", true),
    op("drop_field:table:tickets#title", "data_destructive", false),
    op("delete:view:desk/edit", "metadata_delete", true),
  ];

  it("lists what loses data first and leaves out empty headings", () => {
    expect(groupOps(ops).map((g) => g.cls)).toEqual(["data_destructive", "metadata_delete", "safe"]);
  });

  it("offers each operation with its default tick", () => {
    expect([...defaultTicks(ops)].sort()).toEqual(["delete:view:desk/edit", "update:table:tickets"]);
    expect(tickable(op("x", "unsupported", false))).toBe(false);
  });

  it("asks for the entry's name only when something ticked loses data", () => {
    const plan = { operations: ops, blockers: [] };
    const ticks = defaultTicks(ops);
    expect(confirmationWord(ops, ticks, "tickets")).toBeNull();
    expect(canConfirm(plan, ticks, "", "tickets")).toBe(true);

    ticks.add("drop_field:table:tickets#title");
    expect(confirmationWord(ops, ticks, "tickets")).toBe("tickets");
    expect(canConfirm(plan, ticks, "ticket", "tickets")).toBe(false);
    expect(canConfirm(plan, ticks, " tickets ", "tickets")).toBe(true);
  });

  it("is not confirmed while anything blocks it", () => {
    const plan = { operations: ops, blockers: [{ message: "no" }] };
    expect(canConfirm(plan, defaultTicks(ops), "", "tickets")).toBe(false);
  });

  it("says where what is stored differs from what was typed", () => {
    expect(changeLine({ path: "fields[title]", after: { name: "title" } })).toBe(
      'fields[title]: — → {"name":"title"}',
    );
  });
});
