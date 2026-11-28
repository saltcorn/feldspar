// The Plan & build screen's decisions (TODO-app-constructor AC4, ported to the
// module's screen in AC13.5), as pure functions so they are tested without a
// browser.
//
// The screen shows one application's construction in five numbered tabs:
// the specification, the research (optional: clarifying questions, AC8, and
// mockups, AC7), the phases, the progress of their tasks, and the admin's
// feedback on what the phases built (AC9). Which one opens follows from the
// construction's status. The phases are edited as a
// draft on the screen and saved whole, as the server's `savePhases` takes
// them.

import {
  sameId,
  type Construction,
  type ConstructionFeedback,
  type ConstructionMockup,
  type ConstructionPhase,
  type ConstructionTask,
  type Id,
  type MockupScreens,
  type PhaseDraft,
  type Requirement,
} from "./types";

export type {
  Construction,
  ConstructionErrorEntry,
  ConstructionFeedback,
  ConstructionMockup,
  ConstructionPhase,
  ConstructionQuestion,
  ConstructionTask,
  Id,
  PhaseDraft,
} from "./types";
export type RequirementDraft = Requirement;

/** The priorities a requirement can have, most important first. */
export const PRIORITIES = ["must", "should", "could"] as const;

/** How often the screen reloads the construction while something runs. */
export const POLL_MS = 2000;

// The screen's own routes, relative to where the admin mounts it (`host.path`
// and `host.href`): `""` is the construction on the current step's tab.

/** The construction opened on its Phases tab. */
export function phasesPath(): string {
  return "/phases";
}

/** One phase's own page: its requirements, and the tasks planned from them. */
export function phasePath(phaseId: Id): string {
  return `${phasesPath()}/${encodeURIComponent(String(phaseId))}`;
}

/** The construction opened on its Feedback tab, with `phaseId` picked for
 * new feedback when given. */
export function feedbackPath(phaseId?: Id): string {
  const base = "/feedback";
  return phaseId !== undefined
    ? `${base}/${encodeURIComponent(String(phaseId))}`
    : base;
}

/** Where a path of the screen points: the construction, opened on a tab or
 * on one phase's page, and the phase new feedback is for. `null` for any
 * other path. */
export function constructRoute(path: string): {
  tab: "phases" | "feedback" | null;
  phaseId: string | null;
  feedbackPhase: string | null;
} | null {
  const feedback = path.match(/^\/feedback(?:\/([^/]+))?\/?$/);
  if (feedback) {
    return {
      tab: "feedback",
      phaseId: null,
      feedbackPhase: feedback[1] ? decodeURIComponent(feedback[1]) : null,
    };
  }
  const match = path.match(/^(?:\/phases(?:\/([^/]+))?)?\/?$/);
  if (!match) return null;
  const phaseId = match[1] ? decodeURIComponent(match[1]) : null;
  return {
    tab: path.startsWith("/phases") && !phaseId ? "phases" : null,
    phaseId,
    feedbackPhase: null,
  };
}

/** Whether the App Constructor can build `app`: React applications only, as
 * the server's `createConstruction` checks. */
export function constructible(app: { framework: { name: string } }): boolean {
  return app.framework.name === "react";
}

/** The step the screen opens on. */
export type Step =
  | "start"
  | "specification"
  | "research"
  | "phases"
  | "activity";

/** Which step is open for `construction` (`null`: the application has none).
 *
 * - None yet: start one.
 * - A draft has no phases: write the specification, or, once questions are
 *   asked or mockups proposed (or being), the research: answer them, choose
 *   a mockup.
 * - Planned: edit the phases, until a task has run. From then on the
 *   activity is what the admin comes back to see.
 * - Running, stopped, failed or done: the activity. */
export function currentStep(construction: Construction | null): Step {
  if (!construction) return "start";
  if (construction.planning) return "phases";
  if (construction.designing || construction.asking) return "research";
  switch (construction.status) {
    case "draft":
      return researched(construction) ? "research" : "specification";
    case "planned":
      return started(construction) ? "activity" : "phases";
    default:
      return "activity";
  }
}

/** What the screen shows for an application with no construction yet: an
 * empty specification to write, before anything is saved. */
