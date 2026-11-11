// Source control's model: Status's groups and ticks, a commit's body with its
// conflicts kept as mine, the link form, the review's ticks under the policy,
// the ledger's "restore this version", and the registry's diff tabs.

import { describe, expect, it } from "vitest";

import type { PlannedOp } from "./registry";
import {
  commitBody,
  defaultSelection,
  deployRefusal,
  diffLine,
  entryTitle,
  groupStatus,
  jsonChanges,
  ledgerSummary,
  linkBody,
  linkForm,
  linkProblem,
  restorable,
  reviewCanConfirm,
  reviewTickable,
  reviewTicks,
  selectable,
  statusGroup,
  withoutPointers,
  type Deployment,
  type StatusEntry,
  type SyncLinkInfo,
} from "./structureSync";

const t = (s: string, args?: Record<string, string | number>) =>
  s.replace(/\{(\w+)\}/g, (_, k: string) => String(args?.[k] ?? `{${k}}`));

const entry = (key: string, cls: string, over: Partial<StatusEntry> = {}): StatusEntry => ({
  key,
  kind: key.split(":")[0],
  name: key.split(":")[1] ?? key,
  class: cls,
  application: null,
  path: `${key}.json`,
  in_registry: true,
  ...over,
});

const ENTRIES: StatusEntry[] = [
  entry("table:tickets", "incoming"),
  entry("table:old_notes", "incoming_delete"),
  entry("view:edit", "local_change", { application: "helpdesk" }),
  entry("table:mine", "local_new"),
  entry("file_store:web", "pin_moved"),
  entry("page:home", "conflict"),
];

const op = (id: string, cls: string, ticked: boolean): PlannedOp => ({
  id,
  step: "tables",
  class: cls,
  ticked,
  kind: "table",
  entity: id,
  action: "x",
  summary: id,
});

describe("Status", () => {
  it("groups by what each class asks of the admin, decisions first", () => {
    expect(groupStatus(ENTRIES).map((g) => [g.group, g.entries.map((e) => e.key)])).toEqual([
      ["conflict", ["page:home"]],
      ["local", ["view:edit", "table:mine", "file_store:web"]],
      ["incoming", ["table:tickets"]],
      ["incoming_delete", ["table:old_notes"]],
    ]);
    // A moved pin is this instance's to commit, like any local change.
    expect(statusGroup("pin_moved")).toBe("local");
    expect(statusGroup("incoming_new")).toBe("incoming");
  });

  it("ticks every local change to begin with, and leaves a conflict to the admin", () => {
    expect([...defaultSelection(ENTRIES)].sort()).toEqual(["file_store:web", "table:mine", "view:edit"]);
    expect(selectable(entry("page:home", "conflict"))).toBe(true);
    expect(selectable(entry("table:tickets", "incoming"))).toBe(false);
  });

  it("commits what is ticked, keeping a ticked conflict as mine", () => {
    const body = commitBody("  Rename status  ", ENTRIES, new Set(["view:edit", "page:home", "table:tickets"]));
    // An incoming change cannot be committed, however it got ticked.
    expect(body).toEqual({
      message: "Rename status",
      keys: ["view:edit", "page:home"],
      keep_mine: ["page:home"],
    });
    expect(commitBody("x", ENTRIES, new Set())).toBeNull();
  });

  it("says why Deploy is refused, naming each uncommitted change", () => {
    expect(deployRefusal({ deploy_blockers: [] }, t)).toBeNull();
    expect(deployRefusal({ deploy_blockers: ["view:helpdesk/list"] }, t)).toContain("view:helpdesk/list");
  });

  it("names a view by its application", () => {
    expect(entryTitle(entry("view:edit", "local_change", { application: "helpdesk" }))).toBe("helpdesk / edit");
    expect(entryTitle(entry("table:tickets", "incoming"))).toBe("tickets");
  });

  it("writes a change as a line", () => {
    expect(diffLine({ path: "fields[status]/name", before: "status", after: "state" })).toBe(
      'fields[status]/name: "status" → "state"',
    );
    expect(diffLine({ path: "", before: null, after: { name: "x" } })).toBe('(whole): — → {"name":"x"}');
  });
});

