// One checklist run (TODO-checklists C1.16, CHECKLISTS.md §5): what each step
// did, how long it took, and what it left behind — the values it captured, the
// browser console's errors and the requests that failed while it ran, and the
// screenshot of a failed browser step (C2.5) or of a `browser.screenshot`.
//
// A skipped step says why: `incomplete` means the host could not run some of
// the steps, and a run is never green by not running (§1).

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { checklistPath, checklistRunPath, outcomeTone, type ChecklistRun as Run } from "../checklists";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { RunStatus } from "./Checklists";
import { when } from "./WorkflowRuns";
import { T, useT } from "../i18n";

export function ChecklistRun({ runId, appId }: { runId: string; appId: string | null }) {
  const { t } = useT();
  const [run, setRun] = useState<Run | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setRun(null);
    api
      .getChecklistRun(runId)
      .then((found) => {
        setRun(found);
        setError(null);
      })
      .catch((err) => setError(errorMessage(err, t("Could not load the run."))));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [runId]);

  const again = async () => {
    if (!run) return;
    setBusy(true);
    setError(null);
    try {
      const next = await api.runChecklist(run.checklist);
      navigate(checklistRunPath(next.id, appId));
    } catch (err) {
      setError(errorMessage(err, t("Could not run the checklist.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <PageHeader
        pretitle={t("Checklist run")}
        title={run?.checklist_name ?? ""}
        actions={
          <>
            <Button
              variant="outline-secondary"
              disabled={!run}
              onClick={() => run && navigate(checklistPath(run.checklist, appId))}
            >
              <IconArrowLeft className="icon-2" />
              <T text="Checklist" />
            </Button>
            <Button disabled={busy || !run} onClick={() => void again()}>
              <T text="Run again" />
            </Button>
          </>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {run && (
          <>
            <Card className="mb-3">
              <Card.Body>
                <div className="datagrid">
                  <div className="datagrid-item">
                    <div className="datagrid-title"><T text="Result" /></div>
                    <div className="datagrid-content">
                      <RunStatus status={run.status} />
                    </div>
                  </div>
                  <div className="datagrid-item">
                    <div className="datagrid-title"><T text="Started" /></div>
                    <div className="datagrid-content">{when(run.started_at)}</div>
                  </div>
                  <div className="datagrid-item">
                    <div className="datagrid-title"><T text="Finished" /></div>
                    <div className="datagrid-content">{when(run.finished_at)}</div>
                  </div>
                  <div className="datagrid-item">
                    <div className="datagrid-title"><T text="Trigger" /></div>
                    <div className="datagrid-content">{run.trigger}</div>
                  </div>
                  <div className="datagrid-item">
                    <div className="datagrid-title"><T text="Target" /></div>
                    <div className="datagrid-content">{run.target}</div>
                  </div>
                </div>
                {run.message && (
                  <Alert
                    variant={run.status === "passed" ? "info" : "warning"}
                    className="mt-3 mb-0"
                  >
                    {run.message}
                  </Alert>
                )}
              </Card.Body>
            </Card>

            {run.cleanup_errors.length > 0 && (
              <Alert variant="danger">
                <div className="fw-bold">
                  <T text="Cleaning up after the run failed; some rows it created may still exist." />
                </div>
                <ul className="mb-0">
                  {run.cleanup_errors.map((e, i) => (
                    <li key={i}>{e}</li>
                  ))}
                </ul>
              </Alert>
            )}

            <Card>
              <Card.Header>
                <h3 className="card-title"><T text="Steps" /></h3>
              </Card.Header>
              <Table responsive className="card-table">
                <thead>
                  <tr>
                    <th className="w-1">#</th>
                    <th><T text="Step" /></th>
                    <th><T text="Outcome" /></th>
                    <th><T text="Message" /></th>
                    <th className="text-end"><T text="Time" /></th>
                  </tr>
                </thead>
                <tbody>
                  {run.steps.length === 0 && (
                    <tr>
                      <td colSpan={5} className="text-muted">
                        <T text="No steps ran." />
                      </td>
                    </tr>
                  )}
                  {run.steps.map((step, index) => {
                    const captured =
                      step.captured !== undefined && step.captured !== null;
                    return (
                      <tr key={`${step.step}-${index}`}>
                        <td className="text-secondary">{index + 1}</td>
                        <td>
                          <div>{step.label || "—"}</div>
                          <code className="small">{step.type}</code>
                        </td>
                        <td>
                          <StatusBadge tone={outcomeTone(step.outcome)} title={step.skip ?? undefined}>
                            {step.outcome}
                          </StatusBadge>
                          {step.skip && <div className="small text-secondary">{step.skip}</div>}
                        </td>
                        <td>
                          {step.message && (
                            <div className="text-break" style={{ whiteSpace: "pre-wrap" }}>
                              {step.message}
                            </div>
                          )}
                          {captured && (
                            <div className="small mt-1">
                              <T text="Captured:" />{" "}
                              <code>{JSON.stringify(step.captured)}</code>
                            </div>
                          )}
                          {step.console_errors.length > 0 && (
                            <div className="small mt-1">
                              <div className="text-danger"><T text="Console errors" /></div>
                              <pre className="mb-0">{step.console_errors.join("\n")}</pre>
                            </div>
                          )}
                          {step.failed_requests.length > 0 && (
                            <div className="small mt-1">
                              <div className="text-danger"><T text="Failed requests" /></div>
                              <pre className="mb-0">{step.failed_requests.join("\n")}</pre>
                            </div>
                          )}
                          {step.screenshot && <Screenshot path={step.screenshot} />}
                        </td>
                        <td className="text-end small text-nowrap">
                          {t("{ms} ms", { ms: step.duration_ms })}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </Table>
            </Card>
          </>
        )}
      </PageBody>
    </>
  );
}

// A screenshot the run kept, fetched when the step is shown; a click shows it
// full size. (Browsers will not open a `data:` URL in a tab of its own.)
function Screenshot({ path }: { path: string }) {
  const { t } = useT();
  const [src, setSrc] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [large, setLarge] = useState(false);

  useEffect(() => {
    setSrc(null);
    setError(null);
    api
      .getChecklistScreenshot({ path })
      .then((shot) => setSrc(`data:${shot.content_type};base64,${shot.data}`))
      .catch((err) => setError(errorMessage(err, t("Could not load the screenshot."))));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path]);

  if (error) return <div className="small text-danger mt-1">{error}</div>;
  if (!src) return <div className="small text-secondary mt-1"><T text="Loading screenshot…" /></div>;
  return (
    <img
      src={src}
      alt={t("Screenshot of the page")}
      title={t("Click to enlarge")}
      className="img-thumbnail d-block mt-2"
      role="button"
      onClick={() => setLarge((v) => !v)}
      style={large ? { maxWidth: "100%" } : { maxWidth: 480, maxHeight: 300, objectFit: "contain" }}
    />
  );
}
