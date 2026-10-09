// A dashboard's filter bar (analytics TODO A6.6): what is filtering its tiles
// now — the selections made on them and the dashboard's own filters — each
// removable, all of them at once, and the form that makes a filter of the
// dashboard's own on a column of one of its datasets.

import { useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { ListDatasetsResponse } from "../client";
import { isNumeric, type StageColumn } from "../datasets/ops";
import { T, useT } from "../i18n";
import { describe, newFilterId, valueText, type Condition, type Selected } from "./filters";

type Dataset = ListDatasetsResponse[number];

/** One chip: a filter in words, and the × that removes it. */
function Chip({ c, kind, dataset, onRemove }: { c: Condition; kind: "selection" | "filter"; dataset?: string; onRemove: () => void }) {
  const { t } = useT();
  const { on, keeps } = describe(c, t, dataset);
  const label = `${on}: ${keeps}`;
  return (
    <span
      className={kind === "selection" ? "an-filter-chip an-selection" : "an-filter-chip"}
      title={
        (kind === "selection" ? t("Selected on a tile") : t("A filter of the dashboard")) + (dataset ? ` — ${dataset}` : "")
      }
    >
      <span className="an-filter-chip-text">{label}</span>
      <button type="button" className="an-filter-chip-remove" aria-label={t("Remove the filter {label}", { label })} onClick={onRemove}>
        ×
      </button>
    </span>
  );
}

export function FilterBar({
  filters,
  selections,
  names,
  onRemoveFilter,
  onRemoveSelection,
  onClear,
  onAdd,
}: {
  filters: Condition[];
  selections: Selected[];
  /** Datasets' names, by id. */
  names: Record<string, string>;
  onRemoveFilter: (id: string) => void;
  onRemoveSelection: (id: string) => void;
  onClear: () => void;
  onAdd: () => void;
}) {
  const { t } = useT();
  const any = filters.length + selections.length > 0;
  return (
    <div className="an-filter-bar" role="toolbar" aria-label={t("Filters")}>
      <span className="text-secondary small">{t("Filters")}</span>
      {!any && (
        <span className="text-secondary small fst-italic">
          <T text="none — click a bar or a feature, or brush a range, to filter the other tiles" />
        </span>
      )}
      {filters.map((c) => (
        <Chip key={c.id} c={c} kind="filter" dataset={names[c.dataset]} onRemove={() => onRemoveFilter(c.id)} />
      ))}
      {selections.map((c) => (
        <Chip key={c.id} c={c} kind="selection" dataset={names[c.dataset]} onRemove={() => onRemoveSelection(c.id)} />
      ))}
      <Button size="sm" variant="outline-secondary" className="py-0" onClick={onAdd}>
        + {t("Filter")}
      </Button>
      {any && (
        <Button size="sm" variant="link" className="py-0" onClick={onClear}>
          {t("Clear all")}
        </Button>
      )}
    </div>
  );
}

/** The kinds of input a filter on a column takes. */
type InputKind = "values" | "number" | "date" | "timestamp";

/** How a column is filtered: by its values (text, true or false, a key, a
 * UUID), or by a range (numbers, dates, instants). */
export function inputKind(column: StageColumn): InputKind | null {
  if (column.key) return "values";
  if (isNumeric(column.type)) return "number";
  if (column.type === "date") return "date";
  if (column.type === "timestamp") return "timestamp";
  if (["text", "bool", "uuid", "time", "unknown"].includes(column.type)) return "values";
  return null;
}

/** A filter of a range made from what was typed: numbers as numbers, an
 * instant as one (a minute's precision, in UTC). `null` with neither end. */
export function rangeCondition(
  id: string,
  dataset: string,
  column: string,
  kind: InputKind,
  min: string,
  max: string,
): Condition | null {
  const read = (s: string): unknown => {
    const v = s.trim();
    if (v === "") return undefined;
    if (kind === "number") {
      const n = Number(v);
      return Number.isFinite(n) ? n : undefined;
    }
    if (kind === "timestamp") return v.length === 16 ? `${v}:00Z` : v;
    return v;
  };
  const [lo, hi] = [read(min), read(max)];
  if (lo === undefined && hi === undefined) return null;
  const range: Condition["range"] = {};
  if (lo !== undefined) range.min = lo;
  if (hi !== undefined) range.max = hi;
  return { id, dataset, column, range };
}

/** The form for a filter of the dashboard's own, on one of `datasets`. */
export function FilterForm({
  datasets: ids,
  onSave,
  onCancel,
}: {
  datasets: string[];
  onSave: (c: Condition) => void;
  onCancel: () => void;
}) {
  const { t } = useT();
  const [all, setAll] = useState<Dataset[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [dataset, setDataset] = useState<string>("");
  const [column, setColumn] = useState<string>("");
  const [choices, setChoices] = useState<unknown[] | null>(null);
  const [chosen, setChosen] = useState<unknown[]>([]);
  const [min, setMin] = useState("");
  const [max, setMax] = useState("");

  useEffect(() => {
    let live = true;
    api
      .listDatasets()
      .then((ds) => {
        if (!live) return;
        setAll(ds);
        const first = ds.find((d) => ids.includes(d.id)) ?? ds[0];
        if (first) setDataset(first.id);
      })
      .catch((err: unknown) => live && setError(errorMessage(err, t("Could not load the datasets."))));
    return () => {
      live = false;
    };
    // The datasets offered are fixed while the form is open.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [t]);

  // The dashboard's datasets first, then the rest.
  const offered = useMemo(
    () => (all ?? []).filter((d) => ids.includes(d.id)).concat((all ?? []).filter((d) => !ids.includes(d.id))),
    [all, ids],
  );
  const current = offered.find((d) => d.id === dataset);
  const columns = useMemo(
    () => ((current?.columns ?? []) as StageColumn[]).filter((c) => inputKind(c) !== null),
    [current],
  );
  const col = columns.find((c) => c.name === column);
  const kind = col ? inputKind(col) : null;

  useEffect(() => {
    setColumn(columns[0]?.name ?? "");
  }, [columns]);

  // The values a column has, most frequent first, to pick from.
  useEffect(() => {
    setChoices(null);
    setChosen([]);
    setMin("");
    setMax("");
    if (!dataset || !column || kind !== "values") return;
    let live = true;
    api
      .getDataset(dataset)
      .then((d) => api.datasetColumnValues({ dataset: d.dataset, column, limit: 200 }))
      .then((values) => live && setChoices(values))
      .catch((err: unknown) => live && setError(errorMessage(err, t("Could not read the column's values."))));
    return () => {
      live = false;
    };
  }, [dataset, column, kind, t]);

  const made: Condition | null = !col
    ? null
    : kind === "values"
      ? chosen.length > 0
        ? { id: newFilterId(), dataset, column, values: chosen }
        : null
      : kind
        ? rangeCondition(newFilterId(), dataset, column, kind, min, max)
        : null;
  const toggle = (v: unknown) =>
    setChosen((old) => (old.some((o) => JSON.stringify(o) === JSON.stringify(v)) ? old.filter((o) => JSON.stringify(o) !== JSON.stringify(v)) : [...old, v]));
  const inputType = kind === "number" ? "number" : kind === "date" ? "date" : "datetime-local";

  return (
    <Modal show onHide={onCancel}>
      <Modal.Header closeButton>
        <Modal.Title>{t("Filter the dashboard")}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {!all && !error && <Spinner animation="border" size="sm" />}
        {all && (
          <>
            <Form.Group className="mb-2">
              <Form.Label>{t("Dataset")}</Form.Label>
              <Form.Select value={dataset} onChange={(e) => setDataset(e.target.value)}>
                {offered.map((d) => (
                  <option key={d.id} value={d.id}>
                    {d.name}
                  </option>
                ))}
              </Form.Select>
              <Form.Text>
                <T text="It filters every tile on this dataset, and the tiles on datasets that refer to the same table through a column." />
              </Form.Text>
            </Form.Group>
            <Form.Group className="mb-2">
              <Form.Label>{t("Column")}</Form.Label>
              <Form.Select value={column} onChange={(e) => setColumn(e.target.value)}>
                {columns.map((c) => (
                  <option key={c.name} value={c.name}>
                    {c.name}
                  </option>
                ))}
              </Form.Select>
            </Form.Group>
            {kind === "values" && (
              <Form.Group>
                <Form.Label>{t("Keep the rows whose value is")}</Form.Label>
                {!choices && <Spinner animation="border" size="sm" className="d-block" />}
                {choices && (
                  <div className="an-filter-values">
                    {choices.length === 0 && <span className="text-secondary small">{t("The column has no values.")}</span>}
                    {choices.map((v) => (
                      <Form.Check
                        key={JSON.stringify(v)}
                        id={`fv-${JSON.stringify(v)}`}
                        type="checkbox"
                        label={valueText(v, t("(missing)"))}
                        checked={chosen.some((o) => JSON.stringify(o) === JSON.stringify(v))}
                        onChange={() => toggle(v)}
                      />
                    ))}
                  </div>
                )}
              </Form.Group>
            )}
            {kind && kind !== "values" && (
              <div className="d-flex gap-2 align-items-end">
                <Form.Group className="flex-fill">
                  <Form.Label>{t("From")}</Form.Label>
                  <Form.Control type={inputType} value={min} onChange={(e) => setMin(e.target.value)} />
                </Form.Group>
                <Form.Group className="flex-fill">
                  <Form.Label>{t("To")}</Form.Label>
                  <Form.Control type={inputType} value={max} onChange={(e) => setMax(e.target.value)} />
                </Form.Group>
              </div>
            )}
            {kind === "timestamp" && (
              <Form.Text>
                <T text="Times are in UTC." />
              </Form.Text>
            )}
          </>
        )}
      </Modal.Body>
      <Modal.Footer>
        <Button variant="secondary" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button variant="primary" disabled={!made} onClick={() => made && onSave(made)}>
          <T text="Add filter" />
        </Button>
      </Modal.Footer>
    </Modal>
  );
}