export function blankConstruction(appId: string): Construction {
  return {
    id: "",
    application: appId,
    status: "draft",
    specification: "",
    planning: false,
    plan_only: false,
    designing: false,
    asking: false,
    questions: [],
    mockups: [],
    feedback: [],
    errors: [],
    phases: [],
  };
}

/** The screen's tabs, in order: the steps, then the activity, which spans
 * them all. */
export const TABS = [
  "specification",
  "research",
  "phases",
  "feedback",
  "activity",
] as const;
export type Tab = typeof TABS[number];

/** The tab the screen opens on: the current step's, except that a build under
 * way opens on its phases — the plan is what an admin comes back to, and the
 * activity is one click from there. */
export function landingTab(step: Exclude<Step, "start">): Tab {
  return step === "activity" ? "phases" : step;
}

/** Whether `tab` can be opened yet: the specification always, the research
 * once there is a specification to research, the phases once they are planned or
 * being planned, the feedback once a phase has built something to give it
 * on, and the activity always: its cards say what there is, even when that
 * is nothing yet. */
export function tabOpen(construction: Construction, tab: Tab): boolean {
  switch (tab) {
    case "specification":
      return true;
    case "research":
      return (
        construction.specification.trim() !== "" ||
        construction.designing ||
        construction.asking ||
        researched(construction)
      );
    case "phases":
      return construction.planning || construction.phases.length > 0;
    case "feedback":
      return (
        construction.feedback.length > 0 ||
        feedbackPhases(construction).length > 0
      );
    case "activity":
      return true;
  }
}

/** Whether the agent has asked questions or proposed mockups. */
function researched(construction: Construction): boolean {
  return construction.questions.length > 0 || construction.mockups.length > 0;
}

/** Whether any task of the construction has run or been skipped. */
export function started(construction: Construction): boolean {
  return construction.phases.some((p) =>
    p.tasks.some((t) => t.status !== "todo")
  );
}

/** Whether the construction agent is busy with a run for the construction:
 * planning the phases, proposing mockups, asking questions or planning
 * feedback. One at a time. */
export function agentBusy(construction: Construction): boolean {
  return (
    construction.planning ||
    construction.designing ||
    construction.asking ||
    construction.feedback.some((f) => f.planning)
  );
}

/** Whether the screen should keep reloading: while the construction runs, or
 * the construction agent is busy with it. */
export function shouldPoll(construction: Construction | null): boolean {
  return (
    !!construction &&
    (construction.status === "running" || agentBusy(construction))
  );
}

/** Keep `construction` up to date while it runs or is being planned: every
 * `interval` ms `load` is called and its answer handed to `update`, until an
 * answer says there is nothing left to watch. A failed load is reported and
 * tried again, since a server restart is a reason to keep watching.
 *
 * Answers the function that stops the polling, for when the screen closes or
 * a newer answer replaces the one this started from. */
export function pollConstruction(
  construction: Construction | null,
  load: () => Promise<Construction | null>,
  update: (construction: Construction | null) => void,
  failed: (err: unknown) => void,
  interval: number = POLL_MS
): () => void {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let stopped = false;
  const schedule = () => {
    timer = setTimeout(() => void tick(), interval);
  };
  const tick = async () => {
    try {
      const next = await load();
      if (stopped) return;
      update(next);
      if (shouldPoll(next)) schedule();
    } catch (err) {
      if (stopped) return;
      failed(err);
      schedule();
    }
  };
  if (shouldPoll(construction)) schedule();
  return () => {
    stopped = true;
    if (timer) clearTimeout(timer);
  };
}

/** Why the last plan run failed, to show with the specification: the
 * construction's error while it is not being planned or run. Once tasks have
 * run, the error is the activity's to show. */
export function planError(construction: Construction): string | null {
  if (construction.planning || construction.designing || construction.asking)
    return null;
  if (currentStep(construction) === "activity") return null;
  return construction.error ?? null;
}

/** How many rows a text area needs to show `text` whole, at about 90
 * characters a line: at least 3, at most 12, beyond which it scrolls. */
