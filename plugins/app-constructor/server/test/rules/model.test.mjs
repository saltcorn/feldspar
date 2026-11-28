// The rules with no database (ported from the unit tests in crates/sc-construct/src): a task
// depends only on earlier ones, what the runner does next, what is plannable, the replan note,
// what a round of mockups, a list of questions and a feedback plan may be, and the texts the
// agent is told.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const m = require("../../rules/model.js");

let nextId = 1;
const phase = (title) => ({ id: nextId++, construction: 1, position: 1, title, description: "", requirements: [] });
const input = (title, depends_on = []) => ({ kind: "feature", title, brief: `Make ${title}`, depends_on });
const withTasks = (p, tasks) => ({ phase: p, tasks });
const withStatus = (task, status) => ({ ...task, status });

test("depends_on names earlier tasks by number", () => {
  const tasks = m.buildTasks(1, [input("A"), input("B", [1]), input("C", [1, 2, 1])]);
  assert.deepEqual(tasks[1].depends_on, [tasks[0].uid]);
  assert.deepEqual(tasks[2].depends_on, [tasks[0].uid, tasks[1].uid]);
  assert.equal(tasks[2].position, 3);
  assert.notEqual(tasks[0].uid, tasks[1].uid);

  assert.throws(() => m.buildTasks(1, [input("A", [1])]), /task 1 depends on task 1/);
  assert.throws(() => m.buildTasks(1, [input("A"), input("B", [3])]), /not before it/);
  assert.throws(() => m.buildTasks(1, []), /at least one task/);
  assert.throws(() => m.buildTasks(1, [{ ...input("A"), brief: "  " }]), /task 1 has no brief/);
  assert.throws(() => m.buildTasks(1, Array.from({ length: m.MAX_TASKS + 1 }, (_, i) => input(`T${i}`))), /too many/);
});

test("a task list arrives as JSON and is refused with the task it is in", () => {
  const tasks = m.parseTasks([{ kind: "data_model", title: "Tables", brief: "Make them" }]);
  assert.deepEqual(tasks, [{ kind: "data_model", title: "Tables", brief: "Make them", depends_on: [] }]);
  assert.throws(() => m.parseTasks({}), /`tasks` must be a list/);
  assert.throws(() => m.parseTasks([{ kind: "data_model", title: "A", brief: "B" }, { kind: "code", title: "A", brief: "B" }]), /task 2: `kind` must be one of `data_model`, `feature`, not "code"/);
  assert.throws(() => m.parseTasks([{ kind: "feature", title: "A" }]), /task 1 has no `brief`/);
  assert.throws(() => m.parseTasks([{ kind: "feature", title: "A", brief: "B", after: 1 }]), /task 1 has the unknown field `after`/);
  assert.throws(() => m.parseTasks([{ kind: "feature", title: "A", brief: "B", depends_on: [-1] }]), /task 1: `depends_on` item 1 must be at least 1/);
});

test("the next step plans a phase only when it is reached", () => {
  const [first, second] = [phase("Data model"), phase("Contacts")];
  const tables = m.newTask(first.id, 1, "data_model", "Tables", "Make them");
  let plan = [withTasks(first, [tables]), withTasks(second, [])];
  assert.deepEqual(m.nextStep(plan, null), { kind: "run_task", task: tables });

  plan = [withTasks(first, [withStatus(tables, "done")]), withTasks(second, [])];
  assert.deepEqual(m.nextStep(plan, null), { kind: "plan_tasks", phase: second });
  assert.deepEqual(m.nextStep(plan, first.id), { kind: "finished" });
  // An id as text, as a URL gives it, is the same phase.
  assert.deepEqual(m.nextStep(plan, String(first.id)), { kind: "finished" });
});

test("depends_on orders the tasks and a skip counts as done", () => {
  const p = phase("Contacts");
  const tasks = m.buildTasks(p.id, [input("List"), input("Edit", [1])]);
  // Saved in another order: the dependency still decides.
  assert.deepEqual(m.nextStep([withTasks(p, [tasks[1], tasks[0]])]), { kind: "run_task", task: tasks[0] });
  const skipped = withStatus(tasks[0], "skipped");
  assert.deepEqual(m.nextStep([withTasks(p, [skipped, tasks[1]])]), { kind: "run_task", task: tasks[1] });
});

test("a failed task stops and a running one is resumed first", () => {
  const p = phase("Contacts");
  const tasks = m.buildTasks(p.id, [input("List"), input("Edit")]);
  const failed = withStatus(tasks[0], "failed");
  assert.deepEqual(m.nextStep([withTasks(p, [failed, tasks[1]])]), { kind: "failed", task: failed });

  const running = withStatus(tasks[1], "running");
  assert.deepEqual(m.nextStep([withTasks(p, [tasks[0], running])]), { kind: "run_task", task: running });

  // Waiting on a task of a later phase can never start.
  const stuck = { ...tasks[0], depends_on: [globalThis.crypto.randomUUID()] };
  assert.deepEqual(m.nextStep([withTasks(p, [stuck])]), { kind: "blocked", phase: p });
});

