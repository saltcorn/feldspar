/**
 * The Plan & build screen's decisions (TODO-app-constructor AC4.4): which step
 * is shown for each status, when polling runs, which task buttons appear, and
 * the phase draft edits.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  addPhase,
  addRequirement,
  blankConstruction,
  constructPath,
  constructRoute,
  createdEntity,
  dependencyTitles,
  landingTab,
  unfinishedBefore,
  constructionCalls,
  constructible,
  currentStep,
  currentWork,
  draftsChanged,
  followedDrafts,
  movePhase,
  phaseDeletable,
  phasePlanning,
  phasePath,
  phaseProgress,
  phasesPath,
  phasesEditable,
  planError,
  pollConstruction,
  progress,
  removePhase,
  removeRequirement,
  runHref,
  shouldPoll,
  tabOpen,
  taskActions,
  textRows,
  toDrafts,
  updatePhase,
  updateRequirement,
  type Construction,
  type ConstructionPhase,
  type ConstructionTask,
} from "./construction";

function task(status: string, extra: Partial<ConstructionTask> = {}): ConstructionTask {
  return {
    id: `t-${status}`,
    kind: "feature",
    title: `a ${status} task`,
    brief: "",
    depends_on: [],
    status,
    runs: [],
    summary: null,
    created: [],
    ...extra,
  };
}

function phase(id: string, tasks: ConstructionTask[] = [], status = "unplanned"): ConstructionPhase {
  return {
    id,
    title: `Phase ${id}`,
    description: "",
    requirements: [{ text: "lists books", priority: "must" }],
    status,
    tasks,
  };
}

function construction(status: string, extra: Partial<Construction> = {}): Construction {
  return {
    id: "c1",
    application: "a1",
    status,
    agent: "construct-shop",
    specification: "A shop.",
    started_by: null,
    plan_run: null,
    planning: false,
    error: null,
    until_phase: null,
    plan_only: false,
    updated_at: null,
    phases: [],
    ...extra,
  };
}

describe("the step the screen opens on", () => {
  it("offers to start a construction when the application has none", () => {
    expect(currentStep(null)).toBe("start");
  });

  it("asks for the specification while it is a draft", () => {
    expect(currentStep(construction("draft"))).toBe("specification");
  });

  it("shows the phases while they are planned, and once they are", () => {
    expect(currentStep(construction("draft", { planning: true }))).toBe("phases");
    expect(currentStep(construction("planned", { phases: [phase("p1")] }))).toBe("phases");
  });

  it("shows the progress once a task has run, and whenever it runs or ran", () => {
    const ran = construction("planned", { phases: [phase("p1", [task("done"), task("todo")])] });
    expect(currentStep(ran)).toBe("progress");
    for (const status of ["running", "stopped", "failed", "done"]) {
      expect(currentStep(construction(status))).toBe("progress");
    }
  });
});

describe("a text area's height", () => {
  it("fits the text, between 3 and 12 rows", () => {
    expect(textRows("")).toBe(3);
    expect(textRows("x".repeat(450))).toBe(5);
    expect(textRows("a\nb\nc\nd")).toBe(4);
    expect(textRows("x".repeat(5000))).toBe(12);
  });
});

describe("the tabs", () => {
  it("opens the phases once they are planned or being planned", () => {
    expect(tabOpen(construction("draft"), "specification")).toBe(true);
    expect(tabOpen(construction("draft"), "phases")).toBe(false);
    expect(tabOpen(construction("draft", { planning: true }), "phases")).toBe(true);
    expect(tabOpen(construction("planned", { phases: [phase("p1")] }), "phases")).toBe(true);
  });

  it("opens the progress once a run has started", () => {
    expect(tabOpen(construction("planned", { phases: [phase("p1")] }), "progress")).toBe(false);
    expect(tabOpen(construction("running", { phases: [phase("p1")] }), "progress")).toBe(true);
    const ran = construction("planned", { phases: [phase("p1", [task("done")])] });
    expect(tabOpen(ran, "progress")).toBe(true);
  });
});

describe("a failed plan", () => {
  const error = "planning the phases failed: the LLM provider failed: invalid x-api-key";

  it("is shown with the specification once the plan run has ended", () => {
    expect(planError(construction("draft", { error }))).toBe(error);
    expect(planError(construction("planned", { error, phases: [phase("p1")] }))).toBe(error);
    expect(planError(construction("draft"))).toBeNull();
  });

  it("is not shown while planning again, nor once the progress shows the error", () => {
    expect(planError(construction("draft", { error, planning: true }))).toBeNull();
    expect(planError(construction("failed", { error }))).toBeNull();
  });
});

describe("polling", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("is wanted while the construction runs or is being planned, and not otherwise", () => {
    expect(shouldPoll(null)).toBe(false);
    expect(shouldPoll(construction("running"))).toBe(true);
    expect(shouldPoll(construction("draft", { planning: true }))).toBe(true);
    for (const status of ["draft", "planned", "stopped", "failed", "done"]) {
      expect(shouldPoll(construction(status))).toBe(false);
    }
  });

  it("reloads every interval while running, and stops when the status says done", async () => {
    const answers = [construction("running"), construction("done")];
    const load = vi.fn(async () => answers.shift() ?? null);
    const update = vi.fn();
    pollConstruction(construction("running"), load, update, () => {}, 2000);

    await vi.advanceTimersByTimeAsync(1999);
    expect(load).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(load).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(2000);
    expect(load).toHaveBeenCalledTimes(2);
    expect(update).toHaveBeenLastCalledWith(expect.objectContaining({ status: "done" }));
    // Done: nothing is scheduled any more.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(load).toHaveBeenCalledTimes(2);
  });

  it("does not start for a construction that is not running", async () => {
    const load = vi.fn(async () => construction("planned"));
    pollConstruction(construction("planned"), load, () => {}, () => {}, 2000);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(load).not.toHaveBeenCalled();
  });

  it("stops when it is cancelled, even with a load scheduled", async () => {
    const load = vi.fn(async () => construction("running"));
    const stop = pollConstruction(construction("running"), load, () => {}, () => {}, 2000);
    await vi.advanceTimersByTimeAsync(2000);
    stop();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(load).toHaveBeenCalledTimes(1);
  });

  it("reports a failed load and keeps watching", async () => {
    const load = vi
      .fn<() => Promise<Construction | null>>()
      .mockRejectedValueOnce(new Error("server restarting"))
      .mockResolvedValue(construction("stopped"));
    const failed = vi.fn();
    pollConstruction(construction("running"), load, () => {}, failed, 2000);
    await vi.advanceTimersByTimeAsync(2000);
    expect(failed).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(2000);
    expect(load).toHaveBeenCalledTimes(2);
  });
});

describe("what the admin may do", () => {
  it("edits phases unless they are being planned or run", () => {
    expect(phasesEditable(construction("planned"))).toBe(true);
    expect(phasesEditable(construction("failed"))).toBe(true);
    expect(phasesEditable(construction("running"))).toBe(false);
    expect(phasesEditable(construction("draft", { planning: true }))).toBe(false);
  });

  it("retries a failed task and skips a waiting or failed one", () => {
    const stopped = construction("stopped");
    expect(taskActions(task("failed"), stopped)).toEqual({ retry: true, skip: true });
    expect(taskActions(task("todo"), stopped)).toEqual({ retry: false, skip: true });
    for (const status of ["done", "skipped", "running"]) {
      expect(taskActions(task(status), stopped)).toEqual({ retry: false, skip: false });
    }
  });

  it("offers no task buttons while the construction runs", () => {
    expect(taskActions(task("todo"), construction("running"))).toEqual({
      retry: false,
      skip: false,
    });
  });

  it("does not offer to delete a phase that has tasks", () => {
    const phases = [phase("p1", [task("done")]), phase("p2")];
    const drafts = toDrafts(phases);
    expect(phaseDeletable(drafts[0], phases)).toBe(false);
    expect(phaseDeletable(drafts[1], phases)).toBe(true);
    expect(phaseDeletable(addPhase(drafts)[2], phases)).toBe(true);
  });
});

describe("links and progress", () => {
  it("has its route under the application, and links a task's run to its page", () => {
    expect(constructPath("a 1")).toBe("/applications/a%201/construct");
    expect(runHref("r1")).toBe("#/runs/r1");
  });

  it("builds React applications only", () => {
    expect(constructible({ framework: { name: "react" } })).toBe(true);
    expect(constructible({ framework: { name: "saltcorn" } })).toBe(false);
  });

  it("counts the phases done", () => {
    const c = construction("running", {
      phases: [phase("p1", [], "done"), phase("p2", [], "running"), phase("p3")],
    });
    expect(progress(c)).toEqual({ done: 1, total: 3 });
  });

  it("counts a phase's finished tasks, a skipped one among them", () => {
    const p = {
      ...phase("p1"),
      tasks: [task("done"), task("skipped"), task("failed"), task("todo")],
    };
    expect(phaseProgress(p)).toEqual({ done: 2, total: 4 });
  });
});


describe("editing the phases", () => {
  const phases = [phase("p1"), phase("p2"), phase("p3")];

  it("starts from the saved phases and notices a change", () => {
    const drafts = toDrafts(phases);
    expect(drafts.map((d) => d.id)).toEqual(["p1", "p2", "p3"]);
    expect(draftsChanged(drafts, phases)).toBe(false);
    expect(draftsChanged(updatePhase(drafts, 0, { title: "Data" }), phases)).toBe(true);
  });

  it("reorders, and ignores a move past either end", () => {
    const drafts = toDrafts(phases);
    expect(movePhase(drafts, 0, 1).map((d) => d.id)).toEqual(["p2", "p1", "p3"]);
    expect(movePhase(drafts, 2, -1).map((d) => d.id)).toEqual(["p1", "p3", "p2"]);
    expect(movePhase(drafts, 0, -1)).toBe(drafts);
    expect(movePhase(drafts, 2, 1)).toBe(drafts);
  });

  it("adds and deletes phases, a new one without an id", () => {
    const added = addPhase(toDrafts(phases));
    expect(added).toHaveLength(4);
    expect(added[3].id).toBeNull();
    expect(removePhase(added, 1).map((d) => d.id)).toEqual(["p1", "p3", null]);
  });

  it("adds, rewords and deletes a requirement", () => {
    let drafts = addRequirement(toDrafts(phases), 1);
    expect(drafts[1].requirements).toEqual([
      { text: "lists books", priority: "must" },
      { text: "", priority: "should" },
    ]);
    drafts = updateRequirement(drafts, 1, 1, { text: "search", priority: "could" });
    expect(drafts[1].requirements[1]).toEqual({ text: "search", priority: "could" });
    drafts = removeRequirement(drafts, 1, 0);
    expect(drafts[1].requirements).toEqual([{ text: "search", priority: "could" }]);
    // The other phases are untouched.
    expect(drafts[0].requirements).toEqual(phases[0].requirements);
  });
});

describe("what the buttons call", () => {
  /** A client that records each call and answers a construction. */
  function recording() {
    const calls: unknown[][] = [];
    const answer = (name: string) =>
      vi.fn(async (...args: unknown[]) => {
        calls.push([name, ...args]);
        return construction("running");
      });
    const client = {
      createConstruction: answer("createConstruction"),
      saveConstructionSpecification: answer("saveConstructionSpecification"),
      planConstruction: answer("planConstruction"),
      savePhases: answer("savePhases"),
      runConstruction: answer("runConstruction"),
      stopConstruction: answer("stopConstruction"),
      retryTask: answer("retryTask"),
      skipTask: answer("skipTask"),
      planPhaseTasks: answer("planPhaseTasks"),
    };
    return { client, calls };
  }

  it("calls stop, retry and skip on the application's construction", async () => {
    const { client, calls } = recording();
    const buttons = constructionCalls(client, "a1");
    await buttons.stop();
    await buttons.retry("t1");
    await buttons.skip("t2");
    expect(calls).toEqual([
      ["stopConstruction", "a1"],
      ["retryTask", "a1", "t1"],
      ["skipTask", "a1", "t2"],
    ]);
  });

  it("plans one phase's tasks, with or without a note", async () => {
    const { client, calls } = recording();
    const buttons = constructionCalls(client, "a1");
    await buttons.planTasks("p1", null);
    await buttons.planTasks("p1", "no admin screen");
    expect(calls).toEqual([
      ["planPhaseTasks", "a1", "p1", { note: null }],
      ["planPhaseTasks", "a1", "p1", { note: "no admin screen" }],
    ]);
  });

  it("starts a construction without naming a provider: the application's settings choose it", async () => {
    const { client, calls } = recording();
    await constructionCalls(client, "a1").create();
    expect(calls).toEqual([["createConstruction", "a1", {}]]);
  });

  it("shows an application without a construction its empty specification", () => {
    const blank = blankConstruction("a1");
    expect(blank).toMatchObject({ application: "a1", specification: "", phases: [] });
    expect(currentStep(blank)).toBe("specification");
    expect(tabOpen(blank, "specification")).toBe(true);
    expect(tabOpen(blank, "phases")).toBe(false);
    expect(tabOpen(blank, "progress")).toBe(false);
  });

  it("runs all phases or up to one", async () => {
    const { client, calls } = recording();
    const buttons = constructionCalls(client, "a1");
    await buttons.run(null);
    await buttons.run("p2");
    expect(calls).toEqual([
      ["runConstruction", "a1", { until_phase: null }],
      ["runConstruction", "a1", { until_phase: "p2" }],
    ]);
  });

  it("saves the specification on the screen before planning from it", async () => {
    const { client, calls } = recording();
    await constructionCalls(client, "a1").plan("A shop for books.");
    expect(calls).toEqual([
      ["saveConstructionSpecification", "a1", { specification: "A shop for books." }],
      ["planConstruction", "a1"],
    ]);
  });
});

