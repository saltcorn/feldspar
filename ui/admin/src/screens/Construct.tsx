// Plan & build (TODO-app-constructor AC4): an application built from a
// description. Three steps, each a numbered tab:
//
// 1. **Specification**: what the application should do, and "Plan phases",
//    which has the construction agent turn it into phases.
// 2. **Phases**: the plan, edited before anything runs, then "Run all" or
//    a phase's own "Run", which runs the phases before it first.
// 3. **Progress**: each phase's tasks as the runner works through them, with
//    their runs, summaries, and Stop / Retry / Skip.
//
// The screen marks the current step's number and opens on it, except that a
// build under way opens on its phases (`landingTab`); a step not reached yet
// cannot be opened. While the
// construction runs or is being planned the screen reloads it every two
// seconds (`pollConstruction`).

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Nav from "react-bootstrap/Nav";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage, errorStatus } from "../api";
import type { ListApplicationsResponse } from "../client";
import {
  PRIORITIES,
  TABS,
  addPhase,
  addRequirement,
  constructible,
  constructionCalls,
  currentStep,
  draftsChanged,
  followedDrafts,
  movePhase,
  phaseDeletable,
  phasePath,
  phasePlanning,
  phaseProgress,
  phasesEditable,
  phasesPath,
  landingTab,
  currentWork,
  blankConstruction,
  unfinishedBefore,
  createdEntity,
  dependencyTitles,
  planError,
  pollConstruction,
  progress,
  removePhase,
  removeRequirement,
  runHref,
  tabOpen,
  taskActions,
  textRows,
  toDrafts,
  updatePhase,
  updateRequirement,
  type Tab,
  type Construction,
  type CurrentWork,
  type ConstructionPhase,
  type ConstructionTask,
  type PhaseDraft,
} from "../construction";
import {
  IconArrowDown,
  IconArrowLeft,
  IconChevronRight,
  IconArrowUp,
  IconCheck,
  IconCode,
  IconListCheck,
  IconPlayerPlay,
  IconRefresh,
  IconPlayerStop,
  IconPlus,
  IconSparkles,
  IconTable,
  IconTrash,
} from "../icons";
import { ClampedText } from "../ClampedText";
import { InlineCode } from "../inlineCode";
import { firstSentence } from "../runStory";
import { tableHref } from "./TableDetail";
import { PageBody, PageHeader, StatusBadge, type Tone } from "../layout";
import { T, useT } from "../i18n";

type AppItem = ListApplicationsResponse[number];
type Translate = ReturnType<typeof useT>["t"];

/** A construction's, phase's or task's status in the admin's words. */
function statusLabel(t: Translate, status: string): string {
  switch (status) {
    case "draft":
      return t("Draft");
    case "planned":
      return t("Planned");
    case "unplanned":
      return t("Not planned");
    case "running":
      return t("Running");
    case "stopped":
      return t("Stopped");
    case "failed":
      return t("Failed");
    case "done":
      return t("Done");
    case "todo":
      return t("Waiting");
    case "skipped":
      return t("Skipped");
    default:
      return status;
  }
}

function statusTone(status: string): Tone {
  switch (status) {
    case "done":
      return "green";
    case "failed":
      return "red";
    case "running":
      return "blue";
    case "stopped":
      return "yellow";
    default:
      return "secondary";
  }
}

function Status({ status }: { status: string }) {
  const { t } = useT();
  // Never wrapped or squeezed: it sits beside inputs that take the room left.
  return (
    <StatusBadge tone={statusTone(status)} className="text-nowrap flex-shrink-0">
      {statusLabel(t, status)}
    </StatusBadge>
  );
}

/** A step's card header: its number, its title, and whether it is the one
 * that matters now. */
function StepHeader({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <Card.Header>
      <Card.Title className="mb-0">{title}</Card.Title>
      {children && <div className="card-actions btn-list">{children}</div>}
    </Card.Header>
  );
}

/** The numbered tabs, one per step: the current step's number is marked, and
 * a step not reached yet cannot be opened. */
export function StepTabs({
  construction,
  tab,
  onPick,
}: {
  construction: Construction;
  tab: Tab;
  onPick: (tab: Tab) => void;
}) {
  const { t } = useT();
  const titles: Record<Tab, string> = {
    specification: t("Specification"),
    phases: t("Phases"),
    progress: t("Progress"),
  };
  const step = currentStep(construction);
  return (
    <Nav
      variant="tabs"
      className="construct-steps mb-3"
      activeKey={tab}
      onSelect={(key) => {
        const picked = TABS.find((name) => name === key);
        if (picked) onPick(picked);
      }}
    >
      {TABS.map((name, i) => (
        <Nav.Item key={name}>
          <Nav.Link eventKey={name} disabled={!tabOpen(construction, name)}>
            <span
              className={`badge me-2 ${name === step ? "bg-primary text-primary-fg" : "bg-secondary-lt"}`}
            >
              {i + 1}
            </span>
            {titles[name]}
          </Nav.Link>
        </Nav.Item>
      ))}
    </Nav>
  );
}