export function textRows(text: string): number {
  const lines = text
    .split("\n")
    .reduce((rows, line) => rows + Math.max(1, Math.ceil(line.length / 90)), 0);
  return Math.min(12, Math.max(3, lines));
}

/** Whether the phases can be edited now: not while they are planned or run. */
export function phasesEditable(construction: Construction): boolean {
  return !agentBusy(construction) && construction.status !== "running";
}

/** Whether `phase`'s tasks can be planned on their own, before a run, as the
 * server's `planPhaseTasks` allows: `"plan"` when it has none yet, `"replan"`
 * when none of them has started, `null` otherwise. Only the phase the runner
 * reaches next (every phase before it done), since its tasks are planned
 * against what those made; never while the construction runs or its phases
 * are planned. */
export function phasePlanning(
  construction: Construction,
  phase: ConstructionPhase
): "plan" | "replan" | null {
  if (construction.planning || construction.status === "running") return null;
  const index = construction.phases.findIndex((p) => sameId(p.id, phase.id));
  if (index < 0) return null;
  if (construction.phases.slice(0, index).some((p) => p.status !== "done"))
    return null;
  if (phase.tasks.some((t) => t.status !== "todo" || t.runs.length > 0))
    return null;
  return phase.tasks.length === 0 ? "plan" : "replan";
}

/** The buttons a task offers, as the server allows them: Retry for a failed
 * task, Skip for a waiting or failed one. Neither while the construction runs,
 * since the runner may be about to start the task. */
export function taskActions(
  task: Pick<ConstructionTask, "status">,
  construction: Pick<Construction, "status">
): { retry: boolean; skip: boolean } {
  if (construction.status === "running") return { retry: false, skip: false };
  return {
    retry: task.status === "failed",
    skip: task.status === "todo" || task.status === "failed",
  };
}

/** Where a task's run is shown: the run's page in the agent's history. */
/** What a running construction is doing right now: planning a phase's tasks
 * (the phase it reached has none yet), or working on a task. `null` when it is
 * not running. The runner takes the phases in order, so the phase it is on is
 * the first that is not done. */
export type CurrentWork =
  | { kind: "planning"; phase: ConstructionPhase }
  | { kind: "task"; phase: ConstructionPhase; task: ConstructionTask };

export function currentWork(construction: Construction): CurrentWork | null {
  if (construction.status !== "running") return null;
  // Planning one phase only: that is all the run does, and it stops before a
  // task starts.
  if (construction.plan_only) {
    const phase = construction.phases.find((p) =>
      sameId(p.id, construction.until_phase)
    );
    return phase ? { kind: "planning", phase } : null;
  }
  for (const phase of construction.phases) {
    const task = phase.tasks.find((t) => t.status === "running");
    if (task) return { kind: "task", phase, task };
  }
  const phase = construction.phases.find((p) => p.status !== "done");
  if (!phase) return null;
  if (phase.tasks.length === 0) return { kind: "planning", phase };
  // Between two tasks: the next one is about to start.
  const next = phase.tasks.find((t) => t.status === "todo");
  return next ? { kind: "task", phase, task: next } : null;
}

/** What the construction agent is doing for the construction, outside the
 * runner: planning the phases, proposing mockups, asking questions or
 * planning a feedback. `null` when it is free. */
export type AgentWork =
  | { kind: "phases" | "mockups" | "questions" }
  | { kind: "feedback"; feedback: ConstructionFeedback };

export function agentWork(construction: Construction): AgentWork | null {
  if (construction.planning) return { kind: "phases" };
  if (construction.designing) return { kind: "mockups" };
  if (construction.asking) return { kind: "questions" };
  const feedback = construction.feedback.find((f) => f.planning);
  return feedback ? { kind: "feedback", feedback } : null;
}

export function runHref(run: string): string {
  return `#/runs/${encodeURIComponent(run)}`;
}

/** How far the construction has got: phases done of all. */
export function progress(construction: Construction): {
  done: number;
  total: number;
} {
  return {
    done: construction.phases.filter((p) => p.status === "done").length,
    total: construction.phases.length,
  };
}

/** How far one phase's tasks have got: a skipped task counts as finished, since
 * the runner has moved past it. */
