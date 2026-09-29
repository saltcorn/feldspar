// A posterior fit on the instance screen (Stan TODO §18).
//
// While it runs: the stage, a progress bar per chain, and Cancel — a fit here
// is compiles and processes, and can take an hour. After: the **warnings first**,
// in plain language and in the order they matter (chains that disagree before
// a tree depth that was hit), then the diagnostics, then one section per
// variable: its summary labelled by the database (`alpha[Aitkin]`, not
// `alpha.1`), and for a chosen element its trace per chain and its histogram;
// for a one-axis labelled variable, the forest plot a hierarchical model is
// read by. Download run and Write back are here because this is where the
// admin decides the fit is worth keeping.
//
// Like the rest of the model screens, **nothing here names Stan**: it renders
// what the host computed from the draws, which is the same for any provider
// that returns them.

import { useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import ProgressBar from "react-bootstrap/ProgressBar";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, downloadModelRun, errorMessage } from "../api";
import { catalog, columnType, type TableInfo } from "../codeTypes";
import {
  MAIN_DATASET,
  buildPosteriorWrite,
  chainPercent,
  chainTraces,
  elementAt,
  elementCount,
  elementName,
  elementSelection,
  forestRows,
  forestable,
  formatNumber,
  matchElements,
  metricRows,
  orderWarnings,
  readDataset,
  readMetrics,
  readParameters,
  readProgress,
  readRelated,
  readVariables,
  summaryTable,
  updateTarget,
  type ChainTrace,
  type ForestSort,
  type InstanceDetail,
  type ModelItem,
  type RecordedAxes,
  type SummaryTable,
  type WarningKind,
  type WriteBackForm,
} from "../models";
import type { BindReport } from "./ModelBindings";
import { stageText } from "./ModelForm";
import { ForestPlot, HistogramPlot, TracePlot, chainClass } from "./PosteriorPlots";
import { T, useT } from "../i18n";

/** How many rows of a summary are listed before the search box is the way to
 * the rest — `log_lik` has one per observation. */
const LISTED_ROWS = 100;

