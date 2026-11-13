// The Plan & build screen's decisions (TODO-app-constructor AC4), as pure
// functions so they are tested without a browser.
//
// The screen shows one application's construction in three numbered tabs:
// the specification, the phases, and the progress of their tasks. Which one
// opens follows from the construction's status. The phases are edited as a
// draft on the screen and saved whole, as the server's `savePhases` takes
// them.

import type { ApiClient, GetConstructionResponse, SavePhasesRequest } from "./client";

export type Construction = GetConstructionResponse;
export type ConstructionPhase = Construction["phases"][number];
export type ConstructionTask = ConstructionPhase["tasks"][number];

/** A phase as the admin edits it before saving. */
export type PhaseDraft = SavePhasesRequest["phases"][number];
export type RequirementDraft = PhaseDraft["requirements"][number];

/** The priorities a requirement can have, most important first. */
export const PRIORITIES = ["must", "should", "could"] as const;

/** How often the screen reloads the construction while something runs. */
export const POLL_MS = 2000;

/** The screen's route for `appId`. */
export function constructPath(appId: string): string {
  return `/applications/${encodeURIComponent(appId)}/construct`;
}

/** The construction opened on its Phases tab. */
export function phasesPath(appId: string): string {
  return `${constructPath(appId)}/phases`;
}

/** One phase's own page: its requirements, and the tasks planned from them. */
export function phasePath(appId: string, phaseId: string): string {
  return `${phasesPath(appId)}/${encodeURIComponent(phaseId)}`;
}

/** Where a Plan & build route points: the construction, opened on a tab or on
 * one phase's page. `null` for any other path. */
export function constructRoute(
  path: string,
): { appId: string; tab: "phases" | null; phaseId: string | null } | null {
  const match = path.match(/^\/applications\/([^/]+)\/construct(?:\/phases(?:\/([^/]+))?)?$/);
  if (!match) return null;
  const phaseId = match[2] ? decodeURIComponent(match[2]) : null;
  return {
    appId: decodeURIComponent(match[1]),
    tab: path.includes("/construct/phases") && !phaseId ? "phases" : null,
    phaseId,
  };
}

/** Whether the App Constructor can build `app`: React applications only, as
 * the server's `createConstruction` checks. */
export function constructible(app: { framework: { name: string } }): boolean {
  return app.framework.name === "react";
}

/** The step the screen opens on. */
export type Step = "start" | "specification" | "phases" | "progress";

/** Which step is open for `construction` (`null`: the application has none).
 *
 * - None yet: start one.
 * - A draft has no phases: write the specification.
 * - Planned: edit the phases, until a task has run. From then on the progress
 *   is what the admin comes back to see.
 * - Running, stopped, failed or done: the progress. */
export function currentStep(construction: Construction | null): Step {
  if (!construction) return "start";
  if (construction.planning) return "phases";
  switch (construction.status) {
    case "draft":
      return "specification";
    case "planned":
      return started(construction) ? "progress" : "phases";
    default:
      return "progress";
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
    phases: [],
  };
}

/** The screen's tabs, in order; each is one step. */
export const TABS = ["specification", "phases", "progress"] as const;
export type Tab = (typeof TABS)[number];

/** The tab the screen opens on: the current step's, except that a build under
 * way opens on its phases — the plan is what an admin comes back to, and each
 * phase's progress is one click from there. */
export function landingTab(step: Exclude<Step, "start">): Tab {
  return step === "progress" ? "phases" : step;
}

/** Whether `tab` can be opened yet: the specification always, the phases once
 * they are planned or being planned, the progress once the step is reached. */
export function tabOpen(construction: Construction, tab: Tab): boolean {
  switch (tab) {
    case "specification":
      return true;
    case "phases":
      return construction.planning || construction.phases.length > 0;
    case "progress":
      return currentStep(construction) === "progress";
  }
}

/** Whether any task of the construction has run or been skipped. */
export function started(construction: Construction): boolean {
  return construction.phases.some((p) => p.tasks.some((t) => t.status !== "todo"));
}