export function phaseProgress(phase: ConstructionPhase): {
  done: number;
  total: number;
} {
  return {
    done: phase.tasks.filter(
      (t) => t.status === "done" || t.status === "skipped"
    ).length,
    total: phase.tasks.length,
  };
}

/** Something a task created, as the runner records it: `{kind, name}` — today
 * the tables its run added. `null` for an entry not of that shape. */
export function createdEntity(
  entry: unknown
): { kind: string; name: string } | null {
  if (!entry || typeof entry !== "object") return null;
  const { kind, name } = entry as { kind?: unknown; name?: unknown };
  if (typeof name !== "string" || name === "") return null;
  return { kind: typeof kind === "string" ? kind : "", name };
}

/** The titles of the tasks `task` waits for, in the phase's order. A task it
 * names that is not in the phase is left out. */
export function dependencyTitles(
  task: ConstructionTask,
  phase: ConstructionPhase
): string[] {
  // The module's tasks name each other by `uid`; an id is matched too.
  return phase.tasks
    .filter((t) =>
      task.depends_on.some(
        (d) => sameId(d, t.id) || (t.uid !== undefined && sameId(d, t.uid))
      )
    )
    .map((t) => t.title);
}

/** The phases before `phaseId` that are not done: running up to that phase
 * runs them first, so the admin is asked before they start. Empty when every
 * earlier phase is done, or the phase is not in the plan. */
export function unfinishedBefore(
  construction: Construction,
  phaseId: Id
): ConstructionPhase[] {
  const index = construction.phases.findIndex((p) => sameId(p.id, phaseId));
  if (index < 0) return [];
  return construction.phases.slice(0, index).filter((p) => p.status !== "done");
}

/** How many mockups a proposal asks for unless the admin picks another number,
 * as the server's default. */
export const DEFAULT_MOCKUPS = 2;

/** The widths a mockup is shown at, in CSS pixels. */
export const MOCKUP_WIDTHS = { desktop: 1280, phone: 390 } as const;
export type MockupWidth = keyof typeof MOCKUP_WIDTHS;

/** The last round's mockups, in order: what the admin chooses from. */
export function latestMockups(
  construction: Construction
): ConstructionMockup[] {
  const round = Math.max(0, ...construction.mockups.map((m) => m.round));
  return construction.mockups.filter((m) => m.round === round);
}

/** The chosen mockup, if one is. It may be from an earlier round. */
export function chosenMockup(
  construction: Construction
): ConstructionMockup | null {
  return construction.mockups.find((m) => m.status === "chosen") ?? null;
}

/** The feedback a revision sends: what the admin wrote per mockup, blank ones
 * left out, for the last round's mockups only. */
export function revisionFeedback(
  construction: Construction,
  notes: Record<string, string>
): { mockup: Id; text: string }[] {
  return latestMockups(construction)
    .map((m) => ({ mockup: m.id, text: (notes[String(m.id)] ?? "").trim() }))
    .filter((f) => f.text !== "");
}

/** Whether questions can be asked or answered, and mockups proposed, revised
 * or chosen, now: not while the agent plans, proposes or asks (one run of it
 * at a time), or the construction runs. */
export function researchEditable(construction: Construction): boolean {
  return !agentBusy(construction) && construction.status !== "running";
}

/** The phases feedback can be given on, as the server's
 * `giveConstructionFeedback` allows: those that built something (a task
 * done) and have no failed task, which would stop the runner before the
 * follow-up tasks. */
export function feedbackPhases(
  construction: Construction
): ConstructionPhase[] {
  return construction.phases.filter(
    (p) =>
      p.tasks.some((t) => t.status === "done") &&
      !p.tasks.some((t) => t.status === "failed")
  );
}

/** The follow-up tasks planned for `feedback`, in order. */
export function feedbackTasks(
  construction: Construction,
  feedback: ConstructionFeedback
): ConstructionTask[] {
  return construction.phases.flatMap((p) =>
    p.tasks.filter((t) => sameId(t.feedback, feedback.id))
  );
}

