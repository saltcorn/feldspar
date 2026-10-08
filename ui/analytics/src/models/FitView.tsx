// One fit in the model editor (analytics TODO A3.5; the admin UI's instance
// screen, TODO "Predictive models" task 6.5, moved here): its outputs, what
// its rows came to, what it was searched over, and what it answers about a
// row.
//
// The **outputs** are what the fit's provider declared (A3.1–A3.2): the
// metrics and parameter tables, and plots as specs over the fit's output data,
// drawn on the server. They replace the instance screen's metrics and
// parameter cards. The metrics are the **host's**, computed by scoring the fit
// back over each split with one piece of code, which is what makes two
// providers comparable; the parameters are the provider's own, in its own
// vocabulary.
//
// A **posterior** (Stan TODO §18) has its own reading below the outputs —
// warnings, diagnostics, a variable's summary, trace and histogram, write
// back — in `PosteriorInstance.tsx`.
//
// And "try a row" is an instance being *applied* rather than read: a literal
// row, typed, answered by this fit.

import { useCallback, useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import {
  featureInputs,
  formatNumber,
  formatTimestamp,
  instanceLabel,
  outcomeSummary,
  predictionSummary,
  printGridValue,
  readEncoding,
  readMetrics,
  readModelDataset,
  readOutcome,
  readPrediction,
  readRowCounts,
  readSearch,
  typedFeatureValue,
  type ClassMetrics,
  type FeatureInput,
  type InstanceDetail,
  type Metrics,
  type ModelItem,
  type SplitMetrics,
} from "./models";
import { includeParam, readOutputs, type ModelOutput } from "./outputs";
import { OutputsPanel } from "./OutputsPanel";
import { PosteriorInstance, Warnings } from "./PosteriorInstance";
import { StatusBadge, fitTone } from "./StatusBadge";

export function FitView({
  model,
  instanceId,
  collapsed,
  plots,
  onToggle,
  onPlots,
  onChanged,
}: {
  model: ModelItem;
  /** The fit to show. */
  instanceId: string;
  collapsed: string[];
  plots: string[];
  onToggle: (name: string) => void;
  onPlots: (plots: string[]) => void;
  /** The fit changed here: activated, written back, cancelled. */
  onChanged: () => void;
}) {
  const { t } = useT();
  const [instance, setInstance] = useState<InstanceDetail | null>(null);
  const [outputs, setOutputs] = useState<ModelOutput[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const include = includeParam(plots);

  const load = useCallback(async () => {
    try {
      setInstance(await api.getModelInstance(instanceId));
    } catch (err) {
      setError(errorMessage(err, t("Could not load this fit.")));
    }
  }, [instanceId, t]);

  useEffect(() => {
    void load();
  }, [load]);

  // The outputs, again whenever the optional plots chosen change: each is
  // drawn on the server only when asked for.
  useEffect(() => {
    let cancelled = false;
    void api
      .getModelOutputs(model.id, { fit: instanceId, include })
      .then((answer) => {
        if (!cancelled) setOutputs(readOutputs(answer.outputs));
      })
      .catch((err: unknown) => {
        if (!cancelled) setError(errorMessage(err, t("Could not load this fit's outputs.")));
      });
    return () => {
      cancelled = true;
    };
  }, [model.id, instanceId, include, t]);

  if (error) return <Alert variant="danger">{error}</Alert>;
  if (!instance) {
    return (
      <div className="py-4 text-center">
        <Spinner animation="border" size="sm" />
      </div>
    );
  }

  const outcome = readOutcome(instance.outcome);
  const metrics = readMetrics(instance.metrics);
  const search = readSearch(instance.search);
  const rows = readRowCounts(instance.rows);
  const posterior = outcome?.outcome === "posterior";
  const dataset = readModelDataset(model.dataset);

  return (
    <>
      <div className="d-flex align-items-center gap-2 mb-3 flex-wrap">
        <span className="fw-bold">{instanceLabel(instance)}</span>
        <StatusBadge tone={fitTone(instance.status)}>{instance.status}</StatusBadge>
        {instance.active && (
          <StatusBadge tone="green">
            <T text="active" />
          </StatusBadge>
        )}
        <StatusBadge tone="blue">{outcomeSummary(outcome)}</StatusBadge>
        {instance.name.trim() !== "" && <span className="text-muted small">{formatTimestamp(instance.created)}</span>}
      </div>

      {/* The failure sentence is on the row, because the request that started
          the fit returned long before it failed. */}
      {instance.error && <Alert variant="danger">{instance.error}</Alert>}
      {instance.dataset_changed && (
        <Alert variant="info">
          <T text="The dataset has changed since this fit. This instance keeps the version it read, and its predictions read the rows that way; fit again to use the new one." />
        </Alert>
      )}

      {posterior && <Warnings warnings={instance.warnings} />}
      {posterior && instance.program_changed && (
        <Alert variant="info">
          <T text="The program has changed since this fit. This instance keeps the copy it ran, so what it says is still about that program; fit again to see the new one." />
        </Alert>
      )}

      {rows && !posterior && (
        <Card className="mb-3">
          <Card.Header>
            <T text="Rows" />
          </Card.Header>
          <Card.Body className="d-flex flex-wrap gap-4">
            <Counted label={t("Selected")} value={rows.selected} />
            <Counted label={t("Train")} value={rows.train} />
            <Counted label={t("Validation")} value={rows.validation} />
            <Counted label={t("Test")} value={rows.test} />
            <Counted label={t("Dropped")} value={rows.dropped} />
          </Card.Body>
          {rows.dropped > 0 && (
            <Card.Footer className="text-muted small">
              {t(
                "{dropped} rows could not be represented by the encoding — a null in a feature, most often — and were dropped. A fit over {kept} of {selected} rows is a different claim from a fit over {selected}.",
                {
                  dropped: rows.dropped,
                  kept: rows.selected - rows.dropped,
                  selected: rows.selected,
                },
              )}
            </Card.Footer>
          )}
        </Card>
      )}

      {outputs === null ? (
        <div className="py-3 text-center">
          <Spinner animation="border" size="sm" />
        </div>
      ) : (
        <OutputsPanel outputs={outputs} collapsed={collapsed} plots={plots} onToggle={onToggle} onPlots={onPlots} />
      )}
      {!posterior && outputs && outputs.some((o) => o.name === "metrics") && (
        <p className="text-muted small">
          <T text="The metrics are computed by the host, by scoring this fit back over each split — so the same numbers mean the same thing for every provider. Read the test column: the training one is measured on the rows the fit was computed from." />
        </p>
      )}

      <ClassificationDetail metrics={metrics} />

      {posterior && (
        <PosteriorInstance
          instance={instance}
          model={model}
          onChanged={() => {
            void load();
            onChanged();
          }}
        />
      )}

      {search.length > 0 && (
        <Card className="mb-3">
          <Card.Header>
            <T text="Hyperparameter search" />
          </Card.Header>
          <div className="table-responsive">
            <Table size="sm" className="card-table table-vcenter">
              <thead>
                <tr>
                  {searchKeys(search).map((key) => (
                    <th key={key}>{key}</th>
                  ))}
                  <th>
                    <T text="Validation score" />
                  </th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {search.map((point, index) => {
                  const chosen = samePoint(point.hyperparameters, instance.hyperparameters);
                  return (
                    <tr key={index} className={chosen ? "table-active" : undefined}>
                      {searchKeys(search).map((key) => (
                        <td key={key} className="font-monospace">
                          {printGridValue(point.hyperparameters[key])}
                        </td>
                      ))}
                      <td>{point.error ? "—" : formatNumber(point.score)}</td>
                      <td className="text-muted small">
                        {/* A point that failed is recorded with its sentence
                            rather than dropped: "I tried k = 12 and it could
                            not be fitted" is the answer to "why k = 8". */}
                        {point.error ?? (chosen ? t("chosen") : "")}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </Table>
          </div>
          <Card.Footer className="text-muted small">
            <T text="Every point tried, scored on the validation rows. The chosen one was refitted and is what the metrics above are of." />
          </Card.Footer>
        </Card>
      )}

      {instance.status === "fitted" && outcome && outcome.outcome !== "test" && !posterior && (
        <TryARow
          key={instanceId}
          instanceId={instanceId}
          features={featureInputs(readEncoding(instance.encoding), dataset)}
          label={labelOf(outcome)}
        />
      )}
    </>
  );
}

/** One row count, as a Tabler-ish figure. */
function Counted({ label, value }: { label: string; value: number }) {
  return (
    <div>
      <div className="text-muted small">{label}</div>
      <div className="h3 mb-0">{value}</div>
    </div>
  );
}

/** A classification's per-class scores and its confusion matrix, off the test
 * rows where there are any.
 *
 * Support is beside precision and recall on purpose: a recall of 1.0 over three
 * rows reads like a recall of 1.0 over three thousand without it. */
function ClassificationDetail({ metrics }: { metrics: SplitMetrics }) {
  const { t } = useT();
  const set = firstOf(metrics);
  if (!set || set.metrics.metrics !== "classification") return null;
  const classification = set.metrics;
  return (
    <Card className="mb-3">
      <Card.Header className="text-capitalize">
        {t("Classes ({part})", { part: set.part })}
      </Card.Header>
      <div className="table-responsive">
        <Table size="sm" className="card-table table-vcenter">
          <thead>
            <tr>
              <th><T text="Class" /></th>
              <th><T text="Precision" /></th>
              <th><T text="Recall" /></th>
              <th>F1</th>
              <th><T text="Support" /></th>
              {classification.classes.map((klass: ClassMetrics) => (
                <th key={`predicted-${klass.class}`} className="text-muted fw-normal">
                  {t("predicted {class_}", { class_: klass.class })}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {classification.classes.map((klass: ClassMetrics, index: number) => (
              <tr key={klass.class}>
                <th scope="row" className="fw-normal">
                  {klass.class}
                </th>
                <td>{formatNumber(klass.precision)}</td>
                <td>{formatNumber(klass.recall)}</td>
                <td>{formatNumber(klass.f1)}</td>
                <td>{klass.support}</td>
                {classification.classes.map((_, column: number) => (
                  <td
                    key={column}
                    className={index === column ? "fw-bold" : "text-muted"}
                  >
                    {classification.confusion[index]?.[column] ?? 0}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </Table>
      </div>
      <Card.Footer className="text-muted small">
        <T text="The right-hand block is the confusion matrix: each row is what the rows actually were, each column what this fit said they are." />
      </Card.Footer>
    </Card>
  );
}

/** The split whose numbers should be read: the held-out rows, else whatever
 * there is. */
function firstOf(metrics: SplitMetrics): { part: string; metrics: Metrics } | null {
  for (const part of ["test", "validation", "train"] as const) {
    const set = metrics[part];
    if (set) return { part, metrics: set };
  }
  return null;
}

/** The label column of a supervised outcome — the one thing "try a row" must not
 * ask for, because it is what the fit answers. */
function labelOf(outcome: NonNullable<ReturnType<typeof readOutcome>>): string | null {
  return outcome.outcome === "regression" || outcome.outcome === "classification"
    ? outcome.label
    : null;
}

/** The union of every hyperparameter named by any point of a search. */
function searchKeys(search: { hyperparameters: Record<string, unknown> }[]): string[] {
  const keys: string[] = [];
  for (const point of search) {
    for (const key of Object.keys(point.hyperparameters)) {
      if (!keys.includes(key)) keys.push(key);
    }
  }
  return keys;
}

/** Whether a grid point is the one this fit used. */
function samePoint(point: Record<string, unknown>, chosen: unknown): boolean {
  if (!chosen || typeof chosen !== "object") return false;
  const other = chosen as Record<string, unknown>;
  const keys = new Set([...Object.keys(point), ...Object.keys(other)]);
  for (const key of keys) {
    if (JSON.stringify(point[key]) !== JSON.stringify(other[key])) return false;
  }
  return true;
}

/**
 * Ask this fit about a row that is not in the table.
 *
 * A literal row, not a filter: the point is a what-if, and its derived columns
 * are whatever is typed here because there is nothing to derive them from. The
 * boxes come from the **encoding**, so they are exactly the features the fit
 * uses — the label is not among them, which is right: it is the thing being
 * predicted, and demanding it would make a fitted model unusable on exactly the
 * rows it exists to answer about.
 */
function TryARow({
  instanceId,
  features,
  label,
}: {
  instanceId: string;
  features: FeatureInput[];
  label: string | null;
}) {
  const { t } = useT();
  const [values, setValues] = useState<Record<string, string>>({});
  const [answer, setAnswer] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const ask = async () => {
    setBusy(true);
    setError(null);
    setAnswer(null);
    try {
      const row: Record<string, unknown> = {};
      for (const feature of features) {
        const typed = values[feature.name] ?? "";
        // Typed as the fit's frame is typed, not as text: a row is encoded the
        // way the fit was, or it fails.
        if (typed.trim() !== "") row[feature.name] = typedFeatureValue(typed, feature.kind);
      }
      const answered = await api.predictRows({ instance: instanceId, rows: [row] });
      setAnswer(predictionSummary(readPrediction(answered.predictions[0]?.prediction)));
    } catch (err) {
      // Includes the refusal that matters most: a category this fit never saw is
      // an error naming the column and the value, not a row of zeros.
      setError(errorMessage(err, t("Could not predict that row.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card className="mb-3">
      <Card.Header><T text="Try a row" /></Card.Header>
      <Card.Body>
        <Row>
          {features.map((feature) => (
            <Col md={4} key={feature.name}>
              <Form.Group className="mb-3" controlId={`try-${feature.name}`}>
                <Form.Label>{feature.name}</Form.Label>
                {feature.categories ? (
                  // The categories this fit was shown. Any other value is
                  // refused by name at predict time rather than encoded as a row
                  // of zeros, so a free-text box here would only offer a way to
                  // be told no.
                  <Form.Select
                    value={values[feature.name] ?? ""}
                    onChange={(e) =>
                      setValues((v) => ({ ...v, [feature.name]: e.target.value }))
                    }
                  >
                    <option value="">—</option>
                    {feature.categories.map((category) => (
                      <option key={category} value={category}>
                        {category}
                      </option>
                    ))}
                  </Form.Select>
                ) : (
                  <Form.Control
                    value={values[feature.name] ?? ""}
                    onChange={(e) =>
                      setValues((v) => ({ ...v, [feature.name]: e.target.value }))
                    }
                  />
                )}
                <Form.Text muted className="font-monospace">
                  {feature.expr ?? feature.kind}
                </Form.Text>
              </Form.Group>
            </Col>
          ))}
        </Row>
        <div className="btn-list align-items-center">
          <Button onClick={() => void ask()} disabled={busy}>
            {busy ? <T text="Asking…" /> : <T text="Predict" />}
          </Button>
          {answer !== null && (
            <span>
              <span className="text-muted">{label ?? t("answer")}: </span>
              <strong>{answer}</strong>
            </span>
          )}
        </div>
        {error && (
          <Alert variant="danger" className="mt-3 mb-0">
            {error}
          </Alert>
        )}
      </Card.Body>
    </Card>
  );
}
