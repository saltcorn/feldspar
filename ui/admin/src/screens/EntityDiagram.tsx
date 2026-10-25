// The home page's entity relationship diagram: the tables the entities list
// would show, each a box listing its fields, with an edge from every key field
// to the field it references. Drawn with React Flow after its "database schema
// node" (https://reactflow.dev/ui/components/database-schema-node), in Tabler's
// colours rather than shadcn's. Clicking a box opens that table's page.
//
// What is drawn and where is `entityDiagram.ts`'s; this fetches each table's
// fields and renders.

import { useEffect, useMemo, useState } from "react";
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
import {
  HEADER_HANDLE,
  HEADER_HEIGHT,
  NODE_WIDTH,
  ROW_HEIGHT,
  buildDiagram,
  type DiagramField,
  type DiagramTable,
} from "../entityDiagram";
import { T, useT } from "../i18n";

type TableNodeData = { name: string; fields: DiagramField[] };

const NODE_TYPES = { table: TableNode };

/** Where a table's page is. */
function tableHref(table: string): string {
  return `#/tables/${encodeURIComponent(table)}`;
}

/** The diagram of the tables named in `tables`. */
export function EntityDiagram({ tables }: { tables: string[] }) {
  const { t } = useT();
  /** Each table's fields, by table name, fetched once per table. */
  const [fetched, setFetched] = useState<Record<string, DiagramTable["fields"]>>({});
  const [error, setError] = useState<string | null>(null);

  const missing = tables.filter((name) => !(name in fetched));
  const missingKey = missing.join("\n");
  useEffect(() => {
    if (missing.length === 0) return;
    let live = true;
    Promise.all(
      missing.map((name) =>
        api.listFields(name).then((fields) => [name, fields] as const),
      ),
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
    const diagram = buildDiagram(tables.map((name) => ({ name, fields: fetched[name] ?? [] })));
    const nodes: Node[] = diagram.nodes.map((n) => ({
      id: n.id,
      type: "table",
      position: n.position,
      data: { name: n.id, fields: n.fields } satisfies TableNodeData,
    }));
    const edges: Edge[] = diagram.edges.map((e) => ({
      ...e,
      type: "smoothstep",
      markerEnd: { type: MarkerType.ArrowClosed },
    }));
    return { nodes, edges };
  }, [loaded, tables, fetched]);

  if (error) return <Alert variant="danger" className="m-3">{error}</Alert>;
  if (!loaded) return <div className="p-3 text-secondary"><T text="Loading…" /></div>;
  if (tables.length === 0) {
    return <div className="p-3 text-secondary"><T text="No table matches." /></div>;
  }

  return (
    <div className="entity-diagram" aria-label={t("Entity relationship diagram")}>
      <ReactFlow
        // A new set of tables is a new layout, framed afresh.
        key={tables.join("\n")}
        nodes={nodes}
        edges={edges}
        nodeTypes={NODE_TYPES}
        onNodeClick={(_, node) => {
          window.location.hash = tableHref(node.id);
        }}
        nodesConnectable={false}
        elementsSelectable={false}
        fitView
        minZoom={0.1}
      >
        <Background />
        <Controls showInteractive={false} />
      </ReactFlow>
    </div>
  );
}

/** One table: its name, then a row per field with the field's type. Every row
 * has a handle on each side, so a key's edge leaves from its own row and ends
 * on the row of the field it references. */
function TableNode({ data }: NodeProps) {
  const table = data as TableNodeData;
  return (
    <div className="erd-node" style={{ width: NODE_WIDTH }} title={table.name}>
      <div className="erd-node-head" style={{ height: HEADER_HEIGHT }}>
        <Handle type="target" id={HEADER_HANDLE} position={Position.Left} />
        <a href={tableHref(table.name)}>{table.name}</a>
      </div>
      {table.fields.length === 0 && (
        <div className="erd-node-row text-secondary" style={{ height: ROW_HEIGHT }}>
          <T text="No fields" />
        </div>
      )}
      {table.fields.map((field) => (
        <div key={field.name} className="erd-node-row" style={{ height: ROW_HEIGHT }}>
          <Handle type="target" id={field.name} position={Position.Left} />
          <span className={field.primaryKey ? "fw-bold" : undefined}>
            {field.name}
            {field.primaryKey && <span className="erd-pk"> PK</span>}
          </span>
          <span className="erd-type">{field.keyTo ? `→ ${field.keyTo}` : field.type}</span>
          <Handle type="source" id={field.name} position={Position.Right} />
        </div>
      ))}
    </div>
  );
}
