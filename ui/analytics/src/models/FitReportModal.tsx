// A fit's report, in a modal (the model editor's fit list: "Report"). The
// model editor itself shows no outputs: a fit is read here.
//
// **Report** is the report as a Report workspace would hold it
// (`fitReport.ts`), drawn read-only (`ReportPreview`): the facts of the fit,
// its warnings, its rows, its outputs — the provider's own, then the optional
// plots added from "More plots", which are the model's view state as they
// were on the editor — its classes and its search. Nothing is stored until
// **Save as report workspace**, which creates the workspace with that state
// and links it to the fit, so the fit's row links to it from then on and it
// is on the Analytics front page like any other workspace. **Print** prints
// the report as the workspace's Export PDF does.
//
// **Posterior** (a posterior fit) is the interactive reading a report cannot
// be: every variable's summary, trace and histogram, the forest plot, write
// back and the run's download (`PosteriorInstance.tsx`). **Try a row** asks
// the fit about a row typed in.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Nav from "react-bootstrap/Nav";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { printReport } from "../report/print";
import { ReportPreview } from "../report/ReportPreview";
import { routeHash } from "../router";
import { useShell } from "../shell";
import { buildFitReport, fitReportName } from "./fitReport";
import {
  featureInputs,
  predictionSummary,
  readEncoding,
  readModelDataset,
  readOutcome,
  readPrediction,
  typedFeatureValue,
  type FeatureInput,
  type InstanceDetail,
  type ModelItem,
} from "./models";
import { includeParam, moreOutputs, readOutputs, shownOutputs, type ModelOutput } from "./outputs";
import { PosteriorInstance } from "./PosteriorInstance";

export type ReportTab = "report" | "posterior" | "try";
type Tab = ReportTab;

