// The list of datasets, on the Analytics UI's front page (analytics TODO A1.16,
// A1.21): every dataset, each edited, cloned or deleted from its row, and a new
// one made on a base — a table, or another dataset. The list is global, because
// models and panels share the datasets; one opens in the Dataset editor.

import { useCallback, useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { DatasetUsageResponse, ListDatasetsResponse, ListDatasetTablesResponse } from "../client";
import { T, useT } from "../i18n";
import { navigate } from "../router";
import { describeGrain, type Base, type Grain } from "./ops";

export type DatasetItem = ListDatasetsResponse[number];
type TableItem = ListDatasetTablesResponse[number];

/** What a base says in the list. */
export function describeBase(base: unknown, datasets: DatasetItem[]): string {
  const b = (base ?? {}) as Partial<{ kind: string; table: string; dataset: string }>;
  if (b.kind === "dataset") {
    return datasets.find((d) => d.id === b.dataset)?.name ?? b.dataset ?? "";
  }
  return b.table ?? "";
}

export function DatasetList({ onOpen }: { onOpen: (id: string) => void }) {
  const { t } = useT();
  const [datasets, setDatasets] = useState<DatasetItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<{ item: DatasetItem; usage: DatasetUsageResponse } | null>(
    null,
  );

  const load = useCallback(async () => {
    try {
      setDatasets(await api.listDatasets());
    } catch (err) {
      setError(errorMessage(err, t("Could not load the datasets.")));
    }
  }, [t]);

  useEffect(() => {
    void load();
  }, [load]);

  const clone = async (item: DatasetItem) => {
    try {
      const copy = await api.cloneDataset(item.id, {});
      const made = copy.dataset as { id: string };
      await load();
      onOpen(made.id);
    } catch (err) {
      setError(errorMessage(err, t("Could not clone the dataset.")));
    }
  };

  const askDelete = async (item: DatasetItem) => {
    try {
      setDeleting({ item, usage: await api.datasetUsage(item.id) });
    } catch (err) {
      setError(errorMessage(err, t("Could not check what uses the dataset.")));
    }
  };

  return (
    <section className="mb-5">
      <h2 className="h3 mb-3">
        <T text="Datasets" />
      </h2>
      {error && (
        <Alert variant="danger" dismissible onClose={() => setError(null)}>
          {error}
        </Alert>
      )}
      <NewDatasetForm datasets={datasets ?? []} onCreated={onOpen} />
      {datasets && datasets.length === 0 && (
        <p className="text-secondary">
          <T text="No datasets yet. Make one above, on a table." />
        </p>
      )}
      {datasets && datasets.length > 0 && (
        <Table hover responsive className="card-table">
          <thead>
            <tr>
              <th>
                <T text="Name" />
              </th>
              <th>
                <T text="Based on" />
              </th>
              <th>
                <T text="Operations" />
              </th>
              <th>
                <T text="Rows" />
              </th>
              <th />
            </tr>
          </thead>
          <tbody>
            {datasets.map((d) => (
              <tr key={d.id}>
                <td>
                  <Button variant="link" className="p-0" onClick={() => onOpen(d.id)}>
                    {d.name}
                  </Button>
                  {d.description && <div className="text-secondary small">{d.description}</div>}
                </td>
                <td>{describeBase(d.base, datasets)}</td>
                <td>{d.operations}</td>
                <td>
                  {d.error ? (
                    <Badge bg="danger-lt" title={d.error}>
                      <T text="has an error" />
                    </Badge>
                  ) : (
                    <span className="text-secondary small">{describeGrain(d.grain as Grain, t)}</span>
                  )}
                </td>
                <td className="text-end text-nowrap">
                  <Button size="sm" variant="outline-primary" onClick={() => onOpen(d.id)}>
                    <T text="Edit" />
                  </Button>{" "}
                  <Button size="sm" variant="outline-secondary" onClick={() => void clone(d)}>
                    <T text="Clone" />
                  </Button>{" "}
                  <Button size="sm" variant="outline-danger" onClick={() => void askDelete(d)}>
                    <T text="Delete" />
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </Table>
      )}

      <Modal show={deleting !== null} onHide={() => setDeleting(null)}>
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="Delete dataset" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <p>
            <T text="Delete “{name}”?" args={{ name: deleting?.item.name ?? "" }} />
          </p>
          {deleting && deleting.usage.models.length > 0 && (
            <Alert variant="warning">
              <T text="These models use it, and will not fit until they are given another dataset:" />
              <ul className="mb-0">
                {deleting.usage.models.map((m) => (
                  <li key={m.id}>{m.name}</li>
                ))}
              </ul>
            </Alert>
          )}
          {deleting && deleting.usage.datasets.length > 0 && (
            <Alert variant="danger">
              <T text="These datasets read it, so it cannot be deleted until they are changed or deleted:" />
              <ul className="mb-0">
                {deleting.usage.datasets.map((d) => (
                  <li key={d.id}>{d.name}</li>
                ))}
              </ul>
            </Alert>
          )}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={() => setDeleting(null)}>
            <T text="Cancel" />
          </Button>
          <Button
            variant="danger"
            disabled={(deleting?.usage.datasets.length ?? 0) > 0}
            onClick={async () => {
              if (!deleting) return;
              try {
                await api.deleteDataset(deleting.item.id);
              } catch (err) {
                setError(errorMessage(err, t("Could not delete the dataset.")));
              }
              setDeleting(null);
              void load();
            }}
          >
            <T text="Delete" />
          </Button>
        </Modal.Footer>
      </Modal>
    </section>
  );
}

/** A new dataset: its name, and its base — picked now and never changed. */
export function NewDatasetForm({
  datasets,
  initialTable,
  onCreated,
}: {
  datasets: DatasetItem[];
  initialTable?: string | null;
  onCreated: (id: string) => void;
}) {
  const { t } = useT();
  const [tables, setTables] = useState<TableItem[]>([]);
  const [name, setName] = useState("");
  const [base, setBase] = useState(initialTable ? `table:${initialTable}` : "");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .listDatasetTables()
      .then((list) => {
        setTables(list);
        setBase((current) => current || (list[0] ? `table:${list[0].name}` : ""));
      })
      .catch(() => undefined);
  }, []);

  const create = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    const [kind, value] = base.split(/:(.*)/s, 2);
    const chosen: Base =
      kind === "dataset" ? { kind: "dataset", dataset: value } : { kind: "table", table: value };
    try {
      const made = await api.createDataset({ name: name.trim(), base: chosen, operations: [] });
      onCreated((made.dataset as { id: string }).id);
    } catch (err) {
      setError(errorMessage(err, t("Could not create the dataset.")));
    }
  };

  return (
    <Card className="mb-4">
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        <Form onSubmit={create} className="d-flex gap-2 flex-wrap align-items-end">
          <Form.Group controlId="dataset-name">
            <Form.Label>
              <T text="New dataset" />
            </Form.Label>
            <Form.Control
              value={name}
              placeholder={t("House prices by area")}
              onChange={(e) => setName(e.target.value)}
            />
          </Form.Group>
          <Form.Group controlId="dataset-base">
            <Form.Label>
              <T text="Based on" />
            </Form.Label>
            <Form.Select value={base} onChange={(e) => setBase(e.target.value)}>
              <optgroup label={t("Tables")}>
                {tables.map((table) => (
                  <option key={table.name} value={`table:${table.name}`}>
                    {table.name}
                  </option>
                ))}
              </optgroup>
              {datasets.length > 0 && (
                <optgroup label={t("Datasets")}>
                  {datasets.map((d) => (
                    <option key={d.id} value={`dataset:${d.id}`}>
                      {d.name}
                    </option>
                  ))}
                </optgroup>
              )}
            </Form.Select>
          </Form.Group>
          <Button type="submit" disabled={name.trim() === "" || base === ""}>
            <T text="Create" />
          </Button>
        </Form>
        <Form.Text muted>
          <T text="The base cannot be changed later: every operation is written against the columns it provides." />
        </Form.Text>
      </Card.Body>
    </Card>
  );
}

/** `#/datasets/new`: a new dataset on a page of its own — where the admin UI's
 * model form sends someone who has no dataset yet. */
export function NewDatasetPage({ table }: { table: string | null }) {
  const [datasets, setDatasets] = useState<DatasetItem[]>([]);
  useEffect(() => {
    api.listDatasets().then(setDatasets).catch(() => undefined);
  }, []);
  return (
    <div className="an-page">
      <h2 className="h3 mb-3">
        <T text="New dataset" />
      </h2>
      <NewDatasetForm
        datasets={datasets}
        initialTable={table}
        onCreated={(id) => navigate({ name: "dataset", id })}
      />
    </div>
  );
}
