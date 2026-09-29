// One fit: what it produced, what it scored, what it was searched over, and what
// it answers about a row (TODO "Predictive models", task 6.5).
//
// The screen renders **three variants of parameter block and nothing else**
// (§7). A provider owns its parameters — a coefficient table, a cluster centre,
// an explained-variance ratio — and structures them as a scalar, a table or a
// block of text, so this screen never has to know what any of them mean. That is
// what lets a scikit-learn estimator from a bundled module render here beside a
// smartcore regression with no change.
//
// The metrics are the other way round: they are the **host's**, computed by
// scoring the fit back over each split with one piece of code, which is what
// makes two providers comparable. So they are shown per split, because "R² 0.94"
// is not a claim about anything until you know whether it was measured on the
// rows the fit was computed from.
//
// A **posterior** (Stan TODO §18) is a different reading of the same row — its
// parameters are draws, its metrics the sampler's diagnostics — and is rendered
// by `PosteriorInstance.tsx`, which this screen hands the whole of the body to.
//
// And "try a row" is the point of the whole milestone in one box: an instance is
// something you *read* (the coefficients, the p-values) and something you
// *apply*, and the second half should not need a trigger to see.

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
import { navigate } from "../App";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import {
  featureInputs,
  formatNumber,
  formatParameterCell,
  formatTimestamp,
  instanceLabel,
  isPValueColumn,
  metricRows,
  outcomeSummary,
  predictionSummary,
  printGridValue,
  readDataset,
  readEncoding,
  readMetrics,
  readOutcome,
  readParameters,
  readPrediction,
  readRowCounts,
  readSearch,
  significanceStars,
  typedFeatureValue,
  type ClassMetrics,
  type Dataset,
  type FeatureInput,
  type InstanceDetail,
  type Metrics,
  type ModelItem,
  type ParameterBlock,
  type SplitMetrics,
} from "../models";
import { fitTone } from "./Models";
import { PosteriorInstance } from "./PosteriorInstance";
import { T, useT } from "../i18n";

/** How often a fit still running is re-read (§8). */
const POLL_MS = 1500;

/** The three splits, in the order they are read in. */
const PARTS = ["train", "validation", "test"] as const;