test("only the next phase whose tasks have not started is plannable", () => {
  const [first, second] = [phase("Data model"), phase("Contacts")];
  const tasks = m.buildTasks(first.id, [input("Tables")]);
  // Nothing planned: the first phase is, the second waits on it.
  const fresh = [withTasks(first, []), withTasks(second, [])];
  assert.deepEqual(m.requirePlannable(fresh, first.id).tasks, []);
  assert.throws(() => m.requirePlannable(fresh, second.id), /`Data model` is not done yet/);
  // Planned, nothing started: planned again, answering the old tasks.
  const planned = [withTasks(first, tasks), withTasks(second, [])];
  assert.deepEqual(m.requirePlannable(planned, first.id).tasks, tasks);
  // A task that ran (even one put back to todo by a stop) has started.
  const stopped = { ...tasks[0], runs: ["run-1"] };
  assert.throws(() => m.requirePlannable([withTasks(first, [stopped])], first.id), /has started/);
  // The first done (a skipped task counts): the second is next.
  const done = [withTasks(first, [withStatus(tasks[0], "skipped")]), withTasks(second, [])];
  assert.doesNotThrow(() => m.requirePlannable(done, second.id));
  assert.throws(() => m.requirePlannable(done, 999999), (e) => e.kind === "not_found");
});

test("a replan is told the old plan and the note", () => {
  const tasks = m.buildTasks(1, [input("Tables")]);
  const note = m.replanNote(tasks, "  no audit log ");
  assert.match(note, /planned before/);
  assert.ok(note.includes("1. (feature) Tables: Make Tables"), note);
  assert.ok(note.includes("note on how to plan this phase: no audit log\n"), note);
  const first = m.replanNote([], "contacts only");
  assert.ok(!first.includes("planned before"), first);
  assert.equal(m.replanNote([], "  "), null);
  assert.equal(m.replanNote([], null), null);
});

test("the task list numbers the tasks and names what each waits on", () => {
  const tasks = m.buildTasks(1, [input("List"), input("Edit", [1])]);
  assert.equal(m.taskList(tasks), "1. List [feature, todo]\n2. Edit [feature, todo] after 1\n");
  assert.equal(m.taskList([]), "No tasks yet.");
});

test("a phase's status follows from its tasks", () => {
  const t = m.newTask(1, 1, "data_model", "T", "B");
  const u = m.newTask(1, 2, "data_model", "U", "B");
  assert.equal(m.phaseStatus([]), "unplanned");
  assert.equal(m.phaseStatus([t, u]), "planned");
  assert.equal(m.phaseStatus([withStatus(t, "running"), u]), "running");
  assert.equal(m.phaseStatus([withStatus(t, "failed"), u]), "failed");
  assert.equal(m.phaseStatus([withStatus(t, "done"), withStatus(u, "skipped")]), "done");
});

const screen = (name) => ({ name, html: "<main>hi</main>" });

test("a round needs one to three mockups each with screens", () => {
  assert.throws(() => m.parseMockups([]));
  const one = { title: "Cards", rationale: "", screens: [screen("List")] };
  assert.equal(m.parseMockups([one]).length, 1);
  assert.throws(() => m.parseMockups([one, one, one, one]));
  assert.throws(() => m.parseMockups([{ title: "Cards", rationale: "", screens: [] }]), /mockup 1: `screens` must have between 1 and/);
  assert.throws(() => m.parseMockups([{ title: "Cards", rationale: "" }]), /mockup 1 has no `screens`/);
});

test("screens need distinct names and some HTML", () => {
  assert.throws(() => m.parseMockups([{ title: "A", rationale: "", screens: [screen("Orders list"), screen("orders  LIST")] }]), /two screens/);
  assert.throws(() => m.parseMockups([{ title: "A", rationale: "", screens: [{ name: "List", html: " " }] }]), /no HTML/);
  const big = "x".repeat(m.MAX_SCREEN_HTML + 1);
  assert.throws(() => m.parseMockups([{ title: "A", rationale: "", screens: [{ name: "List", html: big }] }]), /longer than/);
  assert.throws(() => m.parseMockups([{ title: " ", rationale: "", screens: [screen("List")] }]), /no title/);
});

test("a screen's file is its name as a slug", () => {
  assert.equal(m.screenFile("Orders list"), "orders-list.html");
  assert.equal(m.screenFile("  Edit / new order! "), "edit-new-order.html");
  assert.equal(m.screenFile("???"), "screen.html");
});

test("the prompt text names each screen's file and carries its HTML", () => {
  const text = m.mockupText({
    id: 1,
    construction: 1,
    round: 1,
    position: 1,
    title: "Cards",
    rationale: "One card per order.",
    screens: [{ name: "Orders list", html: "<table><th>Customer</th></table>" }],
    status: "chosen",
    feedback: "bigger buttons",
  });
  assert.ok(text.includes("The chosen mockup: Cards"));
  assert.ok(text.includes("`mockups/orders-list.html`"));
  assert.ok(text.includes("<th>Customer</th>"));
  assert.ok(text.includes("bigger buttons"));
});

