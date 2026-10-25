// What the home page's entity relationship diagram draws, apart from drawing
// it. Three kinds of box:
//
// - a **table**, listing its fields, with an edge per `Key` field from that
//   field's row to the row of the field it points at;
// - a **trigger**, with an edge from the table its insert/update/delete event
//   is on;
// - a **stream**, with an edge to each trigger that fires when an element
//   arrives on it.
//
// With a search, the diagram is the matching entities and everything one edge
// away from them, and the matches are marked so they can be found. Where dagre
// puts each box is here too. Here rather than in the component so it can be
// tested without a browser (`entityDiagram.test.ts`);
// `screens/EntityDiagram.tsx` hands the result to React Flow.

import dagre from "@dagrejs/dagre";

import { configHref, type Entity } from "./entities";
import type { FieldKind, FieldItem } from "./fieldForm";

/** A table's fields, as the diagram is drawn from them: only the parts of
 * `listFields`' answer it reads. */
export type DiagramFields = Pick<FieldItem, "name" | "type" | "primary_key" | "kind">[];

/** One row of a table's box. */
export type DiagramField = {
  name: string;
  type: string;
  primaryKey: boolean;
  /** The table this field is a key to, when it is one. */
  keyTo: string | null;
};

export type DiagramNodeKind = "table" | "trigger" | "stream";

/** One box, placed. */
export type DiagramNode = {
  /** `kind:id`, unique across kinds (a trigger may share a table's name). */
  id: string;
  kind: DiagramNodeKind;
  name: string;
  /** The entity's configuration screen, which clicking the box opens. */
  href: string;
  /** A table's fields; empty for a trigger or a stream. */
  fields: DiagramField[];
  /** The line under a trigger's or a stream's name: its event, its provider. */
  detail: string;
  /** Matched by the search, and so drawn with a blue border. */
  match: boolean;
  position: { x: number; y: number };
};

/** One edge. A key runs from its field's row to the referenced field's row
 * (handle ids are field names, unique within a table); a trigger edge from a
 * table's heading or a stream to the trigger. */
export type DiagramEdge = {
  id: string;
  kind: "key" | "trigger";
  source: string;
  sourceHandle: string;
  target: string;
  targetHandle: string;
};

/** A box's width, and the heights of its heading and of each row, in pixels.
 * The component draws to these, so the layout knows each box's size before
 * React Flow has measured it. */
export const NODE_WIDTH = 240;
export const HEADER_HEIGHT = 36;
export const ROW_HEIGHT = 26;

/** The handles on a box's heading, on its left and right: where an edge ends
 * on a table that has no field row for it, and where a table's trigger edges
 * and a stream's or a trigger's edges leave and arrive. */
export const HEAD_IN = "__in";
export const HEAD_OUT = "__out";

/** The events whose channel is a table. */
const TABLE_EVENTS = ["insert", "update", "delete"];

/** A box's height: its heading and a row per field, or the one detail row. */
export function nodeHeight(rows: number): number {
  return HEADER_HEIGHT + Math.max(1, rows) * ROW_HEIGHT;
}

/** The node id of an entity. */
export function nodeId(kind: string, id: string): string {
  return `${kind}:${id}`;
}

/** The field a key points at: the one it names, or the target's primary key
 * when it names none (the server's default), or the target's heading when it
 * has neither. */
function keyTargetHandle(kind: FieldKind, fields: DiagramFields): string {
  const named = kind?.target_field;
  if (named && fields.some((f) => f.name === named)) return named;
  return fields.find((f) => f.primary_key)?.name ?? HEAD_IN;
}

/**
 * The diagram of `entities` (their tables, triggers and streams; other kinds
 * are ignored), with `fields` holding each table's fields by name.
 *
 * `matches` is the set of node ids the search kept, or `null` with no search.
 * With one, the diagram is those entities and their neighbours — whatever one
 * edge joins to a match — and only the matches are marked. An edge to an
 * entity that is not drawn is left out.
 */
