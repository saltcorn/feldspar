// The runs of one workflow (§10.3, phase 6.6): what each is doing, where it got
// to, and when it next wants the engine.
//
// A workflow that fires on every insert has as many runs as the table has rows,
// so the list is filtered by state and paged — the two questions an admin
// actually arrives with are "is anything stuck?" and "what happened to the one
// from this morning?", and both are answered by a state filter over the newest
// first.
//
// `wake_at` is the column that makes a *waiting* run legible: a run waiting for
// a timer says when it will carry on, and a run waiting for a **person** says
// nothing at all — which is the honest reading of "no clock will make this
// runnable", and the reason the run detail screen has a form on it.

import { useCallback, useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import type { ListWorkflowRunsResponse } from "../client";
import { IconArrowLeft } from "../icons";
import { triggerCrumbs } from "../breadcrumbs";
import { PageBody, PageHeader, StatusBadge, type Tone } from "../layout";
import { T, useT } from "../i18n";

type RunItem = ListWorkflowRunsResponse[number];

/** How many runs a page holds. */
const PAGE = 25;

/** The states a run can be in, as the engine stores them. */
const STATES = ["running", "waiting", "done", "failed", "aborted"];

/** The colour a state is shown in — the same vocabulary the triggers list uses
 * for a trigger that is off or broken. */
export function runTone(state: string): Tone {
  switch (state) {
    case "done":
      return "green";
    case "failed":
      return "red";
    case "waiting":
      return "yellow";
    case "running":
      return "blue";
    default:
      return "secondary";
  }
}

/** A timestamp as a local, readable time — or a dash where there is none. */
export function when(at: string | null | undefined): string {
  if (!at) return "—";
  const date = new Date(at);
  return Number.isNaN(date.getTime()) ? at : date.toLocaleString();
}

export function WorkflowRuns({ triggerId }: { triggerId: string }) {
  const { t } = useT();
  const [runs, setRuns] = useState<RunItem[] | null>(null);
  const [name, setName] = useState<string>("");
  const [state, setState] = useState("");
  const [page, setPage] = useState(0);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setRuns(
        await api.listWorkflowRuns(triggerId, {
          ...(state === "" ? {} : { state }),
          limit: PAGE,
          offset: page * PAGE,
        }),
      );
      setError(null);
    } catch (err) {
      setError(errorMessage(err, "Could not load the runs of this workflow."));
    }
  }, [triggerId, state, page]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    void (async () => {
      try {
        const triggers = await api.listTriggers();
        setName(triggers.find((t) => t.id === triggerId)?.name ?? "");
      } catch {
        setName("");
      }
    })();
  }, [triggerId]);

  return (
    <>
      <PageHeader
        crumbs={triggerCrumbs(t, triggerId, name || null)}
        title={t("Runs")}
        docTitle={name ? `${name} · ${t("Runs")}` : t("Runs")}
        actions={
          <>
            <Button variant="outline-secondary" onClick={() => navigate("/triggers")}>
              <IconArrowLeft className="icon-2" />
              <T text="Back" />
            </Button>
            <Button
              variant="outline-secondary"
              href={`#/triggers/${encodeURIComponent(triggerId)}/workflow`}
            >
              <T text="Edit the workflow" />
            </Button>
            <Button variant="outline-secondary" onClick={() => void load()}>
              <T text="Refresh" />
            </Button>
          </>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        <div className="card">
          <div className="card-body border-bottom py-2 d-flex align-items-center gap-2">
            <Form.Label htmlFor="runState" className="mb-0 text-muted">
              <T text="State" />
            </Form.Label>
            <Form.Select
              id="runState"
              className="w-auto"
              value={state}
              onChange={(e) => {
                setPage(0);
                setState(e.target.value);
              }}
            >
              <option value=""><T text="All" /></option>
              {STATES.map((s) => (
                <option key={s} value={s}>
                  {s}
                </option>
              ))}
            </Form.Select>
          </div>
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Started" /></th>
                <th><T text="State" /></th>
                <th><T text="Step" /></th>
                <th><T text="Version" /></th>
                <th><T text="Wakes" /></th>
                <th><T text="Started by" /></th>
                <th className="text-end" />
              </tr>
            </thead>
            <tbody>
              {runs?.length === 0 && (
                <tr>
                  <td colSpan={7} className="text-muted">
                    {page === 0 ? "This workflow has not run yet." : "No more runs."}
                  </td>
                </tr>
              )}
              {runs?.map((run) => (
                <tr key={run.id}>
                  <td className="small">{when(run.created_at)}</td>
                  <td>
                    <StatusBadge tone={runTone(run.state)} title={run.error ?? undefined}>
                      {run.state}
                    </StatusBadge>
                    {run.error && (
                      <div className="text-danger small text-break">{run.error}</div>
                    )}
                  </td>
                  <td>{run.current_step ?? "—"}</td>
                  <td>{run.subject_version ?? "—"}</td>
                  {/* A run waiting on a person has no wake time, and that is the
                      point: nothing but an answer will move it. */}
                  <td className="small">
                    {run.state === "waiting" && !run.wake_at ? (
                      <span className="text-muted"><T text="waiting for someone" /></span>
                    ) : (
                      when(run.wake_at)
                    )}
                  </td>
                  <td className="small">{run.user ?? "—"}</td>
                  <td className="text-end">
                    <Button
                      size="sm"
                      variant="outline-secondary"
                      href={`#/runs/${encodeURIComponent(run.id)}`}
                    >
                      <T text="Open" />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
          <div className="card-footer d-flex align-items-center justify-content-between">
            <span className="text-muted small">
              {t("Page {page}", { page: page + 1 })}
            </span>
            <div className="btn-list">
              <Button
                size="sm"
                variant="outline-secondary"
                disabled={page === 0}
                onClick={() => setPage((p) => Math.max(0, p - 1))}
              >
                <T text="Previous" />
              </Button>
              <Button
                size="sm"
                variant="outline-secondary"
                disabled={(runs?.length ?? 0) < PAGE}
                onClick={() => setPage((p) => p + 1)}
              >
                <T text="Next" />
              </Button>
            </div>
          </div>
        </div>
      </PageBody>
    </>
  );
}
