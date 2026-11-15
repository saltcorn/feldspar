// The visual workflow editor (§10.3, phase 6.3): a trigger's body drawn as a
// graph, edited on a canvas, and saved as a **new version**.
//
// The screen holds one source of truth — the `Workflow` — and derives the canvas
// from it with `stepsToGraph`. Every canvas gesture that means something goes
// back through `graphToSteps`, so "what the edges say" and "what the program is"
// cannot drift: dragging an edge onto a step *is* setting that step's `next`,
// and there is one implementation of what that means (`workflowGraph.ts`), not
// one here and another in the inspector.
//
// Saving mints a version rather than rewriting one (decision 2), which is what
// lets a run suspended on version 1 finish on version 1 while an admin edits
// version 4. The history is listed beside the canvas, and reverting mints a new
// version whose steps are an old one's — because rewriting history is exactly
// what append-only says no to.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import type { GetWorkflowResponse, ListActionsResponse } from "../client";
import { IconArrowLeft, IconPlus } from "../icons";
import { AlertBody, PageBody, PageHeader } from "../layout";
import { useRoles } from "../roles";
import {
  KIND_INFO,
  STEP_KINDS,
  graphToSteps,
  layout,
  makeEdge,
  newStep,
  referencesTo,
  replaceStep,
  stepsToGraph,
  uniqueStepName,
  validate,
  type FlowEdge,
  type Graph,
  type Issue,
  type StepKindName,
  type Workflow,
} from "../workflowGraph";
import { WorkflowCanvas, type Positions } from "./WorkflowCanvas";
import { StepInspector } from "./WorkflowInspector";
import { T, useT } from "../i18n";

type ActionInfo = ListActionsResponse[number];

/** How deep the undo stack goes. Deep enough that an afternoon's editing is
 * recoverable, bounded because each entry is a whole workflow. */
const HISTORY = 50;