describe("a phase's own page", () => {
  it("has a route under the construction, and the route is read back", () => {
    expect(phasesPath("a1")).toBe("/applications/a1/construct/phases");
    expect(phasePath("a1", "p 1")).toBe("/applications/a1/construct/phases/p%201");
    expect(constructRoute("/applications/a1/construct")).toEqual({
      appId: "a1",
      tab: null,
      phaseId: null,
    });
    expect(constructRoute("/applications/a1/construct/phases")).toEqual({
      appId: "a1",
      tab: "phases",
      phaseId: null,
    });
    expect(constructRoute(phasePath("a1", "p 1"))).toEqual({
      appId: "a1",
      tab: null,
      phaseId: "p 1",
    });
    expect(constructRoute("/applications/a1/construct/other")).toBeNull();
    expect(constructRoute("/applications/a1/tables")).toBeNull();
  });

  it("reads what a task created", () => {
    expect(createdEntity({ kind: "table", name: "machines" })).toEqual({
      kind: "table",
      name: "machines",
    });
    expect(createdEntity({ name: "listSets" })).toEqual({ kind: "", name: "listSets" });
    expect(createdEntity({ kind: "table" })).toBeNull();
    expect(createdEntity("machines")).toBeNull();
  });

  it("names the tasks a task waits for, in the phase's order", () => {
    const a = { ...task("done"), id: "a", title: "Tables" };
    const b = { ...task("done"), id: "b", title: "Queries" };
    const c = { ...task("todo"), id: "c", title: "Page", depends_on: ["b", "a", "gone"] };
    expect(dependencyTitles(c, phase("p1", [a, b, c]))).toEqual(["Tables", "Queries"]);
  });
});

