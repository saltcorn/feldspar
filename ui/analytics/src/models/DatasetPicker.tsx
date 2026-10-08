// A model's dataset: one of the **named datasets** (analytics TODO A1.11,
// A3.5).
//
// Datasets are built in the Dataset editor — a base and a list of operations,
// shared by every model, panel and dataset that reads it — so the model editor
// picks one rather than building one. "Edit dataset" opens the chosen one there,
// with Back returning to the model; "New dataset" opens the editor on a new
// one. "Use a copy" clones the chosen dataset and picks the copy, which is how a
// cloned model changes its columns without changing the original's: a clone
// shares its datasets. The preview underneath is the first rows and the types
// they came back as, which is what a provider's form is built from.

import { useEffect, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListDatasetsResponse, PreviewDatasetResponse } from "../client";
import { T, useT } from "../i18n";
import { useAnnounce, usePane } from "../panes";

/** One stored dataset, as the picker lists it. */
export type DatasetItem = ListDatasetsResponse[number];

/** The option text for a dataset: its name, its table, and a mark when it
 * does not read. */
export function datasetOptionLabel(item: DatasetItem): string {
  const table = item.table ? ` — ${item.table}` : "";
  return item.error ? `${item.name}${table} ⚠` : `${item.name}${table}`;
}

export function DatasetPicker({
  value,
  onChange,
  datasets,
  idPrefix,
  preview = true,
  previewNote,
  back,
  onCopied,
  beforeEdit,
}: {
  /** The chosen dataset's id; empty for none yet. */
  value: string;
  onChange: (datasetId: string) => void;
  datasets: DatasetItem[];
  idPrefix: string;
  /** Show the first rows under the picker. */
  preview?: boolean;
  /** A sentence under the preview saying what it is for here. */
  previewNote?: (preview: PreviewDatasetResponse) => ReactNode;
  /** Where the Dataset editor's Back returns: the model being edited. */
  back?: string;
  /** Offer "Use a copy"; called with the copy once it is made. */
  onCopied?: (copy: DatasetItem) => void | Promise<void>;
  /** Called before "Edit dataset" leaves the page — the model editor saves
   * the model, so what is on its form is what it comes back to. A rejection
   * stays on the page. */
  beforeEdit?: () => Promise<void>;
}) {
  const { t } = useT();
  const pane = usePane();
  const changed = useAnnounce();
  const [rows, setRows] = useState<PreviewDatasetResponse | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [copyError, setCopyError] = useState<string | null>(null);
  const chosen = datasets.find((d) => d.id === value);

  const copy = async () => {
    setCopyError(null);
    try {
      const made = await api.cloneDataset(value, {});
      const dataset = made.dataset as { id: string; name: string };
      changed("dataset", dataset.id);
      const listed = (await api.listDatasets()).find((d) => d.id === dataset.id);
      if (listed && onCopied) await onCopied(listed);
    } catch (err) {
      setCopyError(errorMessage(err, t("Could not copy the dataset.")));
    }
  };

  useEffect(() => {
    if (!preview || value === "") {
      setRows(null);
      setPreviewError(null);
      return undefined;
    }
    let cancelled = false;
    void api
      .previewDataset({ dataset: { dataset_id: value }, limit: 10 })
      .then((answer) => {
        if (cancelled) return;
        setRows(answer);
        setPreviewError(null);
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        setRows(null);
        setPreviewError(errorMessage(err, "This dataset could not be read."));
      });
    return () => {
      cancelled = true;
    };
  }, [value, preview]);

  return (
    <>
      <Form.Group className="mb-3" controlId={`${idPrefix}-dataset`}>
        <Form.Label>
          <T text="Dataset" /><span className="text-danger"> *</span>
        </Form.Label>
        <div className="d-flex gap-2 align-items-center flex-wrap">
          <Form.Select
            className="w-auto"
            value={value}
            onChange={(e) => onChange(e.target.value)}
          >
            {value === "" && <option value="">{t("(choose a dataset)")}</option>}
            {value !== "" && !chosen && (
              <option value={value}>{t("{id} (missing)", { id: value })}</option>
            )}
            {datasets.map((item) => (
              <option key={item.id} value={item.id}>
                {datasetOptionLabel(item)}
              </option>
            ))}
          </Form.Select>
          {value !== "" && (
            <a
              href={pane.href({ name: "dataset", id: value, back })}
              onClick={(e) => {
                if (!beforeEdit) return;
                e.preventDefault();
                void beforeEdit()
                  .then(() => pane.go({ name: "dataset", id: value, back }))
                  .catch(() => undefined);
              }}
            >
              <T text="Edit dataset" />
            </a>
          )}
          {value !== "" && onCopied && (
            <Button size="sm" variant="outline-secondary" onClick={() => void copy()}>
              <T text="Use a copy" />
            </Button>
          )}
          <a href={pane.href({ name: "newDataset", table: null })}>
            <T text="New dataset" />
          </a>
        </div>
        <Form.Text muted>
          <T text="A dataset is a table, then operations — calculated columns, filters, aggregates, joins — and every column but the label is a feature. A fit records the dataset it read, and says when the dataset has changed since. A clone of a model shares its dataset: “Use a copy” gives this model one of its own to change." />
        </Form.Text>
      </Form.Group>
      {copyError && <Alert variant="danger">{copyError}</Alert>}
      {chosen?.error && <Alert variant="warning">{chosen.error}</Alert>}
      {preview && value !== "" && (
        <>
          <h4 className="h5"><T text="Preview" /></h4>
          {previewError && <Alert variant="warning">{previewError}</Alert>}
          {rows && (
            <>
              {rows.split_error && <Alert variant="warning">{rows.split_error}</Alert>}
              <div className="table-responsive">
                <Table size="sm" className="table-vcenter">
                  <thead>
                    <tr>
                      {rows.columns.map((column) => (
                        <th key={column.name}>
                          {column.name}
                          <div className="text-muted fw-normal small">{column.type}</div>
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {rows.rows.map((row, index) => (
                      <tr key={index}>
                        {rows.columns.map((column) => (
                          <td key={column.name} className="text-nowrap">
                            {cellText((row as Record<string, unknown>)[column.name])}
                          </td>
                        ))}
                      </tr>
                    ))}
                  </tbody>
                </Table>
              </div>
              {previewNote && <p className="text-muted small mb-0">{previewNote(rows)}</p>}
            </>
          )}
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