export function Construct({
  appId,
  tab: opened = null,
  phaseId = null,
}: {
  appId: string;
  /** The tab the route names (`/construct/phases`), else the current step's. */
  tab?: Tab | null;
  /** One phase's own page (`/construct/phases/{id}`) instead of the tabs. */
  phaseId?: string | null;
}) {
  const { t } = useT();
  const [app, setApp] = useState<AppItem | null>(null);
  // `undefined` until the first load; `null` when the application has none.
  const [construction, setConstruction] = useState<Construction | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const calls = constructionCalls(api, appId);

  const load = async (): Promise<Construction | null> => {
    try {
      return await api.getConstruction(appId);
    } catch (err) {
      if (errorStatus(err) === 404) return null;
      throw err;
    }
  };

  useEffect(() => {
    setConstruction(undefined);
    load()
      .then(setConstruction)
      .catch((err) => setError(errorMessage(err, t("Could not load the construction."))));
    api
      .listApplications()
      .then((apps) => setApp(apps.find((a) => a.id === appId) ?? null))
      .catch(() => setApp(null));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId]);

  // Each answer starts a new poller from itself; the previous one is stopped.
  useEffect(
    () =>
      pollConstruction(construction ?? null, load, setConstruction, (err) =>
        setError(errorMessage(err, t("Could not load the construction."))),
      ),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [construction],
  );

  /** Call the server, show its answer, or show why it refused. */
  const act = async (call: () => Promise<Construction>, failure: string) => {
    setBusy(true);
    setError(null);
    try {
      setConstruction(await call());
    } catch (err) {
      setError(errorMessage(err, failure));
    } finally {
      setBusy(false);
    }
  };

  const stop = () => void act(calls.stop, t("Could not stop the construction."));

  const step = construction === undefined ? null : currentStep(construction);
  const running = construction?.status === "running";
  // The tab the admin picked; a new step (the phases planned, a run started)
  // opens its own tab.
  const [picked, setPicked] = useState<Tab | null>(opened);
  // A step reached while the screen is open (the phases planned, a run
  // started) opens its own tab; the first step seen, on loading, does not
  // replace the tab the route named.
  const seenStep = useRef<typeof step>(null);
  useEffect(() => {
    if (step === null) return;
    if (seenStep.current !== null && seenStep.current !== step) {
      setPicked(step === "start" ? null : step);
    }
    seenStep.current = step;
  }, [step]);
  // An application without a construction opens on an empty specification;
  // the construction is made when the specification is first saved or planned.
  const blank = useMemo(
    () =>
      construction === null && (!app || constructible(app)) ? blankConstruction(appId) : null,
    [construction, app, appId],
  );
  const shown = construction ?? blank;
  const tab: Tab | null =
    construction && step && step !== "start"
      ? picked && tabOpen(construction, picked)
        ? picked
        : landingTab(step)
      : blank
        ? "specification"
        : null;
  /** Make the construction first, when there is none yet. */
  const started = async () => {
    if (!construction) setConstruction(await calls.create());
  };

  const runUntil = (until: string) =>
    void act(() => calls.run(until), t("Could not run the construction."));
  // A hook, so called on every render, before the branch for a phase's page
  // that uses it.
  const confirmedRun = useConfirmedRun(construction ?? null, runUntil);

  if (phaseId !== null) {
    const index = construction ? construction.phases.findIndex((p) => p.id === phaseId) : -1;
    const phase = construction && index >= 0 ? construction.phases[index] : null;
    return (
      <>
        <PageHeader
          pretitle={
            construction && phase
              ? t("Phase {n} of {total}", { n: index + 1, total: construction.phases.length })
              : t("Plan & build")
          }
          title={phase ? phase.title : t("Phase")}
          actions={
            <>
              {construction && <WorkingSpinner construction={construction} />}
              {phase && <Status status={phase.status} />}
              <Button variant="outline-secondary" href={`#${phasesPath(appId)}`}>
                <IconArrowLeft className="icon-2" />
                <T text="All phases" />
              </Button>
              {construction && phase && (
                <Button
                  variant="outline-primary"
                  disabled={busy || !phasesEditable(construction)}
                  onClick={() => confirmedRun.request(phase)}
                >
                  <IconPlayerPlay className="icon-2" />
                  <T text="Run" />
                </Button>
              )}
              {running && (
                <Button variant="outline-danger" disabled={busy} onClick={stop}>
                  <IconPlayerStop className="icon-2" />
                  <T text="Stop" />
                </Button>
              )}
            </>
          }
        />
        <PageBody>
          {error && (
            <Alert variant="danger" dismissible onClose={() => setError(null)}>
              {error}
            </Alert>
          )}
          {construction === undefined && !error && <Spinner animation="border" size="sm" />}
          {construction !== undefined && !phase && (
            <Alert variant="secondary">
              <T text="This phase is not part of the plan any more." />
            </Alert>
          )}
          {confirmedRun.modal}
          {construction && phase && (
            <PhaseDetail
              construction={construction}
              phase={phase}
              busy={busy}
              onRetry={(task) =>
                void act(() => calls.retry(task.id), t("Could not retry the task."))
              }
              onSkip={(task) =>
                void act(() => calls.skip(task.id), t("Could not skip the task."))
              }
              onPlan={(note) =>
                void act(() => calls.planTasks(phase.id, note), t("Could not plan the tasks."))
              }
            />
          )}
        </PageBody>
      </>
    );
  }

  return (
    <>
      <PageHeader
        pretitle={app?.name ?? ""}
        title={t("Plan & build")}
        actions={
          construction && (
            <>
              <WorkingSpinner construction={construction} />
              <Status status={construction.status} />
              {running && (
                <Button variant="outline-danger" disabled={busy} onClick={stop}>
                  <IconPlayerStop className="icon-2" />
                  <T text="Stop" />
                </Button>
              )}
            </>
          )
        }
      />
      <PageBody>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            {error}
          </Alert>
        )}
        {construction === undefined && !error && <Spinner animation="border" size="sm" />}
        {construction === null && app && !constructible(app) && (
          <Alert variant="secondary">
            <T text="Plan & build builds React applications. This application uses another framework." />
          </Alert>
        )}
        {shown && tab && (
          <>
            <StepTabs construction={shown} tab={tab} onPick={setPicked} />
            {tab === "specification" && (
              <SpecificationCard
                construction={shown}
                busy={busy}
                onPlan={(specification) =>
                  void act(async () => {
                    await started();
                    return calls.plan(specification);
                  }, t("Could not plan the phases."))
                }
                onSave={(specification) =>
                  void act(async () => {
                    await started();
                    return calls.saveSpecification(specification);
                  }, t("Could not save the specification."))
                }
              />
            )}
            {construction && tab === "phases" && (
              <PhasesCard
                construction={construction}
                busy={busy}
                onSave={(phases) =>
                  void act(() => calls.savePhases(phases), t("Could not save the phases."))
                }
                onRun={(until) =>
                  void act(() => calls.run(until), t("Could not run the construction."))
                }
              />
            )}
            {construction && tab === "progress" && (
              <ProgressCard
                construction={construction}
                busy={busy}
                onRetry={(task) =>
                  void act(() => calls.retry(task.id), t("Could not retry the task."))
                }
                onSkip={(task) =>
                  void act(() => calls.skip(task.id), t("Could not skip the task."))
                }
              />
            )}
          </>
        )}
      </PageBody>
    </>
  );
}

