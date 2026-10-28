// The home page's entity relationship diagram: tables, each a box listing its
// fields with an edge from every key field to the field it references; the
// triggers on them; and the streams that fire triggers. Drawn with React Flow
// after its "database schema node"
// (https://reactflow.dev/ui/components/database-schema-node), in Tabler's
// colours rather than shadcn's. Clicking a box opens that entity's page.
//
// What is drawn and where is `entityDiagram.ts`'s; this fetches each table's
// fields and renders.

import { useEffect, useMemo, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import {
  Background,
  Controls,
  Handle,
  MarkerType,
  Position,
  ReactFlow,
  type Edge,
  type Node,
  type NodeProps,
} from "@xyflow/react";

import { api, errorMessage } from "../api";
import type { Entity } from "../entities";
import {
  HEAD_IN,
  HEAD_OUT,
  HEADER_HEIGHT,
  NODE_WIDTH,
  ROW_HEIGHT,
  buildDiagram,
  type DiagramFields,
  type DiagramNode,
} from "../entityDiagram";
import { IconBolt, IconBroadcast, IconTable } from "../icons";
import { T, useT } from "../i18n";

const NODE_TYPES = { entity: EntityNode };

const KIND_ICON: Record<DiagramNode["kind"], ReactNode> = {
  table: <IconTable />,
  trigger: <IconBolt />,
  stream: <IconBroadcast />,
};

/** The diagram of `entities` (tables, triggers and streams). `matches` is the
 * node ids the search kept, or `null` with no search: see `buildDiagram`. */
export function EntityDiagram({
  entities,
  matches,
}: {
  entities: Entity[];
  matches: Set<string> | null;
}) {
  const { t } = useT();
  /** Each table's fields, by table name, fetched once per table. */
  const [fetched, setFetched] = useState<Record<string, DiagramFields>>({});
  const [error, setError] = useState<string | null>(null);

  const missing = entities
    .filter((e) => e.kind === "table" && !(e.id in fetched))
    .map((e) => e.id);
  const missingKey = missing.join("\n");
  useEffect(() => {
    if (missing.length === 0) return;
    let live = true;
    Promise.all(
      missing.map((name) => api.listFields(name).then((fields) => [name, fields] as const)),
    )
      .then((pairs) => {
        if (live) setFetched((current) => ({ ...current, ...Object.fromEntries(pairs) }));
      })
      .catch((err) => {
        if (live) setError(errorMessage(err, "Could not load the tables' fields."));
      });
    return () => {
      live = false;
    };
    // `missingKey` stands for `missing`, which is a new array every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [missingKey]);

  const loaded = missing.length === 0;
  const { nodes, edges } = useMemo(() => {
    if (!loaded) return { nodes: [], edges: [] };
    const diagram = buildDiagram(entities, fetched, matches);
    const nodes: Node[] = diagram.nodes.map((n) => ({
      id: n.id,
      type: "entity",
      position: n.position,
      data: n,
    }));
    const edges: Edge[] = diagram.edges.map((e) => ({
      id: e.id,
      source: e.source,
      sourceHandle: e.sourceHandle,
      target: e.target,
      targetHandle: e.targetHandle,
      type: "smoothstep",
      className: `erd-edge-${e.kind}`,
      markerEnd: { type: MarkerType.ArrowClosed },
    }));
    return { nodes, edges };
  }, [loaded, entities, fetched, matches]);

  let body: ReactNode;
  if (error) {
    body = <Alert variant="danger" className="m-3">{error}</Alert>;
  } else if (!loaded) {
    body = <div className="p-3 text-secondary"><T text="Loading…" /></div>;
  } else if (nodes.length === 0) {
    body = <div className="p-3 text-secondary"><T text="No entity matches." /></div>;
  } else {
    body = (
      <ReactFlow
        // A new set of boxes is a new layout, framed afresh.
        key={nodes.map((n) => n.id).join("\n")}
        nodes={nodes}
        edges={edges}
        nodeTypes={NODE_TYPES}
        onNodeClick={(_, node) => {
          window.location.hash = (node.data as DiagramNode).href;
        }}
        nodesConnectable={false}
        elementsSelectable={false}
        fitView
        minZoom={0.1}
      >
        <Background />
        <Controls showInteractive={false} />
      </ReactFlow>
    );
  }
  return (
    <div className="entity-diagram" aria-label={t("Entity relationship diagram")}>
      {body}
    </div>
  );
}

/** One box: the entity's name under its kind's icon, then a row per field
 * with the field's type for a table, or one line of detail for a trigger or a
 * stream. A table's rows have a handle on each side, so a key's edge leaves
 * from its own row and ends on the row of the field it references. */
function EntityNode({ data }: NodeProps) {
  const node = data as DiagramNode;
  const classes = ["erd-node", `erd-node-${node.kind}`, node.match ? "erd-node-match" : ""]
    .filter(Boolean)
    .join(" ");
  return (
    <div className={classes} style={{ width: NODE_WIDTH }} title={node.name}>
      <div className="erd-node-head" style={{ height: HEADER_HEIGHT }}>
        <Handle type="target" id={HEAD_IN} position={Position.Left} />
        {KIND_ICON[node.kind]}
        <a href={node.href}>{node.name}</a>
        <Handle type="source" id={HEAD_OUT} position={Position.Right} />
      </div>
      {node.kind !== "table" && (
        <div className="erd-node-row text-secondary" style={{ height: ROW_HEIGHT }}>
          <span>{node.detail}</span>
        </div>
      )}
      {node.kind === "table" && node.fields.length === 0 && (
        <div className="erd-node-row text-secondary" style={{ height: ROW_HEIGHT }}>
          <T text="No fields" />
        </div>
      )}
      {node.fields.map((field) => (
        <div key={field.name} className="erd-node-row" style={{ height: ROW_HEIGHT }}>
          <Handle type="target" id={field.name} position={Position.Left} />
          <span className={field.primaryKey ? "fw-bold" : undefined}>
            {field.name}
            {field.primaryKey && <span className="erd-pk"> <T text="PK" /></span>}
          </span>
          <span className="erd-type">{field.keyTo ? `→ ${field.keyTo}` : field.type}</span>
          <Handle type="source" id={field.name} position={Position.Right} />
        </div>
      ))}
    </div>
  );
}
