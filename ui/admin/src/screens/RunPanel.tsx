// A coding run's own record, beside its transcript (TODO 10.6): what it cost,
// the plan a planner run holds, and the diff of what it and its sessions
// changed.
//
// The numbers and the lines are `runInfo.ts`'s; this is the drawing, shared by
// the chat (a one-line bar that opens) and a run's own page (`#/runs/{id}`),
// which is where each feature of a plan links its sessions to.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { conclusionNotice, transcriptFromRun, type Conclusion } from "../agentChat";
import { prettyContext } from "../agentRuns";
import type { GetRunDiffResponse, GetRunResponse } from "../client";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import {
  diffLines,
  featureMark,
  planProgress,
  runTotals,
  scopeLabel,
  totalsLabel,
} from "../runInfo";
import { Transcript, TranscriptActions } from "./AgentChat";
import { runTone } from "./WorkflowRuns";
import { T, useT } from "../i18n";

type Plan = NonNullable<GetRunResponse["plan"]>;

/** The link to one run's own page. */
export function runHref(id: string): string {
  return `#/runs/${encodeURIComponent(id)}`;
}

/** The plan as a checklist: each feature, its status, and its sessions. */
export function PlanChecklist({ plan }: { plan: Plan }) {
  const { t } = useT();
  return (
    <div className="run-plan">
      <div className="text-secondary small mb-1">
        {t("Plan · {progress}", { progress: planProgress(plan) })}
      </div>
      <ul className="list-unstyled mb-0">
        {plan.features.map((feature) => {
          const mark = featureMark(feature.status);
          const entries = plan.progress.filter((p) => p.feature === feature.id);
          const last = entries[entries.length - 1];
          return (
            <li key={feature.id} className="run-plan-feature">
              <span
                className={`run-plan-mark run-plan-${feature.status}`}
                title={mark.label}
                aria-label={mark.label}
              >
                {mark.mark}
              </span>
              <div className="flex-fill overflow-hidden">
                <div className={mark.done ? "text-secondary" : undefined}>
                  <code className="me-1">{feature.id}</code>
                  {feature.title}
                  {feature.kind === "bug" && (
                    <StatusBadge tone="red" title={t("Reproduced before it is fixed")}>
                      <T text="bug" />
                    </StatusBadge>
                  )}
                </div>
                {last?.check && <div className="text-secondary small text-truncate">{last.check}</div>}
                {feature.runs.length > 0 && (
                  <div className="small">
                    {feature.runs.map((run, i) => (
                      <a key={run} href={runHref(run)} className="me-2">
                        {t("session {n}", { n: i + 1 })}
                      </a>
                    ))}
                  </div>
                )}
              </div>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

/** The run's diff, its sessions' included: the diffstat, then each file's lines. */
export function RunDiffView({ runId }: { runId: string }) {
  const { t } = useT();
  const [diff, setDiff] = useState<GetRunDiffResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setDiff(null);
    setError(null);
    api
      .getRunDiff(runId)
      .then((found) => {
        if (!cancelled) setDiff(found);
      })
      .catch((err: unknown) => {
        if (!cancelled) setError(errorMessage(err, "Could not read this run's changes."));
      });
    return () => {
      cancelled = true;
    };
  }, [runId]);

  if (error) return <Alert variant="danger">{error}</Alert>;
  if (!diff) return <Spinner animation="border" size="sm" role="status" />;
  if (diff.scopes.length === 0) {
    return <p className="text-secondary mb-0"><T text="This run changed no files." /></p>;
  }
  return (
    <>
      {diff.runs.length > 1 && (
        <p className="text-secondary small">
          {t(
            "Changes from this run and {count} sessions it started, against what the files hold now.",
            { count: diff.runs.length - 1 },
          )}
        </p>
      )}
      {diff.scopes.map((scope) => (
        <div key={scopeLabel(scope)} className="mb-3">
          <div className="fw-medium mb-1">
            <code>{scopeLabel(scope)}</code>
          </div>
          <pre className="run-diff-stat">{scope.stat}</pre>
          <pre className="run-diff">
            {diffLines(scope.unified).map((line, i) => (
              <div key={i} className={`run-diff-${line.kind}`}>
                {line.text || " "}
              </div>
            ))}
          </pre>
        </div>
      ))}
    </>
  );
}

/** One line under the chat's top bar: the current run's cost, and the plan and
 * changes it opens onto. Absent until the run has spent anything. */
export function RunBar({ run }: { run: GetRunResponse }) {
  const totals = runTotals(run.context);
  if (totals.steps === 0 && !run.plan) return null;
  return (
    <details className="chat-runbar">
      <summary>
        <span className="text-truncate">
          {totalsLabel(totals)}
          {run.plan && ` · plan ${planProgress(run.plan)}`}
        </span>
      </summary>
      <div className="chat-runbar-body">
        {run.plan && <PlanChecklist plan={run.plan} />}
        <a href={runHref(run.id)} className="small">
          <T text="Open this run: its changes and its sessions" />
        </a>
      </div>
    </details>
  );
}

/** An agent run's own page: its record, its changes and its transcript. */
export function AgentRunDetail({ run, onRefresh }: { run: GetRunResponse; onRefresh: () => void }) {
  const { t } = useT();
  const totals = runTotals(run.context);
  const entries = transcriptFromRun(run.context);
  const notice = conclusionNotice(run.conclusion as Conclusion | null);
  return (
    <>
      <PageHeader
        pretitle={`Run of ${run.subject}`}
        title={
          <>
            {run.description || "Run"}{" "}
            <StatusBadge tone={runTone(run.state)}>{run.state}</StatusBadge>
          </>
        }
        actions={
          <>
            <Button
              variant="outline-secondary"
              onClick={() => navigate(`/agents/${encodeURIComponent(run.subject)}/chat`)}
            >
              <IconArrowLeft className="icon-2" />
              {t("Chat with {agent}", { agent: run.subject })}
            </Button>
            <Button
              variant="outline-secondary"
              href={`#/agents/${encodeURIComponent(run.subject)}/runs`}
            >
              <T text="All runs" />
            </Button>
            <Button variant="outline-secondary" onClick={onRefresh}>
              <T text="Refresh" />
            </Button>
          </>
        }
      />
      <PageBody>
        {run.error && <Alert variant="danger">{run.error}</Alert>}
        {notice && <Alert variant="warning">{notice}</Alert>}
        <p className="text-secondary">{totalsLabel(totals)}</p>
        {run.plan && (
          <Card className="mb-3">
            <Card.Body>
              <PlanChecklist plan={run.plan} />
            </Card.Body>
          </Card>
        )}
        <Card className="mb-3">
          <Card.Header><T text="Changes" /></Card.Header>
          <Card.Body>
            <RunDiffView key={run.updated_at} runId={run.id} />
          </Card.Body>
        </Card>
        <Card>
          <Card.Header><T text="Transcript" /></Card.Header>
          <Card.Body>
            {entries.length === 0 && <p className="text-secondary mb-0"><T text="Nothing was said." /></p>}
            {/* Read-only: a run's page shows what was asked, and answers in the chat. */}
            <TranscriptActions.Provider value={{ runId: run.id, awaitingId: null }}>
              <Transcript entries={entries} />
            </TranscriptActions.Provider>
          </Card.Body>
        </Card>
        {/* The loop state as stored — what the transcript above is read out
            of, for when the question is about something it does not draw. */}
        <Card className="mt-3">
          <Card.Header><T text="Context" /></Card.Header>
          <Card.Body>
            <pre className="mb-0 small" style={{ maxHeight: "40rem", overflow: "auto" }}>
              {prettyContext(run.context)}
            </pre>
          </Card.Body>
        </Card>
      </PageBody>
    </>
  );
}