export function SpecificationCard({
  construction,
  busy,
  onPlan,
  onSave,
}: {
  construction: Construction;
  busy: boolean;
  onPlan: (specification: string) => void;
  onSave: (specification: string) => void;
}) {
  const { t } = useT();
  const [text, setText] = useState(construction.specification);
  // A new answer from the server replaces the text only when it is not being
  // edited, so a poll does not overwrite what the admin types.
  const [saved, setSaved] = useState(construction.specification);
  useEffect(() => {
    if (text === saved) setText(construction.specification);
    setSaved(construction.specification);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [construction.specification]);

  const locked = construction.status === "running" || construction.planning;
  const replan = construction.phases.length > 0;
  const failure = planError(construction);
  const dirty = text !== saved;
  const words = text.trim() === "" ? 0 : text.trim().split(/\s+/).length;
  return (
    <Card className="mb-3">
      <StepHeader title={t("Specification")} />
      <Card.Body>
        <div className="spec-layout">
          <div className="spec-main">
            <Form.Control
              as="textarea"
              rows={10}
              className="spec-editor"
              value={text}
              disabled={locked}
              aria-label={t("Specification")}
              placeholder={t("What should the application do, and for whom?")}
              onChange={(e) => setText(e.target.value)}
            />
            <div className="spec-footer">
              <span className="spec-meta">
                {t("{n} words", { n: words })}
                {dirty && (
                  <span className="spec-dirty">
                    <T text="Unsaved changes" />
                  </span>
                )}
              </span>
              <div className="btn-list">
                {dirty && (
                  <Button
                    variant="outline-secondary"
                    disabled={busy || locked}
                    onClick={() => onSave(text)}
                  >
                    <T text="Save" />
                  </Button>
                )}
                <Button
                  disabled={busy || locked || !text.trim()}
                  variant={replan ? "outline-primary" : "primary"}
                  onClick={() => onPlan(text)}
                >
                  {construction.planning ? (
                    <Spinner animation="border" size="sm" className="me-2" />
                  ) : (
                    <IconSparkles className="icon-2" />
                  )}
                  {replan ? t("Plan phases again") : t("Plan phases")}
                </Button>
              </div>
            </div>
            {replan && (
              <div className="spec-note">
                <T text="Planning again replaces the phases. Phases that have tasks are kept." />
              </div>
            )}
            {failure && (
              <Alert variant="danger" className="mt-3 mb-0">
                <div>{failure}</div>
                {construction.plan_run && (
                  <a href={runHref(construction.plan_run)}>
                    <T text="See the planning run" />
                  </a>
                )}
              </Alert>
            )}
          </div>
          <aside className="spec-tips">
            <div className="spec-tips-title">
              <T text="A good specification says" />
            </div>
            <ul>
              <li>
                <T text="Who uses the application, and what for." />
              </li>
              <li>
                <T text="What they record, and what they look at afterwards." />
              </li>
              <li>
                <T text="What belongs to whom, and who may see or change it." />
              </li>
              <li>
                <T text="What matters most, and what can wait." />
              </li>
            </ul>
          </aside>
        </div>
      </Card.Body>
    </Card>
  );
}

/** A phase's Run: it runs the phases before it first, so when any of those is
 * not done the admin is asked, with them named, before anything starts. */
function useConfirmedRun(construction: Construction | null, run: (until: string) => void) {
  const { t } = useT();
  const [asking, setAsking] = useState<ConstructionPhase | null>(null);
  const pending = (phase: ConstructionPhase) =>
    construction ? unfinishedBefore(construction, phase.id) : [];
  const before = asking ? pending(asking) : [];
  const request = (phase: ConstructionPhase) => {
    if (pending(phase).length === 0) run(phase.id);
    else setAsking(phase);
  };
  const modal = (
    <Modal show={asking !== null} onHide={() => setAsking(null)} centered>
      <Modal.Header closeButton>
        <Modal.Title>
          <T text="Run the earlier phases too?" />
        </Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <p>
          {t("“{phase}” builds on the phases before it. These are not done yet, and run first:", {
            phase: asking?.title ?? "",
          })}
        </p>
        <ul className="run-confirm-list">
          {before.map((phase) => (
            <li key={phase.id}>
              <span>
                {(construction?.phases.indexOf(phase) ?? 0) + 1}. {phase.title}
              </span>
              <Status status={phase.status} />
            </li>
          ))}
        </ul>
      </Modal.Body>
      <Modal.Footer>
        <Button variant="outline-secondary" onClick={() => setAsking(null)}>
          <T text="Cancel" />
        </Button>
        <Button
          onClick={() => {
            if (asking) run(asking.id);
            setAsking(null);
          }}
        >
          <IconPlayerPlay className="icon-2" />
          {t("Run {n} phases", { n: before.length + 1 })}
        </Button>
      </Modal.Footer>
    </Modal>
  );
  return { request, modal };
}

export function PhasesCard({
  construction,
  busy,
  onSave,
  onRun,
}: {
  construction: Construction;
  busy: boolean;
  onSave: (phases: PhaseDraft[]) => void;
  onRun: (until: string | null) => void;
}) {
  const { t } = useT();
  const now = currentWork(construction);
  const [drafts, setDrafts] = useState<PhaseDraft[]>(() => toDrafts(construction.phases));
  // A changed plan from the server (a plan run, a save, a poll) replaces the
  // drafts unless the admin has edits that are not saved.
  // Saved drafts follow the save's answer too (see `followedDrafts`).
  const [base, setBase] = useState(construction.phases);
  const [saved, setSaved] = useState<PhaseDraft[] | null>(null);
  useEffect(() => {
    const next = followedDrafts(drafts, base, construction.phases, saved);
    if (next !== null) {
      setDrafts(next);
      setSaved(null);
    }
    setBase(construction.phases);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [construction.phases]);
  const save = () => {
    setSaved(drafts);
    onSave(drafts);
  };

  const editable = phasesEditable(construction);
  const changed = draftsChanged(drafts, construction.phases);
  const confirmed = useConfirmedRun(construction, onRun);
  const canRun = editable && !changed && construction.phases.length > 0;

  return (
    <Card className="mb-3">
      <StepHeader title={t("Phases")}>
        {changed && (
          <Button size="sm" disabled={busy || !editable} onClick={save}>
            <T text="Save phases" />
          </Button>
        )}
        <Button size="sm" variant="primary" disabled={busy || !canRun} onClick={() => onRun(null)}>
          <T text="Run all" />
        </Button>
      </StepHeader>
      <Card.Body>
        {construction.planning && (
          <div className="d-flex align-items-center gap-2 text-secondary mb-3">
            <Spinner animation="border" size="sm" />
            <T text="The construction agent is planning the phases…" />
          </div>
        )}
        {changed && (
          <div className="small text-secondary mb-3">
            <T text="Save the phases before running them." />
          </div>
        )}
        {drafts.length > 0 && (
          <ol className="phase-list">
            {drafts.map((draft, i) => (
              <PhaseEditor
            key={draft.id ?? `new-${i}`}
            n={i + 1}
            draft={draft}
            saved={construction.phases.find((p) => p.id === draft.id) ?? null}
            working={now !== null && now.phase.id === draft.id ? now : null}
            href={draft.id ? `#${phasePath(construction.application, draft.id)}` : null}
            dirty={changed}
            first={i === 0}
            last={i === drafts.length - 1}
            editable={editable && !busy}
            deletable={phaseDeletable(draft, construction.phases)}
            canRun={canRun && !busy}
            onChange={(change) => setDrafts(updatePhase(drafts, i, change))}
            onMove={(by) => setDrafts(movePhase(drafts, i, by))}
            onRemove={() => setDrafts(removePhase(drafts, i))}
            onAddRequirement={() => setDrafts(addRequirement(drafts, i))}
            onRequirement={(r, change) => setDrafts(updateRequirement(drafts, i, r, change))}
            onRemoveRequirement={(r) => setDrafts(removeRequirement(drafts, i, r))}
            onRun={() => {
              const phase = construction.phases.find((p) => p.id === draft.id);
              if (phase) confirmed.request(phase);
            }}
              />
            ))}
          </ol>
        )}
        {confirmed.modal}
        {editable && (
          <button type="button" className="phase-add" onClick={() => setDrafts(addPhase(drafts))}>
            <IconPlus className="icon-2" />
            <T text="Add phase" />
          </button>
        )}
      </Card.Body>
    </Card>
  );
}

function PhaseEditor({
  n,
  draft,
  saved,
  working,
  href,
  dirty,
  first,
  last,
  editable,
  deletable,
  canRun,
  onChange,
  onMove,
  onRemove,
  onAddRequirement,
  onRequirement,
  onRemoveRequirement,
  onRun,
}: {
  n: number;
  draft: PhaseDraft;
  saved: ConstructionPhase | null;
  /** What the run is doing in this phase, when it is the one it is on. */
  working: CurrentWork | null;
  /** The phase's own page, once it is saved. */
  href: string | null;
  /** Whether the plan has edits not saved, which leaving would lose. */
  dirty: boolean;
  first: boolean;
  last: boolean;
  editable: boolean;
  deletable: boolean;
  canRun: boolean;
  onChange: (change: Partial<PhaseDraft>) => void;
  onMove: (by: -1 | 1) => void;
  onRemove: () => void;
  onAddRequirement: () => void;
  onRequirement: (r: number, change: Partial<PhaseDraft["requirements"][number]>) => void;
  onRemoveRequirement: (r: number) => void;
  onRun: () => void;
}) {
  const { t } = useT();
  // A phase not saved yet has no status of its own; one whose tasks are being
  // planned has none yet either, but is running.
  const status = working ? "running" : (saved?.status ?? "new");
  return (
    <li className={`phase phase-${status}${working ? " phase-working" : ""}`}>
      <div className="phase-marker" aria-hidden="true">
        {status === "done" ? <IconCheck className="icon-2" /> : n}
      </div>
      <div className="phase-card">
        <div className="phase-head">
          <Form.Control
            className="phase-title"
            value={draft.title}
            disabled={!editable}
            aria-label={t("Phase title")}
            placeholder={t("Phase title")}
            onChange={(e) => onChange({ title: e.target.value })}
          />
          {saved && <Status status={status} />}
          {working && (
            <span className="phase-now">
              <Spinner animation="border" size="sm" />
              <span className="phase-now-text">
                {working.kind === "planning"
                  ? t("Planning its tasks…")
                  : t("Working on “{task}”", { task: working.task.title })}
              </span>
            </span>
          )}
          <div className="phase-tools">
            <button
              type="button"
              className="phase-icon-btn"
              disabled={!editable || first}
              aria-label={t("Move up")}
              title={t("Move up")}
              onClick={() => onMove(-1)}
            >
              <IconArrowUp className="icon-2" />
            </button>
            <button
              type="button"
              className="phase-icon-btn"
              disabled={!editable || last}
              aria-label={t("Move down")}
              title={t("Move down")}
              onClick={() => onMove(1)}
            >
              <IconArrowDown className="icon-2" />
            </button>
            <button
              type="button"
              className="phase-icon-btn phase-icon-btn-danger"
              disabled={!editable || !deletable}
              aria-label={t("Delete phase")}
              title={deletable ? t("Delete phase") : t("A phase that has tasks cannot be deleted")}
              onClick={onRemove}
            >
              <IconTrash className="icon-2" />
            </button>
            {saved && href && (
              <Button
                size="sm"
                variant="outline-secondary"
                className="ms-1"
                href={dirty ? undefined : href}
                disabled={dirty}
                title={dirty ? t("Save the phases before opening one") : undefined}
              >
                <T text="Details" />
                <IconChevronRight className="icon-2 ms-1 me-0" />
              </Button>
            )}
            {saved && (
              <Button
                size="sm"
                variant="outline-primary"
                className="ms-1"
                disabled={!canRun}
                onClick={onRun}
              >
                <IconPlayerPlay className="icon-2" />
                <T text="Run" />
              </Button>
            )}
          </div>
        </div>
        <Form.Control
          as="textarea"
          rows={textRows(draft.description)}
          className="phase-description"
          value={draft.description}
          disabled={!editable}
          aria-label={t("Description")}
          placeholder={t("What this phase delivers")}
          onChange={(e) => onChange({ description: e.target.value })}
        />
        <div className="phase-section-label">
          <T text="Requirements" />
          {draft.requirements.length > 0 && (
            <span className="phase-count">{draft.requirements.length}</span>
          )}
        </div>
        {draft.requirements.length > 0 && (
          <ul className="phase-reqs">
            {draft.requirements.map((req, r) => (
              <li key={r} className="phase-req">
                <Form.Select
                  size="sm"
                  className={`phase-priority phase-priority-${req.priority}`}
                  value={req.priority}
                  disabled={!editable}
                  aria-label={t("Priority")}
                  onChange={(e) => onRequirement(r, { priority: e.target.value })}
                >
                  {PRIORITIES.map((p) => (
                    <option key={p} value={p}>
                      {p === "must" ? t("Must") : p === "should" ? t("Should") : t("Could")}
                    </option>
                  ))}
                </Form.Select>
                <Form.Control
                  size="sm"
                  className="phase-req-text"
                  value={req.text}
                  disabled={!editable}
                  aria-label={t("Requirement")}
                  onChange={(e) => onRequirement(r, { text: e.target.value })}
                />
                {editable && (
                  <button
                    type="button"
                    className="phase-icon-btn phase-icon-btn-danger phase-req-remove"
                    aria-label={t("Delete requirement")}
                    title={t("Delete requirement")}
                    onClick={() => onRemoveRequirement(r)}
                  >
                    <IconTrash className="icon-2" />
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
        {editable && (
          <button type="button" className="phase-add-req" onClick={onAddRequirement}>
            <IconPlus className="icon-2" />
            <T text="Add a requirement" />
          </button>
        )}
      </div>
    </li>
  );
}

/** A spinner beside the status badge while the construction works: planning
 * the phases, or running. */
function WorkingSpinner({ construction }: { construction: Construction }) {
  const { t } = useT();
  if (construction.status !== "running" && !construction.planning) return null;
  return (
    <Spinner
      animation="border"
      size="sm"
      className="text-secondary align-self-center"
      role="status"
      aria-label={t("Working")}
    />
  );
}

/** The line at the top of the progress: what the run is on right now. */
function NowWorking({ now }: { now: CurrentWork }) {
  const { t } = useT();
  const run = now.kind === "task" ? now.task.runs[now.task.runs.length - 1] : undefined;
  return (
    <div className="build-now">
      <Spinner animation="border" size="sm" />
      <span className="build-now-text">
        {now.kind === "planning"
          ? t("Planning the tasks of “{phase}”. This takes a minute or two.", {
              phase: now.phase.title,
            })
          : t("Working on “{task}”", { task: now.task.title })}
      </span>
      {run && (
        <a className="build-now-link" href={runHref(run)}>
          <T text="Watch the run" />
        </a>
      )}
    </div>
  );
}

export function ProgressCard({
  construction,
  busy,
  onRetry,
  onSkip,
}: {
  construction: Construction;
  busy: boolean;
  onRetry: (task: ConstructionTask) => void;
  onSkip: (task: ConstructionTask) => void;
}) {
  const { t } = useT();
  const { done, total } = progress(construction);
  const now = currentWork(construction);
  return (
    <Card className="mb-3">
      <StepHeader title={t("Progress")}>
        <span className="text-secondary small">
          {t("{done} of {total} phases done", { done, total })}
        </span>
      </StepHeader>
      <div
        className="build-meter"
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={total}
        aria-valuenow={done}
      >
        <div style={{ width: `${total ? (100 * done) / total : 0}%` }} />
      </div>
      <Card.Body>
        {construction.error && (
          <Alert variant={construction.status === "failed" ? "danger" : "warning"}>
            {construction.error}
          </Alert>
        )}
        {now && <NowWorking now={now} />}
        <ol className="phase-list">
          {construction.phases.map((phase, i) => {
            const tasks = phaseProgress(phase);
            const planning = now?.kind === "planning" && now.phase.id === phase.id;
            return (
              <li
                key={phase.id}
                className={`phase phase-${planning ? "running" : phase.status}${now?.phase.id === phase.id ? " phase-working" : ""}`}
              >
                <div className="phase-marker" aria-hidden="true">
                  {phase.status === "done" ? <IconCheck className="icon-2" /> : i + 1}
                </div>
                <div className="phase-card">
                  <div className="phase-head build-phase-head">
                    <a
                      className="build-phase-title"
                      href={`#${phasePath(construction.application, phase.id)}`}
                    >
                      {phase.title}
                    </a>
                    <Status status={phase.status} />
                    {tasks.total > 0 && (
                      <span className="build-phase-count">
                        {t("{done} of {total} tasks", tasks)}
                      </span>
                    )}
                  </div>
                  {phase.tasks.length === 0 ? (
                    <div className="build-empty">
                      {planning ? (
                        <>
                          <Spinner animation="border" size="sm" className="me-2" />
                          <T text="Planning its tasks…" />
                        </>
                      ) : (
                        <T text="Its tasks are planned when the phase is reached." />
                      )}
                    </div>
                  ) : (
                    <ul className="build-tasks">
                      {phase.tasks.map((task) => (
                        <TaskRow
                          key={task.id}
                          task={task}
                          construction={construction}
                          busy={busy}
                          onRetry={() => onRetry(task)}
                          onSkip={() => onSkip(task)}
                        />
                      ))}
                    </ul>
                  )}
                </div>
              </li>
            );
          })}
        </ol>
      </Card.Body>
    </Card>
  );
}

/** A task: its status, title and kind, its runs and what can be done about
 * it, and what the agent said it did — three lines of it until opened. */
function TaskRow({
  task,
  construction,
  busy,
  onRetry,
  onSkip,
}: {
  task: ConstructionTask;
  construction: Construction;
  busy: boolean;
  onRetry: () => void;
  onSkip: () => void;
}) {
  const { t } = useT();
  const actions = taskActions(task, construction);
  const summary = task.summary?.trim() ?? "";
  return (
    <li className={`build-task build-task-${task.status}`}>
      <span className="build-task-dot" aria-hidden="true">
        {task.status === "running" && <Spinner animation="border" size="sm" />}
      </span>
      <div className="build-task-body">
        <div className="build-task-head">
          <span className="build-task-title">
            <InlineCode text={task.title} />
          </span>
          <span className="build-task-kind">
            {task.kind === "data_model" ? t("Data model") : t("Feature")}
          </span>
          <Status status={task.status} />
          <span className="build-task-runs">
            {task.runs.map((run, r) => (
              <a key={run} href={runHref(run)} className="build-run-link">
                {t("Run {n}", { n: r + 1 })}
              </a>
            ))}
          </span>
          {(actions.retry || actions.skip) && (
            <div className="btn-list flex-nowrap ms-auto">
              {actions.retry && (
                <Button size="sm" variant="outline-primary" disabled={busy} onClick={onRetry}>
                  <T text="Retry" />
                </Button>
              )}
              {actions.skip && (
                <Button size="sm" variant="outline-secondary" disabled={busy} onClick={onSkip}>
                  <T text="Skip" />
                </Button>
              )}
            </div>
          )}
        </div>
        {summary && <ClampedText text={summary} />}
      </div>
    </li>
  );
}

/** A requirement's priority, as the coloured word the phases use. */
function Priority({ priority }: { priority: string }) {
  const { t } = useT();
  return (
    <span className={`req-pill phase-priority-${priority}`}>
      {priority === "must" ? t("Must") : priority === "should" ? t("Should") : t("Could")}
    </span>
  );
}

/** One phase's own page: what it is to deliver, how that becomes tasks, and
 * each task in full — what it was told, what it waits for, what it made and
 * what it reported. */
export function PhaseDetail({
  construction,
  phase,
  busy,
  onRetry,
  onSkip,
  onPlan,
}: {
  construction: Construction;
  phase: ConstructionPhase;
  busy: boolean;
  onRetry: (task: ConstructionTask) => void;
  onSkip: (task: ConstructionTask) => void;
  /** Plan the phase's tasks (again, told the note) and stop before they run. */
  onPlan: (note: string | null) => void;
}) {
  const { t } = useT();
  const tasks = phaseProgress(phase);
  const planning = phasePlanning(construction, phase);
  const [replanning, setReplanning] = useState(false);
  return (
    <>
      <Card className="mb-3">
        <Card.Header>
          <div>
            <Card.Title className="mb-0">
              <T text="Tasks" />
            </Card.Title>
            <div className="text-secondary small mt-1">
              <T text="Planned by the construction agent from the phase's requirements: a few sizeable tasks, such as the tables, their queries, a whole screen. Each task's run shows what it was told and what it answered." />
            </div>
          </div>
          {tasks.total > 0 && (
            <div className="card-actions d-flex align-items-center gap-2 text-secondary small text-nowrap">
              {t("{done} of {total} tasks", tasks)}
              {planning === "replan" && (
                <Button
                  size="sm"
                  variant="outline-secondary"
                  disabled={busy}
                  title={t("Throw these tasks away and have the construction agent plan the phase again, told what to change.")}
                  onClick={() => setReplanning(true)}
                >
                  <IconRefresh className="icon-2" />
                  <T text="Plan again" />
                </Button>
              )}
            </div>
          )}
        </Card.Header>
        <Card.Body>
          {phase.tasks.length === 0 ? (
            <div className="build-empty px-0">
              {currentWork(construction)?.phase.id === phase.id ? (
                <>
                  <Spinner animation="border" size="sm" className="me-2" />
                  <T text="Planning its tasks…" />
                </>
              ) : (
                <>
                  <T text="Not planned yet: the construction agent plans this phase's tasks from its requirements when the run reaches it." />
                  {planning === "plan" && (
                    <div className="mt-3">
                      <Button size="sm" variant="outline-primary" disabled={busy} onClick={() => onPlan(null)}>
                        <IconListCheck className="icon-2" />
                        <T text="Plan tasks" />
                      </Button>
                      <div className="small mt-1">
                        <T text="Plans the tasks now and stops before they run, so you can review them first." />
                      </div>
                    </div>
                  )}
                </>
              )}
            </div>
          ) : (
            <ol className="pd-tasks">
              {phase.tasks.map((task, n) => (
                <PhaseTask
                  key={task.id}
                  n={n + 1}
                  task={task}
                  phase={phase}
                  construction={construction}
                  busy={busy}
                  onRetry={() => onRetry(task)}
                  onSkip={() => onSkip(task)}
                />
              ))}
            </ol>
          )}
        </Card.Body>
      </Card>
      {replanning && (
        <PlanAgainModal
          phase={phase}
          onCancel={() => setReplanning(false)}
          onPlan={(note) => {
            setReplanning(false);
            onPlan(note);
          }}
        />
      )}
      {/* What the phase was asked for: there to check against, not to read
          beside the tasks — they do not match one to one. */}
      <details className="card run-context">
        <summary className="card-header">
          <span className="card-title mb-0">
            <T text="What the phase was asked for" />
          </span>
          <span className="text-secondary small ms-2">
            {t("{n} requirements", { n: phase.requirements.length })}
          </span>
        </summary>
        <div className="card-body">
          {phase.description.trim() && <p className="pd-description">{phase.description}</p>}
          {phase.requirements.length > 0 && (
            <ul className="pd-reqs">
              {phase.requirements.map((req, r) => (
                <li key={r}>
                  <Priority priority={req.priority} />
                  <span>
                    <InlineCode text={req.text} />
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
      </details>
    </>
  );
}

/** Plan a phase's tasks again: what to change, for the construction agent,
 * which is told the old tasks too. */
function PlanAgainModal({
  phase,
  onCancel,
  onPlan,
}: {
  phase: ConstructionPhase;
  onCancel: () => void;
  onPlan: (note: string | null) => void;
}) {
  const { t } = useT();
  const [note, setNote] = useState("");
  return (
    <Modal show onHide={onCancel} centered>
      <Modal.Header closeButton>
        <Modal.Title>{t("Plan “{phase}” again", { phase: phase.title })}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <p className="text-secondary">
          <T text="These tasks are thrown away and the construction agent plans the phase again. It is shown the old tasks and your note, and stops before the new tasks run." />
        </p>
        <Form.Group controlId="plan-again-note">
          <Form.Label>
            <T text="What should change?" />
          </Form.Label>
          <Form.Control
            as="textarea"
            rows={4}
            autoFocus
            value={note}
            placeholder={t("For example: one task for both list screens; no admin screen.")}
            onChange={(e) => setNote(e.target.value)}
          />
        </Form.Group>
      </Modal.Body>
      <Modal.Footer>
        <Button variant="outline-secondary" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button onClick={() => onPlan(note.trim() === "" ? null : note.trim())}>
          <IconRefresh className="icon-2" />
          <T text="Plan again" />
        </Button>
      </Modal.Footer>
    </Modal>
  );
}

function PhaseTask({
  n,
  task,
  phase,
  construction,
  busy,
  onRetry,
  onSkip,
}: {
  n: number;
  task: ConstructionTask;
  phase: ConstructionPhase;
  construction: Construction;
  busy: boolean;
  onRetry: () => void;
  onSkip: () => void;
}) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const actions = taskActions(task, construction);
  const waitsFor = dependencyTitles(task, phase);
  const created = task.created
    .map(createdEntity)
    .filter((c): c is { kind: string; name: string } => c !== null);
  const summary = task.summary?.trim() ?? "";
  const brief = task.brief.trim();
  // One line to scan by: what came of it, else what it is to do.
  const gist = firstSentence(summary || brief);
  return (
    <li className={`pd-task build-task-${task.status}`}>
      <div className="pd-task-head">
        <span className="pd-task-n">{n}</span>
        <span className="build-task-title">
          <InlineCode text={task.title} />
        </span>
        <span className="build-task-kind">
          {task.kind === "data_model" ? t("Data model") : t("Feature")}
        </span>
        <Status status={task.status} />
        <span className="pd-task-end">
          {task.runs.map((run, r) => (
            <a
              key={run}
              href={runHref(run)}
              className="build-run-link"
              title={t("What it was told, what it did and what it answered")}
            >
              {t("Run {n}", { n: r + 1 })}
            </a>
          ))}
          {actions.retry && (
            <Button size="sm" variant="outline-primary" disabled={busy} onClick={onRetry}>
              <T text="Retry" />
            </Button>
          )}
          {actions.skip && (
            <Button size="sm" variant="outline-secondary" disabled={busy} onClick={onSkip}>
              <T text="Skip" />
            </Button>
          )}
        </span>
      </div>
      {gist && (
        <div className="pd-task-gist">
          <InlineCode text={gist} />
        </div>
      )}
      {created.length > 0 && (
        <div className="pd-created-row">
          {created.map((entity) => {
            const key = `${entity.kind}:${entity.name}`;
            const label =
              entity.kind === "table"
                ? t("New table {name}", { name: entity.name })
                : entity.kind === "query"
                  ? t("New query {name}", { name: entity.name })
                  : entity.name;
            const icon =
              entity.kind === "table" ? (
                <IconTable className="icon-2" />
              ) : entity.kind === "query" ? (
                <IconCode className="icon-2" />
              ) : null;
            // A table has a page to open; a query is edited among its API's.
            return entity.kind === "table" ? (
              <a key={key} className="pd-created" href={tableHref(entity.name)} title={label}>
                {icon}
                {entity.name}
              </a>
            ) : (
              <span key={key} className="pd-created" title={label}>
                {icon}
                {entity.name}
              </span>
            );
          })}
        </div>
      )}
      {(waitsFor.length > 0 || (task.runs.length === 0 && brief)) && (
        <div className="pd-task-meta">
          {waitsFor.length > 0 && (
            <span className="pd-waits">
              {t("after {tasks}", { tasks: waitsFor.join(", ").replace(/`/g, "") })}
            </span>
          )}
          {/* What it is told and what it answers are on its run's page; a task
              not run yet has no run, so its brief is here. */}
          {task.runs.length === 0 && brief && (
            <button type="button" className="build-more pd-toggle" onClick={() => setOpen(!open)}>
              {open ? t("Hide the brief") : t("Brief")}
            </button>
          )}
        </div>
      )}
      {open && task.runs.length === 0 && (
        <div className="pd-task-text pd-brief">
          <InlineCode text={brief} />
        </div>
      )}
    </li>
  );
}