export function PosteriorInstance({
  instance,
  model,
  cancellable,
  onChanged,
}: {
  instance: InstanceDetail;
  model: ModelItem | null;
  /** Whether the provider can stop a running fit. */
  cancellable: boolean;
  /** Re-read the instance: after a cancel, a write-back. */
  onChanged: () => void;
}) {
  const { t } = useT();
  const [error, setError] = useState<string | null>(null);
  const [downloading, setDownloading] = useState(false);

  const variables = useMemo(() => readVariables(instance.variables), [instance.variables]);
  const names = Object.keys(variables);
  const tables = useMemo(() => {
    const out = new Map<string, SummaryTable>();
    for (const block of readParameters(instance.parameters)) {
      if (block.block !== "table") continue;
      const axes = variables[block.name];
      out.set(
        block.name,
        summaryTable(block.columns, block.rows.map((r) => r.cells), axes?.dims.length ?? 0),
      );
    }
    return out;
  }, [instance.parameters, variables]);
  // The program's order, which is the order the stored tables are in, then
  // anything recorded without a table (a large generated quantity).
  const ordered = [
    ...[...tables.keys()].filter((n) => variables[n]),
    ...names.filter((n) => !tables.has(n)),
  ];
  const [chosen, setChosen] = useState<string | null>(null);
  const variable = chosen && variables[chosen] ? chosen : (ordered[0] ?? null);

  const metrics = readMetrics(instance.metrics).train ?? null;
  const progress = readProgress(instance.progress);
  const binding = (instance.binding ?? null) as BindReport | null;
  const datasets = useMemo(
    () =>
      model
        ? [
            { name: MAIN_DATASET, table: readDataset(model.dataset, model.table_name).table },
            ...readRelated(model.related).map((r) => ({ name: r.name, table: r.dataset.table })),
          ]
        : [],
    [model],
  );

  const cancel = async () => {
    setError(null);
    try {
      await api.cancelModelFit(instance.id);
      onChanged();
    } catch (err) {
      setError(errorMessage(err, "Could not cancel the fit."));
    }
  };

  const download = async () => {
    setDownloading(true);
    setError(null);
    try {
      await downloadModelRun(instance.id);
    } catch (err) {
      setError(errorMessage(err, "Could not download the run."));
    } finally {
      setDownloading(false);
    }
  };

  return (
    <>
      {error && <Alert variant="danger">{error}</Alert>}

      {instance.status === "fitting" && (
        <Card className="mb-3">
          <Card.Header className="d-flex align-items-center gap-2">
            <Spinner animation="border" size="sm" />
            <span>
              {progress ? stageText(t, progress.stage) : t("starting")}
            </span>
            {cancellable && (
              <Button
                size="sm"
                variant="outline-danger"
                className="ms-auto"
                disabled={instance.cancel_requested}
                onClick={() => void cancel()}
              >
                {instance.cancel_requested ? <T text="Stopping…" /> : <T text="Cancel" />}
              </Button>
            )}
          </Card.Header>
          {/* `.viz`, so the chain keys have the plots' colours. */}
          <Card.Body className="viz">
            {progress && progress.chains.length > 0 ? (
              progress.chains.map((c) => (
                <div className="mb-2" key={c.chain}>
                  <div className="d-flex small text-secondary">
                    <span>
                      <span className={`viz-key ${chainClass(c.chain)}`} />
                      {t("Chain {chain}", { chain: c.chain })}
                    </span>
                    <span className="ms-auto">
                      {c.phase === "warmup"
                        ? t("warmup, iteration {i} of {n}", { i: c.iteration, n: c.total })
                        : t("sampling, iteration {i} of {n}", { i: c.iteration, n: c.total })}
                    </span>
                  </div>
                  <ProgressBar
                    now={chainPercent(c)}
                    variant={c.phase === "warmup" ? "secondary" : "primary"}
                    aria-label={t("Chain {chain}", { chain: c.chain })}
                  />
                </div>
              ))
            ) : (
              <p className="text-muted mb-0">
                {progress?.stage === "compiling" ? (
                  <T text="Compiling the program — a C++ compile, a minute or so, once per program: the next fit of the same program starts sampling at once." />
                ) : progress?.stage === "queued" ? (
                  <T text="Waiting for the server's process budget: other fits' chains are running." />
                ) : (
                  <T text="The fit is running on the server; this screen asks again every second or so." />
                )}
              </p>
            )}
          </Card.Body>
        </Card>
      )}

      <Warnings warnings={instance.warnings} />

      {instance.program_changed && (
        <Alert variant="info">
          <T text="The program has changed since this fit. This instance keeps the copy it ran, so what it says is still about that program; fit again to see the new one." />
        </Alert>
      )}

      {instance.status === "fitted" && (
        <div className="btn-list mb-3">
          <Button variant="outline-secondary" disabled={downloading} onClick={() => void download()}>
            {downloading ? <T text="Preparing…" /> : <T text="Download run" />}
          </Button>
          <span className="text-muted small align-self-center">
            <T text="A zip of the run: CmdStan's own CSVs when the model keeps its raw runs, otherwise the draws as per-chain CSVs — readable by cmdstanpy, ArviZ or R's posterior." />
          </span>
        </div>
      )}

      {metrics && metricRows(metrics).length > 0 && (
        <Card className="mb-3">
          <Card.Header><T text="Diagnostics" /></Card.Header>
          <div className="table-responsive">
            <Table size="sm" className="card-table table-vcenter">
              <tbody>
                {metricRows(metrics).map((row) => (
                  <tr key={row.label}>
                    <th scope="row" className="fw-normal">
                      {row.label}
                    </th>
                    <td>{row.value}</td>
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
          <Card.Footer className="text-muted small">
            {metrics.metrics === "posterior_approximation" ? (
              <T text="Pathfinder's draws are independent draws from one approximation, not chains, so there is no R̂: nothing is being compared." />
            ) : metrics.metrics === "posterior_mode" ? (
              <T text="The optimiser finds one point, the posterior mode: there are no draws to mix and no intervals." />
            ) : (
              <T text="Computed by the host from the draws, with the published thresholds: R̂ at most 1.01, at least 100 effective draws per chain, no divergences, E-BFMI at least 0.3." />
            )}
          </Card.Footer>
        </Card>
      )}

      {binding && <BindingCard report={binding} />}

      {instance.status === "fitted" && variable && (
        <Row>
          <Col lg={3} className="mb-3">
            <Card>
              <Card.Header><T text="Variables" /></Card.Header>
              <div className="list-group list-group-flush">
                {ordered.map((name) => (
                  <button
                    type="button"
                    key={name}
                    className={`list-group-item list-group-item-action d-flex${name === variable ? " active" : ""}`}
                    onClick={() => setChosen(name)}
                  >
                    <span className="font-monospace">{name}</span>
                    <span className="ms-auto small opacity-75">
                      {variables[name].dims.length === 0 ? "" : variables[name].dims.join(" × ")}
                    </span>
                  </button>
                ))}
              </div>
            </Card>
          </Col>
          <Col lg={9}>
            <VariableView
              key={variable}
              instanceId={instance.id}
              name={variable}
              axes={variables[variable]}
              stored={tables.get(variable) ?? null}
              datasets={datasets}
              onWritten={onChanged}
            />
          </Col>
        </Row>
      )}
    </>
  );
}

/** The heading each kind of warning is read under. */
function useWarningTitle(): (kind: WarningKind) => string {
  const { t } = useT();
  return (kind) => {
    switch (kind) {
      case "rhat":
        return t("The chains disagree");
      case "divergent":
        return t("Divergent transitions");
      case "ebfmi":
        return t("The sampler moved poorly between energy levels");
      case "ess":
        return t("Too few effective draws");
      case "treedepth":
        return t("Trajectories were cut short");
      case "other":
        return t("Worth knowing");
    }
  };
}

/** The warnings, ordered and headed. A fit with warnings is still fitted — a
 * posterior is not wrong because it is hard — and these say what to do. */
function Warnings({ warnings }: { warnings: string[] }) {
  const title = useWarningTitle();
  if (warnings.length === 0) return null;
  return (
    <>
      {orderWarnings(warnings).map((w) => (
        <Alert variant={w.serious ? "danger" : "warning"} key={w.text}>
          <div className="fw-bold">{title(w.kind)}</div>
          {w.text}
        </Alert>
      ))}
    </>
  );
}

/** What the binder bound: each dataset's rows, each dimension's size, drops. */
function BindingCard({ report }: { report: BindReport }) {
  const { t } = useT();
  return (
    <Card className="mb-3">
      <Card.Header><T text="Data" /></Card.Header>
      <Card.Body>
        <div className="d-flex flex-wrap gap-4 mb-2">
          {Object.entries(report.dimensions ?? {}).map(([name, size]) => (
            <div key={name}>
              <div className="text-muted small font-monospace">{name}</div>
              <div className="h3 mb-0">{size}</div>
            </div>
          ))}
        </div>
        <div className="text-muted small">
          {(report.datasets ?? [])
            .map((d) =>
              d.read === d.bound
                ? t("{name}: {rows} rows", { name: d.name, rows: d.read })
                : t("{name}: {bound} of {read} rows", { name: d.name, bound: d.bound, read: d.read }),
            )
            .join(" · ")}
        </div>
        {(report.drops ?? []).map((d) => (
          <Alert variant="secondary" className="mt-2 mb-0 py-2" key={d.sentence}>
            {d.sentence}
          </Alert>
        ))}
        {(report.warnings ?? []).map((w) => (
          <Alert variant="warning" className="mt-2 mb-0 py-2" key={w}>
            {w}
          </Alert>
        ))}
      </Card.Body>
    </Card>
  );
}

/**
 * One variable: its summary, its forest plot when it has one, and a chosen
 * element's trace and histogram.
 */
function VariableView({
  instanceId,
  name,
  axes,
  stored,
  datasets,
  onWritten,
}: {
  instanceId: string;
  name: string;
  axes: RecordedAxes;
  stored: SummaryTable | null;
  datasets: { name: string; table: string }[];
  onWritten: () => void;
}) {
  const { t } = useT();
  const [table, setTable] = useState<SummaryTable | null>(stored);
  const [summaryError, setSummaryError] = useState<string | null>(null);
  const [summarising, setSummarising] = useState(false);
  const [query, setQuery] = useState("");
  const [row, setRow] = useState(0);
  const [sort, setSort] = useState<ForestSort>("mean");
  const [traces, setTraces] = useState<ChainTrace[] | null>(null);
  const [drawsError, setDrawsError] = useState<string | null>(null);
  const [writing, setWriting] = useState(false);

  // A variable too large to have been summarised at fit time is summarised on
  // demand — from its draws, now — and the answer carries the keys too.
  const summarise = async () => {
    setSummarising(true);
    setSummaryError(null);
    try {
      const answer = await api.getPosteriorSummary(instanceId, { variable: name });
      setTable(
        summaryTable(
          answer.columns,
          answer.rows as unknown[][],
          axes.dims.length,
          answer.keys as unknown[][],
        ),
      );
    } catch (err) {
      setSummaryError(errorMessage(err, "Could not summarise this variable."));
    } finally {
      setSummarising(false);
    }
  };

  // The stored table has labels but not keys; one summary request adds them,
  // so the search box finds an element by its key too. Only for what is small.
  useEffect(() => {
    if (!stored || elementCount(axes) > 1000) return;
    if (!axes.dimensions.some(Boolean)) return;
    let cancelled = false;
    void api
      .getPosteriorSummary(instanceId, { variable: name })
      .then((answer) => {
        if (!cancelled) {
          setTable(
            summaryTable(
              answer.columns,
              answer.rows as unknown[][],
              axes.dims.length,
              answer.keys as unknown[][],
            ),
          );
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [instanceId, name, stored, axes]);

  // The chosen element's draws.
  const element = elementAt(row, axes.dims);
  const elementKey = elementSelection(element);
  useEffect(() => {
    let cancelled = false;
    setTraces(null);
    setDrawsError(null);
    void api
      .getModelDraws(instanceId, { variable: name, elements: elementKey })
      .then((answer) => {
        if (!cancelled) setTraces(chainTraces(answer));
      })
      .catch((err: unknown) => {
        if (!cancelled) setDrawsError(errorMessage(err, "Could not read the draws."));
      });
    return () => {
      cancelled = true;
    };
  }, [instanceId, name, elementKey]);

  const matched = useMemo(() => (table ? matchElements(table, query) : []), [table, query]);
  const forest = useMemo(
    () => (table && forestable(axes) ? forestRows(table, sort) : []),
    [table, axes, sort],
  );
  const label = elementName(name, table?.rows[row]?.labels ?? [], element);
  const pooled = useMemo(() => (traces ?? []).flatMap((c) => c.values), [traces]);

  return (
    <>
      <Card className="mb-3">
        <Card.Header className="d-flex align-items-center gap-2 flex-wrap">
          <span className="font-monospace fw-bold">{name}</span>
          <span className="text-muted small">
            {axes.dims.length === 0
              ? t("scalar")
              : axes.dims
                  .map((d, k) => (axes.dimensions[k] ? `${d} ${axes.dimensions[k]}` : String(d)))
                  .join(" × ")}
          </span>
          <Button size="sm" variant="outline-primary" className="ms-auto" onClick={() => setWriting(true)}>
            <T text="Write back" />
          </Button>
        </Card.Header>
        {!table && (
          <Card.Body>
            <p className="text-muted">
              {t("{name} has {count} elements, more than a fit summarises on its own.", {
                name,
                count: elementCount(axes),
              })}
            </p>
            <Button size="sm" disabled={summarising} onClick={() => void summarise()}>
              {summarising ? <T text="Summarising…" /> : <T text="Summarise from the draws" />}
            </Button>
            {summaryError && <Alert variant="danger" className="mt-3 mb-0">{summaryError}</Alert>}
          </Card.Body>
        )}
        {table && (
          <>
            {table.rows.length > 1 && (
              <Card.Body className="py-2 border-bottom">
                <Form.Control
                  size="sm"
                  value={query}
                  placeholder={t("Find an element by key or label (or #position)")}
                  aria-label={t("Find an element")}
                  onChange={(e) => setQuery(e.target.value)}
                />
              </Card.Body>
            )}
            <div className="table-responsive summary-scroll">
              <Table size="sm" hover className="card-table table-vcenter">
                <thead>
                  <tr>
                    {table.labelColumns.map((c) => (
                      <th key={`l-${c}`}>{c}</th>
                    ))}
                    {table.statColumns.map((c) => (
                      <th key={`s-${c}`} className="text-end">
                        {c}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {matched.slice(0, LISTED_ROWS).map((i) => (
                    <tr
                      key={i}
                      className={i === row ? "table-active" : undefined}
                      role="button"
                      onClick={() => setRow(i)}
                    >
                      {table.rows[i].labels.map((l, k) => (
                        <td key={`l${k}`}>{l}</td>
                      ))}
                      {table.rows[i].stats.map((v, k) => (
                        <td key={`s${k}`} className="text-end font-monospace">
                          {formatNumber(v)}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </Table>
            </div>
            {matched.length > LISTED_ROWS && (
              <Card.Footer className="text-muted small">
                {t("The first {shown} of {count}; search for the others.", {
                  shown: LISTED_ROWS,
                  count: matched.length,
                })}
              </Card.Footer>
            )}
            {matched.length === 0 && (
              <Card.Footer className="text-muted small">
                <T text="No element has that key or label." />
              </Card.Footer>
            )}
          </>
        )}
      </Card>

      {forest.length > 0 && (
        <Card className="mb-3">
          <Card.Header className="d-flex align-items-center gap-2">
            <T text="Forest plot" />
            <Form.Select
              size="sm"
              className="w-auto ms-auto"
              value={sort}
              aria-label={t("Order the rows by")}
              onChange={(e) => setSort(e.target.value as ForestSort)}
            >
              <option value="mean">{t("by mean")}</option>
              <option value="label">{t("by label")}</option>
              <option value="position">{t("in the dimension's order")}</option>
            </Form.Select>
          </Card.Header>
          <Card.Body>
            <ForestPlot rows={forest} selected={row} onSelect={setRow} />
          </Card.Body>
          <Card.Footer className="text-muted small">
            <T text="The mean and the 90% interval (5% to 95%) of each element. A group with little data has a wide interval, pulled toward the others — that is partial pooling. Click a row for its trace." />
          </Card.Footer>
        </Card>
      )}

      <Card className="mb-3">
        <Card.Header>
          <span className="font-monospace">{label}</span>
        </Card.Header>
        <Card.Body>
          {drawsError && <Alert variant="secondary" className="mb-0">{drawsError}</Alert>}
          {!drawsError && !traces && (
            <div className="text-center py-3">
              <Spinner animation="border" size="sm" />
            </div>
          )}
          {traces && traces.length > 0 && (
            <Row>
              <Col xl={7}>
                <h4 className="h5"><T text="Trace per chain" /></h4>
                <TracePlot traces={traces} />
              </Col>
              <Col xl={5}>
                <h4 className="h5"><T text="Histogram" /></h4>
                <HistogramPlot values={pooled} />
              </Col>
            </Row>
          )}
        </Card.Body>
        <Card.Footer className="text-muted small">
          <T text="Chains that mixed overlap in one band with no trend; a chain off on its own is what a large R̂ means." />
        </Card.Footer>
      </Card>

      {writing && (
        <WriteBack
          instanceId={instanceId}
          variable={name}
          axes={axes}
          table={table}
          datasets={datasets}
          onClose={() => setWriting(false)}
          onWritten={onWritten}
        />
      )}
    </>
  );
}

/**
 * Write a variable's summary back into the database (`writePosterior`, Stan
 * TODO §16): **update** puts statistics into the rows of the table its one axis
 * is the rows of, matched by key; **insert** makes one row per element in any
 * table, with the element's coordinates.
 */
function WriteBack({
  instanceId,
  variable,
  axes,
  table,
  datasets,
  onClose,
  onWritten,
}: {
  instanceId: string;
  variable: string;
  axes: RecordedAxes;
  table: SummaryTable | null;
  datasets: { name: string; table: string }[];
  onClose: () => void;
  onWritten: () => void;
}) {
  const { t } = useT();
  const target = updateTarget(axes, datasets);
  const [schema, setSchema] = useState<TableInfo[]>([]);
  const [form, setForm] = useState<WriteBackForm>({
    mode: target ? "update" : "insert",
    statistics: {},
    table: "",
    coordinates: (table?.labelColumns ?? []).map((axis) => ({ axis, field: "", value: "key" as const })),
    instanceField: "",
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);

  useEffect(() => {
    void catalog()
      .then(setSchema)
      .catch(() => setSchema([]));
  }, []);

  const into = form.mode === "update" ? target ?? "" : form.table;
  const fields = schema.find((s) => s.name === into)?.columns ?? [];
  const numeric = fields.filter((c) => columnType(c).startsWith("number")).map((c) => c.name);
  const statistics = table?.statColumns ?? ["mean", "sd", "q5", "q50", "q95"];

  const submit = async () => {
    setBusy(true);
    setError(null);
    setDone(null);
    try {
      const answer = await api.writePosterior(instanceId, buildPosteriorWrite(variable, form));
      setDone(t("Wrote {count} rows into {table}.", { count: answer.written, table: answer.table }));
      onWritten();
    } catch (err) {
      setError(errorMessage(err, "Could not write the posterior back."));
    } finally {
      setBusy(false);
    }
  };

  const fieldSelect = (value: string, onChange: (v: string) => void, options: string[], label: string) => (
    <Form.Select size="sm" value={value} aria-label={label} onChange={(e) => onChange(e.target.value)}>
      <option value="">—</option>
      {options.map((o) => (
        <option key={o} value={o}>
          {o}
        </option>
      ))}
    </Form.Select>
  );

  return (
    <Modal show onHide={onClose} size="lg">
      <Modal.Header closeButton>
        <Modal.Title>
          {t("Write {variable} back", { variable })}
        </Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <Form.Group className="mb-3">
          <Form.Check
            type="radio"
            id="write-back-update"
            name="write-back-mode"
            disabled={!target}
            checked={form.mode === "update"}
            onChange={() => setForm((f) => ({ ...f, mode: "update" }))}
            label={
              target
                ? t("Update the rows of {table}, matched by key", { table: target })
                : t("Update — only a variable with one axis over a dataset's rows can be")
            }
          />
          <Form.Check
            type="radio"
            id="write-back-insert"
            name="write-back-mode"
            checked={form.mode === "insert"}
            onChange={() => setForm((f) => ({ ...f, mode: "insert" }))}
            label={t("Insert one new row per element into a table")}
          />
        </Form.Group>

        {form.mode === "insert" && (
          <Form.Group className="mb-3" controlId="write-back-table">
            <Form.Label><T text="Table" /></Form.Label>
            <Form.Select
              value={form.table}
              onChange={(e) => setForm((f) => ({ ...f, table: e.target.value }))}
            >
              <option value="">—</option>
              {schema.map((s) => (
                <option key={s.name} value={s.name}>
                  {s.name}
                </option>
              ))}
            </Form.Select>
          </Form.Group>
        )}

        <h4 className="h5"><T text="Statistics" /></h4>
        <p className="text-muted small">
          <T text="Each into a number field of the target; an effective sample size may go into a whole-number one." />
        </p>
        <Table size="sm" className="mb-3">
          <tbody>
            {statistics.map((stat) => (
              <tr key={stat}>
                <td className="font-monospace">{stat}</td>
                <td>
                  {fieldSelect(
                    form.statistics[stat] ?? "",
                    (v) => setForm((f) => ({ ...f, statistics: { ...f.statistics, [stat]: v } })),
                    stat.startsWith("ess") ? fields.map((c) => c.name) : numeric,
                    t("The field {stat} goes into", { stat }),
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </Table>

        {form.mode === "insert" && (
          <>
            <h4 className="h5"><T text="Coordinates" /></h4>
            <Table size="sm" className="mb-3">
              <tbody>
                {form.coordinates.map((c, i) => (
                  <tr key={c.axis}>
                    <td className="font-monospace">{c.axis}</td>
                    <td>
                      <Form.Select
                        size="sm"
                        value={c.value}
                        aria-label={t("Which part of {axis}", { axis: c.axis })}
                        onChange={(e) =>
                          setForm((f) => ({
                            ...f,
                            coordinates: f.coordinates.map((x, j) =>
                              j === i ? { ...x, value: e.target.value as "key" | "label" | "position" } : x,
                            ),
                          }))
                        }
                      >
                        <option value="key">{t("its key")}</option>
                        <option value="label">{t("its label")}</option>
                        <option value="position">{t("its position")}</option>
                      </Form.Select>
                    </td>
                    <td>
                      {fieldSelect(
                        c.field,
                        (v) =>
                          setForm((f) => ({
                            ...f,
                            coordinates: f.coordinates.map((x, j) => (j === i ? { ...x, field: v } : x)),
                          })),
                        fields.map((x) => x.name),
                        t("The field {axis} goes into", { axis: c.axis }),
                      )}
                    </td>
                  </tr>
                ))}
                <tr>
                  <td><T text="this fit's id" /></td>
                  <td />
                  <td>
                    {fieldSelect(
                      form.instanceField,
                      (v) => setForm((f) => ({ ...f, instanceField: v })),
                      fields.map((x) => x.name),
                      t("The field the fit's id goes into"),
                    )}
                  </td>
                </tr>
              </tbody>
            </Table>
          </>
        )}

        <p className="text-muted small mb-0">
          <T text="The rows are written through the table's own rules and fire its triggers, as an edit would. A code body does the same with m.writePosterior(…) — in a workflow, a code step after a fit_model." />
        </p>
        {error && <Alert variant="danger" className="mt-3 mb-0">{error}</Alert>}
        {done && <Alert variant="success" className="mt-3 mb-0">{done}</Alert>}
      </Modal.Body>
      <Modal.Footer>
        <Button variant="outline-secondary" onClick={onClose}>
          <T text="Close" />
        </Button>
        <Button disabled={busy || into === ""} onClick={() => void submit()}>
          {busy ? <T text="Writing…" /> : <T text="Write" />}
        </Button>
      </Modal.Footer>
    </Modal>
  );
}