export function buildDiagram(
  entities: Entity[],
  fields: Record<string, DiagramFields>,
  matches: Set<string> | null = null,
): { nodes: DiagramNode[]; edges: DiagramEdge[] } {
  const boxes = new Map<string, Omit<DiagramNode, "match" | "position">>();
  const tableIds = new Map<string, string>();
  const streamIds = new Map<string, string>();
  for (const e of entities) {
    if (e.kind === "table") tableIds.set(e.id, nodeId("table", e.id));
    if (e.kind === "stream") streamIds.set(e.name, nodeId("stream", e.id));
  }

  const edges: DiagramEdge[] = [];
  for (const e of entities) {
    const id = nodeId(e.kind, e.id);
    if (e.kind === "table") {
      const own = fields[e.id] ?? [];
      const rows = own.map((f) => {
        const kind = f.kind as FieldKind;
        const keyTo = kind?.type === "key" && kind.target_table ? kind.target_table : null;
        const target = keyTo ? tableIds.get(keyTo) : undefined;
        if (keyTo && target) {
          edges.push({
            id: `key:${e.id}.${f.name}`,
            kind: "key",
            source: id,
            sourceHandle: f.name,
            target,
            targetHandle: keyTargetHandle(kind, fields[keyTo] ?? []),
          });
        }
        return { name: f.name, type: f.type, primaryKey: f.primary_key, keyTo };
      });
      boxes.set(id, { id, kind: "table", name: e.name, href: configHref(e), fields: rows, detail: "" });
    } else if (e.kind === "trigger") {
      // A trigger's first detail is its event.
      const when = e.details[0] ?? "";
      const channel = e.channel ?? "";
      const from = TABLE_EVENTS.includes(when)
        ? tableIds.get(channel)
        : when === "stream"
          ? streamIds.get(channel)
          : undefined;
      if (from) {
        edges.push({
          id: `trigger:${e.id}`,
          kind: "trigger",
          source: from,
          sourceHandle: HEAD_OUT,
          target: id,
          targetHandle: HEAD_IN,
        });
      }
      const detail = channel ? `${when} · ${channel}` : when;
      boxes.set(id, { id, kind: "trigger", name: e.name, href: configHref(e), fields: [], detail });
    } else if (e.kind === "stream") {
      const detail = e.details[0] ?? "";
      boxes.set(id, { id, kind: "stream", name: e.name, href: configHref(e), fields: [], detail });
    }
  }

  // With a search: the matches and their neighbours.
  let shown = new Set(boxes.keys());
  if (matches) {
    shown = new Set([...boxes.keys()].filter((id) => matches.has(id)));
    for (const edge of edges) {
      if (matches.has(edge.source)) shown.add(edge.target);
      if (matches.has(edge.target)) shown.add(edge.source);
    }
  }
  const drawnEdges = edges.filter((e) => shown.has(e.source) && shown.has(e.target));
  const drawn = [...boxes.values()].filter((b) => shown.has(b.id));

  const g = new dagre.graphlib.Graph({ multigraph: true });
  g.setGraph({ rankdir: "LR", nodesep: 40, ranksep: 90, marginx: 20, marginy: 20 });
  g.setDefaultEdgeLabel(() => ({}));
  for (const box of drawn) {
    g.setNode(box.id, { width: NODE_WIDTH, height: nodeHeight(box.fields.length) });
  }
  for (const edge of drawnEdges) {
    // A self-reference (a parent key) does not rank a table against itself.
    if (edge.source !== edge.target) g.setEdge(edge.source, edge.target, {}, edge.id);
  }
  dagre.layout(g);

  const nodes = drawn.map((box) => {
    const { x, y, width, height } = g.node(box.id);
    return {
      ...box,
      match: matches?.has(box.id) ?? false,
      // dagre places a node by its centre, React Flow by its top left corner.
      position: { x: x - width / 2, y: y - height / 2 },
    };
  });
  return { nodes, edges: drawnEdges };
}