export function FitReportModal({
  model,
  instanceId,
  providerDescription,
  datasetName,
  plots,
  onPlots,
  onChanged,
  onClose,
  initialTab = "report",
}: {
  model: ModelItem;
  /** The fit to report on. */
  instanceId: string;
  providerDescription?: string | null;
  datasetName?: string | null;
  /** The optional plots chosen, by name (the model's view state). */
  plots: string[];
  onPlots: (plots: string[]) => void;
  /** The fit changed here — written back, its report saved: the list again. */
  onChanged: () => void;
  onClose: () => void;
  /** The tab it opens on: a card's "Try a row" or "Open posterior". */
  initialTab?: ReportTab;
}) {
  const { t } = useT();
  const { application } = useShell();
  const mayCreate = !application || application.createWorkspaces;
  const [instance, setInstance] = useState<InstanceDetail | null>(null);
  const [outputs, setOutputs] = useState<ModelOutput[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>(initialTab);
  const [name, setName] = useState("");
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [printing, setPrinting] = useState(false);
  const page = useRef<HTMLDivElement>(null);
  const include = includeParam(plots);

  const load = useCallback(async () => {
    try {
      const detail = await api.getModelInstance(instanceId);
      setInstance(detail);
      setName((n) => n || fitReportName(model.name, detail, t));
    } catch (err) {
      setError(errorMessage(err, t("Could not load this fit.")));
    }
  }, [instanceId, model.name, t]);

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

  const report = useMemo(
    () =>
      instance && outputs
        ? buildFitReport({
            model,
            providerDescription,
            datasetName,
            instance,
            outputs: shownOutputs(outputs, plots),
            t,
          })
        : null,
    [instance, outputs, plots, model, providerDescription, datasetName, t],
  );

  const outcome = readOutcome(instance?.outcome);
  const posterior = outcome?.outcome === "posterior";
  const tryable = instance?.status === "fitted" && outcome !== null && outcome.outcome !== "test" && !posterior;
  const more = outputs ? moreOutputs(outputs, plots) : [];
  const chosen = outputs ? shownOutputs(outputs, plots).filter((o) => o.optional) : [];
  const saved = instance?.report ?? null;

  const save = async () => {
    if (!report || !instance) return;
    setSaving(true);
    setSaveError(null);
    try {
      await api.createWorkspace({ name: name.trim(), kind: "report", state: report, fit: instance.id });
      await load();
      onChanged();
    } catch (err) {
      setSaveError(errorMessage(err, t("Could not save the report.")));
    } finally {
      setSaving(false);
    }
  };

  const print = async () => {
    if (!page.current || !report) return;
    setPrinting(true);
    try {
      await printReport(page.current, report.page, name, t("Report"));
    } finally {
      setPrinting(false);
    }
  };

  return (
    <Modal show onHide={onClose} size="xl" fullscreen="lg-down" scrollable aria-labelledby="fit-report-title">
      <Modal.Header closeButton>
        <Modal.Title id="fit-report-title">
          {instance ? fitReportName(model.name, instance, t) : t("Report")}
        </Modal.Title>
      </Modal.Header>
      <Modal.Body className="an-fit-report">
        {error && <Alert variant="danger">{error}</Alert>}
        {(posterior || tryable) && (
          <Nav variant="tabs" className="mb-3" activeKey={tab} onSelect={(k) => setTab((k as Tab) ?? "report")}>
            <Nav.Item>
              <Nav.Link eventKey="report">
                <T text="Report" />
              </Nav.Link>
            </Nav.Item>
            {posterior && (
              <Nav.Item>
                <Nav.Link eventKey="posterior">
                  <T text="Posterior" />
                </Nav.Link>
              </Nav.Item>
            )}
            {tryable && (
              <Nav.Item>
                <Nav.Link eventKey="try">
                  <T text="Try a row" />
                </Nav.Link>
              </Nav.Item>
            )}
          </Nav>
        )}

        {tab === "report" && (
          <>
            {(more.length > 0 || chosen.length > 0) && (
              <div className="d-flex align-items-center gap-2 flex-wrap mb-3">
                {chosen.map((o) => (
                  <Button
                    key={o.name}
                    size="sm"
                    variant="outline-secondary"
                    aria-label={t("Take {plot} out of the report", { plot: o.label })}
                    onClick={() => onPlots(plots.filter((p) => p !== o.name))}
                  >
                    {o.label} ×
                  </Button>
                ))}
                {more.length > 0 && (
                  <Dropdown>
                    <Dropdown.Toggle variant="outline-secondary" size="sm" id="fit-report-more-plots">
                      <T text="More plots" />
                    </Dropdown.Toggle>
                    <Dropdown.Menu>
                      {more.map((o) => (
                        <Dropdown.Item key={o.name} onClick={() => onPlots([...plots, o.name])}>
                          {o.label}
                        </Dropdown.Item>
                      ))}
                    </Dropdown.Menu>
                  </Dropdown>
                )}
              </div>
            )}
            {report ? (
              <ReportPreview report={report} pageRef={page} />
            ) : (
              !error && (
                <div className="py-4 text-center">
                  <Spinner animation="border" size="sm" />
                </div>
              )
            )}
          </>
        )}

        {tab === "posterior" && instance && (
          <PosteriorInstance
            instance={instance}
            model={model}
            onChanged={() => {
              void load();
              onChanged();
            }}
          />
        )}

        {tab === "try" && instance && outcome && (
          <TryARow
            key={instance.id}
            instanceId={instance.id}
            features={featureInputs(readEncoding(instance.encoding), readModelDataset(model.dataset))}
            label={outcome.outcome === "regression" || outcome.outcome === "classification" ? outcome.label : null}
          />
        )}
      </Modal.Body>
      <Modal.Footer className="justify-content-start">
        {saved && (
          <span className="me-2">
            <T text="Saved as" />{" "}
            <a href={routeHash({ name: "workspace", id: saved.id })} onClick={onClose}>
              {saved.name}
            </a>
          </span>
        )}
        {mayCreate && report && (
          <Form
            className="d-flex align-items-center gap-2 flex-grow-1"
            onSubmit={(e) => {
              e.preventDefault();
              void save();
            }}
          >
            <Form.Control
              size="sm"
              className="an-fit-report-name"
              value={name}
              aria-label={t("The report workspace's name")}
              onChange={(e) => setName(e.target.value)}
            />
            <Button size="sm" type="submit" disabled={saving || name.trim() === ""}>
              {saving ? (
                <T text="Saving…" />
              ) : saved ? (
                <T text="Save as another report workspace" />
              ) : (
                <T text="Save as report workspace" />
              )}
            </Button>
          </Form>
        )}
        <div className="ms-auto d-flex gap-2">
          {tab === "report" && (
            <Button size="sm" variant="outline-secondary" disabled={!report || printing} onClick={() => void print()}>
              {printing ? <T text="Preparing…" /> : <T text="Print" />}
            </Button>
          )}
          <Button size="sm" variant="secondary" onClick={onClose}>
            <T text="Close" />
          </Button>
        </div>
        {saveError && (
          <Alert variant="danger" className="w-100 mb-0 mt-2">
            {saveError}
          </Alert>
        )}
      </Modal.Footer>
    </Modal>
  );
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
                    onChange={(e) => setValues((v) => ({ ...v, [feature.name]: e.target.value }))}
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
                    onChange={(e) => setValues((v) => ({ ...v, [feature.name]: e.target.value }))}
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
