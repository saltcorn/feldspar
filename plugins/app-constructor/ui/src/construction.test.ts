/**
 * The Plan & build screen's decisions (TODO-app-constructor AC4.4): which step
 * is shown for each status, when polling runs, which task buttons appear, and
 * the phase draft edits; what the buttons ask the module's routes.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  chosenMockup,
  latestMockups,
  mockupDocument,
  mockupPages,
  researchEditable,
  changedAnswers,
  askingDiscards,
  mockupsWithoutQuestions,
  planningUnresearched,
  planningWithoutMockup,
  revisionFeedback,
  type ConstructionMockup,
  addPhase,
  addRequirement,
  blankConstruction,
  constructRoute,
  feedbackPath,
  feedbackPhases,
  feedbackDeletable,
  feedbackTasks,
  feedbackEditable,
  agentBusy,
  agentWork,
  TABS,
  createdEntity,
  dependencyTitles,
  landingTab,
  loadConstruction,
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
import type { Id } from "./types";

function task(
  status: string,
  extra: Partial<ConstructionTask> = {}
): ConstructionTask {
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

function phase(
  id: string,
  tasks: ConstructionTask[] = [],
  status = "unplanned"
): ConstructionPhase {
  return {
    id,
    title: `Phase ${id}`,
    description: "",
    requirements: [{ text: "lists books", priority: "must" }],
    status,
    tasks,
  };
}

function construction(
  status: string,
  extra: Partial<Construction> = {}
): Construction {
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
    designing: false,
    asking: false,
    questions: [],
    mockups: [],
    feedback: [],
    errors: [],
    updated_at: null,
    phases: [],
    ...extra,
  };
}

/** `host.json` that records each request and answers a construction. */
function recording(answer: unknown = construction("running")) {
  const calls: unknown[][] = [];
  const request = vi.fn(async (path: string, init?: RequestInit) => {
    calls.push(
      init?.method === "POST"
        ? [init.method, path, JSON.parse(String(init.body))]
        : [init?.method ?? "GET", path]
    );
    return answer;
  });
  return { request, calls };
}
const base = "/applications/a1/construction";

