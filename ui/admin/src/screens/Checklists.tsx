// The Checklists tab (TODO-checklists C1.16, CHECKLISTS.md §5): one
// application's checklists, or — with no application — the data-only ones that
// belong to the Data Layer.
//
// Each row is what an admin scans for: did it pass last time, when, and is an
// agent waiting for somebody to look at a step it wrote or a change it
// proposed. Run and Run all are here because "is it still green after what I
// just did" is the question this screen is opened to answer; everything else
// about a checklist is on its own screen.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { checklistPath, checklistRunPath, runTone } from "../checklists";
import type { ListChecklistsResponse } from "../client";
import { IconPlus } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { when } from "./WorkflowRuns";
import { T, useT } from "../i18n";

type ChecklistItem = ListChecklistsResponse[number];

/** A run's status in the admin's words. Shared with the detail and run screens. */
export function RunStatus({ status, title }: { status: string; title?: string }) {
  const { t } = useT();
  const label =
    status === "passed"
      ? t("Passed")
      : status === "failed"
        ? t("Failed")
        : status === "error"
          ? t("Error")
          : status === "incomplete"
            ? t("Incomplete")
            : status;
  return (
    <StatusBadge tone={runTone(status)} title={title}>
      {label}
    </StatusBadge>
  );
}

export function Checklists({ appId }: { appId: string | null }) {
  const { t } = useT();
  const [appName, setAppName] = useState<string | null>(null);
  const [items, setItems] = useState<ChecklistItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [creating, setCreating] = useState(false);

  const load = async () => {
    try {
      setItems(await api.listChecklists({ application: appId ?? "none" }));
      setError(null);
    } catch (err) {
      setError(errorMessage(err, t("Could not load the checklists.")));
    }
  };

  useEffect(() => {
    setItems(null);
    void load();
    if (appId) {
      api
        .listApplications()
        .then((apps) => setAppName(apps.find((a) => a.id === appId)?.name ?? appId))
        .catch(() => setAppName(appId));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId]);

  /** Run one checklist and open its result: what the admin pressed Run to see. */
  const runOne = async (item: ChecklistItem) => {
    setBusy(true);
    setError(null);
    try {
      const run = await api.runChecklist(item.id);
      navigate(checklistRunPath(run.id, appId));
    } catch (err) {
      setError(errorMessage(err, t("Could not run the checklist.")));
      setBusy(false);
    }
  };

  /** Run every enabled checklist here; the rows' last results are the report. */
  const runAll = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const runs = await api.runChecklists({ application: appId ?? "none" });
      const passed = runs.filter((r) => r.status === "passed").length;
      setNotice(t("{passed} of {count} checklists passed.", { passed, count: runs.length }));
      await load();
    } catch (err) {
      setError(errorMessage(err, t("Could not run the checklists.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <PageHeader
        pretitle={appId ? (appName ?? "") : t("Data Layer")}
        title={t("Checklists")}
        actions={
          <>
            <Button
              variant="outline-primary"
              disabled={busy || !items || items.length === 0}
              onClick={() => void runAll()}
            >
              <T text="Run all" />
            </Button>
            <Button onClick={() => setCreating(true)}>
              <IconPlus className="icon-2" />
              <T text="New checklist" />
            </Button>
          </>
        }
      />
      <PageBody>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            {error}
          </Alert>
        )}
        {notice && (
          <Alert variant="info" dismissible onClose={() => setNotice(null)}>
            {notice}
          </Alert>
        )}
        {!appId && (
          <p className="text-secondary">
            <T text="Checklists with no application check data only: rows, triggers and the API. An application's checklists are under that application in the sidebar." />
          </p>
        )}

        <Card>
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Steps" /></th>
                <th><T text="Last result" /></th>
                <th><T text="Last run" /></th>
                <th><T text="Pending" /></th>
                <th><T text="Status" /></th>
                <th className="w-1" />
              </tr>
            </thead>
            <tbody>
              {items?.length === 0 && (
                <tr>
                  <td colSpan={7} className="text-muted">
                    <T text="No checklists yet." />
                  </td>
                </tr>
              )}
              {items?.map((item) => (
                <tr key={item.id}>
                  <td>
                    <a href={`#${checklistPath(item.id, appId)}`}>{item.name}</a>
                    {item.description && (
                      <div className="small text-secondary">{item.description}</div>
                    )}
                  </td>
                  <td>{item.steps}</td>
                  <td>
                    {item.last_run ? (
                      <a href={`#${checklistRunPath(item.last_run.id, appId)}`}>
                        <RunStatus status={item.last_run.status} />
                      </a>
                    ) : (
                      <span className="text-secondary"><T text="Never run" /></span>
                    )}
                  </td>
                  <td className="small text-nowrap">{when(item.last_run?.finished_at)}</td>
                  <td>
                    {item.pending > 0 ? (
                      <StatusBadge
                        tone="yellow"
                        title={t("Agent steps and proposals waiting for a person")}
                      >
                        {item.pending}
                      </StatusBadge>
                    ) : (
                      "—"
                    )}
                  </td>
                  <td>
                    <div className="d-flex gap-1 flex-wrap">
                      {item.enabled ? (
                        <StatusBadge tone="green"><T text="Enabled" /></StatusBadge>
                      ) : (
                        <StatusBadge tone="secondary"><T text="Disabled" /></StatusBadge>
                      )}
                      {item.mutates && (
                        <StatusBadge tone="blue" title={t("At least one step writes data")}>
                          <T text="Writes data" />
                        </StatusBadge>
                      )}
                    </div>
                  </td>
                  <td>
                    <div className="btn-list justify-content-end flex-nowrap">
                      <Button
                        size="sm"
                        variant="outline-primary"
                        disabled={busy}
                        onClick={() => void runOne(item)}
                      >
                        <T text="Run" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        href={`#${checklistPath(item.id, appId)}`}
                      >
                        <T text="Open" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        </Card>

        <NewChecklistDialog
          show={creating}
          appId={appId}
          onClose={() => setCreating(false)}
        />
      </PageBody>
    </>
  );
}

/** A name and a description is all a new checklist needs; its steps and its
 * user are set on its own screen, which is where this lands. */
function NewChecklistDialog({
  show,
  appId,
  onClose,
}: {
  show: boolean;
  appId: string | null;
  onClose: () => void;
}) {
  const { t } = useT();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (show) {
      setName("");
      setDescription("");
      setError(null);
    }
  }, [show]);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const created = await api.createChecklist({
        name: name.trim(),
        description: description.trim() || undefined,
        application: appId,
      });
      onClose();
      navigate(checklistPath(created.id, appId));
    } catch (err) {
      setError(errorMessage(err, t("Could not create the checklist.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal show={show} onHide={onClose}>
      <Form onSubmit={(e) => void submit(e)}>
        <Modal.Header closeButton>
          <Modal.Title><T text="New checklist" /></Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {error && <Alert variant="danger">{error}</Alert>}
          <Form.Group className="mb-3" controlId="newChecklistName">
            <Form.Label><T text="Name" /></Form.Label>
            <Form.Control
              value={name}
              required
              autoFocus
              onChange={(e) => setName(e.target.value)}
            />
          </Form.Group>
          <Form.Group className="mb-3" controlId="newChecklistDescription">
            <Form.Label><T text="Description" /></Form.Label>
            <Form.Control
              as="textarea"
              rows={2}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </Form.Group>
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" type="button" onClick={onClose}>
            <T text="Cancel" />
          </Button>
          <Button type="submit" disabled={busy || name.trim() === ""}>
            <T text="Create checklist" />
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}
