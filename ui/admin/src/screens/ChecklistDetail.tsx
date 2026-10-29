// One checklist (TODO-checklists C1.16, CHECKLISTS.md §5): its settings, its
// steps, what an agent is waiting on a person for, and its recent runs.
//
// The pending queue comes first when there is one, because it is the part
// only a person can do (§4): an agent may append steps and propose changes,
// but an unaccepted step is an agent's draft and a proposal changes nothing
// until somebody accepts it. Each proposal is shown beside the newest run's
// result for the step it is about — the failure the agent says it fixes.
//
// A step's form is drawn from its type's `params` spec (`checklists.ts`), so a
// module's or a framework's step type gets a form without anything here
// knowing about it.

import { useEffect, useMemo, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Row from "react-bootstrap/Row";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import {
  attributesWith,
  authorLabel,
  buildParams,
  checklistRunPath,
  checklistsPath,
  controlKind,
  initialValues,
  lastResultFor,
  moveStep,
  orderedSteps,
  outcomeTone,
  pendingSteps,
  proposalSummary,
  proposedStep,
  selectOptions,
  settingsFrom,
  type BuiltParams,
  type CheckUser,
  type ChecklistDetail as Detail,
  type ChecklistProposal,
  type ChecklistStep,
  type FormValues,
  type ParamSpec,
  type ProposalSummary,
  type StepType,
} from "../checklists";
import { IconArrowDown, IconArrowLeft, IconArrowUp, IconPlus } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { RunStatus } from "./Checklists";
import { when } from "./WorkflowRuns";
import { T, useT } from "../i18n";

/** A step as the form saves it. */
type StepDraft = { type: string; params: unknown; label: string; as: string | null };

export function ChecklistDetail({
  checklistId,
  appId,
}: {
  checklistId: string;
  appId: string | null;
}) {
  const { t } = useT();
  const [detail, setDetail] = useState<Detail | null>(null);
  const [users, setUsers] = useState<CheckUser[]>([]);
  const [types, setTypes] = useState<StepType[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // `null` closed, `"new"` adding, a step editing it.
  const [editing, setEditing] = useState<ChecklistStep | "new" | null>(null);

  const load = async () => {
    try {
      setDetail(await api.getChecklist(checklistId));
    } catch (err) {
      setLoadError(errorMessage(err, t("Could not load the checklist.")));
    }
  };

  useEffect(() => {
    void load();
    api
      .listChecklistUsers()
      .then(setUsers)
      .catch(() => setUsers([]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [checklistId]);

  // The step types on offer depend on whether there is an application to
  // drive: a data-only checklist is offered only the types that need none.
  const application = detail?.application ?? null;
  useEffect(() => {
    if (!detail) return;
    api
      .listChecklistStepTypes(application ? { application } : {})
      .then(setTypes)
      .catch(() => setTypes([]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [application, detail === null]);

  /** Do one thing, then show the checklist as it now is. An action that
   * answers with the checklist (accept, reject, reorder) saves a reload. */
  const act = async (fallback: string, action: () => Promise<Detail | unknown>) => {
    setBusy(true);
    setError(null);
    try {
      const result = await action();
      if (result && typeof result === "object" && "steps" in result && "runs" in result) {
        setDetail(result as Detail);
      } else {
        await load();
      }
    } catch (err) {
      setError(errorMessage(err, fallback));
    } finally {
      setBusy(false);
    }
  };

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api.runChecklist(checklistId);
      navigate(checklistRunPath(result.id, appId));
    } catch (err) {
      setError(errorMessage(err, t("Could not run the checklist.")));
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!detail) return;
    if (!window.confirm(t("Delete the checklist {name}?", { name: detail.name }))) return;
    setBusy(true);
    try {
      await api.deleteChecklist(checklistId);
      navigate(checklistsPath(appId));
    } catch (err) {
      setError(errorMessage(err, t("Could not delete the checklist.")));
      setBusy(false);
    }
  };

  const steps = useMemo(() => orderedSteps(detail?.steps ?? []), [detail]);
  const unaccepted = useMemo(() => pendingSteps(steps), [steps]);

  const move = (index: number, delta: number) => {
    const order = moveStep(
      steps.map((s) => s.id),
      index,
      delta,
    );
    if (!order) return;
    void act(t("Could not reorder the steps."), () =>
      api.reorderChecklistSteps(checklistId, { order }),
    );
  };

  const removeStep = (step: ChecklistStep) => {
    if (!window.confirm(t("Remove the step “{label}”?", { label: step.label || step.type }))) {
      return;
    }
    void act(t("Could not remove the step."), () => api.deleteChecklistStep(step.id));
  };

  const saveStep = async (draft: StepDraft) => {
    const body = {
      type: draft.type,
      params: draft.params,
      label: draft.label,
      as: draft.as,
    };
    if (editing && editing !== "new") {
      await api.updateChecklistStep(editing.id, body);
    } else {
      await api.addChecklistStep(checklistId, body);
    }
    setEditing(null);
    await load();
  };

  const back = (
    <Button variant="outline-secondary" onClick={() => navigate(checklistsPath(appId))}>
      <IconArrowLeft className="icon-2" />
      <T text="Back" />
    </Button>
  );

  if (loadError) {
    return (
      <>
        <PageHeader pretitle={t("Checklist")} title={t("Checklist")} actions={back} />
        <PageBody>
          <Alert variant="danger">{loadError}</Alert>
        </PageBody>
      </>
    );
  }

  return (
    <>
      <PageHeader
        pretitle={t("Checklist")}
        title={detail?.name ?? ""}
        actions={
          <>
            {back}
            <Button disabled={busy || !detail} onClick={() => void run()}>
              <T text="Run" />
            </Button>
            <Button variant="outline-danger" disabled={busy || !detail} onClick={() => void remove()}>
              <T text="Delete" />
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
        {detail && (
          <>
            {(detail.proposals.length > 0 || unaccepted.length > 0) && (
              <PendingQueue
                detail={detail}
                steps={steps}
                unaccepted={unaccepted}
                users={users}
                busy={busy}
                onAcceptProposal={(p) =>
                  void act(t("Could not accept the proposal."), () =>
                    api.acceptChecklistProposal(p.id),
                  )
                }
                onRejectProposal={(p) =>
                  void act(t("Could not reject the proposal."), () =>
                    api.rejectChecklistProposal(p.id),
                  )
                }
                onAcceptStep={(s) =>
                  void act(t("Could not accept the step."), () => api.acceptChecklistStep(s.id))
                }
                onRejectStep={removeStep}
              />
            )}

            <Card className="mb-3">
              <Card.Header>
                <h3 className="card-title"><T text="Steps" /></h3>
                <div className="card-actions">
                  <Button size="sm" disabled={busy} onClick={() => setEditing("new")}>
                    <IconPlus className="icon-2" />
                    <T text="Add step" />
                  </Button>
                </div>
              </Card.Header>
              <Table responsive className="card-table table-vcenter">
                <thead>
                  <tr>
                    <th className="w-1">#</th>
                    <th><T text="Label" /></th>
                    <th><T text="Type" /></th>
                    <th><T text="Captures" /></th>
                    <th><T text="Author" /></th>
                    <th><T text="Accepted" /></th>
                    <th className="w-1" />
                  </tr>
                </thead>
                <tbody>
                  {steps.length === 0 && (
                    <tr>
                      <td colSpan={7} className="text-muted">
                        <T text="No steps yet." />
                      </td>
                    </tr>
                  )}
                  {steps.map((step, index) => (
                    <tr key={step.id}>
                      <td className="text-secondary">{index + 1}</td>
                      <td>{step.label || "—"}</td>
                      <td>
                        <code>{step.type}</code>
                      </td>
                      <td>{step.as ? <code>{step.as}</code> : "—"}</td>
                      <td className="small">
                        <AuthorName author={step.author} users={users} />
                      </td>
                      <td>
                        {step.accepted ? (
                          <StatusBadge tone="green"><T text="Accepted" /></StatusBadge>
                        ) : (
                          <StatusBadge tone="yellow"><T text="Not accepted" /></StatusBadge>
                        )}
                      </td>
                      <td>
                        <div className="btn-list justify-content-end flex-nowrap">
                          <Button
                            size="sm"
                            variant="outline-secondary"
                            className="btn-icon"
                            aria-label={t("Move up")}
                            title={t("Move up")}
                            disabled={busy || index === 0}
                            onClick={() => move(index, -1)}
                          >
                            <IconArrowUp className="icon-2" />
                          </Button>
                          <Button
                            size="sm"
                            variant="outline-secondary"
                            className="btn-icon"
                            aria-label={t("Move down")}
                            title={t("Move down")}
                            disabled={busy || index === steps.length - 1}
                            onClick={() => move(index, 1)}
                          >
                            <IconArrowDown className="icon-2" />
                          </Button>
                          <Button
                            size="sm"
                            variant="outline-secondary"
                            disabled={busy}
                            onClick={() => setEditing(step)}
                          >
                            <T text="Edit" />
                          </Button>
                          <Button
                            size="sm"
                            variant="outline-danger"
                            disabled={busy}
                            onClick={() => removeStep(step)}
                          >
                            <T text="Remove" />
                          </Button>
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            </Card>

            <SettingsCard
              detail={detail}
              users={users}
              onSaved={(next) => setDetail(next)}
            />

            <RecentRuns detail={detail} appId={appId} />

            <StepDialog
              editing={editing}
              types={types}
              onClose={() => setEditing(null)}
              onSave={saveStep}
            />
          </>
        )}
      </PageBody>
    </>
  );
}

/** Who wrote a step or a proposal: a person by email, an agent by its run. */
function AuthorName({ author, users }: { author: string; users: CheckUser[] }) {
  const { t } = useT();
  const who = authorLabel(author, users);
  if (who.kind === "agent") {
    return (
      <StatusBadge tone="blue" title={who.id}>
        {t("Agent run {run}", { run: who.label })}
      </StatusBadge>
    );
  }
  return <>{who.label}</>;
}

/** A step's type, label and params, for a proposal's before and after. */
function StepSummary({
  step,
}: {
  step: { type: string; params: unknown; label?: string; as?: string | null } | null;
}) {
  if (!step) return <span className="text-secondary">—</span>;
  return (
    <div>
      <div className="fw-bold">{step.label || "—"}</div>
      <div>
        <code>{step.type}</code>
        {step.as && (
          <>
            {" → "}
            <code>{step.as}</code>
          </>
        )}
      </div>
      <pre className="small mb-0 mt-1">{JSON.stringify(step.params ?? {}, null, 2)}</pre>
    </div>
  );
}

/** What an agent is waiting on a person for (§4): its proposals, and the steps
 * it wrote that nobody has accepted yet. */
function PendingQueue({
  detail,
  steps,
  unaccepted,
  users,
  busy,
  onAcceptProposal,
  onRejectProposal,
  onAcceptStep,
  onRejectStep,
}: {
  detail: Detail;
  steps: ChecklistStep[];
  unaccepted: ChecklistStep[];
  users: CheckUser[];
  busy: boolean;
  onAcceptProposal: (p: ChecklistProposal) => void;
  onRejectProposal: (p: ChecklistProposal) => void;
  onAcceptStep: (s: ChecklistStep) => void;
  onRejectStep: (s: ChecklistStep) => void;
}) {
  const { t } = useT();
  const describe = (summary: ProposalSummary): string => {
    switch (summary.kind) {
      case "edit":
        return t("Change step {step}", { step: summary.step ?? "?" });
      case "insert_before":
        return t("Insert a step before step {step}", { step: summary.step ?? "?" });
      case "reorder":
        return t("Reorder the steps");
      case "run_as":
        return t("Run as {user}", { user: summary.user ?? t("an anonymous visitor") });
      default:
        return summary.name;
    }
  };
  return (
    <Card className="mb-3 border-warning">
      <Card.Header>
        <h3 className="card-title"><T text="Waiting for review" /></h3>
      </Card.Header>
      <Card.Body>
        <p className="text-secondary">
          <T text="An agent may add steps and propose changes, but only a person can accept them. Until then a proposal changes nothing, and the checklist keeps running the steps it has." />
        </p>
        {detail.proposals.map((proposal) => {
          const summary = proposalSummary(proposal, steps, users);
          const current = steps.find((s) => s.id === proposal.step) ?? null;
          const proposed = proposedStep(proposal);
          const result = lastResultFor(proposal.step, detail.runs);
          const change = (proposal.change ?? {}) as { order?: unknown };
          const order = Array.isArray(change.order) ? (change.order as string[]) : null;
          return (
            <div key={proposal.id} className="border rounded p-3 mb-3">
              <div className="d-flex align-items-center gap-2 mb-2 flex-wrap">
                <strong>{describe(summary)}</strong>
                <span className="small text-secondary">
                  <AuthorName author={proposal.author} users={users} /> · {when(proposal.created_at)}
                </span>
              </div>
              {proposal.reason && <p className="mb-2">{proposal.reason}</p>}
              {result && (
                <div className="mb-2 small">
                  <T text="Last run:" />{" "}
                  <StatusBadge tone={outcomeTone(result.outcome)}>{result.outcome}</StatusBadge>{" "}
                  {result.message}
                </div>
              )}
              {proposed && (
                <Row className="g-3 mb-2">
                  <Col md={6}>
                    <div className="small text-secondary mb-1">
                      {proposal.kind === "edit" ? t("Now") : t("Before this step")}
                    </div>
                    <StepSummary step={current} />
                  </Col>
                  <Col md={6}>
                    <div className="small text-secondary mb-1"><T text="Proposed" /></div>
                    <StepSummary step={proposed} />
                  </Col>
                </Row>
              )}
              {order && (
                <ol className="mb-2">
                  {order.map((id) => {
                    const s = steps.find((x) => x.id === id);
                    return <li key={id}>{s ? s.label || s.type : id}</li>;
                  })}
                </ol>
              )}
              <div className="btn-list">
                <Button size="sm" disabled={busy} onClick={() => onAcceptProposal(proposal)}>
                  <T text="Accept" />
                </Button>
                <Button
                  size="sm"
                  variant="outline-danger"
                  disabled={busy}
                  onClick={() => onRejectProposal(proposal)}
                >
                  <T text="Reject" />
                </Button>
              </div>
            </div>
          );
        })}
        {unaccepted.map((step) => (
          <div key={step.id} className="border rounded p-3 mb-3">
            <div className="d-flex align-items-center gap-2 mb-2 flex-wrap">
              <strong>
                {t("New step {step}", { step: steps.findIndex((s) => s.id === step.id) + 1 })}
              </strong>
              <span className="small text-secondary">
                <AuthorName author={step.author} users={users} /> · {when(step.created_at)}
              </span>
            </div>
            <StepSummary step={step} />
            <div className="btn-list mt-2">
              <Button size="sm" disabled={busy} onClick={() => onAcceptStep(step)}>
                <T text="Accept" />
              </Button>
              <Button
                size="sm"
                variant="outline-danger"
                disabled={busy}
                onClick={() => onRejectStep(step)}
              >
                <T text="Reject" />
              </Button>
            </div>
          </div>
        ))}
      </Card.Body>
    </Card>
  );
}

/** The checklist's own settings: name, who it runs as, and the sparse ones in
 * `attributes`. */
function SettingsCard({
  detail,
  users,
  onSaved,
}: {
  detail: Detail;
  users: CheckUser[];
  onSaved: (next: Detail) => void;
}) {
  const { t } = useT();
  const initial = () => ({
    name: detail.name,
    description: detail.description,
    runAs: detail.run_as ?? "",
    enabled: detail.enabled,
    ...settingsFrom(detail.attributes),
  });
  const [form, setForm] = useState(initial);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setForm(initial());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [detail]);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const attributes = attributesWith(detail.attributes, form);
    if (!attributes) {
      setError(t("The timeout should be a positive number of seconds."));
      return;
    }
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      const next = await api.updateChecklist(detail.id, {
        name: form.name.trim(),
        description: form.description,
        // Sent as it is: the update is the whole checklist, and leaving the
        // application out would make this a data-only checklist.
        application: detail.application ?? null,
        run_as: form.runAs || null,
        enabled: form.enabled,
        attributes,
      });
      onSaved(next);
      setSaved(true);
    } catch (err) {
      setError(errorMessage(err, t("Could not save the checklist.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card className="mb-3">
      <Card.Header>
        <h3 className="card-title"><T text="Settings" /></h3>
      </Card.Header>
      <Form onSubmit={(e) => void submit(e)}>
        <Card.Body>
          {error && <Alert variant="danger">{error}</Alert>}
          {saved && (
            <Alert variant="success" dismissible onClose={() => setSaved(false)}>
              <T text="Saved." />
            </Alert>
          )}
          <Row className="g-3">
            <Col md={6}>
              <Form.Group controlId="checklistName">
                <Form.Label><T text="Name" /></Form.Label>
                <Form.Control
                  value={form.name}
                  required
                  onChange={(e) => setForm({ ...form, name: e.target.value })}
                />
              </Form.Group>
            </Col>
            <Col md={6}>
              <Form.Group controlId="checklistRunAs">
                <Form.Label><T text="Run as" /></Form.Label>
                <Form.Select
                  value={form.runAs}
                  onChange={(e) => setForm({ ...form, runAs: e.target.value })}
                >
                  <option value="">{t("Anonymous visitor")}</option>
                  {users.map((u) => (
                    <option key={u.id} value={u.id}>
                      {u.check_user ? t("{email} (check user)", { email: u.email }) : u.email}
                    </option>
                  ))}
                  {/* A user the list no longer has (deleted, or not visible
                      here) is still who it runs as until somebody changes it. */}
                  {form.runAs && !users.some((u) => u.id === form.runAs) && (
                    <option value={form.runAs}>{form.runAs}</option>
                  )}
                </Form.Select>
                <Form.Text muted>
                  <T text="The whole checklist runs as this user. An agent may only choose a check user." />
                </Form.Text>
              </Form.Group>
            </Col>
            <Col md={12}>
              <Form.Group controlId="checklistDescription">
                <Form.Label><T text="Description" /></Form.Label>
                <Form.Control
                  as="textarea"
                  rows={2}
                  value={form.description}
                  onChange={(e) => setForm({ ...form, description: e.target.value })}
                />
              </Form.Group>
            </Col>
            <Col md={6}>
              <Form.Group controlId="checklistTimeout">
                <Form.Label><T text="Step timeout (seconds)" /></Form.Label>
                <Form.Control
                  type="number"
                  min={1}
                  value={form.timeout}
                  placeholder={t("Default")}
                  onChange={(e) => setForm({ ...form, timeout: e.target.value })}
                />
              </Form.Group>
            </Col>
            <Col md={6}>
              <Form.Check
                id="checklistEnabled"
                className="mt-md-4"
                type="switch"
                label={t("Enabled")}
                checked={form.enabled}
                onChange={(e) => setForm({ ...form, enabled: e.target.checked })}
              />
              <Form.Check
                id="checklistStopOnFailure"
                type="switch"
                label={t("Stop at the first failing step")}
                checked={form.stopOnFailure}
                onChange={(e) => setForm({ ...form, stopOnFailure: e.target.checked })}
              />
              <Form.Check
                id="checklistImplicitClean"
                type="switch"
                label={t("Check every opened page is clean (no console errors or failed requests)")}
                checked={form.implicitClean}
                onChange={(e) => setForm({ ...form, implicitClean: e.target.checked })}
              />
            </Col>
            <Col md={12}>
              <Form.Check
                id="checklistAllowOnProduction"
                type="switch"
                label={t("Allow on production")}
                checked={form.allowOnProduction}
                onChange={(e) => setForm({ ...form, allowOnProduction: e.target.checked })}
              />
              <Form.Text muted>
                <T text="A checklist that writes data is refused on an environment marked production unless this is set. The rows it creates are removed afterwards, but they exist while it runs." />
                {detail.mutates && (
                  <>
                    {" "}
                    <strong><T text="This checklist writes data." /></strong>
                  </>
                )}
              </Form.Text>
            </Col>
          </Row>
        </Card.Body>
        <Card.Footer className="text-end">
          <Button type="submit" disabled={busy || form.name.trim() === ""}>
            <T text="Save settings" />
          </Button>
        </Card.Footer>
      </Form>
    </Card>
  );
}

/** The newest runs, each a link to its detail. */
function RecentRuns({ detail, appId }: { detail: Detail; appId: string | null }) {
  return (
    <Card className="mb-3">
      <Card.Header>
        <h3 className="card-title"><T text="Recent runs" /></h3>
      </Card.Header>
      <Table hover responsive className="card-table table-vcenter">
        <thead>
          <tr>
            <th><T text="Started" /></th>
            <th><T text="Result" /></th>
            <th><T text="Trigger" /></th>
            <th><T text="Message" /></th>
          </tr>
        </thead>
        <tbody>
          {detail.runs.length === 0 && (
            <tr>
              <td colSpan={4} className="text-muted">
                <T text="It has not run yet." />
              </td>
            </tr>
          )}
          {detail.runs.map((run) => (
            <tr key={run.id}>
              <td className="small text-nowrap">
                <a href={`#${checklistRunPath(run.id, appId)}`}>{when(run.started_at)}</a>
              </td>
              <td>
                <RunStatus status={run.status} />
              </td>
              <td className="small">{run.trigger}</td>
              <td className="small">{run.message ?? ""}</td>
            </tr>
          ))}
        </tbody>
      </Table>
    </Card>
  );
}

/** Add or edit a step: its type, then the type's own parameters. */
function StepDialog({
  editing,
  types,
  onClose,
  onSave,
}: {
  editing: ChecklistStep | "new" | null;
  types: StepType[];
  onClose: () => void;
  onSave: (draft: StepDraft) => Promise<void>;
}) {
  const { t } = useT();
  const [type, setType] = useState("");
  const [label, setLabel] = useState("");
  const [as, setAs] = useState("");
  const [values, setValues] = useState<FormValues>({});
  // The params of a step whose type this server does not offer (a module
  // since removed): kept as JSON rather than lost behind an empty form.
  const [raw, setRaw] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const stepType = types.find((x) => x.name === type) ?? null;

  useEffect(() => {
    if (editing === null) return;
    setError(null);
    if (editing === "new") {
      setType("");
      setLabel("");
      setAs("");
      setValues({});
      setRaw("{}");
    } else {
      const known = types.find((x) => x.name === editing.type);
      setType(editing.type);
      setLabel(editing.label);
      setAs(editing.as ?? "");
      setValues(known ? initialValues(known.params, editing.params) : {});
      setRaw(JSON.stringify(editing.params ?? {}, null, 2));
    }
  }, [editing, types]);

  const chooseType = (name: string) => {
    setType(name);
    const next = types.find((x) => x.name === name);
    setValues(next ? initialValues(next.params, {}) : {});
  };

  const problem = (built: Extract<BuiltParams, { ok: false }>) =>
    built.problem === "required"
      ? t("{label} is required.", { label: built.label })
      : built.problem === "number"
        ? t("{label} should be a number.", { label: built.label })
        : t("{label} is not valid JSON: {problem}", {
          label: built.label,
          problem: built.detail ?? "",
        });

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    let params: unknown;
    if (stepType) {
      const built = buildParams(stepType.params, values);
      if (!built.ok) {
        setError(problem(built));
        return;
      }
      params = built.params;
    } else {
      try {
        params = JSON.parse(raw || "{}");
      } catch (err) {
        setError(
          t("{label} is not valid JSON: {problem}", {
            label: t("Parameters"),
            problem: err instanceof Error ? err.message : String(err),
          }),
        );
        return;
      }
    }
    setBusy(true);
    setError(null);
    try {
      await onSave({ type, params, label: label.trim(), as: as.trim() || null });
    } catch (err) {
      setError(errorMessage(err, t("Could not save the step.")));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal show={editing !== null} onHide={onClose} size="lg">
      <Form onSubmit={(e) => void submit(e)}>
        <Modal.Header closeButton>
          <Modal.Title>
            {editing === "new" ? t("Add step") : t("Edit step")}
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {error && <Alert variant="danger">{error}</Alert>}
          <Form.Group className="mb-3" controlId="stepType">
            <Form.Label><T text="Step type" /></Form.Label>
            <Form.Select value={type} required onChange={(e) => chooseType(e.target.value)}>
              <option value="">{t("Choose a step type")}</option>
              {types.map((x) => (
                <option key={x.name} value={x.name}>
                  {x.name}
                </option>
              ))}
              {type && !stepType && <option value={type}>{type}</option>}
            </Form.Select>
            {stepType && (
              <Form.Text muted>
                {stepType.description}
                {stepType.mutates && (
                  <>
                    {" "}
                    <strong><T text="Writes data." /></strong>
                  </>
                )}
              </Form.Text>
            )}
          </Form.Group>
          <Form.Group className="mb-3" controlId="stepLabel">
            <Form.Label><T text="Label" /></Form.Label>
            <Form.Control
              value={label}
              placeholder={t("What a person reads in the list")}
              onChange={(e) => setLabel(e.target.value)}
            />
          </Form.Group>
          <Form.Group className="mb-3" controlId="stepAs">
            <Form.Label><T text="Capture as" /></Form.Label>
            <Form.Control value={as} onChange={(e) => setAs(e.target.value)} />
            <Form.Text muted>
              <T
                text="Optional. Later steps can use the captured value as {example}."
                values={{ example: <code>{`{{ ${as.trim() || "name"} }}`}</code> }}
              />
            </Form.Text>
          </Form.Group>
          {stepType?.params.map((spec) => (
            <ParamControl
              key={spec.name}
              spec={spec}
              value={values[spec.name]}
              onChange={(v) => setValues({ ...values, [spec.name]: v })}
            />
          ))}
          {type && !stepType && (
            <Form.Group className="mb-3" controlId="stepRawParams">
              <Form.Label><T text="Parameters" /></Form.Label>
              <Form.Control
                as="textarea"
                rows={6}
                className="font-monospace"
                value={raw}
                onChange={(e) => setRaw(e.target.value)}
              />
              <Form.Text muted>
                <T text="This server does not offer this step type, so its parameters are edited as JSON." />
              </Form.Text>
            </Form.Group>
          )}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" type="button" onClick={onClose}>
            <T text="Cancel" />
          </Button>
          <Button type="submit" disabled={busy || type === ""}>
            <T text="Save step" />
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}

/** One parameter's control, chosen by its spec (`controlKind`). */
function ParamControl({
  spec,
  value,
  onChange,
}: {
  spec: ParamSpec;
  value: FormValues[string] | undefined;
  onChange: (value: FormValues[string]) => void;
}) {
  const { t } = useT();
  const kind = controlKind(spec);
  const id = `stepParam-${spec.name}`;
  const label = spec.label || spec.name;
  const text = typeof value === "string" ? value : "";
  const help = spec.sublabel ? <Form.Text muted>{spec.sublabel}</Form.Text> : null;
  if (kind === "checkbox") {
    return (
      <Form.Group className="mb-3">
        <Form.Check
          id={id}
          label={label}
          checked={value === true}
          onChange={(e) => onChange(e.target.checked)}
        />
        {help}
      </Form.Group>
    );
  }
  return (
    <Form.Group className="mb-3" controlId={id}>
      <Form.Label className={spec.required ? "required" : undefined}>{label}</Form.Label>
      {kind === "select" ? (
        <Form.Select value={text} required={spec.required} onChange={(e) => onChange(e.target.value)}>
          <option value="">{spec.required ? t("Choose…") : "—"}</option>
          {selectOptions(spec).map((o) => (
            <option key={o.value} value={o.value}>
              {o.label}
            </option>
          ))}
        </Form.Select>
      ) : kind === "number" ? (
        <Form.Control
          type="number"
          step={spec.type === "Int" ? 1 : "any"}
          value={text}
          required={spec.required}
          onChange={(e) => onChange(e.target.value)}
        />
      ) : kind === "json" ? (
        <Form.Control
          as="textarea"
          rows={4}
          className="font-monospace"
          value={text}
          required={spec.required}
          onChange={(e) => onChange(e.target.value)}
        />
      ) : kind === "textarea" ? (
        <Form.Control
          as="textarea"
          rows={3}
          value={text}
          required={spec.required}
          onChange={(e) => onChange(e.target.value)}
        />
      ) : (
        <Form.Control
          value={text}
          required={spec.required}
          onChange={(e) => onChange(e.target.value)}
        />
      )}
      {help}
    </Form.Group>
  );
}
