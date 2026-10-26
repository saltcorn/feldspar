/**
 * The home page's entities list: what a search and the type filter keep, which
 * are the recently edited ones, and where each row's links go.
 */

import { describe, expect, it } from "vitest";

import {
  configHref,
  kindLabel,
  kindLabelPlural,
  matchesFilter,
  recentlyEdited,
  runAction,
  sortEntities,
  timeAgo,
  toggleKind,
  type Entity,
} from "./entities";

function entity(over: Partial<Entity> & Pick<Entity, "kind" | "name">): Entity {
  return {
    id: over.name,
    description: "",
    details: [],
    access: "admin",
    updated_at: null,
    runnable: true,
    subdomain: null,
    channel: null,
    definition: null,
    ...over,
  };
}

const books = entity({ kind: "table", name: "books", description: "The catalogue" });
const greet = entity({
  kind: "trigger",
  id: "7f0c",
  name: "greet",
  details: ["none", "run_js_code"],
  definition: '{"configuration":{"code":"return lookup(\'isbn\');"}}',
});
const docs = entity({ kind: "file_store", id: "a1", name: "docs" });

describe("matchesFilter", () => {
  const none = { query: "", kinds: [], deep: false };

  it("keeps everything when nothing is set", () => {
    expect([books, greet, docs].filter((e) => matchesFilter(e, none))).toHaveLength(3);
  });

  it("searches the name, the description and the kind, every word", () => {
    expect(matchesFilter(books, { ...none, query: "BOOK" })).toBe(true);
    expect(matchesFilter(books, { ...none, query: "catalogue" })).toBe(true);
    expect(matchesFilter(greet, { ...none, query: "trigger" })).toBe(true);
    expect(matchesFilter(docs, { ...none, query: "file store" })).toBe(true);
    expect(matchesFilter(books, { ...none, query: "books trigger" })).toBe(false);
  });

  it("looks inside the definition only for a deep search", () => {
    expect(matchesFilter(greet, { ...none, query: "isbn" })).toBe(false);
    expect(matchesFilter(greet, { ...none, query: "isbn", deep: true })).toBe(true);
  });

  it("narrows to the kinds switched on, and none switched on is every kind", () => {
    const tables = { ...none, kinds: ["table" as const] };
    expect([books, greet, docs].filter((e) => matchesFilter(e, tables))).toEqual([books]);
    const two = { ...none, kinds: ["table" as const, "trigger" as const] };
    expect([books, greet, docs].filter((e) => matchesFilter(e, two))).toEqual([books, greet]);
  });
});

describe("toggleKind", () => {
  it("adds and removes a kind, keeping the filter's order", () => {
    expect(toggleKind([], "agent")).toEqual(["agent"]);
    expect(toggleKind(["agent"], "table")).toEqual(["table", "agent"]);
    expect(toggleKind(["table", "agent"], "table")).toEqual(["agent"]);
  });
});

describe("recentlyEdited", () => {
  it("is the newest edits first, at most five, leaving out the undated", () => {
    const dated = Array.from({ length: 7 }, (_, n) =>
      entity({
        kind: "agent",
        name: `a${n}`,
        updated_at: new Date(Date.UTC(2026, 9, 1 + n)).toISOString(),
      }),
    );
    const recent = recentlyEdited([books, ...dated]);
    expect(recent.map((e) => e.name)).toEqual(["a6", "a5", "a4", "a3", "a2"]);
  });
});

describe("sortEntities", () => {
  it("orders by name whatever the kind, as Saltcorn 1 did", () => {
    expect(sortEntities([greet, docs, books]).map((e) => e.name)).toEqual([
      "books",
      "docs",
      "greet",
    ]);
  });
});

describe("links", () => {
  it("goes to each kind's own configuration screen", () => {
    expect(configHref(books)).toBe("#/tables/books");
    expect(configHref(greet)).toBe("#/triggers/7f0c/edit");
    expect(configHref(docs)).toBe("#/file-stores/a1/edit");
    expect(configHref(entity({ kind: "agent", id: "b2", name: "helper" }))).toBe(
      "#/agents/b2/edit",
    );
    expect(configHref(entity({ kind: "stream", id: "d4", name: "boiler" }))).toBe(
      "#/streams/d4/edit",
    );
    expect(configHref(entity({ kind: "application", id: "c3", name: "Blog" }))).toBe(
      "#/applications/c3/edit",
    );
  });

  it("runs an instance where that means something", () => {
    expect(runAction(books)).toMatchObject({ type: "link", href: "#/tables/books/data" });
    expect(runAction(docs)).toMatchObject({ type: "link", href: "#/files/docs" });
    expect(runAction(greet)).toEqual({ type: "test-run", label: "Test run" });
    expect(runAction(entity({ kind: "agent", name: "helper" }))).toMatchObject({
      href: "#/agents/helper/chat",
    });
    expect(runAction(entity({ kind: "stream", id: "d4", name: "boiler" }))).toMatchObject({
      label: "Observe",
      href: "#/streams/d4/observe",
    });
    const blog = entity({ kind: "application", name: "Blog", subdomain: "blog" });
    expect(runAction(blog, (sub) => `https://${sub}.example.com`)).toEqual({
      type: "link",
      label: "Open",
      href: "https://blog.example.com",
      external: true,
    });
  });

  it("offers no Run link for an entity that cannot run now", () => {
    expect(runAction({ ...greet, runnable: false })).toBeNull();
  });
});

describe("labels in the screen's language", () => {
  // A stand-in translator: what reaches it is the English key, so a label that
  // came back bracketed went through `t` — the only way a catalogue can reach it.
  const t = (text: string) => `[${text}]`;

  it("translates a kind's name, singular and plural, and leaves an unknown kind alone", () => {
    expect(kindLabel("file_store")).toBe("File store");
    expect(kindLabel("file_store", t)).toBe("[File store]");
    expect(kindLabelPlural("agent", t)).toBe("[Agents]");
    expect(kindLabel("widget", t)).toBe("widget");
  });

  it("translates the Run link's label", () => {
    const greet = entity({ kind: "trigger", name: "greet" });
    expect(runAction(greet, undefined, t)).toEqual({ type: "test-run", label: "[Test run]" });
  });
});

describe("timeAgo", () => {
  const now = Date.UTC(2026, 9, 25, 12);
  it("says how long ago, in the largest whole unit", () => {
    expect(timeAgo(new Date(now - 10_000).toISOString(), now)).toBe("just now");
    expect(timeAgo(new Date(now - 5 * 60_000).toISOString(), now)).toBe("5m ago");
    expect(timeAgo(new Date(now - 3 * 3_600_000).toISOString(), now)).toBe("3h ago");
    expect(timeAgo(new Date(now - 4 * 86_400_000).toISOString(), now)).toBe("4d ago");
    expect(timeAgo(new Date(now - 65 * 86_400_000).toISOString(), now)).toBe("2mo ago");
  });
});