/** Whether approved feedback can be planned, run or deleted now: not while
 * the construction runs, nor while the agent is busy with it. Feedback can be
 * given, and declined before it is approved, at any time. */
export function feedbackEditable(construction: Construction): boolean {
  return !agentBusy(construction) && construction.status !== "running";
}

/** Whether `feedback` can be deleted: none of its tasks has started, since
 * what ran stays recorded. */
export function feedbackDeletable(
  construction: Construction,
  feedback: ConstructionFeedback
): boolean {
  return (
    !feedback.planning &&
    feedbackTasks(construction, feedback).every(
      (t) => t.status === "todo" && t.runs.length === 0
    )
  );
}

/** Whether "Plan phases" is asked to confirm: the first plan with nothing
 * researched, no questions asked and no mockups drawn. Once there are phases
 * the admin has decided that already. */
export function planningUnresearched(construction: Construction): boolean {
  return (
    construction.phases.length === 0 &&
    construction.questions.length === 0 &&
    construction.mockups.length === 0
  );
}

/** What the research step's "Plan phases" is asked to confirm: planning
 * with no mockups drawn, or with none of them chosen; `null` when one is
 * chosen. */
export function planningWithoutMockup(
  construction: Construction
): "none" | "unchosen" | null {
  if (construction.mockups.length === 0) return "none";
  return chosenMockup(construction) ? null : "unchosen";
}

/** Whether "Propose mockups" is asked to confirm: no questions asked yet, so
 * the mockups would be drawn from the specification alone, and asking later
 * discards them. */
export function mockupsWithoutQuestions(construction: Construction): boolean {
  return construction.questions.length === 0;
}

/** How many mockups asking questions discards: all of them, as they were drawn
 * without the answers. Asked to confirm when there are any. */
export function askingDiscards(construction: Construction): number {
  return construction.mockups.length;
}

/** The answers to send: those the admin changed on the screen (`drafts`, by
 * question), blank ones included, since saving a blank answer opens the
 * question again. */
export function changedAnswers(
  construction: Construction,
  drafts: Record<string, string>
): { question: Id; text: string }[] {
  return construction.questions
    .filter(
      (q) =>
        String(q.id) in drafts &&
        drafts[String(q.id)].trim() !== (q.answer ?? "")
    )
    .map((q) => ({ question: q.id, text: drafts[String(q.id)].trim() }));
}

/** A screen's HTML as the frame shows it. The page runs no script and loads
 * nothing (the frame is sandboxed and the admin's policy applies), so a link
 * in it would only navigate the frame away from the mockup: the base target
 * sends it to a new window, which the sandbox does not allow, so nowhere. */
export function mockupDocument(html: string): string {
  const base = '<base target="_blank">';
  // Inside `<head>`, or after the doctype: before it, the page would render
  // in quirks mode.
  if (/<head[^>]*>/i.test(html))
    return html.replace(/<head[^>]*>/i, (head) => head + base);
  const doctype = html.match(/^\s*<!doctype[^>]*>/i);
  if (doctype) return doctype[0] + base + html.slice(doctype[0].length);
  return base + html;
}

/** Fetches a mockup's screens with their HTML, in order. */
export type MockupScreensLoader = (mockup: Id) => Promise<{ html: string }[]>;

// The screens' HTML fetched so far, by mockup. The construction lists the
// screens without it, so a poll stays small; a mockup never changes once it is
// saved, so each is fetched once. A failed fetch is forgotten, so the next
// asks again.
const mockupPagesCache = new Map<string, Promise<string[]>>();

/** The HTML of `mockup`'s screens, in order: fetched with `load` the first
 * time, and from then on answered from what that fetch got. */
export function mockupPages(
  load: MockupScreensLoader,
  mockup: Id
): Promise<string[]> {
  const key = String(mockup);
  let pages = mockupPagesCache.get(key);
  if (!pages) {
    pages = load(mockup).then((screens) => screens.map((s) => s.html));
    pages.catch(() => mockupPagesCache.delete(key));
    mockupPagesCache.set(key, pages);
  }
  return pages;
}

/** A request to one of the module's routes, answering its JSON body and
 * throwing the route's `error` on a refusal: `host.json`. */
