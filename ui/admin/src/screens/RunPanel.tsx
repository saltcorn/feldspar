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
import {
  conclusionNotice,
  transcriptFromRun,
  type Conclusion,
} from "../agentChat";
import { prettyContext } from "../agentRuns";
import type { GetRunDiffResponse, GetRunResponse } from "../client";
import { IconArrowLeft, IconChevronDown, IconRefresh } from "../icons";
import { ClampedText } from "../ClampedText";
import { InlineCode } from "../inlineCode";
import {
  runStory,
  type StepTool,
  type StoryStep,
  type ToolCallEntry,
} from "../runStory";
import {
  argumentRows,
  itemSummary,
  preview,
  resultText,
  type ArgValue,
  type ArgumentRow,
} from "../toolCallView";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import {
  compactCount,
  diffLines,
  elapsedLabel,
  featureMark,
  planProgress,
  runTotals,
  scopeLabel,
  totalsLabel,
} from "../runInfo";
import { Transcript, TranscriptActions } from "./AgentChat";
import { runTone, when } from "./WorkflowRuns";
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
                    <StatusBadge
                      tone="red"
                      title={t("Reproduced before it is fixed")}
                    >
                      <T text="bug" />
                    </StatusBadge>
                  )}
                </div>
                {last?.check && (
                  <div className="text-secondary small text-truncate">
                    {last.check}
                  </div>
                )}
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
        if (!cancelled)
          setError(errorMessage(err, "Could not read this run's changes."));
      });
    return () => {
      cancelled = true;
    };
  }, [runId]);

  if (error) return <Alert variant="danger">{error}</Alert>;
  if (!diff) return <Spinner animation="border" size="sm" role="status" />;
  if (diff.scopes.length === 0) {
    return (
      <p className="run-empty">
        <T text="This run changed no files." />
      </p>
    );
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

/** One figure in the strip at the top of a run's page. */
function Stat({
  label,
  value,
  note,
}: {
  label: string;
  value: string;
  note?: string;
}) {
  return (
    <div className="run-stat">
      <div className="run-stat-label">{label}</div>
      <div className="run-stat-value">{value}</div>
      {note && <div className="run-stat-note">{note}</div>}
    </div>
  );
}

/** One step of what the agent did: its remark and the tools it then called,
 * opening onto the whole of what it said. */
function StoryStepRow({ step, n }: { step: StoryStep; n: number }) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  // The tool chip whose calls are shown, by its index in the step.
  const [shown, setShown] = useState<number | null>(null);
  if (step.kind === "problem") {
    return (
      <li className="story-step story-problem">
        <span className="story-n">!</span>
        <div className="story-body">
          <div className="story-line">{step.line}</div>
        </div>
      </li>
    );
  }
  if (step.kind === "person") {
    return (
      <li className="story-step story-person">
        <span className="story-n">{n}</span>
        <div className="story-body">
          <div className="story-who">
            <T text="Follow-up" />
          </div>
          <div className="story-line">
            <InlineCode text={step.line} />
          </div>
        </div>
      </li>
    );
  }
  const more = step.text.trim().length > (step.line?.length ?? 0) + 1;
  return (
    <li className="story-step">
      <span className="story-n">{n}</span>
      <div className="story-body">
        <div className="story-line">
          {step.line ? (
            <InlineCode text={step.line} />
          ) : (
            <span className="text-secondary">
              <T text="Called tools without a remark" />
            </span>
          )}
          {more && (
            <button
              type="button"
              className="build-more ms-2"
              onClick={() => setOpen(!open)}
            >
              {open ? t("Show less") : t("Show all it said")}
            </button>
          )}
        </div>
        {open && (
          <div className="story-text">
            <InlineCode text={step.text.trim()} />
          </div>
        )}
        {step.tools.length > 0 && (
          <div className="story-tools">
            {step.tools.map((tool, i) => (
              <button
                type="button"
                key={i}
                className={`story-tool${tool.lastFailed ? " story-tool-error" : ""}${
                  shown === i ? " story-tool-open" : ""
                }`}
                aria-expanded={shown === i}
                onClick={() => setShown(shown === i ? null : i)}
                title={
                  tool.errors > 0
                    ? t("{n} of these calls failed", { n: tool.errors })
                    : undefined
                }
              >
                {tool.name}
                {tool.count > 1 && (
                  <span className="story-tool-count">×{tool.count}</span>
                )}
                {tool.errors > 0 && tool.count > 1 && (
                  <span className="story-tool-count">
                    {" · "}
                    <span className="text-danger">
                      {t("{n} failed", { n: tool.errors })}
                    </span>
                  </span>
                )}
              </button>
            ))}
          </div>
        )}
        {shown !== null && step.tools[shown] && (
          <ToolCallsDetail tool={step.tools[shown]} />
        )}
        {step.tools.some((tool) => tool.failures.length > 0) && (
          <div className="story-failures-box">
            <div className="story-failures-head">
              <T text="Calls with errors" />
            </div>
            <ul className="story-failures">
              {step.tools.flatMap((tool) =>
                tool.failures.map((why, i) => {
                  // Only the last failure of a group can be the one a retry
                  // fixed; any before it were followed by another failure.
                  const retried =
                    !tool.lastFailed || i < tool.failures.length - 1;
                  return (
                    <li
                      key={`${tool.name}-${i}`}
                      className={retried ? "story-failure-retried" : undefined}
                    >
                      <span className="story-failure-status">
                        {retried ? t("retried") : t("failed")}
                      </span>
                      <code>{tool.name}</code>
                      <span className="story-failure-why" title={why}>
                        {why}
                      </span>
                    </li>
                  );
                }),
              )}
            </ul>
          </div>
        )}
      </div>
    </li>
  );
}

