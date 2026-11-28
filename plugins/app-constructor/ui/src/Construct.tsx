// Plan & build (TODO-app-constructor AC4, AC7; the module's screen since
// AC13.5, ported from the admin's screens/Construct.tsx): an application built
// from a description. Four steps, each a numbered tab:
//
// 1. **Specification**: what the application should do; "Continue to
//    research", or straight to "Plan phases", which has the construction
//    agent turn it into phases.
// 2. **Research** (optional), in order: the agent's questions about what the
//    specification leaves open, whose answers go with it to every run; then a
//    few looks of the application's main screens, drawn from the answers, to
//    choose one of or to comment on and have revised. Asking again discards
//    the mockups. The chosen one shapes the phases, the data model and the
//    code.
// 3. **Phases**: the plan, edited before anything runs, then "Run all" or
//    a phase's own "Run", which runs the phases before it first.
// 4. **Progress**: each phase's tasks as the runner works through them, with
//    their runs, summaries, and Stop / Retry / Skip.
//
// The screen marks the current step's number and opens on it, except that a
// build under way opens on its phases (`landingTab`); a step not reached yet
// cannot be opened. While the
// construction runs or is being planned the screen reloads it every two
// seconds (`pollConstruction`).
//
// The admin mounts the screen with a host (see host.ts): the construction
// comes from the module's routes through it, the screen's own paths (a tab, a
// phase's page) are `host.path`, and links to them are `host.href`.

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Nav from "react-bootstrap/Nav";
import Spinner from "react-bootstrap/Spinner";

import type { Application, Host } from "./host";
import {
  PRIORITIES,
  TABS,
  DEFAULT_MOCKUPS,
  MOCKUP_WIDTHS,
  chosenMockup,
  latestMockups,
  mockupDocument,
  mockupPages,
  researchEditable,
  revisionFeedback,
  changedAnswers,
  askingDiscards,
  mockupsWithoutQuestions,
  planningUnresearched,
  planningWithoutMockup,
  feedbackPath,
  feedbackPhases,
  feedbackTasks,
  feedbackEditable,
  feedbackDeletable,
  type ConstructionFeedback,
  type ConstructionMockup,
  type MockupWidth,
  type MockupScreensLoader,
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
  agentWork,
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
  type AgentWork,
  type ConstructionErrorEntry,
  type ConstructionPhase,
  type ConstructionTask,
  type PhaseDraft,
  type Id,
  loadConstruction,
} from "./construction";
import { sameId } from "./types";
import {
  IconArrowDown,
  IconArrowLeft,
  IconChevronLeft,
  IconChevronDown,
  IconChevronRight,
  IconArrowUp,
  IconCheck,
  IconCode,
  IconDeviceDesktop,
  IconDeviceMobile,
  IconHelpCircle,
  IconLayoutDashboard,
  IconListCheck,
  IconMessagePlus,
  IconPlayerPlay,
  IconRefresh,
  IconPlayerStop,
  IconPlus,
  IconSparkles,
  IconTable,
  IconTrash,
} from "./icons";
import {
  ClampedText,
  InlineCode,
  PageHeader,
  StatusBadge,
  firstSentence,
  historyTime,
  tableHref,
  useHref,
  type Tone,
} from "./bits";
import { T, useT, type Translate } from "./i18n";

/** The admin's page body holds the screen already: its cards go straight in. */
function PageBody({ children }: { children: ReactNode }) {
  return <>{children}</>;
}

/** The route's own message from a refusal (`host.json` throws it), else
 * `fallback`. */
function errorMessage(err: unknown, fallback: string): string {
  return err instanceof Error && err.message ? err.message : fallback;
}

/** A construction's, phase's or task's status in the admin's words. */
function statusLabel(t: Translate, status: string): string {
  switch (status) {
    case "draft":
      return t("Draft");
    case "planned":
      return t("Planned");
    case "unplanned":
    case "open":
      return t("Not planned");
    case "given":
      return t("Awaiting approval");
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
    case "given":
      return "yellow";
    default:
      return "secondary";
  }
}

function Status({ status }: { status: string }) {
  const { t } = useT();
  // Never wrapped or squeezed: it sits beside inputs that take the room left.
  return (
    <StatusBadge
      tone={statusTone(status)}
      className="text-nowrap flex-shrink-0"
    >
      {statusLabel(t, status)}
    </StatusBadge>
  );
}

/** A step's card header: its number, its title, and whether it is the one
 * that matters now. */
