// What a checklist run in progress shows: a toast, top right, naming the
// checklist and the step it is on, updated as each step finishes.
//
// The Run buttons run over a socket rather than `POST …/run` (which answers
// only when the run is over — minutes, for one that drives a browser): the
// server runs the checklist on the socket's own connection and pushes a
// `progress` frame as each step begins and ends, then the runs
// (`crates/sc-server/src/checklist_live.rs`). A socket that cannot be opened
// at all (an older server, a proxy that drops upgrades) falls back to the
// `POST`: the run still happens, without the toast's steps.

import { useCallback, useState } from "react";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "./api";
import { outcomeTone, type ChecklistRun } from "./checklists";
import { useT, type Translator } from "./i18n";
import { StatusBadge } from "./layout";

/** The run sockets' paths (the server's `CHECKLIST_RUN_LIVE_ROUTE` and
 * `CHECKLISTS_RUN_LIVE_ROUTE`; a Rust test asserts the spellings agree). */
export const CHECKLIST_RUN_LIVE_ROUTE = "/api/checklists/{id}/run/live";
export const CHECKLISTS_RUN_LIVE_ROUTE = "/api/checklist-runs/live";

/** How far a run has got, as a `progress` frame carries it. */
export type ChecklistProgress = {
  checklist?: string | null;
  checklists_done: number;
  checklists_total?: number | null;
  total?: number | null;
  step?: number | null;
  label?: string | null;
  type?: string | null;
  steps: Array<{ label: string; type: string; outcome: string }>;
};

/** What to run: one checklist, or an application's (`"none"`: the data-only
 * ones). */
export type RunTarget = { checklist: string } | { application: string };

/** How many finished steps the toast lists: the latest ones. */
export const RECENT_STEPS = 4;

/** The socket URL that runs `target`, on this page's host. */
export function runSocketUrl(location: { protocol: string; host: string }, target: RunTarget): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  const path =
    "checklist" in target
      ? CHECKLIST_RUN_LIVE_ROUTE.replace("{id}", encodeURIComponent(target.checklist))
      : `${CHECKLISTS_RUN_LIVE_ROUTE}?application=${encodeURIComponent(target.application)}`;
  return `${scheme}//${location.host}${path}`;
}

/** One frame from a run socket, or `null` for one this page does not know. */
export type RunFrame =
  | { type: "progress"; progress: ChecklistProgress }
  | { type: "finished"; runs: ChecklistRun[] }
  | { type: "failed"; error: string };

export function readFrame(text: string): RunFrame | null {
  let frame: unknown;
  try {
    frame = JSON.parse(text);
  } catch {
    return null;
  }
  if (!frame || typeof frame !== "object") return null;
  const f = frame as Record<string, unknown>;
  if (f.type === "progress" && f.progress && typeof f.progress === "object") {
    return { type: "progress", progress: f.progress as ChecklistProgress };
  }
  if (f.type === "finished" && Array.isArray(f.runs)) {
    return { type: "finished", runs: f.runs as ChecklistRun[] };
  }
  if (f.type === "failed") return { type: "failed", error: String(f.error ?? "") };
  return null;
}

/** A run that the server reported failing, or whose socket went before it
 * answered: its message is the one to show. */
export class RunError extends Error {}

/** The message for an error from [`useChecklistProgress`]'s `run`. */
export function runErrorMessage(err: unknown, fallback: string): string {
  return err instanceof RunError && err.message ? err.message : errorMessage(err, fallback);
}