describe("the tab the screen opens on", () => {
  it("is the current step's, except that a build under way opens on its phases", () => {
    expect(landingTab("specification")).toBe("specification");
    expect(landingTab("phases")).toBe("phases");
    expect(landingTab("progress")).toBe("phases");
  });
});

describe("running up to a phase", () => {
  it("names the earlier phases that are not done, which it would run first", () => {
    const c = construction("planned", {
      phases: [phase("p1", [], "done"), phase("p2", [], "failed"), phase("p3"), phase("p4")],
    });
    expect(unfinishedBefore(c, "p4").map((p) => p.id)).toEqual(["p2", "p3"]);
    expect(unfinishedBefore(c, "p2")).toEqual([]);
    expect(unfinishedBefore(c, "p1")).toEqual([]);
    expect(unfinishedBefore(c, "gone")).toEqual([]);
  });
});

describe("what a running construction is on", () => {
  const task = (id: string, status: string, runs: string[] = []) => ({
    id, kind: "data", title: `Task ${id}`, brief: "", depends_on: [], status, runs, summary: null, created: [],
  });
  const phaseOf = (id: string, status: string, tasks: ReturnType<typeof task>[]) => ({
    id, title: `Phase ${id}`, description: "", requirements: [], status, tasks,
  });
  const running = (phases: ReturnType<typeof phaseOf>[]) => ({
    ...blankConstruction("a1"), status: "running", phases,
  });

  it("is planning the first unfinished phase when it has no tasks yet", () => {
    const c = running([phaseOf("p1", "unplanned", []), phaseOf("p2", "unplanned", [])]);
    expect(currentWork(c)).toMatchObject({ kind: "planning", phase: { id: "p1" } });
  });

  it("is the running task, else the next one to start", () => {
    const c = running([phaseOf("p1", "running", [task("t1", "done"), task("t2", "running", ["r1"])])]);
    expect(currentWork(c)).toMatchObject({ kind: "task", task: { id: "t2" } });
    const between = running([phaseOf("p1", "planned", [task("t1", "done"), task("t2", "todo")])]);
    expect(currentWork(between)).toMatchObject({ kind: "task", task: { id: "t2" } });
  });

  it("is nothing when the construction is not running", () => {
    expect(currentWork({ ...running([phaseOf("p1", "unplanned", [])]), status: "planned" })).toBeNull();
  });
});

