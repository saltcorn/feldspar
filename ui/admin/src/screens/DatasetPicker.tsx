// A model's dataset: one of the **named datasets** (analytics TODO A1.11).
//
// Datasets are built in the Analytics UI's Dataset editor now — a base and a
// list of operations, shared by every model, panel and dataset that reads it —
// so the model form picks one rather than building one. "Edit in Analytics"
// opens the chosen one there; "New dataset" opens the editor on a new one. The
// preview underneath is the one the builder had: the first rows and the types
// they came back as, which is what a provider's form is built from.

import { useEffect, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListDatasetsResponse, PreviewDatasetResponse } from "../client";
import { analyticsDatasetUrl, newDatasetUrl } from "../models";
import { T, useT } from "../i18n";

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
}) {
  const { t } = useT();
  const [rows, setRows] = useState<PreviewDatasetResponse | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const chosen = datasets.find((d) => d.id === value);

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
            <a href={analyticsDatasetUrl(value)} target="_blank" rel="noreferrer">
              <T text="Edit in Analytics" />
            </a>
          )}
          <a href={newDatasetUrl()} target="_blank" rel="noreferrer">
            <T text="New dataset" />
          </a>
        </div>
        <Form.Text muted>
          <T text="Datasets are built in the Analytics UI: a table, then operations — calculated columns, filters, aggregates, joins. A fit records the dataset it read, and says when the dataset has changed since." />
        </Form.Text>
      </Form.Group>
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
