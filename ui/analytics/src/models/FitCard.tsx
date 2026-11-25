// One fit in the model editor's workbench (analytics design A1): a card that
// keeps the fit table's columns — the fit, its status, its result (a
// posterior's trust), its hyperparameters (a posterior's sampler), its report
// and what can be done with it — and that opens in place on what the fit came
// to: what it was fitted on, its errors by split or a posterior's checks
// against their thresholds, its warnings, its first plots, and the way on to
// the full report, "Try a row", the posterior and saving a report.
//
// The full report stays the modal it was (`FitReportModal.tsx`); a card is
// the summary of it that is read without leaving the list.

import { useCallback, useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, downloadModelRun, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { routeHash } from "../router";
import {
  hyperparameterText,
  posteriorChecks,
  readPosteriorMetrics,
  samplerText,
  shortId,
  splitErrors,
  trustVerdict,
} from "./fitCard";
import { buildFitReport, fitReportName, warningTitle } from "./fitReport";
import type { ReportTab } from "./FitReportModal";
import {
  chainPercent,
  headlineMetric,
  orderWarnings,
  outcomeLabel,
  readEncoding,
  readMetrics,
  readOutcome,
  readRowCounts,
  type InstanceDetail,
  type InstanceItem,
  type ModelItem,
} from "./models";
import { OutputBody, OutputTableView } from "./OutputsPanel";
import { includeParam, moreOutputs, readOutputs, shownOutputs, type ModelOutput } from "./outputs";
import { stageText, type LiveProgress } from "./progress";
import { StatusBadge } from "./StatusBadge";

/** The sentence a cancelled fit fails with (`sc_model::fit::CANCELLED`). */
const CANCELLED = "the fit was cancelled";

/** How many plots an open card draws. */
const THUMBS = 3;

/** What a card needs to know about the model it is a fit of. */
export type FitCardContext = {
  model: ModelItem;
  datasetName?: string | null;
  providerDescription?: string | null;
  /** A formula model's formula and family, as the model has them now. */
  formula?: { formula: string; family: string } | null;
  /** The optional plots chosen (the model's view state). */
  plots: string[];
  onPlots: (plots: string[]) => void;
  /** Whether this user may save a report workspace. */
  mayCreate: boolean;
  /** The model's provider binds data: its fits are posteriors. */
  posterior: boolean;
};

/** The column titles, in the cards' grid. */
export function FitHeader({ posterior }: { posterior: boolean }) {
  return (
    <div className="an-fit-row an-fit-head" aria-hidden="true">
      <span />
      <span className="an-fit-c-name">
        <T text="Fit" />
      </span>
      <span className="an-fit-c-status">
        <T text="Status" />
      </span>
      <span className="an-fit-c-result">{posterior ? <T text="Trust" /> : <T text="Result" />}</span>
      <span className="an-fit-c-hyper">{posterior ? <T text="Sampler" /> : <T text="Hyperparameters" />}</span>
      <span className="an-fit-c-report">
        <T text="Report" />
      </span>
      <span className="an-fit-c-actions">
        <T text="Actions" />
      </span>
    </div>
  );
}

export function FitCard({
  ctx,
  instance,
  number,
  open,
  onToggle,
  live,
  onReport,
  onActivate,
  onRemove,
  onCancel,
  onChanged,
}: {
  ctx: FitCardContext;
  instance: InstanceItem;
  /** Its number, `#1` the oldest. */
  number: number;
  open: boolean;
  onToggle: () => void;
  /** Its progress, when it is the fit being followed. */
  live: LiveProgress | null;
  /** Open its full report on a tab. */
  onReport: (tab: ReportTab) => void;
  onActivate: () => void;
  onRemove: () => void;
  onCancel: () => void;
  /** Its report was saved: read the list again. */
  onChanged: () => void;
}) {
  const { t } = useT();
  const outcome = readOutcome(instance.outcome);
  const metrics = readMetrics(instance.metrics);
  const pm = readPosteriorMetrics(metrics);
  const posterior = outcome?.outcome === "posterior" || pm !== null || ctx.posterior;
  const fitting = instance.status === "fitting";
  const fitted = instance.status === "fitted";
  const cancelled = instance.status === "failed" && instance.error === CANCELLED;
  const failed = instance.status === "failed" && !cancelled;
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // A running fit's age ticks over while it runs.
  const [, tick] = useState(0);
  useEffect(() => {
    if (!fitting) return undefined;
    const timer = window.setInterval(() => tick((n) => n + 1), 15_000);
    return () => window.clearInterval(timer);
  }, [fitting]);

  const createReport = async () => {
    setCreating(true);
    setError(null);
    try {
      await saveReport(ctx, instance.id, t);
      onChanged();
    } catch (err) {
      setError(errorMessage(err, t("Could not save the report.")));
    } finally {
      setCreating(false);
    }
  };

  const label = `#${number}`;
  const classes = ["an-fit-card"];
  if (open) classes.push("is-open");
  if (failed) classes.push("is-failed");

  return (
    <div className={classes.join(" ")} data-fit={instance.id}>
      <div className="an-fit-row">
        <button
          type="button"
          className="an-fit-caret"
          aria-expanded={open}
          aria-label={open ? t("Close {fit}", { fit: label }) : t("Open {fit}", { fit: label })}
          onClick={onToggle}
        >
          {open ? "▾" : "▸"}
        </button>

        <div className="an-fit-c-name">
          <a
            href={routeHash({ name: "model", id: ctx.model.id, fit: instance.id })}
            className="an-fit-number"
            onClick={(e) => {
              e.preventDefault();
              onToggle();
            }}
          >
            {label}
          </a>{" "}
          <span className="an-fit-when">{shortWhen(instance.created, t)}</span>
          {instance.name.trim() !== "" && <div className="an-fit-label">{instance.name}</div>}
        </div>

        <div className="an-fit-c-status">
          <div className="d-flex flex-wrap gap-1">
            {fitting && (
              <StatusBadge tone="blue">
                {posterior ? <T text="sampling" /> : <T text="fitting" />}
              </StatusBadge>
            )}
            {instance.active && <StatusBadge tone="green">● {t("active")}</StatusBadge>}
            {fitted && !instance.active && (
              <StatusBadge tone="secondary">
                <T text="fitted" />
              </StatusBadge>
            )}
            {failed && (
              <StatusBadge tone="red">
                <T text="failed" />
              </StatusBadge>
            )}
            {cancelled && (
              <StatusBadge tone="secondary">
                <T text="cancelled" />
              </StatusBadge>
            )}
            {instance.dataset_changed && (
              <StatusBadge tone="yellow" title={t("The dataset has changed since this fit.")}>
                <T text="dataset changed" />
              </StatusBadge>
            )}
          </div>
          {failed && instance.error && <div className="an-fit-error text-danger small">{instance.error}</div>}
          {cancelled && (
            <div className="small text-secondary">
              <T text="Stopped before it finished." />
            </div>
          )}
        </div>

        <div className="an-fit-c-result">
          {fitting ? (
            <RunningProgress live={live} created={instance.created} />
          ) : posterior ? (
            pm ? (
              (() => {
                const trust = trustVerdict(pm);
                return (
                  <StatusBadge tone={trust.tone}>
                    {trust.tone === "green" ? "✓ " : "⚠ "}
                    {trustText(trust.text, t)}
                  </StatusBadge>
                );
              })()
            ) : (
              <span className="text-secondary">—</span>
            )
          ) : fitted ? (
            <strong>{headlineMetric(metrics) ?? "—"}</strong>
          ) : (
            <span className="text-secondary">—</span>
          )}
        </div>

        <div className="an-fit-c-hyper font-monospace small">
          {(posterior ? (pm ? samplerText(pm) : "") : hyperparameterText(instance.hyperparameters)) || (
            <span className="an-fit-none">—</span>
          )}
        </div>

        <div className="an-fit-c-report small">
          {instance.report ? (
            <a href={routeHash({ name: "workspace", id: instance.report.id })}>{instance.report.name} ↗</a>
          ) : fitted && ctx.mayCreate ? (
            <a
              href="#"
              aria-disabled={creating}
              onClick={(e) => {
                e.preventDefault();
                if (!creating) void createReport();
              }}
            >
              {creating ? t("Saving…") : `+ ${t("Create report")}`}
            </a>
          ) : (
            <span className="text-secondary an-fit-none">—</span>
          )}
        </div>

        <div className="an-fit-c-actions">
          {fitting && (
            <Button size="sm" variant="outline-secondary" disabled={Boolean(live?.cancelRequested)} onClick={onCancel}>
              {live?.cancelRequested ? <T text="Stopping…" /> : <T text="Cancel" />}
            </Button>
          )}
          {!fitting && !fitted && (
            <Button size="sm" variant="outline-secondary" onClick={onRemove}>
              <T text="Remove" />
            </Button>
          )}
          {fitted && !instance.active && (
            <Button size="sm" variant="outline-secondary" onClick={onActivate}>
              <T text="Activate" />
            </Button>
          )}
          {fitted && (
            <Dropdown align="end">
              <Dropdown.Toggle size="sm" variant="outline-secondary" className="an-more" aria-label={t("More")}>
                ⋯
              </Dropdown.Toggle>
              <Dropdown.Menu>
                <Dropdown.Item onClick={() => onReport("report")}>
                  <T text="Full report" />
                </Dropdown.Item>
                <Dropdown.Divider />
                <Dropdown.Item className="text-danger" onClick={onRemove}>
                  <T text="Remove" />
                </Dropdown.Item>
              </Dropdown.Menu>
            </Dropdown>
          )}
        </div>
      </div>
      {error && (
        <Alert variant="danger" className="mx-3 mb-2 py-2 small">
          {error}
        </Alert>
      )}
      {open && !fitting && (
        <FitDetails
          ctx={ctx}
          instanceId={instance.id}
          posterior={posterior}
          onReport={onReport}
          onChanged={onChanged}
        />
      )}
    </div>
  );
}

/** The Result (Trust) column of a fit that is running. */
function RunningProgress({
  live,
  created,
}: {
  live: LiveProgress | null;
  created: string;
}) {
  const { t } = useT();
  const progress = live?.progress ?? null;
  const minutes = Math.max(0, Math.floor((Date.now() - new Date(created).getTime()) / 60_000));
  const age = Number.isFinite(minutes) ? t("{count} min", { count: minutes }) : "";
  if (progress && progress.chains.length > 0) {
    const percents = progress.chains.map(chainPercent);
    const low = Math.min(...percents);
    const high = Math.max(...percents);
    return (
      <div className="an-fit-progress">
        <span className="small">
          {low === high ? t("chains {p}%", { p: low }) : t("chains {low}–{high}%", { low, high })}
        </span>
        <div className="an-fit-bar">
          <span style={{ width: `${low}%` }} />
        </div>
      </div>
    );
  }
  const stages = ["reading", "fitting", "scoring"];
  const at = progress ? stages.indexOf(progress.stage) : -1;
  return (
    <div className="an-fit-progress">
      <span className="small">
        {progress ? stageText(t, progress.stage) : t("starting")}
        {age && ` · ${age}`}
      </span>
      <div className="an-fit-bar">
        <span style={{ width: `${at >= 0 ? Math.round((100 * (at + 1)) / stages.length) : 8}%` }} />
      </div>
    </div>
  );
}

/** An open card: what the fit came to. */
function FitDetails({
  ctx,
  instanceId,
  posterior,
  onReport,
  onChanged,
}: {
  ctx: FitCardContext;
  instanceId: string;
  posterior: boolean;
  onReport: (tab: ReportTab) => void;
  onChanged: () => void;
}) {
  const { t } = useT();
  const { model, plots } = ctx;
  const [detail, setDetail] = useState<InstanceDetail | null>(null);
  const [outputs, setOutputs] = useState<ModelOutput[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [allWarnings, setAllWarnings] = useState(false);
  const [saving, setSaving] = useState(false);
  const [busy, setBusy] = useState(false);
  const include = includeParam(plots);

  const load = useCallback(async () => {
    try {
      setDetail(await api.getModelInstance(instanceId));
    } catch (err) {
      setError(errorMessage(err, t("Could not load this fit.")));
    }
  }, [instanceId, t]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    let cancelled = false;
    void api
      .getModelOutputs(model.id, { fit: instanceId, include })
      .then((answer) => !cancelled && setOutputs(readOutputs(answer.outputs)))
      .catch(() => !cancelled && setOutputs([]));
    return () => {
      cancelled = true;
    };
  }, [model.id, instanceId, include]);

  if (error) {
    return (
      <div className="an-fit-details">
        <Alert variant="danger" className="mb-0">
          {error}
        </Alert>
      </div>
    );
  }
  if (!detail) {
    return (
      <div className="an-fit-details text-center">
        <Spinner animation="border" size="sm" />
      </div>
    );
  }

  const metrics = readMetrics(detail.metrics);
  const pm = readPosteriorMetrics(metrics);
  const outcome = readOutcome(detail.outcome);
  const label = outcomeLabel(outcome);
  const features = (readEncoding(detail.encoding)?.columns ?? []).map((c) => c.column);
  const rows = readRowCounts(detail.rows);
  const errors = splitErrors(metrics);
  const shown = outputs ? shownOutputs(outputs, plots) : [];
  const thumbs = shown.filter((o) => o.kind === "plot").slice(0, THUMBS);
  const tables = shown.filter((o) => o.kind === "table" && !o.error);
  const more = outputs ? moreOutputs(outputs, plots) : [];
  const fitted = detail.status === "fitted";
  const tryable = fitted && outcome !== null && outcome.outcome !== "test" && !posterior;
  const warnings = posterior
    ? orderWarnings(detail.warnings).map((w) => ({ title: warningTitle(w.kind, t), text: w.text }))
    : detail.warnings.map((text) => ({ title: null as string | null, text }));

  const facts: [string, string][] = [];
  if (ctx.datasetName) facts.push([t("Dataset"), ctx.datasetName]);
  if (posterior && ctx.formula && !detail.program_changed) {
    if (ctx.formula.formula.trim()) facts.push([t("Formula"), ctx.formula.formula.trim()]);
    facts.push([t("Family"), ctx.formula.family]);
  }
  if (label) facts.push([t("Predicts"), label]);
  if (features.length > 0) {
    facts.push([
      t("Features"),
      features.length <= 4
        ? `${features.join(", ")} · ${features.length}`
        : t("{first} and {count} more", { first: features.slice(0, 3).join(", "), count: features.length - 3 }),
    ]);
  }
  facts.push([t("Fitted"), shortWhen(detail.created, t)]);
  facts.push([t("Fit id"), shortId(detail.id)]);

  const save = async () => {
    setSaving(true);
    try {
      await saveReport(ctx, detail.id, t, { detail, outputs: outputs ?? undefined });
      await load();
      onChanged();
    } catch (err) {
      setError(errorMessage(err, t("Could not save the report.")));
    } finally {
      setSaving(false);
    }
  };

  const download = async () => {
    setBusy(true);
    try {
      await downloadModelRun(detail.id);
    } catch (err) {
      setError(errorMessage(err, t("Could not download the run.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="an-fit-details">
      <div className="an-fit-columns">
        <div>
          <h4 className="an-fit-h">
            <T text="About this fit" />
          </h4>
          <dl className="an-fit-facts">
            {facts.map(([k, v]) => (
              <div key={k}>
                <dt>{k}</dt>
                <dd className={k === t("Formula") || k === t("Fit id") ? "font-monospace" : undefined}>{v}</dd>
              </div>
            ))}
          </dl>
        </div>
        {posterior && pm ? (
          <div>
            <h4 className="an-fit-h">
              <T text="Sampler diagnostics" />
            </h4>
            <Table size="sm" className="an-fit-table mb-0">
              <thead>
                <tr>
                  <th>
                    <T text="Check" />
                  </th>
                  <th>
                    <T text="Threshold" />
                  </th>
                  <th>
                    <T text="Value" />
                  </th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {posteriorChecks(pm).map((c) => (
                  <tr key={c.key} className={c.ok === false ? "is-warn" : undefined}>
                    <td>{checkLabel(c.key, c.label, t)}</td>
                    <td className="font-monospace text-secondary">{c.threshold}</td>
                    <td className="font-monospace">{c.value}</td>
                    <td className="text-end">{c.ok === null ? "" : c.ok ? "✓" : "⚠"}</td>
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
        ) : (
          errors && (
            <div>
              <h4 className="an-fit-h">
                <T text="Errors by split" />
              </h4>
              <Table size="sm" className="an-fit-table mb-1">
                <thead>
                  <tr>
                    <th>
                      <T text="Split" />
                    </th>
                    {errors.columns.map((c) => (
                      <th key={c}>{errorColumnLabel(c, t)}</th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {errors.rows.map((r) => (
                    <tr key={r.split}>
                      <td>{r.split === "train" ? t("Train") : r.split === "validation" ? t("Validation") : t("Test")}</td>
                      {r.cells.map((c, i) => (
                        <td key={i} className="font-monospace">
                          {c}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </Table>
              {rows && (
                <div className="small text-secondary">
                  {t("{selected} selected", { selected: rows.selected.toLocaleString() })}
                  {rows.dropped > 0 && ` · ${t("{dropped} dropped for blanks", { dropped: rows.dropped.toLocaleString() })}`}
                </div>
              )}
            </div>
          )
        )}
      </div>

      {warnings.length > 0 && (
        <div className="an-fit-warning">
          <div className="d-flex gap-2 align-items-baseline">
            <strong className="text-nowrap">
              ⚠{" "}
              {posterior && warnings[0].title
                ? warnings[0].title
                : warnings.length === 1
                  ? t("1 warning")
                  : t("{count} warnings", { count: warnings.length })}
            </strong>
            <span className={allWarnings ? "d-none" : "an-fit-warning-text"}>
              {posterior ? warnings[0].text : warnings.map((w) => w.text).join("; ")}
            </span>
            {(warnings.length > 1 || !posterior) && (
              <a
                href="#"
                className="ms-auto text-nowrap small"
                onClick={(e) => {
                  e.preventDefault();
                  setAllWarnings((v) => !v);
                }}
              >
                {allWarnings ? t("Show fewer") : t("Show all")}
              </a>
            )}
          </div>
          {allWarnings && (
            <ul className="mb-0 mt-1 small">
              {warnings.map((w, i) => (
                <li key={i}>
                  {w.title && <strong>{w.title}. </strong>}
                  {w.text}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      {posterior && tables.length > 0 && (
        <div className="an-fit-effects">
          <h4 className="an-fit-h">{tables[0].label}</h4>
          {tables[0].table && <OutputTableView table={tables[0].table} />}
          {tables.length > 1 && (
            <div className="small mt-1">
              {tables.slice(1).map((o, i) => (
                <span key={o.name}>
                  {i > 0 && " · "}
                  <a
                    href="#"
                    onClick={(e) => {
                      e.preventDefault();
                      onReport("report");
                    }}
                  >
                    {o.label}
                  </a>
                </span>
              ))}
            </div>
          )}
        </div>
      )}

      {outputs === null ? (
        <div className="py-3 text-center">
          <Spinner animation="border" size="sm" />
        </div>
      ) : (
        thumbs.length > 0 && (
          <div className="an-fit-thumbs">
            {thumbs.map((o) => (
              <figure key={o.name} className="an-fit-thumb">
                <figcaption>{o.label}</figcaption>
                <OutputBody output={o} />
              </figure>
            ))}
          </div>
        )
      )}

      <div className="an-fit-foot">
        {more.length > 0 && (
          <Dropdown>
            <Dropdown.Toggle size="sm" variant="outline-secondary" className="an-more" id={`more-plots-${detail.id}`}>
              + <T text="More plots" />
            </Dropdown.Toggle>
            <Dropdown.Menu>
              {more.map((o) => (
                <Dropdown.Item key={o.name} onClick={() => ctx.onPlots([...plots, o.name])}>
                  {o.label}
                </Dropdown.Item>
              ))}
            </Dropdown.Menu>
          </Dropdown>
        )}
        {posterior ? (
          <>
            {fitted && (
              <Button size="sm" variant="outline-secondary" onClick={() => onReport("posterior")}>
                <T text="Open posterior" />
              </Button>
            )}
            {fitted && (
              <Button size="sm" variant="outline-secondary" onClick={() => onReport("posterior")}>
                <T text="Write back…" />
              </Button>
            )}
            {fitted && (
              <Button size="sm" variant="outline-secondary" disabled={busy} onClick={() => void download()}>
                {busy ? <T text="Preparing…" /> : <T text="Download run" />}
              </Button>
            )}
          </>
        ) : (
          <>
            {tryable && (
              <Button size="sm" variant="outline-secondary" onClick={() => onReport("try")}>
                <T text="Try a row" />
              </Button>
            )}
            <Button size="sm" variant="outline-secondary" onClick={() => onReport("report")}>
              <T text="Full report" />
            </Button>
          </>
        )}
        <span className="ms-auto d-flex align-items-center gap-2 flex-wrap">
          {detail.report && (
            <span className="small">
              <T text="Saved as" />{" "}
              <a href={routeHash({ name: "workspace", id: detail.report.id })}>{detail.report.name} ↗</a>
            </span>
          )}
          {ctx.mayCreate && fitted && (
            <Button size="sm" variant="outline-secondary" disabled={saving || outputs === null} onClick={() => void save()}>
              {saving ? (
                <T text="Saving…" />
              ) : detail.report ? (
                <T text="Save as another report" />
              ) : (
                <T text="Save as report" />
              )}
            </Button>
          )}
        </span>
      </div>
    </div>
  );
}

/**
 * Save a fit's report as a Report workspace under its default name, as the
 * full report's "Save as report workspace" does — the fit and its outputs read
 * first when the caller does not have them.
 */
async function saveReport(
  ctx: FitCardContext,
  instanceId: string,
  t: (text: string, args?: Record<string, string | number>) => string,
  have: { detail?: InstanceDetail; outputs?: ModelOutput[] } = {},
): Promise<void> {
  const [detail, outputs] = await Promise.all([
    have.detail ?? api.getModelInstance(instanceId),
    have.outputs ??
      api
        .getModelOutputs(ctx.model.id, { fit: instanceId, include: includeParam(ctx.plots) })
        .then((answer) => readOutputs(answer.outputs)),
  ]);
  const report = buildFitReport({
    model: ctx.model,
    providerDescription: ctx.providerDescription,
    datasetName: ctx.datasetName,
    instance: detail,
    outputs: shownOutputs(outputs, ctx.plots),
    t,
  });
  await api.createWorkspace({
    name: fitReportName(ctx.model.name, detail, t),
    kind: "report",
    state: report,
    fit: detail.id,
  });
}

/** When a fit was started, short: "today 16:14", "12 Nov 10:42". */
export function shortWhen(value: string, t: (text: string, args?: Record<string, string | number>) => string): string {
  const when = new Date(value);
  if (Number.isNaN(when.getTime())) return value;
  const time = when.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  const now = new Date();
  if (when.toDateString() === now.toDateString()) return t("today {time}", { time });
  const day = when.toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    year: when.getFullYear() === now.getFullYear() ? undefined : "numeric",
  });
  return `${day} ${time}`;
}

// --- words the helpers in ./fitCard return -------------------------------------
//
// `fitCard.ts` returns English, so its tests can read it; these put each phrase
// through `t()` with its message id written out, because the extractor only
// collects a string literal (an id held in a variable never reaches the
// catalogue). Anything not listed shows as the helper gave it.

type Translate = (text: string, args?: Record<string, string | number>) => string;

/** A posterior's verdict, as `trustVerdict` words it. */
function trustText(text: string, t: Translate): string {
  switch (text) {
    case "trustworthy":
      return t("trustworthy");
    case "chains disagree":
      return t("chains disagree");
    case "1 divergence":
      return t("1 divergence");
    case "low E-BFMI":
      return t("low E-BFMI");
    case "few effective draws":
      return t("few effective draws");
    case "tree depth hit":
      return t("tree depth hit");
  }
  const divergences = /^(\d+) divergences$/.exec(text);
  if (divergences) return t("{count} divergences", { count: Number(divergences[1]) });
  return text;
}

/** A sampler diagnostic's name. */
function checkLabel(key: string, label: string, t: Translate): string {
  switch (key) {
    case "rhat":
      return t("Max R̂");
    case "ess_bulk":
      return t("Min bulk ESS");
    case "ess_tail":
      return t("Min tail ESS");
    case "divergent":
      return t("Divergences");
    case "treedepth":
      return t("Tree-depth hits");
    case "ebfmi":
      return t("E-BFMI");
    default:
      return label;
  }
}

/** A column of the errors-by-split table. */
function errorColumnLabel(column: string, t: Translate): string {
  switch (column) {
    case "Rows":
      return t("Rows");
    case "R²":
      return t("R²");
    case "RMSE":
      return t("RMSE");
    case "MAE":
      return t("MAE");
    case "Accuracy":
      return t("Accuracy");
    case "Classes":
      return t("Classes");
    case "WCSS":
      return t("WCSS");
    case "Clusters":
      return t("Clusters");
    case "Explained variance":
      return t("Explained variance");
    default:
      return column;
  }
}