export function ModelInstance({ instanceId }: { instanceId: string }) {
  const { t } = useT();
  const [instance, setInstance] = useState<InstanceDetail | null>(null);
  const [modelName, setModelName] = useState("");
  const [dataset, setDataset] = useState<Dataset | null>(null);
  const [model, setModel] = useState<ModelItem | null>(null);
  const [cancellable, setCancellable] = useState(false);
  const [bindsData, setBindsData] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const detail = await api.getModelInstance(instanceId);
      setInstance(detail);
      // The model is read for its name and its dataset: "try a row" asks for the
      // columns the fit was over, and they live on the model rather than on the
      // fit.
      const model = await api.getModel(detail.model).catch(() => null);
      if (model) {
        setModel(model);
        setModelName(model.name);
        setDataset(readDataset(model.dataset, model.table_name));
        // Whether Cancel is offered is the provider's to say, and so is
        // whether this is a posterior before the fit has recorded its outcome.
        if (detail.status === "fitting") {
          const listed = await api.listModelProviders().catch(() => null);
          const provider = listed?.providers.find((p) => p.name === model.provider);
          setCancellable(Boolean(provider?.cancellable));
          setBindsData(Boolean(provider?.binds_data));
        }
      }
    } catch (err) {
      setLoadError(errorMessage(err, "Could not load this fit."));
    }
  }, [instanceId]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (instance?.status !== "fitting") return undefined;
    const timer = window.setTimeout(() => void load(), POLL_MS);
    return () => window.clearTimeout(timer);
  }, [instance, load]);

  const activate = async () => {
    setError(null);
    try {
      await api.activateModelInstance(instanceId);
      await load();
    } catch (err) {
      setError(errorMessage(err, "Could not activate this fit."));
    }
  };

  const remove = async () => {
    if (!instance) return;
    if (
      !window.confirm(
        t('Remove the fit "{name}"?', { name: instanceLabel(instance) }),
      )
    ) {
      return;
    }
    try {
      await api.deleteModelInstance(instanceId);
      navigate(`/models/${encodeURIComponent(instance.model)}`);
    } catch (err) {
      setError(errorMessage(err, "Could not remove this fit."));
    }
  };

  if (loadError) {
    return (
      <PageBody>
        <Alert variant="danger">{loadError}</Alert>
      </PageBody>
    );
  }
  if (!instance) {
    return (
      <PageBody>
        <div className="py-5 text-center">
          <Spinner animation="border" role="status" />
        </div>
      </PageBody>
    );
  }

  const outcome = readOutcome(instance.outcome);
  const metrics = readMetrics(instance.metrics);
  const parameters = readParameters(instance.parameters);
  const search = readSearch(instance.search);
  const rows = readRowCounts(instance.rows);
  // A running fit has no outcome yet, so a posterior is also known by its
  // provider, or by the progress only a posterior reports.
  const posterior =
    outcome?.outcome === "posterior" ||
    (instance.status === "fitting" && (bindsData || instance.progress != null));

  return (
    <>
      <PageHeader
        pretitle={modelName ? `Models · ${modelName}` : "Models"}
        title={instanceLabel(instance)}
        actions={
          <>
            <Button
              variant="outline-secondary"
              onClick={() => navigate(`/models/${encodeURIComponent(instance.model)}`)}
            >
              <IconArrowLeft className="icon-2" />
              <T text="Back to the model" />
            </Button>
            {instance.status === "fitted" && !instance.active && (
              <Button onClick={() => void activate()}><T text="Activate" /></Button>
            )}
            <Button variant="outline-danger" onClick={() => void remove()}>
              <T text="Remove" />
            </Button>
          </>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        <div className="d-flex align-items-center gap-2 mb-3 flex-wrap">
          <StatusBadge tone={fitTone(instance.status)}>{instance.status}</StatusBadge>
          {instance.active && <StatusBadge tone="green"><T text="active" /></StatusBadge>}
          <StatusBadge tone="blue">{outcomeSummary(outcome)}</StatusBadge>
          <span className="text-muted small">{formatTimestamp(instance.created)}</span>
        </div>

        {instance.status === "fitting" && !posterior && (
          <Alert variant="info">
            <T text="This fit is running on the server. The screen is asking again every second or so — there is nothing to wait on, because the row is the only record of the job." />
          </Alert>
        )}
        {/* The failure sentence is on the row, because the request that started
            the fit returned long before it failed. */}
        {instance.error && <Alert variant="danger">{instance.error}</Alert>}

        {posterior && (
          <PosteriorInstance
            instance={instance}
            model={model}
            cancellable={cancellable}
            onChanged={() => void load()}
          />
        )}

        {rows && !posterior && (
          <Card className="mb-3">
            <Card.Header><T text="Rows" /></Card.Header>
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

        {hasMetrics(metrics) && !posterior && (
          <Card className="mb-3">
            <Card.Header><T text="Metrics" /></Card.Header>
            <MetricsTable metrics={metrics} />
            <Card.Footer className="text-muted small">
              <T text="These are computed by the host, by scoring this fit back over each split — so the same numbers mean the same thing for every provider." />{" "}
              <T
                text="Read the {column} column: the training one is measured on the rows the fit was computed from."
                values={{
                  column: (
                    <strong>
                      <T text="test" />
                    </strong>
                  ),
                }}
              />
            </Card.Footer>
          </Card>
        )}

        <ClassificationDetail metrics={metrics} />

        {parameters.length > 0 && !posterior && (
          <Card className="mb-3">
            <Card.Header><T text="Parameters" /></Card.Header>
            <Card.Body>
              {parameters.map((block, index) => (
                <ParameterView key={`${block.name}-${index}`} block={block} />
              ))}
            </Card.Body>
            <Card.Footer className="text-muted small">
              <T text="These are the provider's own — what it fitted, in its own vocabulary." />
            </Card.Footer>
          </Card>
        )}

        {search.length > 0 && (
          <Card className="mb-3">
            <Card.Header><T text="Hyperparameter search" /></Card.Header>
            <div className="table-responsive">
              <Table size="sm" className="card-table table-vcenter">
                <thead>
                  <tr>
                    {searchKeys(search).map((key) => (
                      <th key={key}>{key}</th>
                    ))}
                    <th><T text="Validation score" /></th>
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
                          {point.error ?? (chosen ? "chosen" : "")}
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

        {instance.status === "fitted" &&
          outcome &&
          outcome.outcome !== "test" &&
          !posterior && (
          <TryARow
            instanceId={instanceId}
            features={featureInputs(readEncoding(instance.encoding), dataset)}
            label={labelOf(outcome)}
          />
        )}
      </PageBody>
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

/** Whether any split scored anything — a hypothesis test scores nothing, and an
 * empty metrics card would say the fit failed to compute them. */
function hasMetrics(metrics: SplitMetrics): boolean {
  return PARTS.some((part) => metricRows(metrics[part]).length > 0);
}

/** The metrics of every split that has rows, one column each.
 *
 * The row labels come from the first split that scored anything: the variant is
 * the outcome's, so every split of one fit has the same rows. */
function MetricsTable({ metrics }: { metrics: SplitMetrics }) {
  const present = PARTS.filter((part) => metricRows(metrics[part]).length > 0);
  const labels = metricRows(metrics[present[0]]).map((row) => row.label);
  return (
    <div className="table-responsive">
      <Table size="sm" className="card-table table-vcenter">
        <thead>
          <tr>
            <th />
            {present.map((part) => (
              <th key={part} className="text-capitalize">
                {part}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {labels.map((label) => (
            <tr key={label}>
              <th scope="row" className="fw-normal">
                {label}
              </th>
              {present.map((part) => (
                <td key={part}>
                  {metricRows(metrics[part]).find((row) => row.label === label)?.value ?? "—"}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </Table>
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

/** One parameter block, in the one of three renderings its variant asks for. */
function ParameterView({ block }: { block: ParameterBlock }) {
  if (block.block === "scalar") {
    return (
      <div className="mb-3">
        <span className="text-muted">{block.name}: </span>
        <strong>
          {isPValueColumn(block.name) ? formatParameterCell("p", block.value) : formatNumber(block.value)}
        </strong>
      </div>
    );
  }
  if (block.block === "text") {
    // Text a provider produced and nobody should reformat — statsmodels'
    // `summary()` is the case this variant exists for.
    return (
      <div className="mb-3">
        <div className="text-muted mb-1">{block.name}</div>
        <pre className="font-monospace small mb-0">{block.body}</pre>
      </div>
    );
  }
  const stars = block.columns.some(isPValueColumn);
  return (
    <div className="mb-4">
      <div className="text-muted mb-1">{block.name}</div>
      <div className="table-responsive">
        <Table size="sm" className="table-vcenter">
          <thead>
            <tr>
              {block.columns.map((column) => (
                <th key={column}>{column}</th>
              ))}
              {stars && <th />}
            </tr>
          </thead>
          <tbody>
            {block.rows.map((row, index) => (
              <tr key={index}>
                {block.columns.map((column, cell) => (
                  <td key={column} className={cell === 0 ? "" : "text-nowrap"}>
                    {formatParameterCell(column, row.cells[cell])}
                  </td>
                ))}
                {stars && (
                  <td className="font-monospace" title="p < 0.001 ***, < 0.01 **, < 0.05 *, < 0.1 .">
                    {significanceStars(row.cells[block.columns.findIndex(isPValueColumn)])}
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </Table>
      </div>
    </div>
  );
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
      setError(errorMessage(err, "Could not predict that row."));
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
            {busy ? "Asking…" : "Predict"}
          </Button>
          {answer !== null && (
            <span>
              <span className="text-muted">{label ?? "answer"}: </span>
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