export type RouteRequest = (
  path: string,
  init?: RequestInit
) => Promise<unknown>;

/** What the screen's buttons do, each answering the construction as the
 * module has it afterwards. Every change is a POST: the module's routes take
 * GET and POST only. */
export function constructionCalls(request: RouteRequest, appId: string) {
  const base = `/applications/${encodeURIComponent(appId)}/construction`;
  const get = async <T>(path: string) => (await request(`${base}${path}`)) as T;
  const post = async <T = Construction>(path: string, body?: unknown) =>
    (await request(`${base}${path}`, {
      method: "POST",
      body: JSON.stringify(body ?? {}),
    })) as T;
  const id = (value: Id) => encodeURIComponent(String(value));
  const saveSpecification = (specification: string) =>
    post("/specification", { specification });
  return {
    /** Start the construction, on the provider the application's settings choose. */
    create: () => post(""),
    saveSpecification,
    /** Save the specification as it is on the screen, then plan from it. */
    plan: async (specification: string) => {
      await saveSpecification(specification);
      return post("/plan");
    },
    /** Save the specification as it is on the screen, then have the agent
     * ask what it leaves open. */
    askQuestions: async (specification: string) => {
      await saveSpecification(specification);
      return post("/questions");
    },
    answerQuestions: (answers: { question: Id; text: string }[]) =>
      post("/questions/answer", { answers }),
    /** Give feedback on a built phase; it waits for approval. */
    giveFeedback: (phase: Id, title: string, description: string) =>
      post("/feedback", { phase, title, description }),
    /** Approve a feedback, which has the agent plan its follow-up tasks; or
     * plan it again when its planning left it without tasks. */
    planFeedback: (feedback: Id) => post(`/feedback/${id(feedback)}/plan`),
    deleteFeedback: (feedback: Id) => post(`/feedback/${id(feedback)}/delete`),
    /** Save the specification as it is on the screen, then have `count`
     * mockups proposed from it. */
    proposeMockups: async (specification: string, count: number) => {
      await saveSpecification(specification);
      return post("/mockups", { count, feedback: null, note: null });
    },
    /** A new round, revising the last one as `feedback` and `note` ask. */
    reviseMockups: (
      feedback: { mockup: Id; text: string }[],
      note: string | null
    ) => post("/mockups", { count: null, feedback, note }),
    chooseMockup: (mockup: Id) => post(`/mockups/${id(mockup)}/choose`),
    /** A mockup's screens with their HTML, which the construction leaves out. */
    mockupScreens: (mockup: Id) =>
      get<MockupScreens>(`/mockups/${id(mockup)}/screens`),
    savePhases: (phases: PhaseDraft[]) => post("/phases", { phases }),
    /** Run every phase, or up to and including the phase `until`. */
    run: (until: Id | null) => post("/run", { until_phase: until }),
    stop: () => post("/stop"),
    retry: (task: Id) => post(`/tasks/${id(task)}/retry`),
    skip: (task: Id) => post(`/tasks/${id(task)}/skip`),
    /** Plan one phase's tasks (again, told `note`) and stop before they run. */
    planTasks: (phase: Id, note: string | null) =>
      post(`/phases/${id(phase)}/plan`, { note }),
  };
}

/** The construction, or `null` when the application has none yet (the
 * route's 404). Through `host.fetch`, since `host.json` does not say the
 * status it failed with. */
export async function loadConstruction(
  fetcher: (path: string, init?: RequestInit) => Promise<Response>,
  appId: string
): Promise<Construction | null> {
  const response = await fetcher(
    `/applications/${encodeURIComponent(appId)}/construction`
  );
  if (response.status === 404) return null;
  const text = await response.text();
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = null;
  }
  if (!response.ok) {
    const said =
      body && typeof body === "object" && "error" in body
        ? String((body as { error: unknown }).error)
        : "";
    throw new Error(said || `${response.status} ${response.statusText}`);
  }
  return body as Construction;
}

/** The saved phases as drafts to edit. */
export function toDrafts(phases: ConstructionPhase[]): PhaseDraft[] {
  return phases.map((p) => ({
    id: p.id,
    title: p.title,
    description: p.description,
    requirements: p.requirements.map((r) => ({
      text: r.text,
      priority: r.priority,
    })),
  }));
}

