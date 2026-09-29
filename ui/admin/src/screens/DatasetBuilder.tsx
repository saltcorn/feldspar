// A dataset's builder: its table, its filter, its columns (each a name and a
// formula, with a picker that writes formulas), optionally its order, and a
// preview of what it answers (TODO "Predictive models" 6.2; Stan TODO §18).
//
// One component because a model now has several datasets — its own and the
// related ones a posterior indexes into (Stan TODO §7) — and each is built
// exactly as the main one is. Two builders would be two places for "what a
// dataset is" to drift.
//
// The picker writes a **formula** and nothing else (§2): three groups, one
// language, and every choice is editable in the row it lands in. The preview
// is what makes a formula a thing you can see the answer of before you fit
// against it — and the types it reports are the *data's*, which no schema
// carries.

import { useEffect, useMemo, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { PreviewDatasetResponse } from "../client";
import type { TableInfo } from "../codeTypes";
import { IconPlus, IconTrash } from "../icons";
import {
  formulaChoices,
  suggestColumnName,
  uniqueColumnName,
  type Dataset,
  type DatasetColumn,
  type DatasetOrder,
} from "../models";
import { T, useT } from "../i18n";

/** How long the builder waits after a keystroke before asking the server what
 * the dataset answers. Long enough that typing a formula is not a request per
 * character, short enough that stopping typing shows the rows. */
const DEBOUNCE_MS = 600;

/** The dataset as the API takes it: the rows with neither a name nor a formula
 * left out, a blank filter as none, and an order key with no formula dropped. */
export function datasetBody(dataset: Dataset): Dataset {
  const order = (dataset.order ?? [])
    .filter((o) => o.expr.trim() !== "")
    .map((o) => (o.descending ? { expr: o.expr.trim(), descending: true } : { expr: o.expr.trim() }));
  return {
    table: dataset.table,
    columns: dataset.columns.filter((c) => c.name.trim() !== "" || c.expr.trim() !== ""),
    filter: dataset.filter && dataset.filter.trim() !== "" ? dataset.filter.trim() : null,
    ...(order.length > 0 ? { order } : {}),
  };
}

export function DatasetBuilder({
  dataset,
  onChange,
  tables,
  schema,
  idPrefix,
  withOrder = false,
  previewNote,
}: {
  dataset: Dataset;
  onChange: (next: Dataset) => void;
  tables: string[];
  schema: TableInfo[];
  idPrefix: string;
  /** Edit the order too: a posterior's rows are ordered (Stan TODO §7), and
   * every other provider ignores it, so it is offered only where it means
   * something. */
  withOrder?: boolean;
  /** A sentence under the preview saying what it is for here, given what the
   * preview answered. */
  previewNote?: (preview: PreviewDatasetResponse) => ReactNode;
}) {
  const { t } = useT();
  const [preview, setPreview] = useState<PreviewDatasetResponse | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);

  const body = datasetBody(dataset);
  const bodyJson = JSON.stringify(body);

  // Skipped while there are no columns, because "it has no columns" is a
  // sentence about a dataset that has not been built yet rather than one that
  // is wrong.
  useEffect(() => {
    const parsed = JSON.parse(bodyJson) as Dataset;
    if (parsed.table === "" || parsed.columns.length === 0) {
      setPreview(null);
      setPreviewError(null);
      return undefined;
    }
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void api
        .previewDataset({ dataset: parsed, limit: 10 })
        .then((answer) => {
          if (cancelled) return;
          setPreview(answer);
          setPreviewError(null);
        })
        .catch((err: unknown) => {
          if (cancelled) return;
          setPreview(null);
          setPreviewError(errorMessage(err, "This dataset could not be read."));
        });
    }, DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [bodyJson]);

  const choices = useMemo(() => formulaChoices(schema, dataset.table), [schema, dataset.table]);
  const grouped = useMemo(() => {
    const groups = new Map<string, { label: string; expr: string; index: number }[]>();
    choices.forEach((choice, index) => {
      const list = groups.get(choice.group) ?? [];
      list.push({ label: choice.label, expr: choice.expr, index });
      groups.set(choice.group, list);
    });
    return [...groups.entries()];
  }, [choices]);

  const columns = dataset.columns;
  const order = dataset.order ?? [];
  const setColumns = (next: DatasetColumn[]) => onChange({ ...dataset, columns: next });
  const setOrder = (next: DatasetOrder[]) => onChange({ ...dataset, order: next });
  const editColumn = (index: number, over: Partial<DatasetColumn>) =>
    setColumns(columns.map((c, i) => (i === index ? { ...c, ...over } : c)));

  const addColumn = (index: number) => {
    const choice = choices[index];
    if (!choice) return;
    setColumns([
      ...columns,
      {
        name: uniqueColumnName(
          choice.name,
          columns.map((c) => c.name),
        ),
        expr: choice.expr,
      },
    ]);
  };

  return (
    <>
      <Row>
        <Col md={4}>
          <Form.Group className="mb-3" controlId={`${idPrefix}-table`}>
            <Form.Label>
              <T text="Table" /><span className="text-danger"> *</span>
            </Form.Label>
            <Form.Select
              value={dataset.table}
              onChange={(e) => onChange({ ...dataset, table: e.target.value })}
            >
              {tables.every((name) => name !== dataset.table) && dataset.table !== "" && (
                <option value={dataset.table}>
                  {t("{name} (missing)", { name: dataset.table })}
                </option>
              )}
              {dataset.table === "" && <option value="">—</option>}
              {tables.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </Form.Select>
            <Form.Text muted>
              <T text="The table every formula below is written over. Changing it leaves the columns as they are — they are formulas, and most of them will not resolve over another table." />
            </Form.Text>
          </Form.Group>
        </Col>
        <Col md={8}>
          <Form.Group className="mb-3" controlId={`${idPrefix}-filter`}>
            <Form.Label><T text="Filter" /></Form.Label>
            <Form.Control
              className="font-monospace"
              value={dataset.filter ?? ""}
              placeholder={t("sold")}
              onChange={(e) => onChange({ ...dataset, filter: e.target.value })}
            />
            <Form.Text muted>
              <T text="One boolean formula deciding which rows are in the data, or blank for all of them." /> <code>user</code> <T text="and the operation flags may not be used: a dataset has no caller." />
            </Form.Text>
          </Form.Group>
        </Col>
      </Row>

      <Table size="sm" className="mb-2">
        <thead>
          <tr>
            <th style={{ width: "30%" }}><T text="Column" /></th>
            <th><T text="Formula" /></th>
            <th style={{ width: "1%" }} />
          </tr>
        </thead>
        <tbody>
          {columns.length === 0 && (
            <tr>
              <td colSpan={3} className="text-muted">
                <T text="No columns yet. Pick one below, or add a blank row and write a formula." />
              </td>
            </tr>
          )}
          {columns.map((column, index) => (
            <tr key={index}>
              <td>
                <Form.Control
                  size="sm"
                  value={column.name}
                  aria-label={t("Column {n} name", { n: index + 1 })}
                  onChange={(e) => editColumn(index, { name: e.target.value })}
                />
              </td>
              <td>
                <Form.Control
                  size="sm"
                  className="font-monospace"
                  value={column.expr}
                  aria-label={t("Column {n} formula", { n: index + 1 })}
                  onChange={(e) => {
                    const expr = e.target.value;
                    // A name the picker suggested follows the formula while it
                    // is still the suggestion; one the admin typed is theirs
                    // and is left alone.
                    const suggested = suggestColumnName(column.expr);
                    editColumn(index, {
                      expr,
                      ...(column.name === suggested || column.name === ""
                        ? { name: suggestColumnName(expr) }
                        : {}),
                    });
                  }}
                />
              </td>
              <td>
                <Button
                  size="sm"
                  variant="outline-danger"
                  aria-label={t("Remove column {n}", { n: index + 1 })}
                  onClick={() => setColumns(columns.filter((_, i) => i !== index))}
                >
                  <IconTrash className="icon-2" />
                </Button>
              </td>
            </tr>
          ))}
        </tbody>
      </Table>

      <div className="d-flex gap-2 align-items-start flex-wrap">
        <Form.Select
          className="w-auto"
          value=""
          aria-label={t("Add a column")}
          onChange={(e) => addColumn(Number(e.target.value))}
        >
          <option value=""><T text="Add a field, join path or aggregation…" /></option>
          {grouped.map(([group, list]) => (
            <optgroup key={group} label={group}>
              {list.map((choice) => (
                <option key={choice.expr} value={String(choice.index)}>
                  {choice.label}
                </option>
              ))}
            </optgroup>
          ))}
        </Form.Select>
        <Button
          variant="outline-secondary"
          onClick={() => setColumns([...columns, { name: "", expr: "" }])}
        >
          <IconPlus className="icon-2" />
          <T text="Blank column" />
        </Button>
      </div>

      {withOrder && (
        <>
          <h4 className="h5 mt-4"><T text="Order" /></h4>
          <p className="text-muted small">
            <T text="The order the rows are bound in: a time series is an order, and the same data in another order with the same seed gives different draws. The primary key is always added last, so the order is total." />
          </p>
          {order.map((key, index) => (
            <div className="d-flex gap-2 mb-2 align-items-center" key={index}>
              <Form.Control
                size="sm"
                className="font-monospace"
                value={key.expr}
                placeholder={t("day")}
                aria-label={t("Order key {n}", { n: index + 1 })}
                onChange={(e) =>
                  setOrder(order.map((o, i) => (i === index ? { ...o, expr: e.target.value } : o)))
                }
              />
              <Form.Check
                type="checkbox"
                id={`${idPrefix}-order-${index}-desc`}
                label={t("descending")}
                checked={Boolean(key.descending)}
                onChange={(e) =>
                  setOrder(
                    order.map((o, i) => (i === index ? { ...o, descending: e.target.checked } : o)),
                  )
                }
              />
              <Button
                size="sm"
                variant="outline-danger"
                aria-label={t("Remove order key {n}", { n: index + 1 })}
                onClick={() => setOrder(order.filter((_, i) => i !== index))}
              >
                <IconTrash className="icon-2" />
              </Button>
            </div>
          ))}
          <Button
            size="sm"
            variant="outline-secondary"
            onClick={() => setOrder([...order, { expr: "" }])}
          >
            <IconPlus className="icon-2" />
            <T text="Order by" />
          </Button>
        </>
      )}

      <hr />

      <h4 className="h5"><T text="Preview" /></h4>
      {previewError && <Alert variant="warning">{previewError}</Alert>}
      {!previewError && !preview && (
        <p className="text-muted mb-0">
          <T text="Add a column to see the first rows and the types they came back as." />
        </p>
      )}
      {preview && (
        <>
          {preview.split_error && !withOrder && (
            <Alert variant="warning">
              {t("{problem} — the dataset reads, and a fit cannot divide it.", {
                problem: preview.split_error,
              })}
            </Alert>
          )}
          <div className="table-responsive">
            <Table size="sm" className="table-vcenter">
              <thead>
                <tr>
                  {preview.columns.map((column) => (
                    <th key={column.name}>
                      {column.name}
                      <div className="text-muted fw-normal small">{column.type}</div>
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {preview.rows.map((row, index) => (
                  <tr key={index}>
                    {preview.columns.map((column) => (
                      <td key={column.name} className="text-nowrap">
                        {cellText((row as Record<string, unknown>)[column.name])}
                      </td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
          {previewNote && <p className="text-muted small mb-0">{previewNote(preview)}</p>}
        </>
      )}
    </>
  );
}

/** A preview cell: a null is an em dash rather than the word "null", and an
 * object is its JSON, because a dataset column can be one. */
export function cellText(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}