export function WorkflowEditor({ triggerId }: { triggerId: string }) {
  const { t } = useT();
  const roles = useRoles();
  const [meta, setMeta] = useState<GetWorkflowResponse | null>(null);
  const [trigger, setTrigger] = useState<{ when: string; channel: string | null } | null>(null);
  const [actions, setActions] = useState<ActionInfo[]>([]);
  const [workflow, setWorkflow] = useState<Workflow | null>(null);
  const [positions, setPositions] = useState<Positions>({});
  const [selected, setSelected] = useState<string | null>(null);
  const [past, setPast] = useState<Workflow[]>([]);
  const [future, setFuture] = useState<Workflow[]>([]);
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  // Set once, when a workflow first arrives: the auto-layout that gives an
  // unplaced graph somewhere to be. Not on every load, or a re-read after a save
  // would throw away the positions the admin dragged nodes to.
  const placed = useRef(false);

  /** Take the workflow the server has, and lay it out. */
  const adopt = useCallback((next: GetWorkflowResponse) => {
    setMeta(next);
    const program = next.workflow as Workflow;
    setWorkflow(program);
    setPast([]);
    setFuture([]);
    setDirty(false);
    if (!placed.current) {
      placed.current = true;
      setPositions(positionsOf(layout(stepsToGraph(program))));
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [found, triggers] = await Promise.all([
          api.getWorkflow(triggerId),
          api.listTriggers(),
        ]);
        if (cancelled) return;
        const record = triggers.find((t) => t.id === triggerId);
        setTrigger({ when: record?.when ?? "none", channel: record?.channel ?? null });
        adopt(found);
      } catch (err) {
        if (!cancelled) setLoadError(errorMessage(err, "Could not load this workflow."));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [triggerId, adopt]);

  // The action declarations for the palette and the step inspector, read for the
  // table this trigger fires on — an action may declare different settings for
  // different tables, and `workflow_step` says which of them can be a step here
  // at all (phase 5.4). An action whose settings could not be filled in is not
  // offered, rather than offered and then refused on save.
  const channel = trigger?.channel ?? undefined;
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const list = await api.listActions(channel ? { table: channel } : undefined);
        if (!cancelled) setActions(list.filter((a) => a.workflow_step));
      } catch {
        if (!cancelled) setActions([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [channel]);

  /** Record an edit: undoable, and marking the workflow unsaved. */
  const edit = useCallback((next: Workflow) => {
    setWorkflow((current) => {
      if (!current) return current;
      setPast((stack) => [...stack, current].slice(-HISTORY));
      setFuture([]);
      setDirty(true);
      return next;
    });
    setSaved(null);
  }, []);

  const undo = () => {
    setPast((stack) => {
      const previous = stack[stack.length - 1];
      if (!previous || !workflow) return stack;
      setFuture((f) => [workflow, ...f]);
      setWorkflow(previous);
      setDirty(true);
      return stack.slice(0, -1);
    });
  };

  const redo = () => {
    setFuture((stack) => {
      const next = stack[0];
      if (!next || !workflow) return stack;
      setPast((p) => [...p, workflow].slice(-HISTORY));
      setWorkflow(next);
      setDirty(true);
      return stack.slice(1);
    });
  };

  const graph: Graph = useMemo(
    () => (workflow ? stepsToGraph(workflow) : { nodes: [], edges: [] }),
    [workflow],
  );

  const issues: Issue[] = useMemo(
    () => (workflow ? validate(workflow, actions.map((a) => a.name)) : []),
    [workflow, actions],
  );
  // Server-side issues too, until the workflow is edited: they are about the
  // half the browser cannot check (a formula that does not resolve, an action's
  // configuration), and they stop applying the moment the program changes.
  const shown = dirty ? issues : [...issues, ...(meta?.issues ?? [])];
  const byStep = useMemo(() => {
    const marks: Record<string, string> = {};
    for (const issue of shown) if (issue.step) marks[issue.step] = issue.problem;
    return marks;
  }, [shown]);

  const addStep = (kind: StepKindName) => {
    if (!workflow) return;
    const name = uniqueStepName(kind, workflow.steps.map((s) => s.name));
    const step = newStep(kind, name);
    // Below whatever is selected, so a step added while looking at one lands
    // where the admin is looking rather than on top of the graph's origin.
    const anchor = selected ? positions[selected] : undefined;
    setPositions((p) => ({
      ...p,
      [name]: anchor
        ? { x: anchor.x, y: anchor.y + 140 }
        : { x: 40, y: 40 + workflow.steps.length * 24 },
    }));
    edit({
      ...workflow,
      steps: [...workflow.steps, step],
      // The very first step of an empty workflow is where a run starts.
      start: workflow.steps.length === 0 ? name : workflow.start,
    });
    setSelected(name);
  };

  /** Delete a step — refused, with their names, when others point at it.
   *
   * Silently repointing the edges that reach it would be inventing a program the
   * admin did not write, and leaving them dangling would save a workflow that
   * refers to a step that is not there. So the third option: say who, and let
   * them decide. */
  const deleteStep = (name: string) => {
    if (!workflow) return;
    const pointing = referencesTo(workflow, name);
    if (pointing.length > 0) {
      setError(
        `\`${name}\` cannot be deleted: ${pointing.map((s) => `\`${s}\``).join(", ")} ` +
          `${pointing.length === 1 ? "points" : "point"} at it. Repoint ${
            pointing.length === 1 ? "it" : "them"
          } first.`,
      );
      return;
    }
    setError(null);
    const steps = workflow.steps.filter((s) => s.name !== name);
    edit({
      ...workflow,
      steps,
      start: workflow.start === name ? (steps[0]?.name ?? "") : workflow.start,
    });
    if (selected === name) setSelected(null);
  };

  /** An edge drawn from one step to another.
   *
   * The rule is stated rather than guessed at: a step with nowhere to go gets a
   * plain `next`; a step that already goes somewhere becomes a **branch** whose
   * first arm is the new edge and whose `otherwise` is where it used to go. That
   * keeps the program the admin already had reachable, and the empty guard is
   * what validation then asks them to fill in.
   */
  const connect = (source: string, target: string, handle: string | null) => {
    if (!workflow) return;
    const outgoing = graph.edges.filter((e) => e.source === source);

    if (handle === "body") {
      const kept = graph.edges.filter((e) => !(e.source === source && e.data.role === "body"));
      return apply([...kept, makeEdge(source, target, "each", false, { role: "body" })]);
    }
    if (outgoing.some((e) => e.data.role === "formula")) {
      setError(
        `\`${source}\` decides what is next with a formula. Change it in the inspector, ` +
          "or the edge would be drawn and never taken.",
      );
      return;
    }
    setError(null);
    const plain = outgoing.find((e) => e.data.role === "next");
    if (plain) {
      const rest = graph.edges.filter((e) => e.id !== plain.id);
      return apply([
        ...rest,
        makeEdge(source, target, "", false, { role: "arm", index: 0, when: "" }),
        makeEdge(source, plain.target, "otherwise", false, { role: "otherwise" }),
      ]);
    }
    const arms = outgoing.filter((e) => e.data.role === "arm");
    if (arms.length > 0) {
      const index = Math.max(...arms.map((e) => (e.data.role === "arm" ? e.data.index : 0))) + 1;
      return apply([
        ...graph.edges,
        makeEdge(source, target, "", false, { role: "arm", index, when: "" }),
      ]);
    }
    const otherwise = outgoing.find((e) => e.data.role === "otherwise");
    if (otherwise) {
      return apply([
        ...graph.edges,
        makeEdge(source, target, "", false, { role: "arm", index: 0, when: "" }),
      ]);
    }
    return apply([...graph.edges, makeEdge(source, target, "", false, { role: "next" })]);
  };

  /** Feed an edited edge set back through the model, which is what decides what
   * the program now is. */
  const apply = (edges: FlowEdge[]) => {
    if (!workflow) return;
    edit(graphToSteps({ nodes: graph.nodes, edges }, workflow));
  };

  const save = async () => {
    if (!workflow) return;
    if (issues.length > 0) {
      setError("Fix what is marked below before saving.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const next = await api.saveWorkflow(triggerId, { workflow, description: null });
      adopt(next);
      setSaved(`Saved as version ${next.version}.`);
    } catch (err) {
      // The server's own refusal — a formula that does not resolve, an action's
      // configuration — is the message that says what to fix.
      setError(errorMessage(err, "Could not save this workflow."));
    } finally {
      setBusy(false);
    }
  };

  const revert = async (version: number) => {
    if (
      !window.confirm(
        t("Restore version {version}? It is saved as a new version.", {
          version,
        }),
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const next = await api.revertWorkflow(triggerId, { version, description: null });
      placed.current = false;
      adopt(next);
      setSaved(`Version ${version} restored as version ${next.version}.`);
    } catch (err) {
      setError(errorMessage(err, "Could not restore that version."));
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
  if (!workflow || !meta) {
    return (
      <PageBody>
        <div className="py-5 text-center">
          <Spinner animation="border" role="status" />
        </div>
      </PageBody>
    );
  }

  const step = workflow.steps.find((s) => s.name === selected) ?? null;

  return (
    <>
      <PageHeader
        pretitle={`Workflow · version ${meta.version}${dirty ? " · unsaved" : ""}`}
        title={meta.name}
        actions={
          <>
            <Button variant="outline-secondary" onClick={() => navigate("/triggers")}>
              <IconArrowLeft className="icon-2" />
              <T text="Back" />
            </Button>
            <Button
              variant="outline-secondary"
              href={`#/triggers/${encodeURIComponent(triggerId)}/runs`}
            >
              <T text="Runs" />
            </Button>
            <Button variant="outline-secondary" disabled={past.length === 0} onClick={undo}>
              <T text="Undo" />
            </Button>
            <Button variant="outline-secondary" disabled={future.length === 0} onClick={redo}>
              <T text="Redo" />
            </Button>
            <Button
              variant="outline-secondary"
              onClick={() => setPositions(positionsOf(layout(graph)))}
            >
              <T text="Tidy up" />
            </Button>
            <Button disabled={busy || !dirty} onClick={() => void save()}>
              {busy ? "Saving…" : "Save a new version"}
            </Button>
          </>
        }
      />
      <PageBody>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        {saved && (
          <Alert variant="success" dismissible onClose={() => setSaved(null)}>
            {saved}
          </Alert>
        )}
        {shown.length > 0 && (
          <Alert variant="warning">
            <AlertBody>
              <div className="mb-1">
                <T text="This workflow will not run until these are fixed:" />
              </div>
              <ul className="mb-0">
                {shown.map((issue, index) => (
                  <li key={index}>{issue.problem}</li>
                ))}
              </ul>
            </AlertBody>
          </Alert>
        )}

        <div className="row g-3">
          <div className="col-12 col-xl-8">
            <Card>
              <Card.Header className="d-flex flex-wrap gap-2 align-items-center">
                <span className="me-2"><T text="Add a step:" /></span>
                {STEP_KINDS.map((kind) => (
                  <Button
                    key={kind}
                    size="sm"
                    variant="outline-secondary"
                    title={KIND_INFO[kind].hint}
                    onClick={() => addStep(kind)}
                  >
                    <IconPlus className="icon-2" />
                    {KIND_INFO[kind].label}
                  </Button>
                ))}
              </Card.Header>
              <Card.Body className="p-0">
                <WorkflowCanvas
                  graph={graph}
                  positions={positions}
                  selected={selected}
                  issues={byStep}
                  onSelect={setSelected}
                  onMove={(id, at) => setPositions((p) => ({ ...p, [id]: at }))}
                  onConnect={connect}
                  onDeleteEdges={(ids) =>
                    apply(graph.edges.filter((e) => !ids.includes(e.id)))
                  }
                />
              </Card.Body>
              <Card.Footer className="text-muted small">
                <T text="Drag from a step's bottom onto another to set what runs next; a loop's body hangs off its right side. Select an edge and press Delete to remove it. Delete steps from the inspector." />
              </Card.Footer>
            </Card>
          </div>

          <div className="col-12 col-xl-4">
            {step ? (
              <StepInspector
                step={step}
                workflow={workflow}
                actions={actions}
                roles={roles}
                channel={channel}
                event={trigger?.when ?? "none"}
                onChange={(next) => {
                  edit(replaceStep(workflow, step.name, next));
                  if (next.name !== step.name) setSelected(next.name);
                }}
                onDelete={() => deleteStep(step.name)}
                onMakeStart={() => edit({ ...workflow, start: step.name })}
              />
            ) : (
              <WorkflowSettings workflow={workflow} onChange={edit} />
            )}
            <VersionHistory meta={meta} busy={busy} onRevert={(v) => void revert(v)} />
          </div>
        </div>
      </PageBody>
    </>
  );
}

/** The settings that are the workflow's rather than any step's, shown when
 * nothing is selected — which is where an admin looks for "about this whole
 * thing". */
function WorkflowSettings({
  workflow,
  onChange,
}: {
  workflow: Workflow;
  onChange: (workflow: Workflow) => void;
}) {
  const { t } = useT();
  const policy = workflow.error_policy ?? { type: "fail" as const };
  return (
    <Card className="mb-3">
      <Card.Header><T text="This workflow" /></Card.Header>
      <Card.Body>
        <Form.Group className="mb-3" controlId="workflowStart">
          <Form.Label><T text="Starts at" /></Form.Label>
          <Form.Select
            value={workflow.start}
            onChange={(e) => onChange({ ...workflow, start: e.target.value })}
          >
            <option value="">—</option>
            {workflow.steps.map((s) => (
              <option key={s.name} value={s.name}>
                {s.name}
              </option>
            ))}
          </Form.Select>
        </Form.Group>
        <Form.Group className="mb-3" controlId="workflowPolicy">
          <Form.Label><T text="When a step fails and says nothing itself" /></Form.Label>
          <Form.Select
            value={policy.type}
            onChange={(e) =>
              onChange({
                ...workflow,
                error_policy:
                  e.target.value === "handler"
                    ? { type: "handler", step: workflow.steps[0]?.name ?? "" }
                    : e.target.value === "retry"
                      ? { type: "retry", max: 3 }
                      : { type: "fail" },
              })
            }
          >
            <option value="fail"><T text="Fail the run" /></option>
            <option value="retry"><T text="Retry, then fail" /></option>
            <option value="handler"><T text="Jump to a step" /></option>
          </Form.Select>
        </Form.Group>
        {policy.type === "handler" && (
          <Form.Group className="mb-3" controlId="workflowHandler">
            <Form.Label><T text="Handled by" /></Form.Label>
            <Form.Select
              value={policy.step}
              onChange={(e) =>
                onChange({ ...workflow, error_policy: { type: "handler", step: e.target.value } })
              }
            >
              <option value="">—</option>
              {workflow.steps.map((s) => (
                <option key={s.name} value={s.name}>
                  {s.name}
                </option>
              ))}
            </Form.Select>
          </Form.Group>
        )}
        <Form.Group className="mb-3" controlId="workflowMaxSteps">
          <Form.Label><T text="Step budget" /></Form.Label>
          <Form.Control
            type="number"
            min={1}
            value={workflow.max_steps ?? 1000}
            onChange={(e) => onChange({ ...workflow, max_steps: Number(e.target.value) })}
          />
          <Form.Text muted>
            <T text="Maximum steps per run before the engine stops it, so a loop with no exit stops with a reason." />
          </Form.Text>
        </Form.Group>
        <Form.Check
          type="checkbox"
          id="workflowTrace"
          label={t("Record a trace")}
          checked={workflow.trace ?? false}
          onChange={(e) => onChange({ ...workflow, trace: e.target.checked })}
        />
        <Form.Text muted>
          <T text="Records the context after every step, for the run timeline. Off by default: it copies the whole context per step." />
        </Form.Text>
      </Card.Body>
    </Card>
  );
}

/** The version history: what was saved, when, and by whom — and the way back to
 * one, which mints a new version rather than rewriting the current one. */
function VersionHistory({
  meta,
  busy,
  onRevert,
}: {
  meta: GetWorkflowResponse;
  busy: boolean;
  onRevert: (version: number) => void;
}) {
  const { t } = useT();
  return (
    <Card>
      <Card.Header><T text="Versions" /></Card.Header>
      <div className="list-group list-group-flush">
        {meta.versions.map((version) => (
          <div
            key={version.version}
            className="list-group-item d-flex align-items-center justify-content-between"
          >
            <div>
              <strong>{t("Version {version}", { version: version.version })}</strong>
              {version.version === meta.version && (
                <span className="badge bg-blue-lt ms-2"><T text="current" /></span>
              )}
              <div className="text-muted small">
                {new Date(version.created_at).toLocaleString()}
                {version.description && ` · ${version.description}`}
              </div>
            </div>
            {version.version !== meta.version && (
              <Button
                size="sm"
                variant="outline-secondary"
                disabled={busy}
                onClick={() => onRevert(version.version)}
              >
                <T text="Restore" />
              </Button>
            )}
          </div>
        ))}
      </div>
    </Card>
  );
}

/** The positions of a laid-out graph, by node id. */
function positionsOf(graph: Graph): Positions {
  const out: Positions = {};
  for (const node of graph.nodes) out[node.id] = node.position;
  return out;
}