describe("the step the screen opens on", () => {
  it("offers to start a construction when the application has none", () => {
    expect(currentStep(null)).toBe("start");
  });

  it("asks for the specification while it is a draft", () => {
    expect(currentStep(construction("draft"))).toBe("specification");
  });

  it("shows the phases while they are planned, and once they are", () => {
    expect(currentStep(construction("draft", { planning: true }))).toBe(
      "phases"
    );
    expect(
      currentStep(construction("planned", { phases: [phase("p1")] }))
    ).toBe("phases");
  });

  it("shows the activity once a task has run, and whenever it runs or ran", () => {
    const ran = construction("planned", {
      phases: [phase("p1", [task("done"), task("todo")])],
    });
    expect(currentStep(ran)).toBe("activity");
    for (const status of ["running", "stopped", "failed", "done"]) {
      expect(currentStep(construction(status))).toBe("activity");
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
    expect(tabOpen(construction("draft", { planning: true }), "phases")).toBe(
      true
    );
    expect(
      tabOpen(construction("planned", { phases: [phase("p1")] }), "phases")
    ).toBe(true);
  });

  it("always opens the activity, which says what there is even when nothing ran", () => {
    expect(tabOpen(construction("draft"), "activity")).toBe(true);
    expect(tabOpen(blankConstruction("a1"), "activity")).toBe(true);
  });

  it("puts the activity last, after the steps", () => {
    expect(TABS[TABS.length - 1]).toBe("activity");
  });
});

describe("a failed plan", () => {
  const error =
    "planning the phases failed: the LLM provider failed: invalid x-api-key";

  it("is shown with the specification once the plan run has ended", () => {
    expect(planError(construction("draft", { error }))).toBe(error);
    expect(
      planError(construction("planned", { error, phases: [phase("p1")] }))
    ).toBe(error);
    expect(planError(construction("draft"))).toBeNull();
  });

  it("is not shown while planning again, nor once the progress shows the error", () => {
    expect(
      planError(construction("draft", { error, planning: true }))
    ).toBeNull();
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
    expect(update).toHaveBeenLastCalledWith(
      expect.objectContaining({ status: "done" })
    );
    // Done: nothing is scheduled any more.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(load).toHaveBeenCalledTimes(2);
  });

  it("does not start for a construction that is not running", async () => {
    const load = vi.fn(async () => construction("planned"));
    pollConstruction(
      construction("planned"),
      load,
      () => {},
      () => {},
      2000
    );
    await vi.advanceTimersByTimeAsync(10_000);
    expect(load).not.toHaveBeenCalled();
  });

  it("stops when it is cancelled, even with a load scheduled", async () => {
    const load = vi.fn(async () => construction("running"));
    const stop = pollConstruction(
      construction("running"),
      load,
      () => {},
      () => {},
      2000
    );
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
    expect(phasesEditable(construction("draft", { planning: true }))).toBe(
      false
    );
  });

  it("retries a failed task and skips a waiting or failed one", () => {
    const stopped = construction("stopped");
    expect(taskActions(task("failed"), stopped)).toEqual({
      retry: true,
      skip: true,
    });
    expect(taskActions(task("todo"), stopped)).toEqual({
      retry: false,
      skip: true,
    });
    for (const status of ["done", "skipped", "running"]) {
      expect(taskActions(task(status), stopped)).toEqual({
        retry: false,
        skip: false,
      });
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
  it("links a task's run to its page in the admin", () => {
    expect(runHref("r1")).toBe("#/runs/r1");
  });

  it("builds React applications only", () => {
    expect(constructible({ framework: { name: "react" } })).toBe(true);
    expect(constructible({ framework: { name: "saltcorn" } })).toBe(false);
  });

  it("counts the phases done", () => {
    const c = construction("running", {
      phases: [
        phase("p1", [], "done"),
        phase("p2", [], "running"),
        phase("p3"),
      ],
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
    expect(
      draftsChanged(updatePhase(drafts, 0, { title: "Data" }), phases)
    ).toBe(true);
  });

  it("reorders, and ignores a move past either end", () => {
    const drafts = toDrafts(phases);
    expect(movePhase(drafts, 0, 1).map((d) => d.id)).toEqual([
      "p2",
      "p1",
      "p3",
    ]);
    expect(movePhase(drafts, 2, -1).map((d) => d.id)).toEqual([
      "p1",
      "p3",
      "p2",
    ]);
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
    drafts = updateRequirement(drafts, 1, 1, {
      text: "search",
      priority: "could",
    });
    expect(drafts[1].requirements[1]).toEqual({
      text: "search",
      priority: "could",
    });
    drafts = removeRequirement(drafts, 1, 0);
    expect(drafts[1].requirements).toEqual([
      { text: "search", priority: "could" },
    ]);
    // The other phases are untouched.
    expect(drafts[0].requirements).toEqual(phases[0].requirements);
  });
});

describe("what the buttons call", () => {
  it("calls stop, retry and skip on the application's construction", async () => {
    const { request, calls } = recording();
    const buttons = constructionCalls(request, "a1");
    await buttons.stop();
    await buttons.retry(11);
    await buttons.skip(12);
    expect(calls).toEqual([
      ["POST", `${base}/stop`, {}],
      ["POST", `${base}/tasks/11/retry`, {}],
      ["POST", `${base}/tasks/12/skip`, {}],
    ]);
  });

  it("answers the construction the route answers", async () => {
    const { request } = recording(construction("stopped"));
    expect(await constructionCalls(request, "a1").stop()).toMatchObject({
      status: "stopped",
    });
  });

  it("plans one phase's tasks, with or without a note", async () => {
    const { request, calls } = recording();
    const buttons = constructionCalls(request, "a1");
    await buttons.planTasks(3, null);
    await buttons.planTasks(3, "no admin screen");
    expect(calls).toEqual([
      ["POST", `${base}/phases/3/plan`, { note: null }],
      ["POST", `${base}/phases/3/plan`, { note: "no admin screen" }],
    ]);
  });

  it("starts a construction without naming a provider: the application's settings choose it", async () => {
    const { request, calls } = recording();
    await constructionCalls(request, "a1").create();
    expect(calls).toEqual([["POST", base, {}]]);
  });

  it("names the application in the path as a path segment", async () => {
    const { request, calls } = recording();
    await constructionCalls(request, "a 1/b").stop();
    expect(calls).toEqual([
      ["POST", "/applications/a%201%2Fb/construction/stop", {}],
    ]);
  });

  it("shows an application without a construction its empty specification", () => {
    const blank = blankConstruction("a1");
    expect(blank).toMatchObject({
      application: "a1",
      specification: "",
      phases: [],
    });
    expect(currentStep(blank)).toBe("specification");
    expect(tabOpen(blank, "specification")).toBe(true);
    expect(tabOpen(blank, "phases")).toBe(false);
    expect(tabOpen(blank, "activity")).toBe(true);
  });

  it("runs all phases or up to one", async () => {
    const { request, calls } = recording();
    const buttons = constructionCalls(request, "a1");
    await buttons.run(null);
    await buttons.run(2);
    expect(calls).toEqual([
      ["POST", `${base}/run`, { until_phase: null }],
      ["POST", `${base}/run`, { until_phase: 2 }],
    ]);
  });

  it("saves the specification on the screen before planning from it", async () => {
    const { request, calls } = recording();
    await constructionCalls(request, "a1").plan("A shop for books.");
    expect(calls).toEqual([
      ["POST", `${base}/specification`, { specification: "A shop for books." }],
      ["POST", `${base}/plan`, {}],
    ]);
  });

  it("saves the phases whole, and gives, plans and deletes feedback", async () => {
    const { request, calls } = recording();
    const buttons = constructionCalls(request, "a1");
    const phases = [
      { id: 1, title: "Tables", description: "", requirements: [] },
    ];
    await buttons.savePhases(phases);
    await buttons.giveFeedback(1, "Sorting", "By title");
    await buttons.planFeedback(7);
    await buttons.deleteFeedback(7);
    expect(calls).toEqual([
      ["POST", `${base}/phases`, { phases }],
      [
        "POST",
        `${base}/feedback`,
        { phase: 1, title: "Sorting", description: "By title" },
      ],
      ["POST", `${base}/feedback/7/plan`, {}],
      ["POST", `${base}/feedback/7/delete`, {}],
    ]);
  });

  it("loads the construction, and none for the route's 404", async () => {
    const answering = (status: number, body: unknown) =>
      vi.fn(
        async (_path: string) => new Response(JSON.stringify(body), { status })
      );
    const found = answering(200, construction("draft"));
    expect(await loadConstruction(found, "a1")).toMatchObject({
      status: "draft",
    });
    expect(found).toHaveBeenCalledWith(base);
    expect(
      await loadConstruction(answering(404, { error: "no construction" }), "a1")
    ).toBeNull();
    await expect(
      loadConstruction(
        answering(403, { error: "Only an admin can do this" }),
        "a1"
      )
    ).rejects.toThrow("Only an admin can do this");
  });
});

describe("a phase's own page", () => {
  it("has a path of the screen's own, and the path is read back", () => {
    expect(phasesPath()).toBe("/phases");
    expect(phasePath("p 1")).toBe("/phases/p%201");
    expect(phasePath(4)).toBe("/phases/4");
    expect(constructRoute("")).toEqual({
      tab: null,
      phaseId: null,
      feedbackPhase: null,
    });
    expect(constructRoute("/")).toEqual({
      tab: null,
      phaseId: null,
      feedbackPhase: null,
    });
    expect(constructRoute("/phases")).toEqual({
      tab: "phases",
      phaseId: null,
      feedbackPhase: null,
    });
    expect(constructRoute(phasePath("p 1"))).toEqual({
      tab: null,
      phaseId: "p 1",
      feedbackPhase: null,
    });
    expect(constructRoute(phasePath(4))?.phaseId).toBe("4");
    // The Feedback tab, with the phase new feedback is for.
    expect(constructRoute(feedbackPath())).toEqual({
      tab: "feedback",
      phaseId: null,
      feedbackPhase: null,
    });
    expect(feedbackPath(4)).toBe("/feedback/4");
    expect(constructRoute(feedbackPath("p 1"))?.feedbackPhase).toBe("p 1");
    expect(constructRoute("/other")).toBeNull();
    expect(constructRoute("/phases/1/tasks")).toBeNull();
  });

  it("reads what a task created", () => {
    expect(createdEntity({ kind: "table", name: "machines" })).toEqual({
      kind: "table",
      name: "machines",
    });
    expect(createdEntity({ name: "listSets" })).toEqual({
      kind: "",
      name: "listSets",
    });
    expect(createdEntity({ kind: "table" })).toBeNull();
    expect(createdEntity("machines")).toBeNull();
  });

  it("names the tasks a task waits for, in the phase's order", () => {
    const a = { ...task("done"), id: "a", title: "Tables" };
    const b = { ...task("done"), id: "b", title: "Queries" };
    const c = {
      ...task("todo"),
      id: "c",
      title: "Page",
      depends_on: ["b", "a", "gone"],
    };
    expect(dependencyTitles(c, phase("p1", [a, b, c]))).toEqual([
      "Tables",
      "Queries",
    ]);
  });
});

describe("the tab the screen opens on", () => {
  it("is the current step's, except that a build under way opens on its phases", () => {
    expect(landingTab("specification")).toBe("specification");
    expect(landingTab("phases")).toBe("phases");
    expect(landingTab("activity")).toBe("phases");
  });
});

describe("running up to a phase", () => {
  it("names the earlier phases that are not done, which it would run first", () => {
    const c = construction("planned", {
      phases: [
        phase("p1", [], "done"),
        phase("p2", [], "failed"),
        phase("p3"),
        phase("p4"),
      ],
    });
    expect(unfinishedBefore(c, "p4").map((p) => p.id)).toEqual(["p2", "p3"]);
    expect(unfinishedBefore(c, "p2")).toEqual([]);
    expect(unfinishedBefore(c, "p1")).toEqual([]);
    expect(unfinishedBefore(c, "gone")).toEqual([]);
  });
});

describe("what a running construction is on", () => {
  const task = (id: string, status: string, runs: string[] = []) => ({
    id,
    kind: "data",
    title: `Task ${id}`,
    brief: "",
    depends_on: [],
    status,
    runs,
    summary: null,
    created: [],
  });
  const phaseOf = (
    id: string,
    status: string,
    tasks: ReturnType<typeof task>[]
  ) => ({
    id,
    title: `Phase ${id}`,
    description: "",
    requirements: [],
    status,
    tasks,
  });
  const running = (phases: ReturnType<typeof phaseOf>[]) => ({
    ...blankConstruction("a1"),
    status: "running",
    phases,
  });

  it("is planning the first unfinished phase when it has no tasks yet", () => {
    const c = running([
      phaseOf("p1", "unplanned", []),
      phaseOf("p2", "unplanned", []),
    ]);
    expect(currentWork(c)).toMatchObject({
      kind: "planning",
      phase: { id: "p1" },
    });
  });

  it("is the running task, else the next one to start", () => {
    const c = running([
      phaseOf("p1", "running", [
        task("t1", "done"),
        task("t2", "running", ["r1"]),
      ]),
    ]);
    expect(currentWork(c)).toMatchObject({ kind: "task", task: { id: "t2" } });
    const between = running([
      phaseOf("p1", "planned", [task("t1", "done"), task("t2", "todo")]),
    ]);
    expect(currentWork(between)).toMatchObject({
      kind: "task",
      task: { id: "t2" },
    });
  });

  it("is nothing when the construction is not running", () => {
    expect(
      currentWork({
        ...running([phaseOf("p1", "unplanned", [])]),
        status: "planned",
      })
    ).toBeNull();
  });
});

describe("the drafts after the server's phases change", () => {
  const base = [phase("p1")];
  const added = [
    ...toDrafts(base),
    { id: null, title: "Checkout", description: "", requirements: [] },
  ];
  const answer = [
    phase("p1"),
    { ...phase("p2"), title: "Checkout", requirements: [] },
  ];

  it("follow the save's answer, so a new phase gets its id and the card is not left unsaved", () => {
    const next = followedDrafts(added, base, answer, added);
    expect(next).toEqual(toDrafts(answer));
    expect(draftsChanged(next!, answer)).toBe(false);
  });

  it("keep the admin's edits when nothing was saved, or the plan came back the same", () => {
    expect(followedDrafts(added, base, answer, null)).toBeNull();
    expect(followedDrafts(added, base, [phase("p1")], added)).toBeNull();
    // Edited again after saving: the newer edits stay.
    const edited = added.map((d) =>
      d.id === null ? { ...d, title: "Pay" } : d
    );
    expect(followedDrafts(edited, base, answer, added)).toBeNull();
  });

  it("follow the server when there are no edits", () => {
    expect(followedDrafts(toDrafts(base), base, answer, null)).toEqual(
      toDrafts(answer)
    );
  });
});

describe("phasePlanning", () => {
  const done = phase("p1", [task("done")], "done");

  it("plans the next phase, and plans again one whose tasks have not started", () => {
    const fresh = phase("p2");
    expect(
      phasePlanning(construction("planned", { phases: [done, fresh] }), fresh)
    ).toBe("plan");
    const planned = phase("p2", [task("todo")], "planned");
    expect(
      phasePlanning(
        construction("planned", { phases: [done, planned] }),
        planned
      )
    ).toBe("replan");
  });

  it("offers nothing for a later phase, one that has started, or while it runs", () => {
    const first = phase("p1");
    const second = phase("p2");
    // Planned against what the first makes: not before it is done.
    expect(
      phasePlanning(
        construction("planned", { phases: [first, second] }),
        second
      )
    ).toBeNull();
    // A task stopped back to todo has run.
    const stopped = phase("p2", [task("todo", { runs: ["r1"] })], "planned");
    expect(
      phasePlanning(
        construction("stopped", { phases: [done, stopped] }),
        stopped
      )
    ).toBeNull();
    const failed = phase("p2", [task("failed")], "failed");
    expect(
      phasePlanning(construction("failed", { phases: [done, failed] }), failed)
    ).toBeNull();
    expect(
      phasePlanning(construction("running", { phases: [done, second] }), second)
    ).toBeNull();
    expect(
      phasePlanning(
        construction("planned", { phases: [done, second], planning: true }),
        second
      )
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
    expect(currentWork(c)).toMatchObject({
      kind: "planning",
      phase: { id: "p2" },
    });
  });
});

describe("the mockups", () => {
  function mockup(
    id: string,
    round: number,
    extra: Partial<ConstructionMockup> = {}
  ) {
    return {
      id,
      round,
      title: `Mockup ${id}`,
      rationale: "",
      screens: [{ name: "Orders list", file: "orders-list.html" }],
      status: "proposed",
      feedback: null,
      ...extra,
    };
  }

  it("open the research step once proposed or being proposed, before the phases", () => {
    expect(currentStep(construction("draft", { designing: true }))).toBe(
      "research"
    );
    expect(
      currentStep(construction("draft", { mockups: [mockup("m1", 1)] }))
    ).toBe("research");
    // Open once there is a specification to research.
    expect(
      tabOpen(construction("draft", { specification: " " }), "research")
    ).toBe(false);
    expect(tabOpen(construction("draft"), "research")).toBe(true);
    expect(
      tabOpen(construction("draft", { designing: true }), "research")
    ).toBe(true);
    // Planning the phases moves on, with or without a chosen mockup.
    const planning = construction("draft", {
      planning: true,
      mockups: [mockup("m1", 1)],
    });
    expect(currentStep(planning)).toBe("phases");
    expect(shouldPoll(construction("draft", { designing: true }))).toBe(true);
    expect(
      planError(construction("draft", { designing: true, error: "x" }))
    ).toBeNull();
  });

  it("are chosen from the last round, and the chosen one may be older", () => {
    const c = construction("draft", {
      mockups: [
        mockup("m1", 1, { status: "chosen" }),
        mockup("m2", 1),
        mockup("m3", 2),
      ],
    });
    expect(latestMockups(c).map((m) => m.id)).toEqual(["m3"]);
    expect(chosenMockup(c)?.id).toBe("m1");
    expect(chosenMockup(construction("draft"))).toBeNull();
    expect(latestMockups(construction("draft"))).toEqual([]);
  });

  it("send the feedback written on the last round, blanks left out", () => {
    const c = construction("draft", {
      mockups: [mockup("m1", 1), mockup("m2", 2), mockup("m3", 2)],
    });
    expect(
      revisionFeedback(c, { m1: "old", m2: " denser ", m3: "  " })
    ).toEqual([{ mockup: "m2", text: "denser" }]);
  });

  it("are not touched while the agent works or the construction runs", () => {
    expect(researchEditable(construction("draft"))).toBe(true);
    expect(researchEditable(construction("draft", { designing: true }))).toBe(
      false
    );
    expect(researchEditable(construction("draft", { asking: true }))).toBe(
      false
    );
    expect(researchEditable(construction("draft", { planning: true }))).toBe(
      false
    );
    expect(researchEditable(construction("running"))).toBe(false);
  });

  it("keep a page's doctype first when links are sent nowhere", () => {
    const base = '<base target="_blank">';
    expect(mockupDocument("<!DOCTYPE html><html><head><title>x</title>")).toBe(
      `<!DOCTYPE html><html><head>${base}<title>x</title>`
    );
    expect(mockupDocument("<!doctype html><main>x</main>")).toBe(
      `<!doctype html>${base}<main>x</main>`
    );
    expect(mockupDocument("<main>x</main>")).toBe(`${base}<main>x</main>`);
  });

  it("save the specification before proposing, and revise or choose by id", async () => {
    const { request, calls } = recording(construction("draft"));
    const buttons = constructionCalls(request, "a1");
    await buttons.proposeMockups("A shop.", 2);
    await buttons.reviseMockups([{ mockup: 1, text: "denser" }], null);
    await buttons.chooseMockup(2);
    await buttons.mockupScreens(2);
    expect(calls).toEqual([
      ["POST", `${base}/specification`, { specification: "A shop." }],
      ["POST", `${base}/mockups`, { count: 2, feedback: null, note: null }],
      [
        "POST",
        `${base}/mockups`,
        { count: null, feedback: [{ mockup: 1, text: "denser" }], note: null },
      ],
      ["POST", `${base}/mockups/2/choose`, {}],
      ["GET", `${base}/mockups/2/screens`],
    ]);
  });

  it("fetch a mockup's screens once, and again only after a failure", async () => {
    const load = vi.fn(async (mockup: Id) => [
      { html: `<p>${mockup} 1</p>` },
      { html: "<p>2</p>" },
    ]);
    expect(await mockupPages(load, "cached-1")).toEqual([
      "<p>cached-1 1</p>",
      "<p>2</p>",
    ]);
    expect(await mockupPages(load, "cached-1")).toEqual([
      "<p>cached-1 1</p>",
      "<p>2</p>",
    ]);
    expect(load).toHaveBeenCalledTimes(1);

    const failing = vi
      .fn<(mockup: Id) => Promise<{ html: string }[]>>()
      .mockRejectedValueOnce(new Error("offline"))
      .mockResolvedValueOnce([{ html: "<p>back</p>" }]);
    await expect(mockupPages(failing, "cached-2")).rejects.toThrow("offline");
    expect(await mockupPages(failing, "cached-2")).toEqual(["<p>back</p>"]);
    expect(failing).toHaveBeenCalledTimes(2);
  });
});

describe("the clarifying questions", () => {
  const question = (id: string, answer: string | null = null) => ({
    id,
    text: `Question ${id}?`,
    answer,
  });

  it("open the research step once asked or being asked, and keep the screen polling", () => {
    expect(currentStep(construction("draft", { asking: true }))).toBe(
      "research"
    );
    expect(
      currentStep(construction("draft", { questions: [question("q1")] }))
    ).toBe("research");
    expect(tabOpen(construction("draft", { asking: true }), "research")).toBe(
      true
    );
    expect(
      tabOpen(
        construction("draft", { questions: [question("q1")] }),
        "research"
      )
    ).toBe(true);
    expect(shouldPoll(construction("draft", { asking: true }))).toBe(true);
    expect(
      planError(construction("draft", { asking: true, error: "x" }))
    ).toBeNull();
  });

  it("send only the answers changed on the screen, a cleared one as blank", () => {
    const c = construction("draft", {
      questions: [
        question("q1", "Sales"),
        question("q2"),
        question("q3", "Euro"),
      ],
    });
    expect(changedAnswers(c, {})).toEqual([]);
    expect(
      changedAnswers(c, { q1: " Sales ", q2: " Everyone ", q3: "", gone: "x" })
    ).toEqual([
      { question: "q2", text: "Everyone" },
      { question: "q3", text: "" },
    ]);
  });

  it("ask to confirm a step that skips what the next one is drawn from", () => {
    const m = {
      id: "m1",
      round: 1,
      title: "Table",
      rationale: "",
      screens: [],
      status: "proposed",
      feedback: null,
    };
    // Planning with nothing researched, but only the first time.
    expect(planningUnresearched(construction("draft"))).toBe(true);
    expect(
      planningUnresearched(
        construction("draft", { questions: [question("q1")] })
      )
    ).toBe(false);
    expect(planningUnresearched(construction("draft", { mockups: [m] }))).toBe(
      false
    );
    expect(
      planningUnresearched(construction("planned", { phases: [phase("p1")] }))
    ).toBe(false);
    // Mockups before any question.
    expect(mockupsWithoutQuestions(construction("draft"))).toBe(true);
    expect(
      mockupsWithoutQuestions(
        construction("draft", { questions: [question("q1")] })
      )
    ).toBe(false);
    // Planning from the research without a chosen mockup.
    expect(planningWithoutMockup(construction("draft"))).toBe("none");
    expect(planningWithoutMockup(construction("draft", { mockups: [m] }))).toBe(
      "unchosen"
    );
    expect(
      planningWithoutMockup(
        construction("draft", { mockups: [{ ...m, status: "chosen" }] })
      )
    ).toBe(null);
    // Asking with mockups there discards them.
    expect(askingDiscards(construction("draft"))).toBe(0);
    expect(
      askingDiscards(
        construction("draft", { mockups: [m, { ...m, id: "m2" }] })
      )
    ).toBe(2);
  });

  it("save the specification before asking, and save the answers", async () => {
    const { request, calls } = recording(construction("draft"));
    const buttons = constructionCalls(request, "a1");
    await buttons.askQuestions("A shop.");
    await buttons.answerQuestions([{ question: 1, text: "Sales" }]);
    expect(calls).toEqual([
      ["POST", `${base}/specification`, { specification: "A shop." }],
      ["POST", `${base}/questions`, {}],
      [
        "POST",
        `${base}/questions/answer`,
        { answers: [{ question: 1, text: "Sales" }] },
      ],
    ]);
  });
});

describe("the feedback on the phases", () => {
  const built = (status: string, extra: Partial<ConstructionTask> = {}) =>
    phase(
      "p1",
      [task("done", { id: "t1" }), task(status, { id: "t2", ...extra })],
      "planned"
    );
  const given = (extra: Partial<Construction["feedback"][number]> = {}) => ({
    id: "f1",
    phase: "p1",
    title: "Sorting",
    description: "Sort by name",
    requirements: [],
    run: "r1",
    planning: false,
    status: "planned",
    ...extra,
  });

  it("is given on a phase that built something and has no failed task", () => {
    expect(
      feedbackPhases(construction("planned", { phases: [phase("p1")] }))
    ).toEqual([]);
    expect(
      feedbackPhases(construction("done", { phases: [built("todo")] })).map(
        (p) => p.id
      )
    ).toEqual(["p1"]);
    expect(
      feedbackPhases(construction("failed", { phases: [built("failed")] }))
    ).toEqual([]);
    // Its tab opens with it.
    expect(
      tabOpen(construction("planned", { phases: [phase("p1")] }), "feedback")
    ).toBe(false);
    expect(
      tabOpen(construction("done", { phases: [built("todo")] }), "feedback")
    ).toBe(true);
  });

  it("keeps the agent busy while it is planned, and waits for the screen", () => {
    const planning = construction("done", {
      feedback: [given({ planning: true })],
    });
    expect(agentBusy(planning)).toBe(true);
    expect(shouldPoll(planning)).toBe(true);
    expect(feedbackEditable(planning)).toBe(false);
    expect(feedbackEditable(construction("running"))).toBe(false);
    expect(feedbackEditable(construction("done"))).toBe(true);
  });

  it("is deleted with its tasks only until one has run", () => {
    const f = given();
    const waiting = construction("done", {
      phases: [built("todo", { feedback: "f1" })],
    });
    expect(feedbackDeletable(waiting, f)).toBe(true);
    const ran = construction("done", {
      phases: [built("done", { feedback: "f1" })],
    });
    expect(feedbackDeletable(ran, f)).toBe(false);
    expect(feedbackDeletable(waiting, { ...f, planning: true })).toBe(false);
  });
});

describe("the activity", () => {
  it("says what the construction agent is doing outside the runner", () => {
    expect(agentWork(construction("draft"))).toBeNull();
    expect(agentWork(construction("draft", { planning: true }))).toEqual({
      kind: "phases",
    });
    expect(agentWork(construction("draft", { designing: true }))).toEqual({
      kind: "mockups",
    });
    expect(agentWork(construction("draft", { asking: true }))).toEqual({
      kind: "questions",
    });
    const feedback = {
      id: "f1",
      phase: "p1",
      title: "Sorting",
      description: "Sort",
      requirements: [],
      run: "r1",
      planning: true,
      status: "open",
    };
    expect(
      agentWork(construction("planned", { feedback: [feedback] }))
    ).toEqual({
      kind: "feedback",
      feedback,
    });
  });
});

describe("the module's row ids", () => {
  // The module's tables have integer ids, which the routes answer as numbers;
  // a path of the screen names them as text.
  const numbered = (
    id: number,
    tasks: ConstructionTask[] = [],
    status = "unplanned"
  ) => ({
    ...phase(`p${id}`, tasks, status),
    id,
  });

  it("finds a phase a path names, and the phases before it", () => {
    const c = construction("planned", {
      phases: [numbered(1, [], "done"), numbered(2), numbered(3)],
    });
    expect(unfinishedBefore(c, "3").map((p) => p.id)).toEqual([2]);
    expect(unfinishedBefore(c, 3).map((p) => p.id)).toEqual([2]);
    expect(phasePlanning(c, numbered(2))).toBe("plan");
  });

  it("follows the phase a plan-only run is on", () => {
    const c = construction("running", {
      plan_only: true,
      until_phase: 2,
      phases: [numbered(1, [task("done")], "done"), numbered(2)],
    });
    expect(currentWork(c)).toMatchObject({
      kind: "planning",
      phase: { id: 2 },
    });
  });

  it("names the tasks a task waits for by their uid, as the module records them", () => {
    const a = { ...task("done", { id: 10, title: "Tables" }), uid: "u-a" };
    const b = {
      ...task("todo", { id: 11, title: "Lists", depends_on: ["u-a"] }),
      uid: "u-b",
    };
    const byId = task("todo", { id: 12, title: "Forms", depends_on: [10] });
    const p = numbered(1, [a, b, byId]);
    expect(dependencyTitles(b, p)).toEqual(["Tables"]);
    expect(dependencyTitles(byId, p)).toEqual(["Tables"]);
  });

  it("matches a feedback's tasks, the answers and the mockup notes by id", () => {
    const fb = {
      id: 5,
      phase: 1,
      title: "Sorting",
      description: "",
      requirements: [],
      run: null,
      planning: false,
      status: "planned",
    };
    const c = construction("done", {
      feedback: [fb],
      phases: [
        numbered(
          1,
          [task("done", { id: 1 }), task("todo", { id: 2, feedback: 5 })],
          "done"
        ),
      ],
      questions: [
        { id: 7, text: "Who?", answer: null },
        { id: 8, text: "Currency?", answer: "Euro" },
      ],
      mockups: [
        {
          id: 3,
          round: 1,
          title: "A",
          rationale: "",
          screens: [],
          status: "proposed",
          feedback: null,
        },
      ],
    });
    expect(feedbackTasks(c, fb).map((t) => t.id)).toEqual([2]);
    expect(changedAnswers(c, { "7": "Sales", "8": "Euro" })).toEqual([
      { question: 7, text: "Sales" },
    ]);
    expect(revisionFeedback(c, { "3": "denser" })).toEqual([
      { mockup: 3, text: "denser" },
    ]);
    expect(
      phaseDeletable(
        { id: 1, title: "", description: "", requirements: [] },
        c.phases
      )
    ).toBe(false);
  });

  it("fetches a mockup's screens once, whether its id comes as a number or text", async () => {
    const load = vi.fn(async (_mockup: Id) => [{ html: "<p>one</p>" }]);
    expect(await mockupPages(load, 900)).toEqual(["<p>one</p>"]);
    expect(await mockupPages(load, "900")).toEqual(["<p>one</p>"]);
    expect(load).toHaveBeenCalledTimes(1);
  });
});
