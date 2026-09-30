// The Dataset editor's edit mode (analytics TODO A1.17): one dataset, its
// operations in a side panel beside the spreadsheet of the stage selected.
//
// Selecting an operation shows the rows as they are after it — the base shows
// the rows it starts from — so the effect of each operation can be seen on its
// own. Operations are added from the Add menu or from the spreadsheet's
// headers, edited in their form, dragged to reorder, switched off and deleted.
// Every change is saved at once; the server compiles the dataset and marks
// each operation, and one that stopped working — a column an earlier operation
// no longer makes — is shown with its sentence rather than removed.

import { useCallback, useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import type { DatasetItem } from "./DatasetList";
import { opKindAbout, opKindName } from "../labels";
import { OperationForm, type Others } from "./OperationForm";
import {
  OP_KINDS,
  describeOperation,
  formulaCompletions,
  insertOperation,
  moveOperation,
  newOperation,
  removeOperation,
  replaceOperation,
  stageOf,
  stageShape,
  toggleOperation,
  type DatasetDef,
  type OpKind,
  type Operation,
  type Report,
} from "./ops";
import { StageGrid, type HeaderAction } from "./StageGrid";

/** An operation being edited in its form: new ones go in at `position`. */
type Editing = { op: Operation; mode: "new" | "edit"; position: number };

export function DatasetEditor({
  id,
  selected,
  onSelect,
  onBack,
  backLabel,
}: {
  id: string;
  /** The operation whose result is shown: an id, `""` for the base, `null` for
   * the last. */
  selected: string | null;
  onSelect: (operation: string | null) => void;
  onBack: () => void;
  backLabel: string;
}) {
  const { t } = useT();
  const [def, setDef] = useState<DatasetDef | null>(null);
  const [report, setReport] = useState<Report | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [others, setOthers] = useState<Others>({ tables: [], datasets: [] });
  const [ticked, setTicked] = useState<string[]>([]);
  const [dragging, setDragging] = useState<number | null>(null);
  const [dropAt, setDropAt] = useState<number | null>(null);

  useEffect(() => {
    let live = true;
    api
      .getDataset(id)
      .then((answer) => {
        if (!live) return;
        setDef(answer.dataset as DatasetDef);
        setReport(answer.report as Report);
      })
      .catch((err: unknown) => setError(errorMessage(err, t("Could not open the dataset."))));
    Promise.all([api.listDatasetTables(), api.listDatasets()])
      .then(([tables, datasets]) => {
        if (live) setOthers({ tables, datasets: datasets.filter((d) => d.id !== id) });
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, [id, t]);

  /** Save a new definition; the server's report comes back with it. */
  const commit = useCallback(
    async (next: DatasetDef) => {
      setDef(next);
      setSaving(true);
      setError(null);
      try {
        const answer = await api.updateDataset(next.id, {
          name: next.name,
          description: next.description,
          base: next.base,
          operations: next.operations,
        });
        setReport(answer.report as Report);
      } catch (err) {
        setError(errorMessage(err, t("Could not save the dataset.")));
      } finally {
        setSaving(false);
      }
    },
    [t],
  );

  const ops = def?.operations ?? [];
  const upto = stageOf(ops, selected);
  const shape = stageShape(report, upto);
  const completions = useMemo(() => formulaCompletions(shape, report), [shape, report]);
  const datasetName = useCallback(
    (other: string) => others.datasets.find((d: DatasetItem) => d.id === other)?.name ?? other,
    [others],
  );

  if (!def) {
    return (
      <div className="an-page">
        {error ? <Alert variant="danger">{error}</Alert> : <Spinner animation="border" size="sm" />}
      </div>
    );
  }

  /** A new operation after the stage being looked at, in its form. */
  const add = (kind: OpKind, params?: Record<string, unknown>) =>
    setEditing({
      op: newOperation(kind, ops, shape?.columns ?? [], params),
      mode: "new",
      position: upto,
    });

  const apply = (edited: Editing, op: Operation) => {
    const next =
      edited.mode === "new"
        ? insertOperation(ops, edited.position, op)
        : replaceOperation(ops, op);
    setEditing(null);
    void commit({ ...def, operations: next });
    onSelect(op.id);
  };

  const onHeader = (action: HeaderAction) => {
    const cols = shape?.columns ?? [];
    switch (action.kind) {
      case "filter":
        add("filter", { formula: `${action.column} ` });
        break;
      case "sort":
        // Nothing to fill in: added as it is.
        void (async () => {
          const op = newOperation("sort", ops, cols, {
            keys: [{ formula: action.column, descending: action.descending }],
          });
          await commit({ ...def, operations: insertOperation(ops, upto, op) });
          onSelect(op.id);
        })();
        break;
      case "group":
        add("aggregate", {
          group_by: [{ name: action.column, formula: action.column }],
          summaries: [{ name: "n", function: "count" }],
        });
        break;
      case "stack":
        add("stack", { columns: action.columns, names_to: "name", values_to: "value" });
        break;
    }
  };

  const rename = (name: string) => {
    if (name.trim() !== "" && name !== def.name) void commit({ ...def, name: name.trim() });
  };

  return (
    <div className="an-editor">
      <aside className="an-ops">
        <Button variant="link" className="p-0 mb-2" onClick={onBack}>
          ← {backLabel}
        </Button>
        <Form.Control
          className="fw-bold mb-1"
          defaultValue={def.name}
          key={def.name}
          aria-label={t("Dataset name")}
          onBlur={(e) => rename(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          }}
        />
        <div className="small text-secondary mb-2">
          {saving ? t("Saving…") : t("Saved")}
          {report?.operations.some((o) => o.status === "invalid") && (
            <Badge bg="danger-lt" className="ms-2">
              <T text="has an error" />
            </Badge>
          )}
        </div>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            {error}
          </Alert>
        )}

        <div
          className={`an-op${upto === 0 ? " selected" : ""}`}
          onClick={() => onSelect("")}
          role="button"
        >
          <div className="small text-secondary">
            <T text="Base" />
          </div>
          <div className="an-op-summary">
            {def.base.kind === "table" ? def.base.table : datasetName(def.base.dataset)}
          </div>
          {report?.base.error && <div className="an-op-error">{report.base.error}</div>}
        </div>

        {ops.map((op, i) => {
          const r = report?.operations[i];
          const invalid = r?.status === "invalid";
          const classes = [
            "an-op",
            upto === i + 1 ? "selected" : "",
            op.enabled ? "" : "disabled",
            invalid ? "invalid" : "",
            dropAt === i && dragging !== null && dragging !== i ? "drop-target" : "",
          ].join(" ");
          return (
            <div
              key={op.id}
              className={classes}
              role="button"
              draggable
              onDragStart={() => setDragging(i)}
              onDragOver={(e) => {
                e.preventDefault();
                setDropAt(i);
              }}
              onDragEnd={() => {
                setDragging(null);
                setDropAt(null);
              }}
              onDrop={(e) => {
                e.preventDefault();
                if (dragging !== null && dragging !== i) {
                  void commit({ ...def, operations: moveOperation(ops, dragging, i) });
                }
                setDragging(null);
                setDropAt(null);
              }}
              onClick={() => onSelect(op.id)}
              onDoubleClick={() => setEditing({ op, mode: "edit", position: i })}
            >
              <div className="d-flex align-items-center gap-2">
                <span className="text-secondary" title={t("Drag to reorder")}>
                  ⠿
                </span>
                <span className="small fw-bold">
                  {i + 1}. {opKindName(op.kind, t)}
                </span>
                <Form.Check
                  type="switch"
                  className="ms-auto"
                  aria-label={t("Enabled")}
                  checked={op.enabled}
                  onClick={(e) => e.stopPropagation()}
                  onChange={() => void commit({ ...def, operations: toggleOperation(ops, op.id) })}
                />
              </div>
              <div className="an-op-summary">{describeOperation(op, t, datasetName)}</div>
              {invalid && <div className="an-op-error">{r?.error}</div>}
              {r?.status === "not_reached" && (
                <div className="small text-secondary">
                  <T text="Not reached: an earlier operation has an error." />
                </div>
              )}
              {upto === i + 1 && (
                <div className="d-flex gap-2 mt-1">
                  <Button
                    size="sm"
                    variant="outline-secondary"
                    onClick={(e) => {
                      e.stopPropagation();
                      setEditing({ op, mode: "edit", position: i });
                    }}
                  >
                    <T text="Edit" />
                  </Button>
                  <Button
                    size="sm"
                    variant="outline-danger"
                    onClick={(e) => {
                      e.stopPropagation();
                      onSelect(i === 0 ? "" : ops[i - 1].id);
                      void commit({ ...def, operations: removeOperation(ops, op.id) });
                    }}
                  >
                    <T text="Delete" />
                  </Button>
                </div>
              )}
            </div>
          );
        })}

        <Dropdown className="mt-2">
          <Dropdown.Toggle variant="primary" size="sm">
            <T text="Add an operation" />
          </Dropdown.Toggle>
          <Dropdown.Menu>
            {(["keep", "change", "combine"] as const).map((group) => (
              <div key={group}>
                <Dropdown.Header>
                  {group === "keep"
                    ? t("Keep the rows")
                    : group === "change"
                      ? t("Change what a row is")
                      : t("Combine")}
                </Dropdown.Header>
                {OP_KINDS.filter((k) => k.group === group).map((k) => (
                  <Dropdown.Item key={k.kind} onClick={() => add(k.kind)} title={opKindAbout(k.kind, t)}>
                    {opKindName(k.kind, t)}
                  </Dropdown.Item>
                ))}
              </div>
            ))}
          </Dropdown.Menu>
        </Dropdown>
        <p className="small text-secondary mt-2">
          <T text="New operations go after the one selected. Double-click one to edit it; drag to reorder." />
        </p>
      </aside>

      <section className="an-stage">
        <StageGrid
          def={def}
          upto={upto}
          selected={ticked.filter((c) => shape?.columns.some((s) => s.name === c))}
          onToggle={(column) =>
            setTicked((list) => (list.includes(column) ? list.filter((c) => c !== column) : [...list, column]))
          }
          onAction={onHeader}
          onAddColumn={() => add("calculated")}
        />
      </section>

      {editing && (
        <OperationForm
          op={editing.op}
          mode={editing.mode}
          position={editing.position}
          def={def}
          columns={stageShape(report, editing.position)?.columns ?? []}
          completions={
            editing.position === upto
              ? completions
              : formulaCompletions(stageShape(report, editing.position), report)
          }
          others={others}
          onApply={(op) => apply(editing, op)}
          onClose={() => setEditing(null)}
        />
      )}
    </div>
  );
}