test("one to ten distinct questions, each a text", () => {
  assert.throws(() => m.parseQuestions([]));
  assert.deepEqual(m.parseQuestions([" Who uses it? ", "How many?"]), ["Who uses it?", "How many?"]);
  assert.throws(() => m.parseQuestions(Array.from({ length: m.MAX_QUESTIONS + 1 }, (_, i) => `Q${i}?`)));
  assert.throws(() => m.parseQuestions(["A?", "a?"]), /asked twice/);
  assert.throws(() => m.parseQuestions(["A?", " "]), /blank/);
  assert.throws(() => m.parseQuestions([1]), /must be a text/);
  assert.throws(() => m.parseQuestions(["x".repeat(m.MAX_QUESTION + 1)]), /longer than/);
});

test("only answered questions are told", () => {
  assert.equal(m.answersText([{ text: "Who?", answer: null }]), null);
  const text = m.answersText([
    { text: "Who uses it?", answer: "The sales team " },
    { text: "Offline?", answer: null },
  ]);
  assert.ok(text.includes("Question: Who uses it?\nAnswer: The sales team"));
  assert.ok(!text.includes("Offline?"));
});

test("a feedback plan is a few requirements and tasks", () => {
  const task = { kind: "feature", title: "Sort", brief: "Sort the list by date" };
  const plan = m.parseFeedbackPlan({ requirements: [{ text: " Sorted by date ", priority: "must" }], tasks: [task] });
  assert.equal(plan.requirements[0].text, "Sorted by date");
  assert.equal(plan.tasks.length, 1);

  assert.throws(() => m.parseFeedbackPlan({ requirements: [], tasks: [task] }), /between 1 and/);
  assert.throws(() => m.parseFeedbackPlan({ requirements: [{ text: " ", priority: "must" }], tasks: [] }), /no text/);
  const six = Array.from({ length: m.MAX_FEEDBACK_TASKS + 1 }, () => task);
  assert.throws(() => m.parseFeedbackPlan({ requirements: [{ text: "x", priority: "must" }], tasks: six }), /`tasks` must have between 1 and 5 items, not 6/);
  assert.throws(() => m.parseFeedbackPlan({ requirements: [{ text: "x", priority: "maybe" }], tasks: [task] }), /requirement 1: `priority` must be one of `must`, `should`, `could`, not "maybe"/);
});

test("a phase list names the bad phase and field, and its text shows id, status and requirements", () => {
  const ok = (title, more = {}) => ({ title, description: "", requirements: [], ...more });
  assert.throws(() => m.parsePhases([ok("A"), ok("B", { priority: 1 })]), /phase 2 has the unknown field `priority`/);
  assert.throws(() => m.parsePhases([ok("A", { requirements: [{ text: "x", priority: "maybe" }] })]), /phase 1: requirement 1: `priority` must be one of/);
  assert.throws(() => m.parsePhases({ title: "A" }), /`phases` must be a list, not an object/);
  assert.throws(() => m.parsePhases([ok("A", { id: "abc" })]), /phase 1: `id` must be an integer, not a text/);
  assert.throws(() => m.parsePhases([{ title: "A" }]), /phase 1 has no `description`/);
  assert.deepEqual(m.parsePhases([ok("A")]), [{ id: null, title: "A", description: "", requirements: [] }]);
  assert.deepEqual(m.parsePhases([ok("A", { id: null })])[0].id, null);

  const p = { id: 7, construction: 1, position: 1, title: "Data model", description: "Tables", requirements: [{ text: "Customers", priority: "should" }] };
  assert.equal(m.phaseList([{ phase: p, tasks: [] }]), "1. Data model [unplanned] (id 7)\n   Tables\n   - (should) Customers");
  assert.equal(m.phaseList([]), "No phases yet.");
});

test("a feedback phase must have built something and have nothing failed", () => {
  const p = phase("Contacts");
  const t = m.newTask(p.id, 1, "feature", "List", "List them");
  assert.throws(() => m.requireFeedbackPhase([withTasks(p, [t])], p.id), /has not built anything yet/);
  const failed = { ...m.newTask(p.id, 2, "feature", "Edit", "Edit them"), status: "failed" };
  assert.throws(() => m.requireFeedbackPhase([withTasks(p, [withStatus(t, "done"), failed])], p.id), /task `Edit` of phase `Contacts` failed/);
  assert.equal(m.requireFeedbackPhase([withTasks(p, [withStatus(t, "done")])], p.id).phase, p);
  assert.throws(() => m.requireFeedbackPhase([], p.id), (e) => e.kind === "not_found");
});

test("a failure names what failed and where", () => {
  assert.deepEqual(m.failure("task", "the build is red", { phase: 1, task: 2, construction_run: 3 }), {
    source: "task",
    message: "the build is red",
    phase: 1,
    task: 2,
    feedback: null,
    run: null,
    construction_run: 3,
  });
  assert.throws(() => m.failure("nothing", "x"));
});
