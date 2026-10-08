// Models side by side (analytics TODO A3.5), at `#/models/compare?ids=…`:
// for each model ticked in the list, the fit it would show — the active one,
// else the newest fitted — and its key outputs, every output that is not
// optional, lined up by name so two regressions' coefficient tables sit next
// to each other. Nothing here is kept: a comparison is read, then left.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { navigate, routeHash } from "../router";
import { instanceLabel, readModelDataset, type ModelItem } from "./models";
import { OutputBody } from "./OutputsPanel";
import { compareRows, readOutputs, readOutputsFit, type ModelOutput, type OutputsFit } from "./outputs";

/** One model's column. */
type Column = { model: ModelItem; fit: OutputsFit | null; outputs: ModelOutput[] } | { id: string; error: string };

export function ModelCompare({ ids }: { ids: string[] }) {
  const { t } = useT();
  const [columns, setColumns] = useState<Column[] | null>(null);

  useEffect(() => {
    let cancelled = false;
    void Promise.all(
      ids.map(async (id): Promise<Column> => {
        try {
          const [model, outputs] = await Promise.all([api.getModel(id), api.getModelOutputs(id, {})]);
          return { model, fit: readOutputsFit(outputs.fit), outputs: readOutputs(outputs.outputs) };
        } catch (err) {
          return { id, error: errorMessage(err, t("Could not load this model.")) };
        }
      }),
    ).then((loaded) => {
      if (!cancelled) setColumns(loaded);
    });
    return () => {
      cancelled = true;
    };
  }, [ids, t]);

  const rows = columns ? compareRows(columns.map((c) => ("model" in c ? c.outputs : []))) : [];

  return (
    <div className="an-page an-page-wide">
      <div className="d-flex align-items-center gap-2 mb-3">
        <Button variant="outline-secondary" size="sm" onClick={() => navigate({ name: "home" })}>
          ← <T text="All models" />
        </Button>
        <h2 className="h3 mb-0">
          <T text="Compare models" />
        </h2>
      </div>
      {ids.length < 2 && (
        <Alert variant="info">
          <T text="Tick two or more models in the list to compare them." />
        </Alert>
      )}
      {!columns && <Spinner animation="border" size="sm" />}
      {columns && (
        <div className="an-model-compare">
          {columns.map((c) => (
            <div key={"model" in c ? c.model.id : c.id} className="an-compare-column">
              {"error" in c ? (
                <Alert variant="danger">{c.error}</Alert>
              ) : (
                <>
                  <Card className="mb-3">
                    <Card.Body>
                      <h3 className="h4 mb-1">
                        <a href={routeHash({ name: "model", id: c.model.id })}>{c.model.name}</a>
                      </h3>
                      <div className="text-secondary small">
                        {c.model.provider} · {readModelDataset(c.model.dataset)?.name ?? "—"}
                      </div>
                      <div className="text-secondary small">
                        {c.fit ? instanceLabel(c.fit) : t("Never fitted")}
                        {c.fit?.dataset_changed && ` · ${t("the dataset has changed since this fit")}`}
                      </div>
                    </Card.Body>
                  </Card>
                  {rows.map((row) => {
                    const output = c.outputs.find((o) => o.name === row.name);
                    return (
                      <Card className="mb-3" key={row.name} data-output={row.name}>
                        <Card.Header className="fw-bold">{row.label}</Card.Header>
                        {output ? (
                          <OutputBody output={output} />
                        ) : (
                          <Card.Body className="text-secondary">
                            <T text="This model's fit has no such output." />
                          </Card.Body>
                        )}
                      </Card>
                    );
                  })}
                </>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** `#/model-instances/<id>`: a fit by its own id — where the admin UI's old
 * links lead. The fit names its model; the model opens with the fit selected. */
export function FitRedirect({ id }: { id: string }) {
  const { t } = useT();
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api
      .getModelInstance(id)
      .then((fit) => window.location.replace(routeHash({ name: "model", id: fit.model, fit: id })))
      .catch((err: unknown) => setError(errorMessage(err, t("There is no such fit."))));
  }, [id, t]);
  return <div className="an-page">{error ? <Alert variant="warning">{error}</Alert> : <Spinner animation="border" size="sm" />}</div>;
}
