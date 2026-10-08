// The models, on the Analytics UI's front page between the datasets and the
// workspaces (analytics TODO A3.5): every model with its dataset, its provider
// and its last fit; each opened, cloned or deleted from its row; a new one
// made; and several ticked and **compared**.
//
// The list is global, as the datasets are, because a model is a named thing
// other things refer to — a `predict("…")` in a calculated field, a
// `fit_model` trigger — and deleting one warns with what does. A model that
// no longer validates is still listed, marked, with the reason in the
// badge's tooltip: opening it is the repair.

import { useCallback, useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ModelUsageResponse } from "../client";
import { T, useT } from "../i18n";
import { workspaceKindName } from "../labels";
import { useAnnounce, useChanges, usePane } from "../panes";
import {
  formatTimestamp,
  headlineMetric,
  instanceLabel,
  outcomeSummary,
  readMetrics,
  readModelDataset,
  readOutcome,
  type ModelItem,
} from "./models";
import { StatusBadge, fitTone } from "./StatusBadge";

/** One model's last fit as a cell: its status, what it scored, and when. */
function LastFit({ model }: { model: ModelItem }) {
  const fit = model.active_instance ?? model.last_fit;
  if (!fit) {
    return (
      <span className="text-muted">
        <T text="Never fitted" />
      </span>
    );
  }
  const headline = headlineMetric(readMetrics(fit.metrics));
  return (
    <>
      <div className="d-flex align-items-center gap-2">
        <StatusBadge tone={fitTone(fit.status)} title={fit.error ?? undefined}>
          {fit.status}
        </StatusBadge>
        {fit.active && (
          <StatusBadge tone="green">
            <T text="active" />
          </StatusBadge>
        )}
      </div>
      <div className="text-muted small">
        {instanceLabel(fit)}
        {headline && ` · ${headline}`}
      </div>
      {fit.name.trim() !== "" && <div className="text-muted small">{formatTimestamp(fit.created)}</div>}
    </>
  );
}

/** Whether a usage report names anything. */
export function inUse(usage: ModelUsageResponse): boolean {
  return usage.fields.length > 0 || usage.triggers.length > 0;
}

