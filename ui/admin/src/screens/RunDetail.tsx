// One run of one workflow (§10.3, phases 6.6 and 6.7): where it got to, how it
// got there, and the three things an admin looking at a stuck one can do.
//
// Three views of the same run, and they are three because each answers a
// different question:
//
//   - **the canvas**, read-only, with the path taken drawn on it and the current
//     step marked — "where is this?", answered on the picture the admin drew.
//     It is drawn on the version the run is *pinned* to, not on today's steps,
//     because a run that started before two edits did not run this program.
//   - **the timeline**, one row per step with its attempt, its duration and the
//     context after it, with what *changed* highlighted — "what happened?".
//   - **the form**, when the run is suspended waiting for a person, rendered by
//     `SettingsFields` from the step's own declaration — which is the whole of
//     phase 6.7 and needs no code that knows what a workflow is.

import { useCallback, useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import type { GetRunResponse } from "../client";
import { IconArrowLeft } from "../icons";
import { triggersCrumbs, workflowRunsCrumbs } from "../breadcrumbs";
import { AlertBody, PageBody, PageHeader, StatusBadge } from "../layout";
import { SettingsFields, buildConfig, type FieldSpec } from "../settings";
import {
  layout,
  runPath,
  stepsToGraph,
  type Graph,
  type RunPath,
  type Workflow,
} from "../workflowGraph";
import { AgentRunDetail } from "./RunPanel";
import { WorkflowCanvas, type Positions } from "./WorkflowCanvas";
import { runTone, when } from "./WorkflowRuns";
import { T, useT } from "../i18n";

type TraceRow = GetRunResponse["trace"][number];

export function RunDetail({ runId }: { runId: string }) {
  const { t } = useT();
  const [run, setRun] = useState<GetRunResponse | null>(null);
  const [graph, setGraph] = useState<Graph | null>(null);
  const [positions, setPositions] = useState<Positions>({});
  const [triggerId, setTriggerId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [answers, setAnswers] = useState<Record<string, string>>({});

  /** Read the run, and — for a workflow run — the **pinned** version's graph. */
  const load = useCallback(async () => {
    const found = await api.getRun(runId);
    setRun(found);
    if (found.kind !== "workflow") return;
    // `subject` is the trigger's name; the workflow endpoints are addressed by
    // its id, so the list is what turns one into the other. A run whose trigger
    // has since been deleted still shows its trace — only the canvas is missing.
    try {
      const triggers = await api.listTriggers();
      const trigger = triggers.find((t) => t.name === found.subject);
      if (!trigger) return;
      setTriggerId(trigger.id);
      const version = await api.getWorkflow(
        trigger.id,
        found.subject_version == null ? undefined : { version: found.subject_version },
      );
      const drawn = layout(stepsToGraph(version.workflow as Workflow));
      setGraph(drawn);
      const at: Positions = {};
      for (const node of drawn.nodes) at[node.id] = node.position;
      setPositions(at);
    } catch {
      // The canvas is the one part of this screen that can be absent without the
      // rest being useless, so a workflow that cannot be read is left out rather
      // than turned into a failure of the whole page.
      setGraph(null);
    }
  }, [runId]);

  useEffect(() => {
    void (async () => {
      try {
        await load();
      } catch (err) {
        setLoadError(errorMessage(err, "Could not load this run."));
      }
    })();
  }, [load]);

  const path: RunPath | null = useMemo(() => {
    if (!graph || !run) return null;
    return runPath(graph, run.trace, run.current_step);
  }, [graph, run]);

  const act = async (what: () => Promise<GetRunResponse>) => {
    setBusy(true);
    setError(null);
    try {
      setRun(await what());
      // The state changed, so the path and the pending form did too.
      await load();
    } catch (err) {
      setError(errorMessage(err, "That did not work."));
    } finally {
      setBusy(false);
    }
  };

  if (loadError) {
    return (
      <PageBody>
        <Alert variant="danger">{loadError}</Alert>
      </PageBody>
    );
  }
  if (!run) {
    return (
      <PageBody>
        <div className="py-5 text-center">
          <Spinner animation="border" role="status" />
        </div>
      </PageBody>
    );
  }

  // An agent run has no canvas and no form: it has a transcript, and — for a
  // coding run — a plan and a diff.
  if (run.kind === "agent") {
    return <AgentRunDetail run={run} onRefresh={() => void load()} />;
  }

  const form = run.pending_form;
  const spec: FieldSpec[] = form?.fields ?? [];
  const live = run.state === "running" || run.state === "waiting";

  return (
    <>
      <PageHeader
        crumbs={
          triggerId ? workflowRunsCrumbs(t, triggerId, run.subject) : triggersCrumbs(t)
        }
        title={
          <>
            {run.description || "Run"}{" "}
            <StatusBadge tone={runTone(run.state)}>{run.state}</StatusBadge>
            {run.subject_version != null && (
              <>
                {" "}
                <StatusBadge tone="secondary">
                  {t("version {version}", { version: String(run.subject_version) })}
                </StatusBadge>
              </>
            )}
          </>
        }
        actions={
          <>
            <Button
              variant="outline-secondary"
              onClick={() =>
                triggerId
                  ? navigate(`/triggers/${encodeURIComponent(triggerId)}/runs`)
                  : navigate("/triggers")
              }
            >
              <IconArrowLeft className="icon-2" />
              <T text="Back" />
            </Button>
            <Button variant="outline-secondary" disabled={busy} onClick={() => void load()}>
              <T text="Refresh" />
            </Button>
            {/* Cancelling is for a run that is going, retrying for one that
                stopped: offering both at once would offer one that cannot do
                anything. */}
            {live && (
              <Button
                variant="outline-danger"
                disabled={busy}
                onClick={() => {
                  // Cancelled at the prompt is not cancelled the run: `null`
                  // means the admin changed their mind, and "" means they had no
                  // reason to give.
                  const reason = window.prompt("Why is this run being cancelled?");
                  if (reason === null) return;
                  void act(() => api.cancelRun(runId, { reason: reason || null }));
                }}
              >
                <T text="Cancel" />
              </Button>
            )}
            {run.state === "failed" && (
              <Button variant="outline-primary" disabled={busy} onClick={() => void act(() => api.retryRun(runId))}>
                {t("Retry from `{step}`", {
                  step: run.current_step ?? t("the failed step"),
                })}
              </Button>
            )}
          </>
        }
      />
      <PageBody>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        {run.error && (
          <Alert variant="danger">
            <AlertBody>
              <strong>{run.current_step ?? "This run"}</strong> failed: {run.error}
            </AlertBody>
          </Alert>
        )}

        {form && (
          <Card className="mb-3">
            <Card.Header><T text="Waiting for an answer" /></Card.Header>
            <Card.Body>
              {/* Rendered from the step's own declaration, by the same component
                  that renders a file store's settings — so an approval form is a
                  settings form and this screen knows nothing about what it asks. */}
              <SettingsFields
                spec={spec}
                values={answers}
                idPrefix="run-form"
                onChange={(name, value) => setAnswers((a) => ({ ...a, [name]: value }))}
              />
              <Button
                disabled={busy}
                onClick={() => void act(() => api.resumeRun(runId, buildConfig(spec, answers)))}
              >
                {busy ? "Sending…" : "Answer and carry on"}
              </Button>
              <div className="text-muted small mt-2">
                <T
                  text="The answers are merged into the run context under {key}."
                  values={{ key: <code>context.{form.assign_to}</code> }}
                />{" "}
                <T text="This is the form the person was shown — the workflow may have been edited since, and the run finishes on the version it started with." />
              </div>
            </Card.Body>
          </Card>
        )}

        <div className="row g-3">
          <div className="col-12 col-xl-7">
            <Card>
              <Card.Header>
                <T text="Where it got to" />
                {run.subject_version != null && (
                  <span className="text-muted small ms-2">
                    {t(
                      "drawn on version {version}, the one this run is pinned to",
                      { version: run.subject_version },
                    )}
                  </span>
                )}
              </Card.Header>
              <Card.Body className="p-0">
                {graph ? (
                  <WorkflowCanvas graph={graph} positions={positions} readOnly path={path} />
                ) : (
                  <div className="p-3 text-muted">
                    <T text="The version this run is pinned to could not be read, so there is no graph to draw it on." />
                  </div>
                )}
              </Card.Body>
            </Card>
          </div>
          <div className="col-12 col-xl-5">
            <Timeline run={run} />
          </div>
        </div>
      </PageBody>
    </>
  );
}

/** The trace as a timeline: one entry per completed step, newest last.
 *
 * The context is shown as **what changed**, because the whole context after
 * every step is the same document twenty times over and the thing an admin is
 * looking for is the one key the step wrote. The whole document is one click
 * away, for when the diff is not the question. */
function Timeline({ run }: { run: GetRunResponse }) {
  const { t } = useT();
  const rows = useMemo(() => [...run.trace].sort((a, b) => a.seq - b.seq), [run.trace]);
  if (rows.length === 0) {
    return (
      <Card>
        <Card.Header><T text="What happened" /></Card.Header>
        <Card.Body className="text-muted">
          <T text="No trace recorded. Turn on tracing in the workflow editor to see this timeline." />
        </Card.Body>
      </Card>
    );
  }
  return (
    <Card>
      <Card.Header><T text="What happened" /></Card.Header>
      <div className="list-group list-group-flush">
        {rows.map((row, index) => (
          <TraceEntry key={row.id} row={row} previous={rows[index - 1] ?? null} />
        ))}
      </div>
      <Card.Footer className="text-muted small">
        {t("Started {created} · last advanced {updated}", {
          created: when(run.created_at),
          updated: when(run.updated_at),
        })}
      </Card.Footer>
    </Card>
  );
}

/** One step of the trace. */
function TraceEntry({ row, previous }: { row: TraceRow; previous: TraceRow | null }) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const changed = changedKeys(previous?.context, row.context);
  const tone = row.outcome === "error" ? "red" : row.outcome === "suspended" ? "yellow" : "green";
  return (
    <div className="list-group-item">
      <div className="d-flex align-items-center justify-content-between">
        <div>
          <strong>{row.step}</strong>
          {row.attempt > 1 && (
            <span className="text-muted small ms-2">
              {t("attempt {n}", { n: row.attempt })}
            </span>
          )}
        </div>
        <div className="d-flex align-items-center gap-2">
          <span className="text-muted small">{duration(row)}</span>
          <StatusBadge tone={tone}>{row.outcome}</StatusBadge>
        </div>
      </div>
      {row.error && <div className="text-danger small text-break mt-1">{row.error}</div>}
      <div className="text-muted small mt-1">
        {changed.length === 0 ? (
          "The context did not change."
        ) : (
          <>
            <T text="Wrote" />{" "}
            {changed.map((key) => (
              <code key={key} className="me-1">
                {key}
              </code>
            ))}
          </>
        )}
        <button
          type="button"
          className="btn btn-link btn-sm p-0 ms-2 align-baseline"
          onClick={() => setOpen((o) => !o)}
        >
          {open ? "hide the context" : "show the context"}
        </button>
      </div>
      {open && (
        <pre className="small text-break text-pre-wrap mt-2 mb-0">
          {JSON.stringify(row.context, null, 2)}
        </pre>
      )}
    </div>
  );
}

/** The top-level context keys this step wrote or changed — the highlight that
 * makes a timeline of whole contexts readable. */
function changedKeys(before: unknown, after: unknown): string[] {
  const a = before && typeof before === "object" ? (before as Record<string, unknown>) : {};
  const b = after && typeof after === "object" ? (after as Record<string, unknown>) : {};
  return Object.keys(b).filter((key) => JSON.stringify(a[key]) !== JSON.stringify(b[key]));
}

/** How long a step took, in the largest unit that is still a small number. */
function duration(row: TraceRow): string {
  const ms = new Date(row.finished_at).getTime() - new Date(row.started_at).getTime();
  if (!Number.isFinite(ms) || ms < 0) return "";
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  return `${Math.round(ms / 60_000)} min`;
}
