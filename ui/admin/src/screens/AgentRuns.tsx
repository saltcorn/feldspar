// Every run of one agent, for the admin: all users' conversations, not just the
// admin's own — the chat's history panel shows the person chatting their runs,
// and this is the screen for "what has everybody been asking it?".
//
// One line per run, newest first. Show opens the run page (transcript, changes
// and the context the loop stored); Delete removes it, which is the only record
// of what was said, so it asks first.

import { useCallback, useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { runUserLabel } from "../agentRuns";
import type { ListRunsResponse, ListUsersResponse } from "../client";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { runTone, when } from "./WorkflowRuns";
import { T, useT } from "../i18n";

type RunItem = ListRunsResponse[number];

export function AgentRuns({ agent }: { agent: string }) {
  const { t } = useT();
  const [runs, setRuns] = useState<RunItem[] | null>(null);
  const [users, setUsers] = useState<ListUsersResponse>([]);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setRuns(await api.listRuns(agent));
      setError(null);
    } catch (err) {
      setError(errorMessage(err, "Could not load the runs of this agent."));
    }
  }, [agent]);

  useEffect(() => {
    void load();
  }, [load]);

  // Only for the emails: a run whose user cannot be looked up still lists, by id.
  useEffect(() => {
    api
      .listUsers()
      .then(setUsers)
      .catch(() => setUsers([]));
  }, []);

  const remove = async (run: RunItem) => {
    if (!window.confirm(t("Delete this run? It is the only record of what happened."))) {
      return;
    }
    try {
      await api.deleteRun(run.id);
      await load();
    } catch (err) {
      setError(errorMessage(err, "Could not delete that run."));
    }
  };

  return (
    <>
      <PageHeader
        pretitle="Agent"
        title={`${agent} · ${t("runs")}`}
        actions={
          <>
            <Button variant="outline-secondary" onClick={() => navigate("/agents")}>
              <IconArrowLeft className="icon-2" />
              <T text="Back" />
            </Button>
            <Button
              variant="outline-secondary"
              href={`#/agents/${encodeURIComponent(agent)}/chat`}
            >
              <T text="Chat" />
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
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Started" /></th>
                <th><T text="Last activity" /></th>
                <th><T text="User" /></th>
                <th><T text="Description" /></th>
                <th><T text="State" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {runs?.length === 0 && (
                <tr>
                  <td colSpan={6} className="text-muted">
                    <T text="This agent has not run yet." />
                  </td>
                </tr>
              )}
              {runs?.map((run) => (
                <tr key={run.id}>
                  <td className="small text-nowrap">{when(run.created_at)}</td>
                  <td className="small text-nowrap">{when(run.updated_at)}</td>
                  <td className="small">{runUserLabel(run, users)}</td>
                  <td className="text-truncate" style={{ maxWidth: "28rem" }}>
                    {run.description || "—"}
                    {run.parent_run && (
                      <span className="text-muted small ms-2"><T text="(delegated)" /></span>
                    )}
                  </td>
                  <td>
                    <StatusBadge tone={runTone(run.state)} title={run.error ?? undefined}>
                      {run.state}
                    </StatusBadge>
                  </td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap">
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        href={`#/runs/${encodeURIComponent(run.id)}`}
                      >
                        <T text="Show" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        onClick={() => void remove(run)}
                      >
                        <T text="Delete" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>
      </PageBody>
    </>
  );
}