/** The calls behind one chip of a step, laid out to read: a header with the
 * call's number and outcome, its arguments as named values, its result as
 * wrapping text (previewed when long), and what it saw. */
export function ToolCallsDetail({ tool }: { tool: StepTool }) {
  const { t } = useT();
  const [current, setCurrent] = useState(0);
  const calls = tool.calls;
  const call = calls[Math.min(current, calls.length - 1)];
  return (
    <div className="story-calls">
      {calls.length > 1 && (
        <div className="story-call-tabs" role="tablist">
          {calls.map((c, i) => (
            <button
              key={c.id}
              type="button"
              role="tab"
              aria-selected={c === call}
              className={`story-call-tab${c === call ? " is-current" : ""}`}
              onClick={() => setCurrent(i)}
            >
              <span
                className={`story-call-dot${c.isError ? " is-failed" : ""}`}
                aria-hidden="true"
              />
              {t("Call {n}", { n: i + 1 })}
              {itemSummary(argumentRows(c.args)).key && (
                <code className="story-call-tab-key">
                  {itemSummary(argumentRows(c.args)).key}
                </code>
              )}
              <span className="story-call-tab-outcome">
                {c.result === null
                  ? t("running")
                  : c.isError
                    ? t("failed")
                    : t("ok")}
              </span>
            </button>
          ))}
        </div>
      )}
      {call && <CallDetail key={call.id} call={call} name={tool.name} />}
    </div>
  );
}

/** Named values as rows, nested values laid out the same way inside. */
function ArgRows({ rows }: { rows: ArgumentRow[] }) {
  return (
    <dl className="story-call-args">
      {rows.map((row) => (
        <div key={row.name} className="story-call-arg">
          <dt>{row.name}</dt>
          <dd>
            <ArgValueView value={row.value} />
          </dd>
        </div>
      ))}
    </dl>
  );
}

/** How many items of a list show before "+N more". */
const LIST_PREVIEW = 4;

function ArgValueView({ value }: { value: ArgValue }) {
  switch (value.kind) {
    case "text":
      return (
        <ClampedText text={value.text} lines={3} className="story-call-text" />
      );
    case "tags":
      return (
        <>
          {value.tags.map((tag, k) => (
            <code key={k} className="story-call-tag">
              {tag}
            </code>
          ))}
        </>
      );
    case "list":
      return <FoldedList items={value.items} />;
    case "object":
      return (
        <div className="story-call-nested">
          <ArgRows rows={value.rows} />
        </div>
      );
    case "objects":
      return (
        <ol className="story-call-objects">
          {value.items.map((rows, k) => (
            <ObjectItem
              key={k}
              n={k + 1}
              rows={rows}
              open={value.items.length === 1}
            />
          ))}
        </ol>
      );
    case "json":
      return <pre className="story-call-json">{value.json}</pre>;
  }
}

/** A list of sentences: the first few, then the rest on request. */
function FoldedList({ items }: { items: string[] }) {
  const { t } = useT();
  const [all, setAll] = useState(false);
  const shown = all ? items : items.slice(0, LIST_PREVIEW);
  const rest = items.length - shown.length;
  return (
    <>
      <ul className="story-call-list">
        {shown.map((item, k) => (
          <li key={k}>
            <InlineCode text={item} />
          </li>
        ))}
      </ul>
      {rest > 0 && (
        <button
          type="button"
          className="build-more"
          onClick={() => setAll(true)}
        >
          {t("+{n} more", { n: rest })}
        </button>
      )}
    </>
  );
}