function StepHeader({
  title,
  number,
  children,
}: {
  title: string;
  /** Its place in the step, when the step has parts to take in order. */
  number?: number;
  children?: ReactNode;
}) {
  return (
    <Card.Header>
      <Card.Title className="mb-0">
        {number !== undefined && (
          <span className="mockup-number mockup-number-sm me-2">{number}</span>
        )}
        {title}
      </Card.Title>
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
    research: t("Research"),
    phases: t("Phases"),
    feedback: t("Feedback"),
    activity: t("Activity"),
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
              className={`badge me-2 ${
                name === step ? "bg-primary text-primary-fg" : "bg-secondary-lt"
              }`}
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
  host,
  tab: opened = null,
  phaseId = null,
  feedbackPhase = null,
}: {
  /** What the admin lends the screen: the application, and its routes. */
  host: Pick<Host, "app" | "fetch" | "json">;
  /** The tab the route names (`/construct/phases`), else the current step's. */
  tab?: Tab | null;
  /** One phase's own page (`/construct/phases/{id}`) instead of the tabs. */
  phaseId?: string | null;
  /** The phase new feedback is for (`/construct/feedback/{id}`). */
  feedbackPhase?: string | null;
}) {
  const { t } = useT();
  const href = useHref();
  const app: Application | null = host.app;
  const appId = app?.id ?? "";
  // `undefined` until the first load; `null` when the application has none.
  const [construction, setConstruction] = useState<
    Construction | null | undefined
  >(undefined);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const calls = constructionCalls(host.json, appId);

  const load = (): Promise<Construction | null> =>
    loadConstruction(host.fetch, appId);

  useEffect(() => {
    if (!appId) return;
    setConstruction(undefined);
    load()
      .then(setConstruction)
      .catch((err) =>
        setError(errorMessage(err, t("Could not load the construction.")))
      );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId]);

  // Each answer starts a new poller from itself; the previous one is stopped.
  useEffect(
    () =>
      pollConstruction(construction ?? null, load, setConstruction, (err) =>
        setError(errorMessage(err, t("Could not load the construction.")))
      ),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [construction]
  );

  /** Call the server, show its answer, or show why it refused. Answers
   * whether the call succeeded. */
  const act = async (
    call: () => Promise<unknown>,
    failure: string
  ): Promise<boolean> => {
    setBusy(true);
    setError(null);
    try {
      setConstruction((await call()) as Construction);
      return true;
    } catch (err) {
      setError(errorMessage(err, failure));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const stop = () =>
    void act(calls.stop, t("Could not stop the construction."));

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
      construction === null && (!app || constructible(app))
        ? blankConstruction(appId)
        : null,
    [construction, app, appId]
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

  const runUntil = (until: Id) =>
    void act(() => calls.run(until), t("Could not run the construction."));
  // A hook, so called on every render, before the branch for a phase's page
  // that uses it.
  const confirmedRun = useConfirmedRun(construction ?? null, runUntil);
  const confirmation = useConfirmation();

  // The research's and the planning's actions, each confirmed first where it
  // skips a step the next one is drawn from.
  const research = (specification: string) =>
    void act(async () => {
      await started();
      return calls.saveSpecification(specification);
    }, t("Could not save the specification.")).then(
      (saved) => saved && setPicked("research")
    );
  const ask = () => {
    if (!construction) return;
    void act(
      () => calls.askQuestions(construction.specification),
      t("Could not ask the questions.")
    );
  };
  const askChecked = () => {
    const discards = construction ? askingDiscards(construction) : 0;
    if (discards === 0) return ask();
    confirmation.confirm({
      title: t("Ask again and discard the mockups?"),
      body: t(
        "The {n} mockups were drawn without the answers to the new questions, so they are discarded, the chosen one too. Propose new ones once you have answered.",
        { n: discards }
      ),
      confirm: t("Ask and discard the mockups"),
      danger: true,
      onConfirm: ask,
    });
  };
  const propose = () => {
    if (!construction) return;
    void act(
      () => calls.proposeMockups(construction.specification, DEFAULT_MOCKUPS),
      t("Could not propose mockups.")
    );
  };
  const proposeChecked = () => {
    if (!construction || !mockupsWithoutQuestions(construction))
      return propose();
    confirmation.confirm({
      title: t("Propose mockups without questions?"),
      body: t(
        "The construction agent has not asked anything yet, so the mockups are drawn from the specification alone. Asking questions afterwards discards them."
      ),
      confirm: t("Propose mockups anyway"),
      onConfirm: propose,
      alternative: { label: t("Ask questions"), onClick: ask },
    });
  };
  const plan = (specification: string) =>
    void act(async () => {
      await started();
      return calls.plan(specification);
    }, t("Could not plan the phases."));
  /** "Plan phases", confirmed when nothing is researched; `instead` is the
   * research step offered in its place. */
  const planChecked = (
    specification: string,
    instead: { label: string; onClick: () => void }
  ) => {
    const current = construction ?? blank;
    if (!current || !planningUnresearched(current)) return plan(specification);
    confirmation.confirm({
      title: t("Plan the phases without research?"),
      body: t(
        "No questions were asked and no mockups drawn, so the phases are planned from the specification alone. You can still do the research later and plan the phases again."
      ),
      confirm: t("Plan phases anyway"),
      onConfirm: () => plan(specification),
      alternative: instead,
    });
  };
  /** The research step's "Plan phases", confirmed when no mockup is chosen:
   * offering to propose some when there are none. */
  const planFromResearch = () => {
    if (!construction) return;
    const plan = () =>
      void act(
        () => calls.plan(construction.specification),
        t("Could not plan the phases.")
      );
    const without = planningWithoutMockup(construction);
    if (without === null) return plan();
    confirmation.confirm(
      without === "none"
        ? {
            title: t("Plan the phases without mockups?"),
            body: t(
              "No mockups were drawn, so the phases are planned without a look of the application to follow."
            ),
            confirm: t("Plan phases anyway"),
            onConfirm: plan,
            alternative: {
              label: t("Propose mockups"),
              onClick: proposeChecked,
            },
          }
        : {
            title: t("Plan the phases without a chosen mockup?"),
            body: t(
              "None of the mockups is chosen, so the phases are planned without a look of the application to follow. Choose one first to have the phases follow it."
            ),
            confirm: t("Plan phases anyway"),
            onConfirm: plan,
          }
    );
  };

  /** The Phases tab's "Plan phases again", always confirmed: it replaces
   * the phases the admin may have edited. */
  const replan = () => {
    if (!construction) return;
    const kept = construction.phases.filter((p) => p.tasks.length > 0).length;
    confirmation.confirm({
      title: t("Plan the phases again?"),
      body:
        t(
          "The construction agent plans the phases anew from the specification, the answers and the chosen mockup, and they replace the current ones."
        ) +
        (kept > 0
          ? " " +
            t("{n} phases have tasks and are kept as they are.", { n: kept })
          : ""),
      confirm: t("Plan phases again"),
      onConfirm: () =>
        void act(
          () => calls.plan(construction.specification),
          t("Could not plan the phases.")
        ),
    });
  };

  if (phaseId !== null) {
    const index = construction
      ? construction.phases.findIndex((p) => sameId(p.id, phaseId))
      : -1;
    const phase =
      construction && index >= 0 ? construction.phases[index] : null;
    return (
      <>
        <PageHeader
          pretitle={
            construction && phase
              ? t("Phase {n} of {total}", {
                  n: index + 1,
                  total: construction.phases.length,
                })
              : t("Plan & build")
          }
          title={phase ? phase.title : t("Phase")}
          actions={
            <>
              {construction && <WorkingSpinner construction={construction} />}
              {phase && <Status status={phase.status} />}
              <Button variant="outline-secondary" href={href(phasesPath())}>
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
              {construction &&
                phase &&
                feedbackPhases(construction).some((p) =>
                  sameId(p.id, phase.id)
                ) && (
                  <Button
                    variant="outline-secondary"
                    href={href(feedbackPath(phase.id))}
                  >
                    <IconMessagePlus className="icon-2" />
                    <T text="Give feedback" />
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
          {construction === undefined && !error && (
            <Spinner animation="border" size="sm" />
          )}
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
                void act(
                  () => calls.retry(task.id),
                  t("Could not retry the task.")
                )
              }
              onSkip={(task) =>
                void act(
                  () => calls.skip(task.id),
                  t("Could not skip the task.")
                )
              }
              onPlan={(note) =>
                void act(
                  () => calls.planTasks(phase.id, note),
                  t("Could not plan the tasks.")
                )
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
        {construction === undefined && !error && (
          <Spinner animation="border" size="sm" />
        )}
        {confirmation.modal}
        {confirmedRun.modal}
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
                  planChecked(specification, {
                    label: t("Continue to research"),
                    onClick: () => research(specification),
                  })
                }
                onResearch={research}
                onSave={(specification) =>
                  void act(async () => {
                    await started();
                    return calls.saveSpecification(specification);
                  }, t("Could not save the specification."))
                }
              />
            )}
            {construction && tab === "research" && (
              <p className="text-secondary">
                <T text="Questions first: the mockups are drawn from the specification and your answers, so asking questions again discards the mockups. Both are optional." />
              </p>
            )}
            {construction && tab === "research" && planError(construction) && (
              <Alert variant="danger">
                <T text="The construction agent's last run failed:" />{" "}
                {planError(construction)}
              </Alert>
            )}
            {construction && tab === "research" && (
              <QuestionsCard
                construction={construction}
                busy={busy}
                onAsk={askChecked}
                onAnswer={(answers) =>
                  void act(
                    () => calls.answerQuestions(answers),
                    t("Could not save the answers.")
                  )
                }
              />
            )}
            {construction && tab === "research" && (
              <MockupsCard
                construction={construction}
                busy={busy}
                onPropose={proposeChecked}
                onRevise={(feedback, note) =>
                  void act(
                    () => calls.reviseMockups(feedback, note),
                    t("Could not revise the mockups.")
                  )
                }
                onChoose={(mockup) =>
                  void act(
                    () => calls.chooseMockup(mockup),
                    t("Could not choose the mockup.")
                  )
                }
                loadScreens={calls.mockupScreens}
                onPlan={planFromResearch}
              />
            )}
            {construction && tab === "phases" && (
              <PhasesCard
                construction={construction}
                busy={busy}
                onSave={(phases) =>
                  void act(
                    () => calls.savePhases(phases),
                    t("Could not save the phases.")
                  )
                }
                onRun={(until) =>
                  void act(
                    () => calls.run(until),
                    t("Could not run the construction.")
                  )
                }
                onReplan={replan}
              />
            )}
            {construction && tab === "feedback" && (
              <FeedbackCard
                construction={construction}
                busy={busy}
                initialPhase={feedbackPhase}
                onGive={(phase, title, description) =>
                  act(
                    () => calls.giveFeedback(phase, title, description),
                    t("Could not give the feedback.")
                  )
                }
                onPlan={(feedback) =>
                  void act(
                    () => calls.planFeedback(feedback.id),
                    t("Could not plan the feedback.")
                  )
                }
                onDelete={(feedback) =>
                  confirmation.confirm({
                    ...(feedback.status === "given"
                      ? {
                          title: t("Decline this feedback?"),
                          body: t(
                            "“{title}” is deleted without being planned.",
                            {
                              title: feedback.title,
                            }
                          ),
                          confirm: t("Decline the feedback"),
                        }
                      : {
                          title: t("Delete this feedback?"),
                          body: t(
                            "“{title}” and its follow-up tasks are deleted. None of them has run, so nothing built is lost.",
                            { title: feedback.title }
                          ),
                          confirm: t("Delete the feedback"),
                        }),
                    danger: true,
                    onConfirm: () =>
                      void act(
                        () => calls.deleteFeedback(feedback.id),
                        t("Could not delete the feedback.")
                      ),
                  })
                }
                onRun={(phase) => confirmedRun.request(phase)}
              />
            )}
            {construction && tab === "activity" && (
              <ActivityCard
                construction={construction}
                busy={busy}
                onRetry={(task) =>
                  void act(
                    () => calls.retry(task.id),
                    t("Could not retry the task.")
                  )
                }
                onSkip={(task) =>
                  void act(
                    () => calls.skip(task.id),
                    t("Could not skip the task.")
                  )
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
  onResearch,
  onSave,
}: {
  construction: Construction;
  busy: boolean;
  onPlan: (specification: string) => void;
  /** Save the specification and go on to the questions and mockups:
   * optional, so "Plan phases" stays. */
  onResearch: (specification: string) => void;
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

  const locked = !researchEditable(construction);
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
                {/* The research is the suggested next step; once there are
                    phases, planning them again is. */}
                <Button
                  disabled={busy || locked || !text.trim()}
                  variant={replan ? "outline-secondary" : "primary"}
                  onClick={() => onResearch(text)}
                >
                  {construction.asking || construction.designing ? (
                    <Spinner animation="border" size="sm" className="me-2" />
                  ) : (
                    <IconHelpCircle className="icon-2" />
                  )}
                  <T text="Continue to research" />
                  <IconChevronRight className="icon-2 ms-1 me-0" />
                </Button>
                <Button
                  disabled={busy || locked || !text.trim()}
                  variant={replan ? "primary" : "outline-primary"}
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
            {replan ? (
              <div className="spec-note">
                <T text="Planning again replaces the phases. Phases that have tasks are kept." />
              </div>
            ) : (
              <div className="spec-note">
                <T text="Research is optional: answer the agent's questions and choose a mockup first, or plan the phases now." />
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

/** The research step's questions (AC8): what the construction agent asked
 * about the specification, each with the admin's answer, saved together.
 * Asking again keeps the answered questions and replaces the open ones. */
export function QuestionsCard({
  construction,
  busy,
  onAsk,
  onAnswer,
}: {
  construction: Construction;
  busy: boolean;
  onAsk: () => void;
  onAnswer: (answers: { question: Id; text: string }[]) => void;
}) {
  const { t } = useT();
  // What the admin typed, by question, until it is saved: a poll does not
  // overwrite it, and a saved answer drops out once the server has it.
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  useEffect(() => {
    setDrafts((d) => {
      const kept: Record<string, string> = {};
      for (const q of construction.questions) {
        if (q.id in d && d[q.id].trim() !== (q.answer ?? ""))
          kept[q.id] = d[q.id];
      }
      return kept;
    });
  }, [construction.questions]);
  const editable = researchEditable(construction);
  const questions = construction.questions;
  const changed = changedAnswers(construction, drafts);
  const answered = questions.filter((q) => (q.answer ?? "") !== "").length;
  const [open, toggle, reveal] = useFoldOpen(QUESTIONS_FOLD_KEY, true);
  return (
    <FoldCard
      title={t("Questions")}
      number={1}
      open={open}
      onToggle={toggle}
      summary={
        construction.asking ? (
          <FoldWorking />
        ) : (
          <span className="text-secondary small">
            {questions.length > 0 ? (
              t("{n} of {total} answered", {
                n: answered,
                total: questions.length,
              })
            ) : (
              <T text="None" />
            )}
          </span>
        )
      }
      actions={
        <Button
          size="sm"
          variant={questions.length === 0 ? "primary" : "outline-secondary"}
          disabled={busy || !editable}
          onClick={() => {
            reveal();
            onAsk();
          }}
        >
          {construction.asking ? (
            <Spinner animation="border" size="sm" className="me-2" />
          ) : (
            <IconHelpCircle className="icon-2" />
          )}
          {questions.length > 0 ? t("Ask again") : t("Ask questions")}
        </Button>
      }
    >
      {construction.asking && (
        <Alert variant="info" className="d-flex align-items-center gap-2">
          <Spinner animation="border" size="sm" />
          <T text="The construction agent is reading the specification and writing its questions." />
        </Alert>
      )}
      {questions.length === 0 && !construction.asking && (
        <div className="text-secondary">
          <T text="The construction agent asks what the specification leaves open, so the mockups and the phases follow your answers." />
        </div>
      )}
      {questions.length > 0 && (
        <>
          <p className="text-secondary">
            <T text="Answer what you can and skip the rest. The answers go with the specification to the mockups, the phases and every task. Asking again keeps the answered questions." />
          </p>
          <ol className="research-questions">
            {questions.map((q, i) => {
              const value = drafts[q.id] ?? q.answer ?? "";
              return (
                <li key={q.id}>
                  <Form.Label
                    htmlFor={`question-${q.id}`}
                    className="research-question"
                  >
                    <span className="mockup-number mockup-number-sm">
                      {i + 1}
                    </span>
                    <span>{q.text}</span>
                  </Form.Label>
                  <Form.Control
                    id={`question-${q.id}`}
                    as="textarea"
                    rows={2}
                    value={value}
                    disabled={!editable}
                    placeholder={t("Your answer (optional)")}
                    onChange={(e) =>
                      setDrafts((d) => ({ ...d, [q.id]: e.target.value }))
                    }
                  />
                </li>
              );
            })}
          </ol>
          <div className="research-footer">
            <span className="text-secondary small">
              {t("{n} of {total} answered", {
                n: answered,
                total: questions.length,
              })}
              {changed.length > 0 && (
                <span className="spec-dirty">
                  <T text="Unsaved answers" />
                </span>
              )}
            </span>
            <Button
              variant={changed.length > 0 ? "primary" : "outline-secondary"}
              disabled={busy || !editable || changed.length === 0}
              onClick={() => onAnswer(changed)}
            >
              <IconCheck className="icon-2" />
              <T text="Save answers" />
            </Button>
          </div>
        </>
      )}
    </FoldCard>
  );
}

/** The feedback step (AC9): give feedback on a phase that built something,
 * recorded until approved; approving it has the construction agent plan it as
 * requirements and follow-up tasks on the phase. Below, the feedback given,
 * newest first, each with its plan and the state of its tasks. The tasks wait
 * for the admin to run them. */
export function FeedbackCard({
  construction,
  busy,
  initialPhase,
  onGive,
  onPlan,
  onDelete,
  onRun,
}: {
  construction: Construction;
  busy: boolean;
  /** The phase picked when the form opens, if it can take feedback. */
  initialPhase: Id | null;
  /** Give it; answers whether the server took it, so the form clears. */
  onGive: (phase: Id, title: string, description: string) => Promise<boolean>;
  onPlan: (feedback: ConstructionFeedback) => void;
  onDelete: (feedback: ConstructionFeedback) => void;
  onRun: (phase: ConstructionPhase) => void;
}) {
  const { t } = useT();
  const phases = feedbackPhases(construction);
  const [phase, setPhase] = useState<Id>(
    phases.find((p) => sameId(p.id, initialPhase))?.id ??
      phases[phases.length - 1]?.id ??
      ""
  );
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const editable = feedbackEditable(construction);
  const chosen: Id =
    phases.find((p) => sameId(p.id, phase))?.id ??
    phases[phases.length - 1]?.id ??
    "";
  const give = () =>
    void onGive(chosen, title.trim(), description.trim()).then((given) => {
      if (given) {
        setTitle("");
        setDescription("");
      }
    });
  const given = [...construction.feedback].reverse();
  return (
    <>
      <Card className="mb-3">
        <StepHeader title={t("Give feedback")} />
        <Card.Body>
          <p className="text-secondary">
            <T text="Say what should change in what a phase built. Once approved, the construction agent turns it into requirements and follow-up tasks on that phase, which wait for you to review and run them." />
          </p>
          {phases.length === 0 ? (
            <div className="text-secondary">
              <T text="Feedback is on what a phase built: run a phase first." />
            </div>
          ) : (
            <>
              <Form.Group className="mb-3" controlId="feedback-phase">
                <Form.Label>
                  <T text="Phase" />
                </Form.Label>
                <Form.Select
                  value={String(chosen)}
                  disabled={busy}
                  onChange={(e) => {
                    // The option's value is the id as text; the phase's own
                    // id is what is sent.
                    const value = e.target.value;
                    setPhase(
                      phases.find((p) => sameId(p.id, value))?.id ?? value
                    );
                  }}
                >
                  {phases.map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.title}
                    </option>
                  ))}
                </Form.Select>
              </Form.Group>
              <Form.Group className="mb-3" controlId="feedback-title">
                <Form.Label>
                  <T text="Title" />
                </Form.Label>
                <Form.Control
                  value={title}
                  disabled={busy}
                  placeholder={t("e.g. Sort the contacts by name")}
                  onChange={(e) => setTitle(e.target.value)}
                />
              </Form.Group>
              <Form.Group className="mb-3" controlId="feedback-description">
                <Form.Label>
                  <T text="What should change" />
                </Form.Label>
                <Form.Control
                  as="textarea"
                  rows={4}
                  value={description}
                  disabled={busy}
                  placeholder={t(
                    "What you saw, and what you expected instead."
                  )}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </Form.Group>
              <div className="d-flex justify-content-end">
                <Button
                  variant="primary"
                  disabled={busy || !title.trim() || !description.trim()}
                  onClick={give}
                >
                  <IconMessagePlus className="icon-2" />
                  <T text="Give feedback" />
                </Button>
              </div>
            </>
          )}
        </Card.Body>
      </Card>
      {given.length > 0 && (
        <Card className="mb-3">
          <StepHeader title={t("Feedback given")} />
          <Card.Body>
            <ul className="feedback-list">
              {given.map((feedback) => (
                <FeedbackItem
                  key={feedback.id}
                  construction={construction}
                  feedback={feedback}
                  busy={busy}
                  editable={editable}
                  onPlan={() => onPlan(feedback)}
                  onDelete={() => onDelete(feedback)}
                  onRun={onRun}
                />
              ))}
            </ul>
          </Card.Body>
        </Card>
      )}
    </>
  );
}

/** One feedback: its phase, what was said, what the agent made of it, and
 * what can be done with it now. */
function FeedbackItem({
  construction,
  feedback,
  busy,
  editable,
  onPlan,
  onDelete,
  onRun,
}: {
  construction: Construction;
  feedback: ConstructionFeedback;
  busy: boolean;
  /** Whether approved feedback can be planned, run or deleted now. */
  editable: boolean;
  onPlan: () => void;
  onDelete: () => void;
  onRun: (phase: ConstructionPhase) => void;
}) {
  const { t } = useT();
  const href = useHref();
  const phase = construction.phases.find((p) => sameId(p.id, feedback.phase));
  const tasks = feedbackTasks(construction, feedback);
  // Its planning ended without tasks: the reason is the construction's last
  // error, unless a run of the tasks has failed or stopped since.
  const unplanned =
    feedback.status === "open" && !feedback.planning && !!feedback.run;
  const reason =
    unplanned && !["failed", "stopped"].includes(construction.status)
      ? construction.error
      : null;
  return (
    <li className="feedback-item">
      <div className="feedback-head">
        <strong>{feedback.title}</strong>
        {feedback.planning ? (
          <StatusBadge tone="blue" className="text-nowrap">
            <Spinner animation="border" size="sm" className="me-1" />
            <T text="Planning" />
          </StatusBadge>
        ) : (
          <Status status={feedback.status} />
        )}
        {phase && (
          <a className="text-secondary small" href={href(phasePath(phase.id))}>
            {t("Phase “{phase}”", { phase: phase.title })}
          </a>
        )}
      </div>
      <div className="feedback-description">{feedback.description}</div>
      {unplanned && (
        <Alert variant="warning" className="mt-2 mb-0">
          <T text="The construction agent did not plan it." /> {reason}
        </Alert>
      )}
      {feedback.requirements.length > 0 && (
        <ul className="pd-reqs mt-2">
          {feedback.requirements.map((req, r) => (
            <li key={r}>
              <Priority priority={req.priority} />
              <span>
                <InlineCode text={req.text} />
              </span>
            </li>
          ))}
        </ul>
      )}
      {tasks.length > 0 && (
        <ol className="feedback-tasks">
          {tasks.map((task) => (
            <li key={task.id}>
              <InlineCode text={task.title} />
              <Status status={task.status} />
            </li>
          ))}
        </ol>
      )}
      <div className="feedback-actions">
        {feedback.status === "given" && (
          <>
            <Button
              size="sm"
              variant="primary"
              disabled={busy || !editable}
              onClick={onPlan}
            >
              <IconCheck className="icon-2" />
              <T text="Approve" />
            </Button>
            <Button
              size="sm"
              variant="outline-danger"
              disabled={busy}
              onClick={onDelete}
            >
              <IconTrash className="icon-2" />
              <T text="Decline" />
            </Button>
          </>
        )}
        {feedback.status === "open" && !feedback.planning && (
          <Button
            size="sm"
            variant="outline-primary"
            disabled={busy || !editable}
            onClick={onPlan}
          >
            <IconRefresh className="icon-2" />
            <T text="Plan again" />
          </Button>
        )}
        {feedback.status === "planned" && phase && (
          <Button
            size="sm"
            variant="primary"
            disabled={busy || !editable}
            onClick={() => onRun(phase)}
          >
            <IconPlayerPlay className="icon-2" />
            <T text="Run" />
          </Button>
        )}
        {feedback.status !== "given" &&
          feedbackDeletable(construction, feedback) && (
            <Button
              size="sm"
              variant="outline-danger"
              disabled={busy || !editable}
              onClick={onDelete}
            >
              <IconTrash className="icon-2" />
              <T text="Delete" />
            </Button>
          )}
      </div>
    </li>
  );
}

/** The research step's mockups (AC7): the last round's mockups, each a few
 * screens in a sandboxed frame, to choose one of or to comment on and have
 * revised. Earlier rounds are listed below, and the chosen one may be among
 * them. */
export function MockupsCard({
  construction,
  busy,
  onRevise,
  onChoose,
  onPlan,
  onPropose,
  loadScreens,
}: {
  construction: Construction;
  busy: boolean;
  /** The first round, from the specification. */
  onPropose: () => void;
  onRevise: (
    feedback: { mockup: Id; text: string }[],
    note: string | null
  ) => void;
  onChoose: (mockup: Id) => void;
  onPlan: () => void;
  /** Fetches a mockup's screens with their HTML, which the construction
   * leaves out. */
  loadScreens: MockupScreensLoader;
}) {
  const { t } = useT();
  const [width, setWidth] = useState<MockupWidth>("desktop");
  // The admin's feedback per mockup, and a note on all of them, until sent.
  const [notes, setNotes] = useState<Record<string, string>>({});
  const [note, setNote] = useState("");
  const latest = latestMockups(construction);
  const chosen = chosenMockup(construction);
  const editable = researchEditable(construction);
  const feedback = revisionFeedback(construction, notes);
  const earlier = construction.mockups.filter((m) => !latest.includes(m));
  const hasPhases = construction.phases.length > 0;
  // Set when the admin picks a different mockup after the phases were
  // planned. The phases still follow the old mockup, so we suggest planning
  // them again. Reset as soon as planning starts.
  const [rechosen, setRechosen] = useState(false);
  useEffect(() => {
    if (construction.planning) setRechosen(false);
  }, [construction.planning]);
  const choose = (mockup: Id) => {
    if (hasPhases && !sameId(mockup, chosen?.id)) setRechosen(true);
    onChoose(mockup);
  };
  // A new round arriving clears what was written about the last one.
  const round = latest[0]?.round ?? 0;
  useEffect(() => {
    setNotes({});
    setNote("");
  }, [round]);

  const [open, toggle, reveal] = useFoldOpen(MOCKUPS_FOLD_KEY, true);
  return (
    <>
      <FoldCard
        title={t("Mockups")}
        number={2}
        open={open}
        onToggle={toggle}
        summary={
          construction.designing ? (
            <FoldWorking />
          ) : chosen ? (
            <span className="text-nowrap">
              <StatusBadge tone="green" className="me-2">
                <T text="Chosen" />
              </StatusBadge>
              {chosen.title}
            </span>
          ) : latest.length === 0 ? (
            <span className="text-secondary small">
              <T text="None" />
            </span>
          ) : null
        }
        actions={
          <>
            {latest.length > 0 && (
              <div className="btn-group" role="group" aria-label={t("Width")}>
                {(["desktop", "phone"] as const).map((w) => (
                  <Button
                    key={w}
                    size="sm"
                    variant={width === w ? "secondary" : "outline-secondary"}
                    aria-pressed={width === w}
                    onClick={() => setWidth(w)}
                  >
                    {w === "desktop" ? (
                      <IconDeviceDesktop className="icon-2" />
                    ) : (
                      <IconDeviceMobile className="icon-2" />
                    )}
                    {w === "desktop" ? t("Desktop") : t("Phone")}
                  </Button>
                ))}
              </div>
            )}
            {latest.length === 0 && (
              <Button
                size="sm"
                variant="primary"
                disabled={busy || !editable}
                onClick={() => {
                  reveal();
                  onPropose();
                }}
              >
                {construction.designing ? (
                  <Spinner animation="border" size="sm" className="me-2" />
                ) : (
                  <IconLayoutDashboard className="icon-2" />
                )}
                <T text="Propose mockups" />
              </Button>
            )}
          </>
        }
      >
        {construction.designing && (
          <Alert variant="info" className="d-flex align-items-center gap-2">
            <Spinner animation="border" size="sm" />
            <T text="The construction agent is drawing the mockups. This takes a minute or two." />
          </Alert>
        )}
        {latest.length === 0 && !construction.designing && (
          <div className="text-secondary">
            {construction.questions.length === 0 ? (
              <T text="A few different looks of the main screens, drawn from the specification. Ask the questions first to have your answers in them." />
            ) : (
              <T text="A few different looks of the main screens, drawn from the specification and your answers, for you to choose one or say what should change." />
            )}
          </div>
        )}
        {rechosen && hasPhases && chosen && (
          <Alert variant="warning" className="d-flex align-items-center gap-3">
            <div className="flex-fill">
              <T text="The phases were planned for another mockup. Plan them again so they follow this one." />
            </div>
            <Button
              size="sm"
              variant="primary"
              disabled={busy || !editable}
              onClick={onPlan}
            >
              <IconSparkles className="icon-2" />
              <T text="Plan phases again" />
            </Button>
          </Alert>
        )}
        {chosen && !latest.includes(chosen) && (
          <Alert variant="secondary">
            {t("The chosen mockup is “{title}”, from an earlier round.", {
              title: chosen.title,
            })}
          </Alert>
        )}
        {latest.length > 1 && (
          // The mockups are alternatives: say so, and name them by number,
          // with a way to each, as the second is a long way down.
          <div className="mockup-compare">
            <span className="text-secondary">
              {t(
                "{n} alternatives: look at each, then choose one or say what should change.",
                {
                  n: latest.length,
                }
              )}
            </span>
            <div className="mockup-compare-links">
              {latest.map((mockup, i) => (
                <Button
                  key={mockup.id}
                  size="sm"
                  variant="outline-secondary"
                  onClick={() =>
                    document
                      .getElementById(mockupAnchor(mockup.id))
                      ?.scrollIntoView({ behavior: "smooth", block: "start" })
                  }
                >
                  <span className="mockup-number mockup-number-sm">
                    {i + 1}
                  </span>
                  {mockup.title}
                </Button>
              ))}
            </div>
          </div>
        )}
        <div
          className={
            width === "phone" ? "mockup-grid mockup-grid-phone" : "mockup-grid"
          }
        >
          {latest.map((mockup, i) => (
            <MockupPanel
              key={mockup.id}
              mockup={mockup}
              number={i + 1}
              total={latest.length}
              loadScreens={loadScreens}
              width={width}
              busy={busy || !editable}
              feedback={notes[mockup.id] ?? ""}
              onFeedback={(text) =>
                setNotes((n) => ({ ...n, [mockup.id]: text }))
              }
              onChoose={() => choose(mockup.id)}
            />
          ))}
        </div>
        {latest.length > 0 && (
          <div className="mockup-revise">
            <Form.Label htmlFor="mockup-note">
              <T text="A note on all of them (optional)" />
            </Form.Label>
            <Form.Control
              id="mockup-note"
              as="textarea"
              rows={2}
              value={note}
              disabled={!editable}
              placeholder={t("e.g. fewer screens, a sidebar instead of tabs")}
              onChange={(e) => setNote(e.target.value)}
            />
            <div className="d-flex justify-content-between align-items-center mt-2">
              <span className="text-secondary small">
                <T text="Revising proposes a new round from your feedback; this round stays below." />
              </span>
              <Button
                variant="outline-primary"
                disabled={
                  busy ||
                  !editable ||
                  (feedback.length === 0 && note.trim() === "")
                }
                onClick={() => onRevise(feedback, note.trim() || null)}
              >
                <IconRefresh className="icon-2" />
                <T text="Revise" />
              </Button>
            </div>
          </div>
        )}
        {earlier.length > 0 && (
          <details className="mockup-earlier">
            <summary>
              {t("Earlier rounds ({n})", { n: earlier.length })}
            </summary>
            <ul className="list-unstyled mb-0 mt-2">
              {earlier.map((m) => (
                <li key={m.id} className="d-flex align-items-start gap-2 mb-2">
                  <span className="badge bg-secondary-lt">
                    {t("Round {n}", { n: m.round })}
                  </span>
                  <div className="flex-fill">
                    <strong>{m.title}</strong>
                    {m.feedback && (
                      <div className="text-secondary small">{m.feedback}</div>
                    )}
                  </div>
                  {m.status === "chosen" ? (
                    <StatusBadge tone="green">{t("Chosen")}</StatusBadge>
                  ) : (
                    <Button
                      size="sm"
                      variant="outline-secondary"
                      disabled={busy || !editable}
                      onClick={() => choose(m.id)}
                    >
                      <T text="Choose" />
                    </Button>
                  )}
                </li>
              ))}
            </ul>
          </details>
        )}
      </FoldCard>
      {/* The research's way on, under both of its parts. */}
      <div className="research-plan">
        <Button
          variant={
            chosen && (!hasPhases || rechosen) ? "primary" : "outline-primary"
          }
          disabled={busy || !editable}
          onClick={onPlan}
        >
          <IconSparkles className="icon-2" />
          {hasPhases ? t("Plan phases again") : t("Plan phases")}
        </Button>
      </div>
    </>
  );
}

/** One mockup: a strip of its screens in miniature, the one picked large in a
 * sandboxed frame with previous and next, its rationale, the admin's feedback
 * on it, and Choose. The miniatures are there so a mockup's other screens are
 * seen at a glance, not found behind a row of tabs. */
function MockupPanel({
  mockup,
  number,
  total,
  loadScreens,
  width,
  busy,
  feedback,
  onFeedback,
  onChoose,
}: {
  mockup: ConstructionMockup;
  /** Its place among the round's mockups, from 1, and how many there are. */
  number: number;
  total: number;
  loadScreens: MockupScreensLoader;
  width: MockupWidth;
  busy: boolean;
  feedback: string;
  onFeedback: (text: string) => void;
  onChoose: () => void;
}) {
  const { t } = useT();
  const [picked, setPicked] = useState(0);
  const count = mockup.screens.length;
  const screen = Math.min(picked, count - 1);
  const shown = mockup.screens[screen];
  const isChosen = mockup.status === "chosen";
  const pages = useMockupPages(mockup.id, loadScreens);
  const page = (i: number) => (Array.isArray(pages) ? pages[i] : undefined);
  return (
    <section
      id={mockupAnchor(mockup.id)}
      className={`mockup-panel${isChosen ? " mockup-chosen" : ""}`}
      aria-label={t("Mockup {n} of {total}", { n: number, total })}
    >
      <div className="d-flex align-items-start gap-3 mb-2">
        {total > 1 && (
          <span className="mockup-number" aria-hidden="true">
            {number}
          </span>
        )}
        <div className="flex-fill">
          {total > 1 && (
            <div className="mockup-pretitle">
              {t("Mockup {n} of {total}", { n: number, total })}
            </div>
          )}
          <h3 className="mb-1">
            {mockup.title}
            {count > 1 && (
              <span className="badge bg-blue-lt ms-2 align-middle">
                {t("{n} screens", { n: count })}
              </span>
            )}
          </h3>
          {mockup.rationale && (
            <div className="text-secondary">{mockup.rationale}</div>
          )}
        </div>
        {isChosen && (
          <StatusBadge tone="green">
            <IconCheck className="icon-2 me-1" />
            {t("Chosen")}
          </StatusBadge>
        )}
      </div>
      {count > 1 && (
        <div
          className="mockup-strip"
          role="tablist"
          aria-label={t("Screens of {title}", { title: mockup.title })}
        >
          {mockup.screens.map((s, i) => (
            <button
              key={s.file}
              type="button"
              role="tab"
              aria-selected={i === screen}
              className={`mockup-thumb${i === screen ? " active" : ""}`}
              onClick={() => setPicked(i)}
            >
              <span className="mockup-thumb-view" aria-hidden="true">
                {/* A picture of the screen, not a page to use: no focus, no
                    pointer, and the same sandbox as the large frame. */}
                {page(i) !== undefined && (
                  <iframe
                    title={s.name}
                    sandbox=""
                    tabIndex={-1}
                    loading="lazy"
                    srcDoc={mockupDocument(page(i) ?? "")}
                  />
                )}
              </span>
              <span className="mockup-thumb-name">
                <span className="mockup-thumb-number">{i + 1}</span>
                {s.name}
              </span>
            </button>
          ))}
        </div>
      )}
      {shown && (
        <div className="mockup-screen-bar">
          <strong>{shown.name}</strong>
          {count > 1 && (
            <div className="d-flex align-items-center gap-2">
              <span className="text-secondary small">
                {t("Screen {n} of {total}", { n: screen + 1, total: count })}
              </span>
              <div className="btn-group">
                <Button
                  size="sm"
                  variant="outline-secondary"
                  aria-label={t("Previous screen")}
                  disabled={screen === 0}
                  onClick={() => setPicked(screen - 1)}
                >
                  <IconChevronLeft className="icon-2 m-0" />
                </Button>
                <Button
                  size="sm"
                  variant="outline-secondary"
                  aria-label={t("Next screen")}
                  disabled={screen === count - 1}
                  onClick={() => setPicked(screen + 1)}
                >
                  <IconChevronRight className="icon-2 m-0" />
                </Button>
              </div>
            </div>
          )}
        </div>
      )}
      {shown && (
        <div className="mockup-frame-wrap">
          {pages === "failed" ? (
            <Alert variant="danger" className="m-3 align-self-start">
              <T text="Could not load the screens of this mockup." />
            </Alert>
          ) : page(screen) === undefined ? (
            <div className="mockup-frame-loading" role="status">
              <Spinner animation="border" size="sm" />
              <T text="Loading…" />
            </div>
          ) : (
            // No `allow-*` at all: the page runs no script, submits no form,
            // opens no window, and has an origin of its own.
            <iframe
              className="mockup-frame"
              title={t("{mockup}: {screen}", {
                mockup: mockup.title,
                screen: shown.name,
              })}
              sandbox=""
              srcDoc={mockupDocument(page(screen) ?? "")}
              style={{
                width: width === "phone" ? MOCKUP_WIDTHS.phone : "100%",
              }}
            />
          )}
        </div>
      )}
      {/* The decision under the screens, where the admin is after looking
          at them: what should change, or this one. */}
      <div className="mockup-decide">
        <Form.Control
          as="textarea"
          rows={2}
          value={feedback}
          disabled={busy}
          aria-label={t("Feedback on {title}", { title: mockup.title })}
          placeholder={t("What should change in this one?")}
          onChange={(e) => onFeedback(e.target.value)}
        />
        {!isChosen && (
          <Button variant="outline-primary" disabled={busy} onClick={onChoose}>
            <IconCheck className="icon-2" />
            {total > 1
              ? t("Choose mockup {n}", { n: number })
              : t("Choose this one")}
          </Button>
        )}
      </div>
    </section>
  );
}

/** The id of a mockup's panel, to scroll to. */
function mockupAnchor(mockup: Id): string {
  return `mockup-${mockup}`;
}

/** The HTML of `mockup`'s screens, in order: `null` while it loads, or
 * `"failed"`. Fetched once per mockup ({@link mockupPages}). */
function useMockupPages(
  mockup: Id,
  load: MockupScreensLoader
): string[] | null | "failed" {
  const [pages, setPages] = useState<string[] | null | "failed">(null);
  useEffect(() => {
    let live = true;
    setPages(null);
    mockupPages(load, mockup).then(
      (got) => live && setPages(got),
      () => live && setPages("failed")
    );
    return () => {
      live = false;
    };
    // `load` is a new function on every render of the screen; the mockup
    // decides what is fetched.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mockup]);
  return pages;
}

/** A question put to the admin before an action: what happens, the action,
 * and optionally another step to take instead. */
type Confirmation = {
  title: string;
  body: string;
  confirm: string;
  /** The action discards something. */
  danger?: boolean;
  onConfirm: () => void;
  alternative?: { label: string; onClick: () => void };
};

/** One confirmation dialog for the screen: `confirm` opens it, and `modal` is
 * rendered once. */
function useConfirmation() {
  const [pending, setPending] = useState<Confirmation | null>(null);
  const close = () => setPending(null);
  const modal = pending && (
    <Modal show onHide={close} centered>
      <Modal.Header closeButton>
        <Modal.Title>{pending.title}</Modal.Title>
      </Modal.Header>
      <Modal.Body>{pending.body}</Modal.Body>
      <Modal.Footer>
        <Button variant="outline-secondary" onClick={close}>
          <T text="Cancel" />
        </Button>
        {pending.alternative && (
          <Button
            variant="outline-primary"
            onClick={() => {
              close();
              pending.alternative?.onClick();
            }}
          >
            {pending.alternative.label}
          </Button>
        )}
        <Button
          variant={pending.danger ? "danger" : "primary"}
          onClick={() => {
            close();
            pending.onConfirm();
          }}
        >
          {pending.confirm}
        </Button>
      </Modal.Footer>
    </Modal>
  );
  return { confirm: setPending, modal };
}

/** A phase's Run: it runs the phases before it first, so when any of those is
 * not done the admin is asked, with them named, before anything starts. */
function useConfirmedRun(
  construction: Construction | null,
  run: (until: Id) => void
) {
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
          {t(
            "“{phase}” builds on the phases before it. These are not done yet, and run first:",
            {
              phase: asking?.title ?? "",
            }
          )}
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
  onReplan,
}: {
  construction: Construction;
  busy: boolean;
  onSave: (phases: PhaseDraft[]) => void;
  onRun: (until: Id | null) => void;
  /** Plan the phases again (the screen confirms it): a phase without tasks
   * is replaced, one with tasks kept. */
  onReplan: () => void;
}) {
  const { t } = useT();
  const href = useHref();
  const now = currentWork(construction);
  const [drafts, setDrafts] = useState<PhaseDraft[]>(() =>
    toDrafts(construction.phases)
  );
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
  // Not over unsaved edits: the new plan would replace them.
  const canReplan = researchEditable(construction) && !changed;

  return (
    <Card className="mb-3">
      <StepHeader title={t("Phases")}>
        {changed && (
          <Button size="sm" disabled={busy || !editable} onClick={save}>
            <T text="Save phases" />
          </Button>
        )}
        {construction.phases.length > 0 && (
          <Button
            size="sm"
            variant="outline-secondary"
            disabled={busy || !canReplan}
            onClick={onReplan}
          >
            {construction.planning ? (
              <Spinner animation="border" size="sm" className="me-2" />
            ) : (
              <IconSparkles className="icon-2" />
            )}
            <T text="Plan phases again" />
          </Button>
        )}
        <Button
          size="sm"
          variant="primary"
          disabled={busy || !canRun}
          onClick={() => onRun(null)}
        >
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
                saved={
                  construction.phases.find((p) => sameId(p.id, draft.id)) ??
                  null
                }
                working={
                  now !== null && sameId(now.phase.id, draft.id) ? now : null
                }
                href={
                  draft.id !== null && draft.id !== undefined
                    ? href(phasePath(draft.id))
                    : null
                }
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
                onRequirement={(r, change) =>
                  setDrafts(updateRequirement(drafts, i, r, change))
                }
                onRemoveRequirement={(r) =>
                  setDrafts(removeRequirement(drafts, i, r))
                }
                onRun={() => {
                  const phase = construction.phases.find((p) =>
                    sameId(p.id, draft.id)
                  );
                  if (phase) confirmed.request(phase);
                }}
              />
            ))}
          </ol>
        )}
        {confirmed.modal}
        {editable && (
          <button
            type="button"
            className="phase-add"
            onClick={() => setDrafts(addPhase(drafts))}
          >
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
  onRequirement: (
    r: number,
    change: Partial<PhaseDraft["requirements"][number]>
  ) => void;
  onRemoveRequirement: (r: number) => void;
  onRun: () => void;
}) {
  const { t } = useT();
  // A phase not saved yet has no status of its own; one whose tasks are being
  // planned has none yet either, but is running.
  const status = working ? "running" : saved?.status ?? "new";
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
              title={
                deletable
                  ? t("Delete phase")
                  : t("A phase that has tasks cannot be deleted")
              }
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
                title={
                  dirty ? t("Save the phases before opening one") : undefined
                }
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
                  onChange={(e) =>
                    onRequirement(r, { priority: e.target.value })
                  }
                >
                  {PRIORITIES.map((p) => (
                    <option key={p} value={p}>
                      {p === "must"
                        ? t("Must")
                        : p === "should"
                        ? t("Should")
                        : t("Could")}
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
          <button
            type="button"
            className="phase-add-req"
            onClick={onAddRequirement}
          >
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
  const run =
    now.kind === "task" ? now.task.runs[now.task.runs.length - 1] : undefined;
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

/** The activity (AC10): what the construction is doing now, the errors it
 * met, newest first, and how far its phases have got. Spans every step: the
 * runner's work and the construction agent's (phases, mockups, questions,
 * feedback). */
export function ActivityCard({
  construction,
  busy,
  onRetry,
  onSkip,
  opened,
}: {
  construction: Construction;
  busy: boolean;
  onRetry: (task: ConstructionTask) => void;
  onSkip: (task: ConstructionTask) => void;
  /** Cards to start open, over what this browser remembers. */
  opened?: Partial<ActivityFold>;
}) {
  const { t } = useT();
  const [open, toggle] = useActivityOpen(opened);
  const now = currentWork(construction);
  const agent = agentWork(construction);
  const working = !!now || !!agent;
  const { done, total } = progress(construction);
  const errorCount = construction.errors.length;
  return (
    <>
      <FoldCard
        title={t("Now")}
        open={open.now}
        onToggle={() => toggle("now")}
        summary={
          working ? (
            <FoldWorking />
          ) : (
            <span className="text-secondary small">
              <T text="Nothing is running" />
            </span>
          )
        }
      >
        {agent && <AgentWorking construction={construction} work={agent} />}
        {now && <NowWorking now={now} />}
        {/* What stopped it, while nothing runs: the Errors card starts
            closed, and a failed build should say why where the admin looks
            first. */}
        {!working && construction.error && (
          <Alert
            variant={construction.status === "failed" ? "danger" : "warning"}
          >
            <ClampedText
              className="activity-error-message"
              text={construction.error}
            />
          </Alert>
        )}
        {!working && (
          <div className="text-secondary">
            <T text="Nothing is running. Planning, mockups, questions, feedback and the build show here while they run." />
          </div>
        )}
      </FoldCard>
      <FoldCard
        title={t("Errors")}
        open={open.errors}
        onToggle={() => toggle("errors")}
        summary={
          errorCount > 0 ? (
            <StatusBadge
              tone={construction.error ? "red" : "secondary"}
              className="text-nowrap"
            >
              {errorCount}
            </StatusBadge>
          ) : (
            <span className="text-secondary small">
              <T text="None" />
            </span>
          )
        }
      >
        {errorCount > 0 ? (
          <ErrorList construction={construction} />
        ) : (
          <div className="text-secondary">
            <T text="No errors so far." />
          </div>
        )}
      </FoldCard>
      <FoldCard
        title={t("Phases")}
        open={open.phases}
        onToggle={() => toggle("phases")}
        summary={
          <span className="text-secondary small">
            {t("{done} of {total} phases done", { done, total })}
          </span>
        }
        meter={{ done, total }}
      >
        {construction.phases.length > 0 ? (
          <PhasesProgress
            construction={construction}
            now={now}
            busy={busy}
            onRetry={onRetry}
            onSkip={onSkip}
          />
        ) : (
          <div className="text-secondary">
            <T text="No phases yet." />
          </div>
        )}
      </FoldCard>
    </>
  );
}

/** Which of the activity's cards are open: the first time only Now, then as
 * the admin left them, in this browser. */
export type ActivityFold = { now: boolean; errors: boolean; phases: boolean };
const ACTIVITY_FOLD_KEY = "feldspar.construct.activity.open";

function useActivityOpen(
  opened: Partial<ActivityFold> = {}
): [ActivityFold, (card: keyof ActivityFold) => void] {
  const [open, setOpen] = useState<ActivityFold>(() => {
    const first = { now: true, errors: false, phases: false };
    try {
      const stored = JSON.parse(
        window.localStorage.getItem(ACTIVITY_FOLD_KEY) ?? "null"
      );
      const remembered = stored && typeof stored === "object" ? stored : {};
      return { ...first, ...remembered, ...opened };
    } catch {
      return { ...first, ...opened };
    }
  });
  const toggle = (card: keyof ActivityFold) =>
    setOpen((was) => {
      const next = { ...was, [card]: !was[card] };
      try {
        window.localStorage.setItem(ACTIVITY_FOLD_KEY, JSON.stringify(next));
      } catch {
        // Not remembered, then: private windows and blocked storage.
      }
      return next;
    });
  return [open, toggle];
}

const QUESTIONS_FOLD_KEY = "feldspar.construct.research.questions.open";
const MOCKUPS_FOLD_KEY = "feldspar.construct.research.mockups.open";

/** A fold card's header while something works on what it holds. */
function FoldWorking() {
  return (
    <span className="text-nowrap">
      <Spinner animation="border" size="sm" className="me-1" />
      <T text="Working" />
    </span>
  );
}

/** Whether one card is open, as the admin left it in this browser, `first`
 * until they have; `open()` opens it (an action in its header was used). */
function useFoldOpen(
  key: string,
  first: boolean
): [boolean, () => void, () => void] {
  const [open, setOpen] = useState<boolean>(() => {
    try {
      const stored = JSON.parse(window.localStorage.getItem(key) ?? "null");
      return typeof stored === "boolean" ? stored : first;
    } catch {
      return first;
    }
  });
  const set = (next: boolean) => {
    setOpen(next);
    try {
      window.localStorage.setItem(key, JSON.stringify(next));
    } catch {
      // Not remembered, then: private windows and blocked storage.
    }
  };
  return [open, () => set(!open), () => set(true)];
}

/** A card whose header opens and closes it, with a summary that stays in
 * the header, and optionally a progress meter under it, so a closed card
 * still says what it holds. */
function FoldCard({
  title,
  number,
  open,
  onToggle,
  summary,
  actions,
  meter,
  children,
}: {
  title: string;
  /** Its place in the step, when the step has parts to take in order. */
  number?: number;
  open: boolean;
  onToggle: () => void;
  summary: ReactNode;
  /** Buttons beside the header's toggle, not in it: a button cannot hold
   * buttons. They work with the card closed too. */
  actions?: ReactNode;
  meter?: { done: number; total: number };
  children: ReactNode;
}) {
  return (
    <Card className="mb-3 fold-card">
      <Card.Header>
        <button
          type="button"
          className="fold-card-toggle"
          aria-expanded={open}
          onClick={onToggle}
        >
          {open ? (
            <IconChevronDown className="icon-2" />
          ) : (
            <IconChevronRight className="icon-2" />
          )}
          <Card.Title className="mb-0">
            {number !== undefined && (
              <span className="mockup-number mockup-number-sm me-2">
                {number}
              </span>
            )}
            {title}
          </Card.Title>
          <span className="ms-auto">{summary}</span>
        </button>
        {actions && <div className="fold-card-actions btn-list">{actions}</div>}
      </Card.Header>
      {meter && (
        <div
          className="build-meter"
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={meter.total}
          aria-valuenow={meter.done}
        >
          <div
            style={{
              width: `${meter.total ? (100 * meter.done) / meter.total : 0}%`,
            }}
          />
        </div>
      )}
      {open && <Card.Body>{children}</Card.Body>}
    </Card>
  );
}

/** The line that says what the construction agent is doing, outside the
 * runner, with a link to its run when it is known. */
function AgentWorking({
  construction,
  work,
}: {
  construction: Construction;
  work: AgentWork;
}) {
  const { t } = useT();
  const [text, run] = (() => {
    switch (work.kind) {
      case "phases":
        return [t("Planning the phases."), construction.plan_run ?? null];
      case "mockups":
        return [t("Proposing mockups."), null];
      case "questions":
        return [t("Asking questions about the specification."), null];
      case "feedback":
        return [
          t("Planning the feedback “{title}”.", { title: work.feedback.title }),
          work.feedback.run ?? null,
        ];
    }
  })();
  return (
    <div className="build-now mb-0">
      <Spinner animation="border" size="sm" />
      <span className="build-now-text">{text}</span>
      {run && (
        <a className="build-now-link" href={runHref(run)}>
          <T text="Watch the run" />
        </a>
      )}
    </div>
  );
}

/** How many errors show before "Show all". */
const ERRORS_SHOWN = 5;

/** The errors the construction met, newest first: when, what failed, why,
 * and its run. The one its last step ended with is marked current. */
export function ErrorList({ construction }: { construction: Construction }) {
  const { t } = useT();
  const [all, setAll] = useState(false);
  const shown = all
    ? construction.errors
    : construction.errors.slice(0, ERRORS_SHOWN);
  return (
    <>
      <ul className="activity-errors">
        {shown.map((entry) => (
          <ErrorRow key={entry.id} construction={construction} entry={entry} />
        ))}
      </ul>
      {!all && construction.errors.length > ERRORS_SHOWN && (
        <Button
          size="sm"
          variant="link"
          className="px-0"
          onClick={() => setAll(true)}
        >
          {t("Show all {n}", { n: construction.errors.length })}
        </Button>
      )}
    </>
  );
}

function ErrorRow({
  construction,
  entry,
}: {
  construction: Construction;
  entry: ConstructionErrorEntry;
}) {
  const { t } = useT();
  const href = useHref();
  const phase = construction.phases.find((p) => sameId(p.id, entry.phase));
  const task = phase?.tasks.find((x) => sameId(x.id, entry.task));
  const feedback = construction.feedback.find((f) =>
    sameId(f.id, entry.feedback)
  );
  const place: ReactNode = (() => {
    const phaseLink = (label: string) =>
      phase ? <a href={href(phasePath(phase.id))}>{label}</a> : label;
    switch (entry.source) {
      case "phases":
        return t("Planning the phases");
      case "tasks":
        return phaseLink(
          phase
            ? t("Planning the tasks of “{phase}”", { phase: phase.title })
            : t("Planning tasks")
        );
      case "task":
        return phaseLink(
          task ? t("Task “{task}”", { task: task.title }) : t("A task")
        );
      case "mockups":
        return t("Proposing mockups");
      case "questions":
        return t("Asking questions");
      case "feedback":
        return feedback
          ? t("Feedback “{title}”", { title: feedback.title })
          : t("Feedback");
      default:
        return phase
          ? phaseLink(t("Phase “{phase}”", { phase: phase.title }))
          : t("The runner");
    }
  })();
  const current = entry.current;
  return (
    <li className={`activity-error${current ? " activity-error-current" : ""}`}>
      <div className="activity-error-head">
        <span className="activity-error-place">{place}</span>
        {current && (
          <StatusBadge tone="red" className="text-nowrap">
            <T text="Current" />
          </StatusBadge>
        )}
        <span className="text-secondary small ms-auto text-nowrap">
          {historyTime(entry.at)}
        </span>
        {entry.run && (
          <a className="small text-nowrap" href={runHref(entry.run)}>
            <T text="Open the run" />
          </a>
        )}
      </div>
      <ClampedText className="activity-error-message" text={entry.message} />
    </li>
  );
}

/** Each phase with its tasks and their results, the one the runner is on
 * marked. */
function PhasesProgress({
  construction,
  now,
  busy,
  onRetry,
  onSkip,
}: {
  construction: Construction;
  now: CurrentWork | null;
  busy: boolean;
  onRetry: (task: ConstructionTask) => void;
  onSkip: (task: ConstructionTask) => void;
}) {
  const { t } = useT();
  const href = useHref();
  return (
    <ol className="phase-list">
      {construction.phases.map((phase, i) => {
        const tasks = phaseProgress(phase);
        const planning =
          now?.kind === "planning" && sameId(now.phase.id, phase.id);
        return (
          <li
            key={phase.id}
            className={`phase phase-${planning ? "running" : phase.status}${
              now && sameId(now.phase.id, phase.id) ? " phase-working" : ""
            }`}
          >
            <div className="phase-marker" aria-hidden="true">
              {phase.status === "done" ? (
                <IconCheck className="icon-2" />
              ) : (
                i + 1
              )}
            </div>
            <div className="phase-card">
              <div className="phase-head build-phase-head">
                <a
                  className="build-phase-title"
                  href={href(phasePath(phase.id))}
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
                <Button
                  size="sm"
                  variant="outline-primary"
                  disabled={busy}
                  onClick={onRetry}
                >
                  <T text="Retry" />
                </Button>
              )}
              {actions.skip && (
                <Button
                  size="sm"
                  variant="outline-secondary"
                  disabled={busy}
                  onClick={onSkip}
                >
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
      {priority === "must"
        ? t("Must")
        : priority === "should"
        ? t("Should")
        : t("Could")}
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
                  title={t(
                    "Throw these tasks away and have the construction agent plan the phase again, told what to change."
                  )}
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
              {sameId(currentWork(construction)?.phase.id, phase.id) ? (
                <>
                  <Spinner animation="border" size="sm" className="me-2" />
                  <T text="Planning its tasks…" />
                </>
              ) : (
                <>
                  <T text="Not planned yet: the construction agent plans this phase's tasks from its requirements when the run reaches it." />
                  {planning === "plan" && (
                    <div className="mt-3">
                      <Button
                        size="sm"
                        variant="outline-primary"
                        disabled={busy}
                        onClick={() => onPlan(null)}
                      >
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
          {phase.description.trim() && (
            <p className="pd-description">{phase.description}</p>
          )}
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
        <Modal.Title>
          {t("Plan “{phase}” again", { phase: phase.title })}
        </Modal.Title>
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
            placeholder={t(
              "For example: one task for both list screens; no admin screen."
            )}
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
  const fromFeedback =
    task.feedback !== null && task.feedback !== undefined
      ? construction.feedback.find((f) => sameId(f.id, task.feedback))
      : undefined;
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
        {fromFeedback && (
          <span className="build-task-kind" title={fromFeedback.title}>
            <T text="From feedback" />
          </span>
        )}
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
            <Button
              size="sm"
              variant="outline-primary"
              disabled={busy}
              onClick={onRetry}
            >
              <T text="Retry" />
            </Button>
          )}
          {actions.skip && (
            <Button
              size="sm"
              variant="outline-secondary"
              disabled={busy}
              onClick={onSkip}
            >
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
              <a
                key={key}
                className="pd-created"
                href={tableHref(entity.name)}
                title={label}
              >
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
              {t("after {tasks}", {
                tasks: waitsFor.join(", ").replace(/`/g, ""),
              })}
            </span>
          )}
          {/* What it is told and what it answers are on its run's page; a task
              not run yet has no run, so its brief is here. */}
          {task.runs.length === 0 && brief && (
            <button
              type="button"
              className="build-more pd-toggle"
              onClick={() => setOpen(!open)}
            >
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