/** Whether `drafts` differ from the saved `phases`. */
export function draftsChanged(
  drafts: PhaseDraft[],
  phases: ConstructionPhase[]
): boolean {
  return JSON.stringify(drafts) !== JSON.stringify(toDrafts(phases));
}

/** The drafts to show when the server's phases change from `base` to `phases`,
 * or `null` to keep the admin's own.
 *
 * Drafts with no edits follow the server. So do drafts the admin saved and has
 * not touched since (`saved`, as sent), once the server answers with a changed
 * plan: that is the save's answer, with ids for the phases it inserted and the
 * text as the server stored it, and keeping the drafts instead would leave
 * them differing from the saved plan for good. A poll that brings back the same
 * plan (the save failed, or has not answered yet) keeps them. */
export function followedDrafts(
  drafts: PhaseDraft[],
  base: ConstructionPhase[],
  phases: ConstructionPhase[],
  saved: PhaseDraft[] | null
): PhaseDraft[] | null {
  if (!draftsChanged(drafts, base)) return toDrafts(phases);
  const answered =
    JSON.stringify(toDrafts(phases)) !== JSON.stringify(toDrafts(base));
  if (
    saved !== null &&
    answered &&
    JSON.stringify(drafts) === JSON.stringify(saved)
  ) {
    return toDrafts(phases);
  }
  return null;
}

/** `drafts` with the phase at `index` moved one place up (`-1`) or down (`1`).
 * A move past either end changes nothing. */
export function movePhase(
  drafts: PhaseDraft[],
  index: number,
  by: -1 | 1
): PhaseDraft[] {
  const to = index + by;
  if (index < 0 || to < 0 || to >= drafts.length) return drafts;
  const next = [...drafts];
  [next[index], next[to]] = [next[to], next[index]];
  return next;
}

/** `drafts` with the phase at `index` changed by `change`. */
export function updatePhase(
  drafts: PhaseDraft[],
  index: number,
  change: Partial<PhaseDraft>
): PhaseDraft[] {
  return drafts.map((d, i) => (i === index ? { ...d, ...change } : d));
}

/** `drafts` without the phase at `index`. */
export function removePhase(drafts: PhaseDraft[], index: number): PhaseDraft[] {
  return drafts.filter((_, i) => i !== index);
}

/** `drafts` with a new, empty phase at the end. */
export function addPhase(drafts: PhaseDraft[]): PhaseDraft[] {
  return [
    ...drafts,
    { id: null, title: "", description: "", requirements: [] },
  ];
}

/** `drafts` with an empty `should` requirement added to the phase at `index`. */
export function addRequirement(
  drafts: PhaseDraft[],
  index: number
): PhaseDraft[] {
  return drafts.map((d, i) =>
    i === index
      ? {
          ...d,
          requirements: [...d.requirements, { text: "", priority: "should" }],
        }
      : d
  );
}

/** `drafts` with requirement `r` of phase `index` changed by `change`. */
export function updateRequirement(
  drafts: PhaseDraft[],
  index: number,
  r: number,
  change: Partial<RequirementDraft>
): PhaseDraft[] {
  return drafts.map((d, i) =>
    i === index
      ? {
          ...d,
          requirements: d.requirements.map((q, j) =>
            j === r ? { ...q, ...change } : q
          ),
        }
      : d
  );
}

/** `drafts` without requirement `r` of phase `index`. */
export function removeRequirement(
  drafts: PhaseDraft[],
  index: number,
  r: number
): PhaseDraft[] {
  return drafts.map((d, i) =>
    i === index
      ? { ...d, requirements: d.requirements.filter((_, j) => j !== r) }
      : d
  );
}

/** Whether a phase can be deleted on the screen: not once it has tasks. The
 * server would keep it anyway, so the screen does not offer it. */
export function phaseDeletable(
  draft: PhaseDraft,
  phases: ConstructionPhase[]
): boolean {
  const saved = phases.find((p) => sameId(p.id, draft.id));
  return !saved || saved.tasks.length === 0;
}