/** One object of a list, folded to a line naming it until opened. */
function ObjectItem({
  n,
  rows,
  open: initial,
}: {
  n: number;
  rows: ArgumentRow[];
  open: boolean;
}) {
  const [open, setOpen] = useState(initial);
  const { key, label } = itemSummary(rows);
  return (
    <li className={`story-call-object${open ? " is-open" : ""}`}>
      <button
        type="button"
        className="story-call-object-head"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <span className="story-call-n">{n}</span>
        {key && <code>{key}</code>}
        {label && <span className="story-call-object-label">{label}</span>}
        <IconChevronDown className="icon-2 story-call-caret flex-shrink-0" />
      </button>
      {open && (
        <div className="story-call-nested">
          <ArgRows rows={rows} />
        </div>
      )}
    </li>
  );
}

function CallDetail({ call, name }: { call: ToolCallEntry; name: string }) {
  const { t } = useT();
  const [all, setAll] = useState(false);
  const args = argumentRows(call.args);
  const text = resultText(call.result, call.isError);
  const { shown, more } = all ? { shown: text, more: 0 } : preview(text);
  return (
    <div className={`story-call${call.isError ? " story-call-failed" : ""}`}>
      <div className="story-call-head">
        <code>{name}</code>
        <span
          className={`story-call-outcome${call.isError ? " is-failed" : ""}`}
        >
          {call.result === null
            ? t("running")
            : call.isError
              ? t("failed")
              : t("ok")}
        </span>
      </div>
      {args.length > 0 && <ArgRows rows={args} />}
      {call.images?.map((src, k) => (
        <img
          key={k}
          src={src}
          alt={`what ${name} saw`}
          className="story-call-image"
        />
      ))}
      {call.result !== null && (
        <div className="story-call-result">
          <div className="story-call-label">
            {call.isError ? t("Error") : t("Result")}
          </div>
          {text.trim() === "" ? (
            <span className="text-secondary">
              <T text="(empty)" />
            </span>
          ) : (
            <pre>{shown}</pre>
          )}
          {more > 0 && (
            <button
              type="button"
              className="build-more"
              onClick={() => setAll(true)}
            >
              {t("Show all ({n} more lines)", { n: more })}
            </button>
          )}
        </div>
      )}
    </div>
  );
}

