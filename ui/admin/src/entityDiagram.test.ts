// The relationship diagram's model: an edge per key between the boxes on the
// page, from the key's row to the referenced field's row, and a layout that
// puts a table to the left of what it points at.

import { describe, expect, it } from "vitest";

import { HEADER_HANDLE, buildDiagram, nodeHeight, type DiagramTable } from "./entityDiagram";

const field = (name: string, extra: Partial<DiagramTable["fields"][number]> = {}) => ({
  name,
  type: "String",
  primary_key: false,
  kind: { type: "plain" },
  ...extra,
});

const key = (name: string, target_table: string, target_field?: string) =>
  field(name, { type: "Integer", kind: { type: "key", target_table, target_field } });

const TABLES: DiagramTable[] = [
  { name: "books", fields: [field("id", { primary_key: true }), field("title"), key("author", "authors", "id")] },
  { name: "authors", fields: [field("id", { primary_key: true }), field("name"), key("parent", "authors")] },
  { name: "loans", fields: [key("book", "books"), key("member", "members")] },
];

describe("buildDiagram", () => {
  const { nodes, edges } = buildDiagram(TABLES);

  it("draws a box per table with its fields, marking keys and primary keys", () => {
    expect(nodes.map((n) => n.id)).toEqual(["books", "authors", "loans"]);
    expect(nodes[0].fields).toEqual([
      { name: "id", type: "String", primaryKey: true, keyTo: null },
      { name: "title", type: "String", primaryKey: false, keyTo: null },
      { name: "author", type: "Integer", primaryKey: false, keyTo: "authors" },
    ]);
  });

  it("joins each key's row to the referenced field's row", () => {
    expect(edges).toEqual([
      { id: "books.author", source: "books", sourceHandle: "author", target: "authors", targetHandle: "id" },
      // No target field named: the target's primary key.
      { id: "authors.parent", source: "authors", sourceHandle: "parent", target: "authors", targetHandle: "id" },
      { id: "loans.book", source: "loans", sourceHandle: "book", target: "books", targetHandle: "id" },
    ]);
  });

  it("leaves out a key to a table that is not drawn", () => {
    expect(edges.some((e) => e.target === "members")).toBe(false);
    expect(nodes[2].fields[1].keyTo).toBe("members");
  });

  it("ends a key on the heading when the target has no field to end on", () => {
    const { edges } = buildDiagram([
      { name: "a", fields: [key("b", "b")] },
      { name: "b", fields: [field("code")] },
    ]);
    expect(edges[0].targetHandle).toBe(HEADER_HANDLE);
  });

  it("lays a table out left of the tables it points at", () => {
    const x = Object.fromEntries(nodes.map((n) => [n.id, n.position.x]));
    expect(x.loans).toBeLessThan(x.books);
    expect(x.books).toBeLessThan(x.authors);
  });

  it("sizes a box by its field count", () => {
    expect(nodeHeight(3)).toBeGreaterThan(nodeHeight(1));
    expect(nodeHeight(0)).toBe(nodeHeight(1));
  });
});