export function ModelList() {
  const { t } = useT();
  const pane = usePane();
  const changed = useAnnounce();
  const [models, setModels] = useState<ModelItem[] | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ticked, setTicked] = useState<string[]>([]);
  const [deleting, setDeleting] = useState<{ model: ModelItem; usage: ModelUsageResponse } | null>(null);

  const load = useCallback(async () => {
    try {
      setModels(await api.listModels());
    } catch (err) {
      // Includes the sentence a server built without model support answers
      // with, which is the honest thing to show.
      setError(errorMessage(err, t("Could not load the models.")));
    }
  }, [t]);

  useEffect(() => {
    void load();
    // A build without the built-in providers says so here, so an empty
    // provider picker reads as the decision it is rather than a bug.
    void api
      .listModelProviders()
      .then((listed) => setNotice(listed.builtins_compiled_out ? (listed.notice ?? null) : null))
      .catch(() => setNotice(null));
  }, [load]);
  // A model saved or fitted on the other side of a split view.
  useChanges(["model", "dataset"], () => void load());

  const clone = async (model: ModelItem) => {
    try {
      const copy = await api.cloneModel(model.id, {});
      pane.go({ name: "model", id: copy.id });
    } catch (err) {
      setError(errorMessage(err, t("Could not clone the model.")));
    }
  };

  const askDelete = async (model: ModelItem) => {
    try {
      setDeleting({ model, usage: await api.modelUsage(model.id) });
    } catch (err) {
      setError(errorMessage(err, t("Could not check what uses the model.")));
    }
  };

  const tick = (id: string, on: boolean) =>
    setTicked((list) => (on ? [...list.filter((x) => x !== id), id] : list.filter((x) => x !== id)));

  return (
    <section className="mb-5">
      <div className="d-flex align-items-center gap-2 mb-3">
        <h2 className="h3 mb-0">
          <T text="Models" />
        </h2>
        <Button size="sm" className="ms-auto" onClick={() => pane.go({ name: "newModel", dataset: null })}>
          <T text="New model" />
        </Button>
        <Button
          size="sm"
          variant="outline-primary"
          disabled={ticked.length < 2}
          title={ticked.length < 2 ? t("Tick two or more models to compare them.") : undefined}
          onClick={() => pane.go({ name: "compareModels", ids: ticked })}
        >
          <T text="Compare" />
          {ticked.length > 0 && ` (${ticked.length})`}
        </Button>
      </div>
      {error && (
        <Alert variant="danger" dismissible onClose={() => setError(null)}>
          {error}
        </Alert>
      )}
      {notice && <Alert variant="info">{notice}</Alert>}
      {models && models.length === 0 && (
        <p className="text-secondary">
          <T text="No models yet. A model is a dataset and a provider that answers a question about it: make one here, or from a dataset's row above." />
        </p>
      )}
      {models && models.length > 0 && (
        <Table hover responsive className="card-table table-vcenter">
          <thead>
            <tr>
              <th />
              <th>
                <T text="Name" />
              </th>
              <th>
                <T text="Dataset" />
              </th>
              <th>
                <T text="Provider" />
              </th>
              <th>
                <T text="Outcome" />
              </th>
              <th>
                <T text="Last fit" />
              </th>
              <th />
            </tr>
          </thead>
          <tbody>
            {models.map((model) => {
              const fit = model.active_instance ?? model.last_fit;
              const dataset = readModelDataset(model.dataset);
              return (
                <tr key={model.id}>
                  <td>
                    <Form.Check
                      aria-label={t("Compare {name}", { name: model.name })}
                      checked={ticked.includes(model.id)}
                      onChange={(e) => tick(model.id, e.target.checked)}
                    />
                  </td>
                  <td>
                    <a href={pane.href({ name: "model", id: model.id })}>{model.name}</a>
                    {model.description && <div className="text-muted small">{model.description}</div>}
                    {model.error && (
                      <StatusBadge tone="red" title={model.error} className="mt-1">
                        <T text="Cannot be fitted" />
                      </StatusBadge>
                    )}
                  </td>
                  <td>
                    {dataset ? (
                      <a href={pane.href({ name: "dataset", id: dataset.dataset_id })}>{dataset.name}</a>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td>{model.provider}</td>
                  {/* The outcome is a fit's: what this build actually produced. */}
                  <td>{fit ? outcomeSummary(readOutcome(fit.outcome)) : "—"}</td>
                  <td>
                    <LastFit model={model} />
                  </td>
                  <td className="text-end text-nowrap">
                    <Button size="sm" variant="outline-primary" href={pane.href({ name: "model", id: model.id })}>
                      <T text="Edit" />
                    </Button>{" "}
                    <Button size="sm" variant="outline-secondary" onClick={() => void clone(model)}>
                      <T text="Clone" />
                    </Button>{" "}
                    <Button size="sm" variant="outline-danger" onClick={() => void askDelete(model)}>
                      <T text="Delete" />
                    </Button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </Table>
      )}

      <Modal show={deleting !== null} onHide={() => setDeleting(null)}>
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="Delete model" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <p>
            <T
              text="Delete “{name}”? Its fits go with it: a fit's parameters mean nothing without the model they were fitted for."
              args={{ name: deleting?.model.name ?? "" }}
            />
          </p>
          {deleting && inUse(deleting.usage) && (
            <Alert variant="warning">
              <T text="These use it by name, and will fail saying the model is missing until they are changed:" />
              <ul className="mb-0">
                {deleting.usage.fields.map((f) => (
                  <li key={`${f.table}.${f.field}`}>
                    {t("the calculated field {table}.{field}", { table: f.table, field: f.field })}
                  </li>
                ))}
                {deleting.usage.triggers.map((tr) => (
                  <li key={tr.id}>
                    {tr.how === "fits"
                      ? t("the trigger {name}, which fits it", { name: tr.name })
                      : t("the trigger {name}, which names it", { name: tr.name })}
                  </li>
                ))}
              </ul>
            </Alert>
          )}
          {deleting && deleting.usage.workspaces.length > 0 && (
            <Alert variant="warning">
              <T text="These workspaces show its fits, and will say that the fit has been deleted where they did:" />
              <ul className="mb-0">
                {deleting.usage.workspaces.map((w) => (
                  <li key={w.id}>
                    <a href={pane.href({ name: "workspace", id: w.id })}>{w.name}</a>{" "}
                    <span className="text-secondary small">
                      {w.panels === 1
                        ? t("{kind}, 1 panel", { kind: workspaceKindName(w.kind, t) })
                        : t("{kind}, {count} panels", { kind: workspaceKindName(w.kind, t), count: w.panels })}
                    </span>
                  </li>
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
            onClick={async () => {
              if (!deleting) return;
              try {
                await api.deleteModel(deleting.model.id);
                changed("model", deleting.model.id);
                setTicked((list) => list.filter((x) => x !== deleting.model.id));
              } catch (err) {
                setError(errorMessage(err, t("Could not delete the model.")));
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