describe("the link", () => {
  const stored: SyncLinkInfo = {
    link: {
      url: "git@example.com:acme/structure.git",
      branch: "main",
      subdirectory: null,
      credential: { kind: "https_token", username: "bot" },
      policy: { destructive: "never", full_backup: "optional" },
    },
    credential: { kind: "https_token", username: "bot" },
  };

  it("starts from what is stored, and sends a token only when one is typed", () => {
    const form = linkForm(stored);
    expect(form.destructive).toBe("never");
    expect(form.username).toBe("bot");
    expect(linkBody(form).credential).toEqual({ kind: "https_token", username: "bot", token: null });
    expect(linkBody({ ...form, token: " tok " }).credential?.token).toBe("tok");
    // A blank subdirectory is the repository's top.
    expect(linkBody({ ...form, subdirectory: "  " }).subdirectory).toBeNull();
  });

  it("refuses a deploy key that was never generated, and a token never given", () => {
    const empty = linkForm(null);
    expect(empty.branch).toBe("main");
    expect(linkProblem(empty, null, t)).toContain("URL");
    const url = { ...empty, url: "git@example.com:a/b.git" };
    expect(linkProblem({ ...url, credential: "deploy_key" }, null, t)).toContain("deploy key");
    expect(linkProblem({ ...url, credential: "https_token" }, null, t)).toContain("token");
    // The stored token is kept, so an untouched form is fine.
    expect(linkProblem(linkForm(stored), stored, t)).toBeNull();
    expect(linkProblem(url, null, t)).toBeNull();
  });
});

describe("the deploy review", () => {
  const ops = [op("rename", "safe", true), op("delete:view", "metadata_delete", true), op("drop", "data_destructive", false)];

  it("starts each operation at its default, and a drop unticked", () => {
    expect([...reviewTicks(ops, { destructive: "confirm" })]).toEqual(["rename", "delete:view"]);
  });

  it("lets nothing that loses data be ticked under `destructive: never`", () => {
    expect(reviewTickable(ops[2], { destructive: "confirm" })).toBe(true);
    expect(reviewTickable(ops[2], { destructive: "never" })).toBe(false);
    expect(reviewTickable(op("type", "unsupported", false), null)).toBe(false);
  });

  it("asks for the word typed once a drop is ticked, and never with a blocker", () => {
    const plan = { operations: ops, blockers: [] };
    expect(reviewCanConfirm(plan, new Set(["rename"]), "", "main")).toBe(true);
    expect(reviewCanConfirm(plan, new Set(["rename", "drop"]), "", "main")).toBe(false);
    expect(reviewCanConfirm(plan, new Set(["rename", "drop"]), " main ", "main")).toBe(true);
    const blocked = { operations: ops, blockers: [{ message: "312 rows have NULL in `status`" }] };
    expect(reviewCanConfirm(blocked, new Set(["rename"]), "", "main")).toBe(false);
  });
});

describe("history", () => {
  const row = (over: Partial<Deployment>): Deployment => ({
    id: "1",
    kind: "deploy",
    result: "converged",
    commit: "abcdef0123456789",
    branch: "main",
    actor: "ann@example.com",
    source: "ui",
    summary: { applied: ["a", "b"], drift: [] },
    deferred: [],
    backups: [],
    before: null,
    after: null,
    started_at: "2026-11-10T10:00:00Z",
    finished_at: "2026-11-10T10:00:01Z",
    ...over,
  });

  it("restores a registry save's file as it was before, and nothing else", () => {
    const edit = row({
      kind: "edit",
      summary: { path: "applications/helpdesk/views/list.json", applied: [] },
      before: { name: "list", description: "" },
    });
    expect(restorable(edit)).toEqual({
      path: "applications/helpdesk/views/list.json",
      value: { name: "list", description: "" },
    });
    // A save that created the file has nothing before it to restore.
    expect(restorable({ ...edit, before: null })).toBeNull();
    expect(restorable(row({}))).toBeNull();
  });

  it("sums a row up", () => {
    expect(ledgerSummary(row({ deferred: ["drop_table:table:old_notes"] }), t)).toBe("applied: 2 · deferred: 1");
    expect(ledgerSummary(row({ kind: "commit", summary: { message: "Add tickets", entities: ["a"] } }), t)).toBe(
      "Add tickets · entities: 1",
    );
  });
});

describe("the registry's diff tabs", () => {
  it("leaves this instance's own values out of the comparison", () => {
    const live = { name: "web", config: { url: "git@x:y.git", dir: "/srv/web", key_path: "/k" } };
    expect(withoutPointers(live, ["/config/dir", "/config/key_path"])).toEqual({
      name: "web",
      config: { url: "git@x:y.git" },
    });
    // The value itself is untouched.
    expect(live.config.dir).toBe("/srv/web");
  });

  it("lists each change, matching named items by name", () => {
    const base = { name: "tickets", fields: [{ name: "title" }, { name: "status", required: false }] };
    const tip = { name: "tickets", description: "Support", fields: [{ name: "status", required: true }, { name: "title" }] };
    expect(jsonChanges(base, tip)).toEqual([
      { path: "description", before: null, after: "Support" },
      { path: "fields[status]/required", before: false, after: true },
    ]);
    expect(jsonChanges(base, base)).toEqual([]);
  });
});
