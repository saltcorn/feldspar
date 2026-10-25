// What the home page's entity relationship diagram draws, apart from drawing
// it: one box per table listing its fields, one edge per `Key` field from that
// field's row to the row of the field it points at, and where dagre puts each
// box. Here rather than in the component so it can be tested without a browser
// (`entityDiagram.test.ts`); `screens/EntityDiagram.tsx` hands the result to
// React Flow.

import dagre from "@dagrejs/dagre";

import type { FieldKind, FieldItem } from "./fieldForm";

/** A table and its fields, as the diagram is drawn from them. */
export type DiagramTable = {
  name: string;
  /** Only the parts of `listFields`' answer the diagram reads. */
  fields: Pick<FieldItem, "name" | "type" | "primary_key" | "kind">[];
};

/** One row of a table's box. */
export type DiagramField = {
  name: string;
  type: string;
  primaryKey: boolean;
  /** The table this field is a key to, when it is one. */
  keyTo: string | null;
};

/** One table's box, placed. */
export type DiagramNode = {
  id: string;
  position: { x: number; y: number };
  fields: DiagramField[];
};

/** One key, from a field's row to the row of the field it references. The
 * handle ids are the field names, which are unique within a table. */
export type DiagramEdge = {
  id: string;
  source: string;
  sourceHandle: string;
  target: string;
  targetHandle: string;
};

/** A box's width, and the heights of its heading and of each field row, in
 * pixels. The component draws to these, so the layout knows each box's size
 * before React Flow has measured it. */
export const NODE_WIDTH = 240;
export const HEADER_HEIGHT = 36;
export const ROW_HEIGHT = 26;

/** A table's box's height. */
export function nodeHeight(fieldCount: number): number {
  return HEADER_HEIGHT + Math.max(1, fieldCount) * ROW_HEIGHT;
}

/** The field a key points at: the one it names, or the target's primary key
 * when it names none (the server's default), or nothing when the target has
 * neither — the edge then ends on the target's heading. */
function targetHandle(kind: FieldKind, target: DiagramTable): string | null {
  const named = kind?.target_field;
  if (named && target.fields.some((f) => f.name === named)) return named;
  return target.fields.find((f) => f.primary_key)?.name ?? null;
}

/** Where a key's edge ends when the target table has no matching field row. */
export const HEADER_HANDLE = "__table";

/** The diagram of `tables`, laid out left to right: a table to the left of the
 * tables its keys point at. A key to a table not among `tables` is left out,
 * since there is no box for it to end on. */
export function buildDiagram(tables: DiagramTable[]): {
  nodes: DiagramNode[];
  edges: DiagramEdge[];
} {
  const byName = new Map(tables.map((t) => [t.name, t]));
  const edges: DiagramEdge[] = [];
  const fieldsOf = new Map<string, DiagramField[]>();

  for (const table of tables) {
    const fields: DiagramField[] = table.fields.map((f) => {
      const kind = f.kind as FieldKind;
      const keyTo = kind?.type === "key" && kind.target_table ? kind.target_table : null;
      const target = keyTo ? byName.get(keyTo) : undefined;
      if (target) {
        edges.push({
          id: `${table.name}.${f.name}`,
          source: table.name,
          sourceHandle: f.name,
          target: target.name,
          targetHandle: targetHandle(kind, target) ?? HEADER_HANDLE,
        });
      }
      return { name: f.name, type: f.type, primaryKey: f.primary_key, keyTo };
    });
    fieldsOf.set(table.name, fields);
  }

  const g = new dagre.graphlib.Graph({ multigraph: true });
  g.setGraph({ rankdir: "LR", nodesep: 40, ranksep: 90, marginx: 20, marginy: 20 });
  g.setDefaultEdgeLabel(() => ({}));
  for (const table of tables) {
    g.setNode(table.name, { width: NODE_WIDTH, height: nodeHeight(table.fields.length) });
  }
  for (const edge of edges) {
    // A self-reference (a parent key) does not rank a table against itself.
    if (edge.source !== edge.target) g.setEdge(edge.source, edge.target, {}, edge.id);
  }
  dagre.layout(g);

  const nodes = tables.map((table) => {
    const { x, y, width, height } = g.node(table.name);
    // dagre places a node by its centre, React Flow by its top left corner.
    return {
      id: table.name,
      position: { x: x - width / 2, y: y - height / 2 },
      fields: fieldsOf.get(table.name) ?? [],
    };
  });
  return { nodes, edges };
}