describe("the drafts after the server's phases change", () => {
  const base = [phase("p1")];
  const added = [...toDrafts(base), { id: null, title: "Checkout", description: "", requirements: [] }];
  const answer = [phase("p1"), { ...phase("p2"), title: "Checkout", requirements: [] }];

  it("follow the save's answer, so a new phase gets its id and the card is not left unsaved", () => {
    const next = followedDrafts(added, base, answer, added);
    expect(next).toEqual(toDrafts(answer));
    expect(draftsChanged(next!, answer)).toBe(false);
  });

  it("keep the admin's edits when nothing was saved, or the plan came back the same", () => {
    expect(followedDrafts(added, base, answer, null)).toBeNull();
    expect(followedDrafts(added, base, [phase("p1")], added)).toBeNull();
    // Edited again after saving: the newer edits stay.
    const edited = added.map((d) => (d.id === null ? { ...d, title: "Pay" } : d));
    expect(followedDrafts(edited, base, answer, added)).toBeNull();
  });

  it("follow the server when there are no edits", () => {
    expect(followedDrafts(toDrafts(base), base, answer, null)).toEqual(toDrafts(answer));
  });
});


describe("phasePlanning", () => {
  const done = phase("p1", [task("done")], "done");

  it("plans the next phase, and plans again one whose tasks have not started", () => {
    const fresh = phase("p2");
    expect(phasePlanning(construction("planned", { phases: [done, fresh] }), fresh)).toBe("plan");
    const planned = phase("p2", [task("todo")], "planned");
    expect(phasePlanning(construction("planned", { phases: [done, planned] }), planned)).toBe(
      "replan",
    );
  });

  it("offers nothing for a later phase, one that has started, or while it runs", () => {
    const first = phase("p1");
    const second = phase("p2");
    // Planned against what the first makes: not before it is done.
    expect(phasePlanning(construction("planned", { phases: [first, second] }), second)).toBeNull();
    // A task stopped back to todo has run.
    const stopped = phase("p2", [task("todo", { runs: ["r1"] })], "planned");
    expect(phasePlanning(construction("stopped", { phases: [done, stopped] }), stopped)).toBeNull();
    const failed = phase("p2", [task("failed")], "failed");
    expect(phasePlanning(construction("failed", { phases: [done, failed] }), failed)).toBeNull();
    expect(phasePlanning(construction("running", { phases: [done, second] }), second)).toBeNull();
    expect(
      phasePlanning(construction("planned", { phases: [done, second], planning: true }), second),
    ).toBeNull();
  });
});

describe("currentWork while only planning", () => {
  it("is planning the chosen phase, even once its tasks are saved", () => {
    const planned = phase("p2", [task("todo")], "planned");
    const c = construction("running", {
      plan_only: true,
      until_phase: "p2",
      phases: [phase("p1", [task("done")], "done"), planned],
    });
    expect(currentWork(c)).toMatchObject({ kind: "planning", phase: { id: "p2" } });
  });
});