/** An agent run's own page: its record, its changes and its transcript. */
export function AgentRunDetail({
  run,
  onRefresh,
}: {
  run: GetRunResponse;
  onRefresh: () => void;
}) {
  const { t } = useT();
  const totals = runTotals(run.context);
  const entries = transcriptFromRun(run.context);
  const story = runStory(entries);
  const notice = conclusionNotice(run.conclusion as Conclusion | null);
  const live = run.state === "running" || run.state === "waiting";
  const cost =
    totals.cost === null
      ? "—"
      : `${totals.cost.toFixed(totals.cost < 1 ? 4 : 2)}`;
  const cachedShare =
    totals.inputTokens > 0 && totals.cachedTokens > 0
      ? t("{n}% cached", {
          n: Math.round((100 * totals.cachedTokens) / totals.inputTokens),
        })
      : undefined;
  const took = elapsedLabel(run.created_at, run.updated_at);
  const started = new Date(run.created_at);
  const startedAt = Number.isNaN(started.getTime())
    ? { time: when(run.created_at), date: undefined }
    : {
        time: started.toLocaleTimeString(undefined, {
          hour: "2-digit",
          minute: "2-digit",
        }),
        date: started.toLocaleDateString(),
      };
  return (
    <>
      <PageHeader
        pretitle={t("Run of {subject}", { subject: run.subject })}
        title={
          <span className="run-title">
            <InlineCode text={run.description || "Run"} />{" "}
            <StatusBadge tone={runTone(run.state)}>{run.state}</StatusBadge>
          </span>
        }
        actions={
          <>
            {/* Back to wherever the run was opened from — a task, a chat, a
                list of runs. Opened on its own, the agent's runs. */}
            <Button
              variant="outline-secondary"
              onClick={() =>
                window.history.length > 1
                  ? window.history.back()
                  : navigate(`/agents/${encodeURIComponent(run.subject)}/runs`)
              }
            >
              <IconArrowLeft className="icon-2" />
              <T text="Back" />
            </Button>
            <Button variant="outline-secondary" onClick={onRefresh}>
              <IconRefresh className="icon-2" />
              <T text="Refresh" />
            </Button>
          </>
        }
      />
      <PageBody>
        {run.error && <Alert variant="danger">{run.error}</Alert>}
        {notice && <Alert variant="warning">{notice}</Alert>}
        <div className="run-stats">
          <Stat
            label={t("Cost")}
            value={cost}
            note={totals.cost === null ? t("A model has no price") : undefined}
          />
          <Stat label={t("Model calls")} value={String(totals.steps)} />
          <Stat
            label={t("Input tokens")}
            value={compactCount(totals.inputTokens)}
            note={cachedShare}
          />
          <Stat
            label={t("Output tokens")}
            value={compactCount(totals.outputTokens)}
          />
          <Stat
            label={t("Tool calls")}
            value={String(story.toolCalls)}
            note={
              story.toolErrors > 0
                ? t("{n} failed", { n: story.toolErrors })
                : undefined
            }
          />
          {!live && took && <Stat label={t("Duration")} value={took} />}
          <Stat
            label={t("Started")}
            value={startedAt.time}
            note={startedAt.date}
          />
        </div>
        {run.plan && (
          <Card className="mb-3">
            <Card.Body>
              <PlanChecklist plan={run.plan} />
            </Card.Body>
          </Card>
        )}
        {story.task && (
          <Card className="mb-3">
            <Card.Header>
              <Card.Title className="mb-0">
                <T text="What it was asked" />
              </Card.Title>
            </Card.Header>
            <Card.Body>
              <ClampedText
                text={story.task.trim()}
                lines={3}
                className="story-task"
              />
            </Card.Body>
          </Card>
        )}
        {story.steps.length > 0 && (
          <Card className="mb-3">
            <Card.Header>
              <Card.Title className="mb-0">
                <T text="What it did" />
              </Card.Title>
              <div className="card-actions text-secondary small">
                {t("{n} steps · {calls} tool calls", {
                  n: story.steps.length,
                  calls: story.toolCalls,
                })}
              </div>
            </Card.Header>
            <Card.Body>
              <ol className="story-steps">
                {story.steps.map((step, i) => (
                  <StoryStepRow key={i} step={step} n={i + 1} />
                ))}
              </ol>
            </Card.Body>
          </Card>
        )}
        {story.outcome && (
          <Card
            className={`mb-3 story-outcome story-outcome-${runTone(run.state)}`}
          >
            <Card.Header>
              <Card.Title className="mb-0">
                <T text="What it answered" />
              </Card.Title>
            </Card.Header>
            <Card.Body>
              <ClampedText
                text={story.outcome.trim()}
                lines={6}
                className="story-answer"
              />
            </Card.Body>
          </Card>
        )}
        <Card className="mb-3 run-section">
          <Card.Header>
            <Card.Title className="mb-0">
              <T text="Changes" />
            </Card.Title>
          </Card.Header>
          <Card.Body>
            <RunDiffView key={run.updated_at} runId={run.id} />
          </Card.Body>
        </Card>
        {/* Every message and call, for when the outline above is not enough. */}
        <details className="card run-context mb-3">
          <summary className="card-header">
            <span className="card-title mb-0">
              <T text="Full transcript" />
            </span>
            <span className="text-secondary small ms-2">
              <T text="every message and tool call" />
            </span>
          </summary>
          <div className="card-body">
            {entries.length === 0 && (
              <p className="text-secondary mb-0">
                <T text="Nothing was said." />
              </p>
            )}
            {/* Read-only: a run's page shows what was asked, and answers in the chat. */}
            <TranscriptActions.Provider
              value={{ runId: run.id, awaitingId: null }}
            >
              <Transcript entries={entries} />
            </TranscriptActions.Provider>
          </div>
        </details>
        {/* The loop state as stored — what the transcript above is read out
            of, for when the question is about something it does not draw.
            Folded: it is long, and it is rarely the question. */}
        <details className="card run-context">
          <summary className="card-header">
            <span className="card-title mb-0">
              <T text="Context" />
            </span>
            <span className="text-secondary small ms-2">
              <T text="the loop state as stored" />
            </span>
          </summary>
          <div className="card-body">
            <pre className="mb-0 small">{prettyContext(run.context)}</pre>
          </div>
        </details>
      </PageBody>
    </>
  );
}