/** A socket, as much of one as [`runLive`] uses (so a test can script it). */
export type SocketLike = {
  onopen: (() => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  close: () => void;
};

/** Run `target` over `open()`'s socket, telling `onProgress` each frame; the
 * `POST` (`fallback`) when the socket never opens. `lost` is the message for
 * a socket that goes after the run began but before it answered — the run
 * carries on, on the server, so nothing is run again. */
export function runLive(
  open: () => SocketLike,
  fallback: () => Promise<ChecklistRun[]>,
  onProgress: (p: ChecklistProgress) => void,
  lost: string,
): Promise<ChecklistRun[]> {
  return new Promise((resolve, reject) => {
    let socket: SocketLike;
    try {
      socket = open();
    } catch {
      fallback().then(resolve, reject);
      return;
    }
    let opened = false;
    let settled = false;
    socket.onopen = () => {
      opened = true;
    };
    socket.onmessage = (event) => {
      const frame = readFrame(String(event.data));
      if (!frame || settled) return;
      if (frame.type === "progress") onProgress(frame.progress);
      else {
        settled = true;
        if (frame.type === "finished") resolve(frame.runs);
        else reject(new RunError(frame.error));
        socket.close();
      }
    };
    socket.onclose = () => {
      if (settled) return;
      settled = true;
      // Never opened: the server did not run anything, so the POST may.
      if (!opened) fallback().then(resolve, reject);
      else reject(new RunError(lost));
    };
  });
}

/** What the toast says, from the last progress it read (`null` before the
 * first). Pure, so the wording is tested without a server. */
export type ProgressView = {
  /** "Checklist 2 of 5", when the request runs several. */
  checklistCount: { index: number; count: number } | null;
  /** The checklist running now. */
  checklist: string | null;
  /** The running step, 1-based, with its label (its type when it has none). */
  step: { number: number; label: string } | null;
  /** The checklist's step count, once known. */
  total: number | null;
  /** How many of its steps have finished. */
  done: number;
  /** Percent of the checklist's steps finished, for the bar. */
  percent: number;
  /** The latest finished steps, newest last. */
  recent: Array<{ label: string; outcome: string }>;
};

export function progressView(p: ChecklistProgress | null): ProgressView {
  if (!p) {
    return {
      checklistCount: null,
      checklist: null,
      step: null,
      total: null,
      done: 0,
      percent: 0,
      recent: [],
    };
  }
  const total = p.total ?? null;
  const done = p.steps.length;
  const count = p.checklists_total ?? null;
  return {
    checklistCount:
      count !== null && count > 1
        ? { index: Math.min(p.checklists_done + 1, count), count }
        : null,
    checklist: p.checklist ?? null,
    step:
      p.step != null ? { number: p.step, label: p.label || p.type || "" } : null,
    total,
    done,
    percent: total ? Math.round((Math.min(done, total) / total) * 100) : 0,
    recent: p.steps
      .slice(-RECENT_STEPS)
      .map((s) => ({ label: s.label || s.type, outcome: s.outcome })),
  };
}

/** Run checklists with the toast showing their progress. `run` resolves to
 * the runs, as `runChecklist`/`runChecklists` answer; its errors are read
 * with [`runErrorMessage`]. */
export function useChecklistProgress() {
  const { t } = useT();
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState<ChecklistProgress | null>(null);
  const [hidden, setHidden] = useState(false);

  const run = useCallback(
    async (target: RunTarget): Promise<ChecklistRun[]> => {
      setProgress(null);
      setHidden(false);
      setRunning(true);
      try {
        return await runLive(
          () => new WebSocket(runSocketUrl(window.location, target)) as unknown as SocketLike,
          async () =>
            "checklist" in target
              ? [await api.runChecklist(target.checklist)]
              : api.runChecklists({ application: target.application }),
          setProgress,
          t(
            "The connection to the run was lost. It carries on; its result will be in the checklist's runs.",
          ),
        );
      } finally {
        setRunning(false);
      }
    },
    [t],
  );

  return {
    run,
    toast: running && !hidden ? (
      <ChecklistProgressToast progress={progress} onHide={() => setHidden(true)} />
    ) : null,
  };
}

/** The toast itself. Hiding it does not stop the run. */
export function ChecklistProgressToast({
  progress,
  onHide,
}: {
  progress: ChecklistProgress | null;
  onHide: () => void;
}) {
  const { t } = useT();
  const v = progressView(progress);
  return (
    <div className="toast-container position-fixed top-0 end-0 p-3">
      <div
        className="toast show checklist-progress-toast border-primary"
        role="status"
        aria-live="polite"
        data-testid="checklist-progress"
      >
        <div className="toast-header">
          <Spinner animation="border" size="sm" className="me-2 text-primary" />
          <strong className="me-auto">
            {v.checklist
              ? t("Running checklist {name}", { name: v.checklist })
              : t("Starting the checklist run…")}
          </strong>
          {v.checklistCount && (
            <small className="text-secondary me-2">
              {t("{index} of {count}", v.checklistCount)}
            </small>
          )}
          <button type="button" className="btn-close" aria-label={t("Hide")} onClick={onHide} />
        </div>
        <div className="toast-body">
          <div className="mb-2">
            {v.step ? (
              v.total !== null ? (
                t("Step {number} of {total}: {label}", {
                  number: v.step.number,
                  total: v.total,
                  label: v.step.label,
                })
              ) : (
                t("Step {number}: {label}", v.step)
              )
            ) : v.total !== null && v.done >= v.total ? (
              t("Finishing up…")
            ) : (
              t("Preparing…")
            )}
          </div>
          {v.total !== null && v.total > 0 && (
            <div
              className="progress progress-sm mb-2"
              role="progressbar"
              aria-valuenow={v.done}
              aria-valuemin={0}
              aria-valuemax={v.total}
            >
              <div className="progress-bar" style={{ width: `${v.percent}%` }} />
            </div>
          )}
          {v.recent.length > 0 && (
            <ul className="list-unstyled mb-0 small">
              {v.recent.map((s, i) => (
                <li key={`${v.done - v.recent.length + i}`} className="d-flex align-items-center gap-2">
                  <StatusBadge tone={outcomeTone(s.outcome)}>{outcomeLabel(s.outcome, t)}</StatusBadge>
                  <span className="text-truncate">{s.label}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>
  );
}

/** A step's outcome in words. Each message id is written out so the
 * extractor can read it; an outcome this list does not know shows as sent. */
function outcomeLabel(outcome: string, t: Translator["t"]): string {
  switch (outcome) {
    case "pass":
      return t("pass");
    case "fail":
      return t("fail");
    case "error":
      return t("error");
    case "skipped":
      return t("skipped");
    default:
      return outcome;
  }
}
