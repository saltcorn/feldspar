// One operation, edited in a form of its own kind (analytics TODO A1.17).
//
// Every kind has its own fields, over the columns of the stage it reads, and
// every form is checked by the server as it is edited: the operation is
// compiled where it would go (`validateDatasetOperation`), and the form shows
// either the sentence saying why it would not work or the columns it would
// make. So nothing reaches the list that the admin has not seen the effect of.

import { useEffect, useMemo, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Row from "react-bootstrap/Row";

import { api, errorMessage } from "../api";
import type { ListDatasetTablesResponse } from "../client";
import { T, useT } from "../i18n";
import type { DatasetItem } from "./DatasetList";
import { opKindName, summaryName, windowFunctionName } from "../labels";
import { FormulaInput } from "./FormulaInput";
import {
  SUMMARY_FUNCTIONS,
  WINDOW_FUNCTIONS,
  type Completion,
  type DatasetDef,
  type Operation,
  type StageColumn,
  type StageShape,
} from "./ops";

type TableItem = ListDatasetTablesResponse[number];

/** The tables and datasets a Join or a Union may read. */
export type Others = { tables: TableItem[]; datasets: DatasetItem[] };

type Params = Record<string, unknown>;

/** How long the form waits after an edit before asking the server. */
const VALIDATE_DELAY_MS = 400;

const s = (v: unknown): string => (typeof v === "string" ? v : "");
const n = (v: unknown, fallback = 0): number => (typeof v === "number" ? v : fallback);
const list = <T,>(v: unknown): T[] => (Array.isArray(v) ? (v as T[]) : []);
const obj = (v: unknown): Params =>
  v && typeof v === "object" && !Array.isArray(v) ? (v as Params) : {};

export function OperationForm({
  op,
  mode,
  position,
  def,
  columns,
  completions,
  others,
  onApply,
  onClose,
}: {
  op: Operation;
  mode: "new" | "edit";
  position: number;
  def: DatasetDef;
  columns: StageColumn[];
  completions: Completion[];
  others: Others;
  onApply: (op: Operation) => void;
  onClose: () => void;
}) {
  const { t } = useT();
  const [params, setParams] = useState<Params>(op.params);
  const [check, setCheck] = useState<{ error?: string | null; shape?: StageShape | null } | null>(
    null,
  );
  const draft = useMemo(() => ({ ...op, params }), [op, params]);
  const draftJson = JSON.stringify(draft);

  useEffect(() => {
    let live = true;
    const timer = window.setTimeout(() => {
      api
        .validateDatasetOperation({
          dataset: def,
          position,
          replace: mode === "edit",
          operation: JSON.parse(draftJson) as Operation,
        })
        .then((answer) => {
          if (live) setCheck({ error: answer.error, shape: answer.shape as StageShape | null });
        })
        .catch((err: unknown) => {
          if (live) setCheck({ error: errorMessage(err, t("Could not check the operation.")) });
        });
    }, VALIDATE_DELAY_MS);
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [draftJson, def, position, mode, t]);

  const set = (key: string, value: unknown) => setParams((p) => ({ ...p, [key]: value }));
  const names = columns.map((c) => c.name);
  const fields = (() => {
    switch (op.kind) {
      case "calculated":
        return (
          <>
            <Field label={t("Column name")} id="op-name">
              <Form.Control id="op-name" value={s(params.name)} onChange={(e) => set("name", e.target.value)} />
            </Field>
            <Field label={t("Formula")} id="op-formula" help={t("For example price / area, or neighbourhoodⱵname.")}>
              <FormulaInput
                id="op-formula"
                autoFocus
                value={s(params.formula)}
                onChange={(v) => set("formula", v)}
                completions={completions}
              />
            </Field>
          </>
        );
      case "filter":
        return (
          <Field label={t("Keep the rows where")} id="op-formula" help={t("A condition, for example price > 100000.")}>
            <FormulaInput
              id="op-formula"
              autoFocus
              value={s(params.formula)}
              onChange={(v) => set("formula", v)}
              completions={completions}
            />
          </Field>
        );
      case "select":
        return <SelectFields params={params} set={set} columns={columns} />;
      case "sort":
        return <SortFields params={params} set={set} completions={completions} />;
      case "window":
        return <WindowFields params={params} set={set} names={names} />;
      case "aggregate":
        return <AggregateFields params={params} set={set} names={names} completions={completions} />;
      case "limit":
        return <LimitFields params={params} set={set} names={names} />;
      case "stack":
        return (
          <>
            <Field label={t("Columns to stack")} id="op-stack">
              <ColumnChecks value={list<string>(params.columns)} onChange={(v) => set("columns", v)} names={names} />
            </Field>
            <Row>
              <Col>
                <Field label={t("Name column")} id="op-names-to">
                  <Form.Control id="op-names-to" value={s(params.names_to)} onChange={(e) => set("names_to", e.target.value)} />
                </Field>
              </Col>
              <Col>
                <Field label={t("Value column")} id="op-values-to">
                  <Form.Control id="op-values-to" value={s(params.values_to)} onChange={(e) => set("values_to", e.target.value)} />
                </Field>
              </Col>
            </Row>
          </>
        );
      case "split":
        return <SplitFields params={params} set={set} names={names} def={def} position={position} />;
      case "complete":
        return <CompleteFields params={params} set={set} names={names} />;
      case "join":
        return <JoinFields params={params} set={set} names={names} others={others} />;
      case "union":
        return <UnionFields params={params} set={set} others={others} />;
    }
  })();

  return (
    <Modal show onHide={onClose} size="lg">
      <Form
        onSubmit={(e) => {
          e.preventDefault();
          onApply(draft);
        }}
      >
        <Modal.Header closeButton>
          <Modal.Title>
            {mode === "new" ? t("Add: {kind}", { kind: opKindName(op.kind, t) }) : opKindName(op.kind, t)}
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {fields}
          {check?.error && <Alert variant="warning" className="mb-0">{check.error}</Alert>}
          {check && !check.error && check.shape && (
            <div className="text-secondary small">
              <T
                text="Columns after it: {columns}"
                args={{ columns: check.shape.columns.map((c) => c.name).join(", ") }}
              />
            </div>
          )}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={onClose}>
            <T text="Cancel" />
          </Button>
          <Button type="submit">{mode === "new" ? t("Add") : t("Apply")}</Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}

// --- the pieces ---------------------------------------------------------------

function Field({ label, id, help, children }: { label: string; id: string; help?: string; children: ReactNode }) {
  return (
    <Form.Group className="mb-3" controlId={id}>
      <Form.Label>{label}</Form.Label>
      {children}
      {help && <Form.Text muted>{help}</Form.Text>}
    </Form.Group>
  );
}

function ColumnSelect({
  value,
  onChange,
  names,
  empty,
  id,
}: {
  value: string;
  onChange: (v: string) => void;
  names: string[];
  empty?: string;
  id?: string;
}) {
  return (
    <Form.Select id={id} value={value} onChange={(e) => onChange(e.target.value)}>
      {empty !== undefined && <option value="">{empty}</option>}
      {value !== "" && !names.includes(value) && <option value={value}>{value}</option>}
      {names.map((name) => (
        <option key={name} value={name}>
          {name}
        </option>
      ))}
    </Form.Select>
  );
}

function ColumnChecks({
  value,
  onChange,
  names,
}: {
  value: string[];
  onChange: (v: string[]) => void;
  names: string[];
}) {
  return (
    <div className="d-flex flex-wrap gap-3">
      {names.map((name) => (
        <Form.Check
          key={name}
          type="checkbox"
          id={`check-${name}`}
          label={name}
          checked={value.includes(name)}
          onChange={(e) =>
            onChange(e.target.checked ? [...value, name] : value.filter((v) => v !== name))
          }
        />
      ))}
    </div>
  );
}

type OrderKey = { column: string; descending?: boolean };

function OrderKeys({
  value,
  onChange,
  names,
}: {
  value: OrderKey[];
  onChange: (v: OrderKey[]) => void;
  names: string[];
}) {
  const { t } = useT();
  return (
    <>
      {value.map((key, i) => (
        <div className="d-flex gap-2 mb-2" key={i}>
          <ColumnSelect
            value={key.column}
            names={names}
            onChange={(column) => onChange(value.map((k, j) => (j === i ? { ...k, column } : k)))}
          />
          <Form.Check
            type="checkbox"
            id={`order-desc-${i}`}
            label={t("descending")}
            checked={Boolean(key.descending)}
            onChange={(e) =>
              onChange(value.map((k, j) => (j === i ? { ...k, descending: e.target.checked } : k)))
            }
          />
          <Button size="sm" variant="outline-danger" onClick={() => onChange(value.filter((_, j) => j !== i))}>
            ×
          </Button>
        </div>
      ))}
      <Button size="sm" variant="outline-secondary" onClick={() => onChange([...value, { column: names[0] ?? "" }])}>
        <T text="Add an order key" />
      </Button>
    </>
  );
}

type Setter = (key: string, value: unknown) => void;

function SelectFields({ params, set, columns }: { params: Params; set: Setter; columns: StageColumn[] }) {
  const { t } = useT();
  const kept = list<{ column: string; rename?: string }>(params.columns);
  const keptNames = kept.map((c) => c.column);
  const move = (i: number, by: number) => {
    const next = [...kept];
    const j = i + by;
    if (j < 0 || j >= next.length) return;
    [next[i], next[j]] = [next[j], next[i]];
    set("columns", next);
  };
  return (
    <>
      <p className="text-secondary small">
        <T text="The columns kept, in order; untick one to drop it, and give it a new name to rename it." />
      </p>
      {kept.map((c, i) => (
        <div className="d-flex gap-2 mb-2 align-items-center" key={c.column}>
          <Form.Check
            type="checkbox"
            id={`keep-${c.column}`}
            checked
            onChange={() => set("columns", kept.filter((_, j) => j !== i))}
            label={c.column}
            className="flex-grow-1"
          />
          <Form.Control
            size="sm"
            style={{ maxWidth: "12rem" }}
            value={c.rename ?? ""}
            placeholder={t("rename to…")}
            onChange={(e) =>
              set(
                "columns",
                kept.map((k, j) => (j === i ? { ...k, rename: e.target.value || undefined } : k)),
              )
            }
          />
          <Button size="sm" variant="outline-secondary" onClick={() => move(i, -1)} aria-label={t("Move up")}>
            ↑
          </Button>
          <Button size="sm" variant="outline-secondary" onClick={() => move(i, 1)} aria-label={t("Move down")}>
            ↓
          </Button>
        </div>
      ))}
      {columns
        .filter((c) => !keptNames.includes(c.name))
        .map((c) => (
          <Form.Check
            key={c.name}
            type="checkbox"
            id={`keep-${c.name}`}
            checked={false}
            label={c.name}
            className="text-secondary"
            onChange={() => set("columns", [...kept, { column: c.name }])}
          />
        ))}
    </>
  );
}

function SortFields({ params, set, completions }: { params: Params; set: Setter; completions: Completion[] }) {
  const { t } = useT();
  const keys = list<{ formula: string; descending?: boolean }>(params.keys);
  return (
    <>
      {keys.map((key, i) => (
        <div className="d-flex gap-2 mb-2" key={i}>
          <div className="flex-grow-1">
            <FormulaInput
              value={key.formula}
              completions={completions}
              onChange={(formula) => set("keys", keys.map((k, j) => (j === i ? { ...k, formula } : k)))}
            />
          </div>
          <Form.Check
            type="checkbox"
            id={`sort-desc-${i}`}
            label={t("descending")}
            checked={Boolean(key.descending)}
            onChange={(e) =>
              set("keys", keys.map((k, j) => (j === i ? { ...k, descending: e.target.checked } : k)))
            }
          />
          <Button size="sm" variant="outline-danger" onClick={() => set("keys", keys.filter((_, j) => j !== i))}>
            ×
          </Button>
        </div>
      ))}
      <Button size="sm" variant="outline-secondary" onClick={() => set("keys", [...keys, { formula: "", descending: false }])}>
        <T text="Add a key" />
      </Button>
    </>
  );
}

function WindowFields({ params, set, names }: { params: Params; set: Setter; names: string[] }) {
  const { t } = useT();
  const fn = WINDOW_FUNCTIONS.find((f) => f.value === params.function) ?? WINDOW_FUNCTIONS[0];
  return (
    <>
      <Row>
        <Col md={6}>
          <Field label={t("Column name")} id="op-name">
            <Form.Control id="op-name" value={s(params.name)} onChange={(e) => set("name", e.target.value)} />
          </Field>
        </Col>
        <Col md={6}>
          <Field label={t("Computes")} id="op-function">
            <Form.Select id="op-function" value={fn.value} onChange={(e) => set("function", e.target.value)}>
              {WINDOW_FUNCTIONS.map((f) => (
                <option key={f.value} value={f.value}>
                  {windowFunctionName(f.value, t)}
                </option>
              ))}
            </Form.Select>
          </Field>
        </Col>
      </Row>
      {fn.column && (
        <Row>
          <Col md={6}>
            <Field label={t("Of the column")} id="op-column">
              <ColumnSelect id="op-column" value={s(params.column)} names={names} onChange={(v) => set("column", v)} />
            </Field>
          </Col>
          {(fn.value === "lag" || fn.value === "lead") && (
            <Col md={6}>
              <Field label={t("Rows away")} id="op-offset">
                <Form.Control
                  id="op-offset"
                  type="number"
                  min={1}
                  value={n(params.offset, 1)}
                  onChange={(e) => set("offset", Number(e.target.value) || 1)}
                />
              </Field>
            </Col>
          )}
        </Row>
      )}
      <Field label={t("Within groups of")} id="op-partition" help={t("None ticked: the rows as one group.")}>
        <ColumnChecks value={list<string>(params.partition)} onChange={(v) => set("partition", v)} names={names} />
      </Field>
      <Field label={t("In the order")} id="op-order" help={t("None: the order the rows already have.")}>
        <OrderKeys value={list<OrderKey>(params.order)} onChange={(v) => set("order", v)} names={names} />
      </Field>
    </>
  );
}

function AggregateFields({
  params,
  set,
  names,
  completions,
}: {
  params: Params;
  set: Setter;
  names: string[];
  completions: Completion[];
}) {
  const { t } = useT();
  const groups = list<{ name: string; formula: string }>(params.group_by);
  const summaries = list<{ name: string; function: string; column?: string; order?: OrderKey }>(params.summaries);
  return (
    <>
      <h4 className="h5">
        <T text="Group by" />
      </h4>
      {groups.map((g, i) => (
        <div className="d-flex gap-2 mb-2" key={i}>
          <Form.Control
            style={{ maxWidth: "12rem" }}
            value={g.name}
            placeholder={t("name")}
            aria-label={t("Group key {n} name", { n: i + 1 })}
            onChange={(e) => set("group_by", groups.map((k, j) => (j === i ? { ...k, name: e.target.value } : k)))}
          />
          <div className="flex-grow-1">
            <FormulaInput
              value={g.formula}
              completions={completions}
              onChange={(formula) =>
                set(
                  "group_by",
                  groups.map((k, j) =>
                    j === i ? { formula, name: k.name === k.formula || k.name === "" ? formula : k.name } : k,
                  ),
                )
              }
            />
          </div>
          <Button size="sm" variant="outline-danger" onClick={() => set("group_by", groups.filter((_, j) => j !== i))}>
            ×
          </Button>
        </div>
      ))}
      <Button
        size="sm"
        variant="outline-secondary"
        className="mb-3"
        onClick={() => set("group_by", [...groups, { name: names[0] ?? "", formula: names[0] ?? "" }])}
      >
        <T text="Add a group key" />
      </Button>
      <h4 className="h5">
        <T text="Summaries" />
      </h4>
      <p className="text-secondary small">
        <T text="With no summaries, the result is the distinct combinations of the group keys." />
      </p>
      {summaries.map((sm, i) => {
        const edit = (over: Params) => set("summaries", summaries.map((k, j) => (j === i ? { ...k, ...over } : k)));
        return (
          <div className="d-flex gap-2 mb-2 flex-wrap" key={i}>
            <Form.Control
              style={{ maxWidth: "10rem" }}
              value={sm.name}
              placeholder={t("name")}
              aria-label={t("Summary {n} name", { n: i + 1 })}
              onChange={(e) => edit({ name: e.target.value })}
            />
            <Form.Select style={{ maxWidth: "12rem" }} value={sm.function} onChange={(e) => edit({ function: e.target.value })}>
              {SUMMARY_FUNCTIONS.map((f) => (
                <option key={f.value} value={f.value}>
                  {summaryName(f.value, t)}
                </option>
              ))}
            </Form.Select>
            <div style={{ maxWidth: "12rem" }}>
              <ColumnSelect
                value={sm.column ?? ""}
                names={names}
                empty={sm.function === "count" ? t("(rows)") : ""}
                onChange={(column) => edit({ column: column || undefined })}
              />
            </div>
            {(sm.function === "first" || sm.function === "last") && (
              <div style={{ maxWidth: "12rem" }}>
                <ColumnSelect
                  value={sm.order?.column ?? ""}
                  names={names}
                  empty={t("(by the rows' order)")}
                  onChange={(column) => edit({ order: column ? { column } : undefined })}
                />
              </div>
            )}
            <Button size="sm" variant="outline-danger" onClick={() => set("summaries", summaries.filter((_, j) => j !== i))}>
              ×
            </Button>
          </div>
        );
      })}
      <Button
        size="sm"
        variant="outline-secondary"
        className="mb-3"
        onClick={() => set("summaries", [...summaries, { name: `summary_${summaries.length + 1}`, function: "count" }])}
      >
        <T text="Add a summary" />
      </Button>
    </>
  );
}

function LimitFields({ params, set, names }: { params: Params; set: Setter; names: string[] }) {
  const { t } = useT();
  const mode = s(params.mode) || "first";
  return (
    <>
      <Row>
        <Col md={6}>
          <Field label={t("Keep")} id="op-mode">
            <Form.Select id="op-mode" value={mode} onChange={(e) => set("mode", e.target.value)}>
              <option value="first">{t("the first rows")}</option>
              <option value="sample">{t("a random sample")}</option>
              <option value="top">{t("the first rows of each group")}</option>
            </Form.Select>
          </Field>
        </Col>
        <Col md={3}>
          <Field label={t("How many")} id="op-n">
            <Form.Control id="op-n" type="number" min={1} value={n(params.n, 100)} onChange={(e) => set("n", Number(e.target.value))} />
          </Field>
        </Col>
        {mode === "sample" && (
          <Col md={3}>
            <Field label={t("Seed")} id="op-seed">
              <Form.Control id="op-seed" type="number" value={n(params.seed)} onChange={(e) => set("seed", Number(e.target.value))} />
            </Field>
          </Col>
        )}
      </Row>
      {mode === "top" && (
        <>
          <Field label={t("Groups")} id="op-groups">
            <ColumnChecks value={list<string>(params.group_by)} onChange={(v) => set("group_by", v)} names={names} />
          </Field>
          <Field label={t("Ranked by")} id="op-order">
            <OrderKeys value={list<OrderKey>(params.order)} onChange={(v) => set("order", v)} names={names} />
          </Field>
        </>
      )}
    </>
  );
}

function SplitFields({
  params,
  set,
  names,
  def,
  position,
}: {
  params: Params;
  set: Setter;
  names: string[];
  def: DatasetDef;
  position: number;
}) {
  const { t } = useT();
  const [reading, setReading] = useState<string | null>(null);
  const readValues = async () => {
    setReading(null);
    try {
      const values = await api.datasetColumnValues({ dataset: def, upto: position, column: s(params.names_from) });
      set("values", values.map((v) => String(v)));
    } catch (err) {
      setReading(errorMessage(err, t("Could not read the values.")));
    }
  };
  return (
    <>
      <Row>
        <Col md={6}>
          <Field label={t("New columns named by")} id="op-names-from">
            <ColumnSelect id="op-names-from" value={s(params.names_from)} names={names} empty="" onChange={(v) => set("names_from", v)} />
          </Field>
        </Col>
        <Col md={6}>
          <Field label={t("Filled from")} id="op-values-from">
            <ColumnSelect id="op-values-from" value={s(params.values_from)} names={names} empty="" onChange={(v) => set("values_from", v)} />
          </Field>
        </Col>
      </Row>
      <Field label={t("One row per")} id="op-ids">
        <ColumnChecks value={list<string>(params.id_columns)} onChange={(v) => set("id_columns", v)} names={names} />
      </Field>
      <Field
        label={t("The new columns")}
        id="op-values"
        help={t("Fixed when the operation is defined, so a new value in the data does not change the columns later operations use. One per line.")}
      >
        <Form.Control
          as="textarea"
          rows={4}
          id="op-values"
          value={list<string>(params.values).join("\n")}
          onChange={(e) => set("values", e.target.value.split("\n").filter((v) => v.trim() !== ""))}
        />
        <Button size="sm" variant="outline-secondary" className="mt-2" disabled={!s(params.names_from)} onClick={() => void readValues()}>
          <T text="Read them from the data" />
        </Button>
        {reading && <div className="text-danger small">{reading}</div>}
      </Field>
      <Field label={t("Several values in one cell")} id="op-summary">
        <Form.Select id="op-summary" value={s(params.summary) || "first"} onChange={(e) => set("summary", e.target.value)}>
          <option value="first">{t("take one")}</option>
          <option value="sum">{t("add them up")}</option>
          <option value="mean">{t("their mean")}</option>
          <option value="count">{t("count them")}</option>
          <option value="min">{t("the smallest")}</option>
          <option value="max">{t("the largest")}</option>
        </Form.Select>
      </Field>
    </>
  );
}

type CompleteColumn = { column: string; values: { source: string; from?: unknown; to?: unknown; step?: unknown } };

function CompleteFields({ params, set, names }: { params: Params; set: Setter; names: string[] }) {
  const { t } = useT();
  const columns = list<CompleteColumn>(params.columns);
  const fill = list<{ column: string; value: unknown }>(params.fill);
  const edit = (i: number, over: Partial<CompleteColumn>) =>
    set("columns", columns.map((c, j) => (j === i ? { ...c, ...over } : c)));
  const parse = (text: string): unknown => {
    const number = Number(text);
    return text.trim() !== "" && !Number.isNaN(number) ? number : text;
  };
  return (
    <>
      <h4 className="h5">
        <T text="Every combination of" />
      </h4>
      {columns.map((c, i) => (
        <div className="d-flex gap-2 mb-2 flex-wrap" key={i}>
          <div style={{ maxWidth: "12rem" }}>
            <ColumnSelect value={c.column} names={names} onChange={(column) => edit(i, { column })} />
          </div>
          <Form.Select
            style={{ maxWidth: "14rem" }}
            value={c.values.source}
            onChange={(e) => edit(i, { values: { source: e.target.value } })}
          >
            <option value="data">{t("the values in the data")}</option>
            <option value="range">{t("a range")}</option>
            <option value="table">{t("every row of the table it refers to")}</option>
          </Form.Select>
          {c.values.source === "range" && (
            <>
              <Form.Control
                style={{ maxWidth: "8rem" }}
                placeholder={t("from")}
                value={String(c.values.from ?? "")}
                onChange={(e) => edit(i, { values: { ...c.values, from: parse(e.target.value) } })}
              />
              <Form.Control
                style={{ maxWidth: "8rem" }}
                placeholder={t("to")}
                value={String(c.values.to ?? "")}
                onChange={(e) => edit(i, { values: { ...c.values, to: parse(e.target.value) } })}
              />
              <Form.Control
                style={{ maxWidth: "8rem" }}
                placeholder={t("step: 1, or month")}
                value={String(c.values.step ?? "")}
                onChange={(e) =>
                  edit(i, { values: { ...c.values, step: e.target.value === "" ? undefined : parse(e.target.value) } })
                }
              />
            </>
          )}
          <Button size="sm" variant="outline-danger" onClick={() => set("columns", columns.filter((_, j) => j !== i))}>
            ×
          </Button>
        </div>
      ))}
      <Button
        size="sm"
        variant="outline-secondary"
        className="mb-3"
        onClick={() => set("columns", [...columns, { column: names[0] ?? "", values: { source: "data" } }])}
      >
        <T text="Add a column" />
      </Button>
      <h4 className="h5">
        <T text="In the added rows" />
      </h4>
      {fill.map((f, i) => (
        <div className="d-flex gap-2 mb-2" key={i}>
          <div style={{ maxWidth: "12rem" }}>
            <ColumnSelect
              value={f.column}
              names={names}
              onChange={(column) => set("fill", fill.map((k, j) => (j === i ? { ...k, column } : k)))}
            />
          </div>
          <Form.Control
            style={{ maxWidth: "10rem" }}
            value={String(f.value ?? "")}
            onChange={(e) => set("fill", fill.map((k, j) => (j === i ? { ...k, value: parse(e.target.value) } : k)))}
          />
          <Button size="sm" variant="outline-danger" onClick={() => set("fill", fill.filter((_, j) => j !== i))}>
            ×
          </Button>
        </div>
      ))}
      <Button size="sm" variant="outline-secondary" onClick={() => set("fill", [...fill, { column: names[0] ?? "", value: 0 }])}>
        <T text="Fill a column" />
      </Button>
    </>
  );
}

/** Which table or dataset a Join or Union reads, as the picker's value. */
function otherValue(other: unknown): string {
  const o = obj(other);
  return o.kind === "dataset" ? `dataset:${s(o.dataset)}` : `table:${s(o.table)}`;
}

function parseOther(value: string): Params {
  const [kind, rest] = value.split(/:(.*)/s, 2);
  return kind === "dataset" ? { kind: "dataset", dataset: rest } : { kind: "table", table: rest };
}

/** The columns of what a Join or Union reads. */
function otherColumns(other: unknown, others: Others): string[] {
  const o = obj(other);
  const raw =
    o.kind === "dataset"
      ? others.datasets.find((d) => d.id === o.dataset)?.columns
      : others.tables.find((tb) => tb.name === o.table)?.columns;
  return (raw ?? []).map((c) => (c as { name: string }).name);
}

function OtherPicker({ value, onChange, others }: { value: unknown; onChange: (v: Params) => void; others: Others }) {
  const { t } = useT();
  return (
    <Form.Select value={otherValue(value)} onChange={(e) => onChange(parseOther(e.target.value))}>
      <option value="table:">{t("(choose)")}</option>
      <optgroup label={t("Tables")}>
        {others.tables.map((tb) => (
          <option key={tb.name} value={`table:${tb.name}`}>
            {tb.name}
          </option>
        ))}
      </optgroup>
      <optgroup label={t("Datasets")}>
        {others.datasets.map((d) => (
          <option key={d.id} value={`dataset:${d.id}`}>
            {d.name}
          </option>
        ))}
      </optgroup>
    </Form.Select>
  );
}

function JoinFields({ params, set, names, others }: { params: Params; set: Setter; names: string[]; others: Others }) {
  const { t } = useT();
  const on = list<{ left: string; right: string }>(params.on);
  const right = otherColumns(params.with, others);
  const asof = params.asof ? (obj(params.asof) as { left: string; right: string }) : null;
  return (
    <>
      <Row>
        <Col md={8}>
          <Field label={t("Join")} id="op-with">
            <OtherPicker value={params.with} onChange={(v) => set("with", v)} others={others} />
          </Field>
        </Col>
        <Col md={4}>
          <Field label={t("Keeping")} id="op-kind">
            <Form.Select id="op-kind" value={s(params.kind) || "left"} onChange={(e) => set("kind", e.target.value)}>
              <option value="left">{t("every row here (left)")}</option>
              <option value="inner">{t("matched rows only (inner)")}</option>
              <option value="full">{t("every row of both (full)")}</option>
            </Form.Select>
          </Field>
        </Col>
      </Row>
      <h4 className="h5">
        <T text="Matching" />
      </h4>
      {on.map((k, i) => (
        <div className="d-flex gap-2 mb-2 align-items-center" key={i}>
          <ColumnSelect value={k.left} names={names} onChange={(left) => set("on", on.map((x, j) => (j === i ? { ...x, left } : x)))} />
          <span>=</span>
          <ColumnSelect value={k.right} names={right} onChange={(r) => set("on", on.map((x, j) => (j === i ? { ...x, right: r } : x)))} />
          <Button size="sm" variant="outline-danger" onClick={() => set("on", on.filter((_, j) => j !== i))}>
            ×
          </Button>
        </div>
      ))}
      <Button
        size="sm"
        variant="outline-secondary"
        className="mb-3"
        onClick={() => set("on", [...on, { left: names[0] ?? "", right: right[0] ?? "" }])}
      >
        <T text="Add a key" />
      </Button>
      <Form.Check
        type="switch"
        id="op-asof"
        className="mb-2"
        label={t("Also match the nearest earlier date (as of)")}
        checked={asof !== null}
        onChange={(e) => set("asof", e.target.checked ? { left: names[0] ?? "", right: right[0] ?? "" } : undefined)}
      />
      {asof && (
        <div className="d-flex gap-2 mb-3 align-items-center">
          <ColumnSelect value={asof.left} names={names} onChange={(left) => set("asof", { ...asof, left })} />
          <span>≥</span>
          <ColumnSelect value={asof.right} names={right} onChange={(r) => set("asof", { ...asof, right: r })} />
        </div>
      )}
      <Field label={t("Suffix for a column name already taken")} id="op-suffix">
        <Form.Control id="op-suffix" value={s(params.suffix)} onChange={(e) => set("suffix", e.target.value)} />
      </Field>
    </>
  );
}

function UnionFields({ params, set, others }: { params: Params; set: Setter; others: Others }) {
  const { t } = useT();
  return (
    <>
      <Field label={t("Append the rows of")} id="op-with" help={t("Columns are matched by name.")}>
        <OtherPicker value={params.with} onChange={(v) => set("with", v)} others={others} />
      </Field>
      <Field label={t("A column saying where each row came from (optional)")} id="op-source">
        <Form.Control
          id="op-source"
          value={s(params.source_column)}
          onChange={(e) => set("source_column", e.target.value || undefined)}
        />
      </Field>
    </>
  );
}
