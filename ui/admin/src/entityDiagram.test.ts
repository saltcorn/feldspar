// The relationship diagram's model: an edge per key between the tables drawn,
// from the key's row to the referenced field's row; an edge from a table to
// each trigger on it and from a stream to each trigger it fires; a search
// that draws its matches and their neighbours; and a layout that puts a
// source to the left of what it points at.

import { describe, expect, it } from "vitest";

import type { Entity } from "./entities";
import { HEAD_IN, HEAD_OUT, buildDiagram, nodeHeight, type DiagramFields } from "./entityDiagram";

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

const field = (name: string, extra: Partial<DiagramFields[number]> = {}) => ({
  name,
  type: "String",
  primary_key: false,
  kind: { type: "plain" },
  ...extra,
});

const key = (name: string, target_table: string, target_field?: string) =>
  field(name, { type: "Integer", kind: { type: "key", target_table, target_field } });

const FIELDS: Record<string, DiagramFields> = {
  books: [field("id", { primary_key: true }), field("title"), key("author", "authors", "id")],
  authors: [field("id", { primary_key: true }), field("name"), key("parent", "authors")],
  loans: [key("book", "books"), key("member", "members")],
};

const ENTITIES: Entity[] = [
  entity({ kind: "table", name: "books" }),
  entity({ kind: "table", name: "authors" }),
  entity({ kind: "table", name: "loans" }),
  entity({ kind: "trigger", id: "t1", name: "notify", details: ["insert", "run_js_code"], channel: "loans" }),
  entity({ kind: "trigger", id: "t2", name: "record", details: ["stream", "insert_row"], channel: "boiler" }),
  entity({ kind: "trigger", id: "t3", name: "nightly", details: ["daily", "run_js_code"] }),
  entity({ kind: "stream", id: "s1", name: "boiler", details: ["mqtt"] }),
  entity({ kind: "agent", id: "a1", name: "helper" }),
];

describe("buildDiagram", () => {
  const { nodes, edges } = buildDiagram(ENTITIES, FIELDS);
  const node = (id: string) => nodes.find((n) => n.id === id)!;

  it("draws a box per table, trigger and stream, and nothing else", () => {
    expect(nodes.map((n) => n.id)).toEqual([
      "table:books",
      "table:authors",
      "table:loans",
      "trigger:t1",
      "trigger:t2",
      "trigger:t3",
      "stream:s1",
    ]);
    expect(node("table:books").fields).toEqual([
      { name: "id", type: "String", primaryKey: true, keyTo: null },
      { name: "title", type: "String", primaryKey: false, keyTo: null },
      { name: "author", type: "Integer", primaryKey: false, keyTo: "authors" },
    ]);
    expect(node("trigger:t1")).toMatchObject({ detail: "insert · loans", href: "#/triggers/t1/edit" });
    expect(node("stream:s1")).toMatchObject({ detail: "mqtt", href: "#/streams/s1/edit" });
    expect(nodes.every((n) => !n.match)).toBe(true);
  });

  it("joins each key's row to the referenced field's row", () => {
    const keys = edges.filter((e) => e.kind === "key");
    expect(keys).toEqual([
      { id: "key:books.author", kind: "key", source: "table:books", sourceHandle: "author", target: "table:authors", targetHandle: "id" },
      // No target field named: the target's primary key.
      { id: "key:authors.parent", kind: "key", source: "table:authors", sourceHandle: "parent", target: "table:authors", targetHandle: "id" },
      { id: "key:loans.book", kind: "key", source: "table:loans", sourceHandle: "book", target: "table:books", targetHandle: "id" },
    ]);
  });

  it("joins a trigger to its table, and a stream to the triggers it fires", () => {
    const triggers = edges.filter((e) => e.kind === "trigger");
    expect(triggers).toEqual([
      { id: "trigger:t1", kind: "trigger", source: "table:loans", sourceHandle: HEAD_OUT, target: "trigger:t1", targetHandle: HEAD_IN },
      { id: "trigger:t2", kind: "trigger", source: "stream:s1", sourceHandle: HEAD_OUT, target: "trigger:t2", targetHandle: HEAD_IN },
    ]);
  });

  it("leaves out a key to a table that is not drawn", () => {
    expect(edges.some((e) => e.target === "table:members")).toBe(false);
    expect(node("table:loans").fields[1].keyTo).toBe("members");
  });

  it("ends a key on the heading when the target has no field to end on", () => {
    const { edges } = buildDiagram(
      [entity({ kind: "table", name: "a" }), entity({ kind: "table", name: "b" })],
      { a: [key("b", "b")], b: [field("code")] },
    );
    expect(edges[0].targetHandle).toBe(HEAD_IN);
  });

  it("with a search, draws the matches and their neighbours, and marks the matches", () => {
    const { nodes, edges } = buildDiagram(ENTITIES, FIELDS, new Set(["table:books"]));
    // books, what it points at (authors) and what points at it (loans) — but
    // not loans' trigger, two edges away.
    expect(nodes.map((n) => [n.id, n.match])).toEqual([
      ["table:books", true],
      ["table:authors", false],
      ["table:loans", false],
    ]);
    expect(edges.map((e) => e.id)).toEqual(["key:books.author", "key:authors.parent", "key:loans.book"]);

    const stream = buildDiagram(ENTITIES, FIELDS, new Set(["stream:s1"]));
    expect(stream.nodes.map((n) => n.id)).toEqual(["trigger:t2", "stream:s1"]);
  });

  it("lays a source out left of what it points at", () => {
    const x = Object.fromEntries(nodes.map((n) => [n.id, n.position.x]));
    expect(x["table:loans"]).toBeLessThan(x["table:books"]);
    expect(x["table:books"]).toBeLessThan(x["table:authors"]);
    expect(x["table:loans"]).toBeLessThan(x["trigger:t1"]);
    expect(x["stream:s1"]).toBeLessThan(x["trigger:t2"]);
  });

  it("sizes a box by its rows", () => {
    expect(nodeHeight(3)).toBeGreaterThan(nodeHeight(1));
    expect(nodeHeight(0)).toBe(nodeHeight(1));
  });
});