/** Whether the screen should keep reloading: while the construction runs or
 * its phases are being planned. */
export function shouldPoll(construction: Construction | null): boolean {
  return !!construction && (construction.status === "running" || construction.planning);
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
  interval: number = POLL_MS,
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
 * run, the error is the progress's to show. */
export function planError(construction: Construction): string | null {
  if (construction.planning || currentStep(construction) === "progress") return null;
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
  return !construction.planning && construction.status !== "running";
}

/** Whether `phase`'s tasks can be planned on their own, before a run, as the
 * server's `planPhaseTasks` allows: `"plan"` when it has none yet, `"replan"`
 * when none of them has started, `null` otherwise. Only the phase the runner
 * reaches next (every phase before it done), since its tasks are planned
 * against what those made; never while the construction runs or its phases
 * are planned. */
export function phasePlanning(
  construction: Construction,
  phase: ConstructionPhase,
): "plan" | "replan" | null {
  if (construction.planning || construction.status === "running") return null;
  const index = construction.phases.findIndex((p) => p.id === phase.id);
  if (index < 0) return null;
  if (construction.phases.slice(0, index).some((p) => p.status !== "done")) return null;
  if (phase.tasks.some((t) => t.status !== "todo" || t.runs.length > 0)) return null;
  return phase.tasks.length === 0 ? "plan" : "replan";
}

/** The buttons a task offers, as the server allows them: Retry for a failed
 * task, Skip for a waiting or failed one. Neither while the construction runs,
 * since the runner may be about to start the task. */
export function taskActions(
  task: Pick<ConstructionTask, "status">,
  construction: Pick<Construction, "status">,
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
    const phase = construction.phases.find((p) => p.id === construction.until_phase);
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

export function runHref(run: string): string {
  return `#/runs/${encodeURIComponent(run)}`;
}

/** How far the construction has got: phases done of all. */
export function progress(construction: Construction): { done: number; total: number } {
  return {
    done: construction.phases.filter((p) => p.status === "done").length,
    total: construction.phases.length,
  };
}

/** How far one phase's tasks have got: a skipped task counts as finished, since
 * the runner has moved past it. */
export function phaseProgress(phase: ConstructionPhase): { done: number; total: number } {
  return {
    done: phase.tasks.filter((t) => t.status === "done" || t.status === "skipped").length,
    total: phase.tasks.length,
  };
}

/** Something a task created, as the runner records it: `{kind, name}` — today
 * the tables its run added. `null` for an entry not of that shape. */
export function createdEntity(entry: unknown): { kind: string; name: string } | null {
  if (!entry || typeof entry !== "object") return null;
  const { kind, name } = entry as { kind?: unknown; name?: unknown };
  if (typeof name !== "string" || name === "") return null;
  return { kind: typeof kind === "string" ? kind : "", name };
}

/** The titles of the tasks `task` waits for, in the phase's order. A task it
 * names that is not in the phase is left out. */
export function dependencyTitles(task: ConstructionTask, phase: ConstructionPhase): string[] {
  return phase.tasks.filter((t) => task.depends_on.includes(t.id)).map((t) => t.title);
}

/** The phases before `phaseId` that are not done: running up to that phase
 * runs them first, so the admin is asked before they start. Empty when every
 * earlier phase is done, or the phase is not in the plan. */
export function unfinishedBefore(
  construction: Construction,
  phaseId: string,
): ConstructionPhase[] {
  const index = construction.phases.findIndex((p) => p.id === phaseId);
  if (index < 0) return [];
  return construction.phases.slice(0, index).filter((p) => p.status !== "done");
}

/** The endpoints the screen calls for one application. */
export type ConstructionClient = Pick<
  ApiClient,
  | "createConstruction"
  | "saveConstructionSpecification"
  | "planConstruction"
  | "savePhases"
  | "runConstruction"
  | "stopConstruction"
  | "retryTask"
  | "skipTask"
  | "planPhaseTasks"
>;

/** What the screen's buttons do, each answering the construction as the
 * server has it afterwards. */
export function constructionCalls(client: ConstructionClient, appId: string) {
  return {
    /** Start the construction, on the provider the application's settings choose. */
    create: () => client.createConstruction(appId, {}),
    saveSpecification: (specification: string) =>
      client.saveConstructionSpecification(appId, { specification }),
    /** Save the specification as it is on the screen, then plan from it. */
    plan: async (specification: string) => {
      await client.saveConstructionSpecification(appId, { specification });
      return client.planConstruction(appId);
    },
    savePhases: (phases: PhaseDraft[]) => client.savePhases(appId, { phases }),
    /** Run every phase, or up to and including the phase `until`. */
    run: (until: string | null) => client.runConstruction(appId, { until_phase: until }),
    stop: () => client.stopConstruction(appId),
    retry: (task: string) => client.retryTask(appId, task),
    skip: (task: string) => client.skipTask(appId, task),
    /** Plan one phase's tasks (again, told `note`) and stop before they run. */
    planTasks: (phase: string, note: string | null) =>
      client.planPhaseTasks(appId, phase, { note }),
  };
}

/** The saved phases as drafts to edit. */
export function toDrafts(phases: ConstructionPhase[]): PhaseDraft[] {
  return phases.map((p) => ({
    id: p.id,
    title: p.title,
    description: p.description,
    requirements: p.requirements.map((r) => ({ text: r.text, priority: r.priority })),
  }));
}

/** Whether `drafts` differ from the saved `phases`. */
export function draftsChanged(drafts: PhaseDraft[], phases: ConstructionPhase[]): boolean {
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
  saved: PhaseDraft[] | null,
): PhaseDraft[] | null {
  if (!draftsChanged(drafts, base)) return toDrafts(phases);
  const answered = JSON.stringify(toDrafts(phases)) !== JSON.stringify(toDrafts(base));
  if (saved !== null && answered && JSON.stringify(drafts) === JSON.stringify(saved)) {
    return toDrafts(phases);
  }
  return null;
}

/** `drafts` with the phase at `index` moved one place up (`-1`) or down (`1`).
 * A move past either end changes nothing. */
export function movePhase(drafts: PhaseDraft[], index: number, by: -1 | 1): PhaseDraft[] {
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
  change: Partial<PhaseDraft>,
): PhaseDraft[] {
  return drafts.map((d, i) => (i === index ? { ...d, ...change } : d));
}

/** `drafts` without the phase at `index`. */
export function removePhase(drafts: PhaseDraft[], index: number): PhaseDraft[] {
  return drafts.filter((_, i) => i !== index);
}

/** `drafts` with a new, empty phase at the end. */
export function addPhase(drafts: PhaseDraft[]): PhaseDraft[] {
  return [...drafts, { id: null, title: "", description: "", requirements: [] }];
}

/** `drafts` with an empty `should` requirement added to the phase at `index`. */
export function addRequirement(drafts: PhaseDraft[], index: number): PhaseDraft[] {
  return drafts.map((d, i) =>
    i === index ? { ...d, requirements: [...d.requirements, { text: "", priority: "should" }] } : d,
  );
}

/** `drafts` with requirement `r` of phase `index` changed by `change`. */
export function updateRequirement(
  drafts: PhaseDraft[],
  index: number,
  r: number,
  change: Partial<RequirementDraft>,
): PhaseDraft[] {
  return drafts.map((d, i) =>
    i === index
      ? { ...d, requirements: d.requirements.map((q, j) => (j === r ? { ...q, ...change } : q)) }
      : d,
  );
}

/** `drafts` without requirement `r` of phase `index`. */
export function removeRequirement(drafts: PhaseDraft[], index: number, r: number): PhaseDraft[] {
  return drafts.map((d, i) =>
    i === index ? { ...d, requirements: d.requirements.filter((_, j) => j !== r) } : d,
  );
}

/** Whether a phase can be deleted on the screen: not once it has tasks. The
 * server would keep it anyway, so the screen does not offer it. */
export function phaseDeletable(draft: PhaseDraft, phases: ConstructionPhase[]): boolean {
  const saved = phases.find((p) => p.id === draft.id);
  return !saved || saved.tasks.length === 0;
}
