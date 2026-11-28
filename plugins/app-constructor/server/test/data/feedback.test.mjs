// The admin's feedback on a built phase (ported from crates/sc-construct/tests/feedback.rs): only
// a phase that built something takes it; its plan adds requirements and follow-up tasks after
// the phase's own, which the runner reaches next; it is planned once; deleting it takes its tasks
// until one has started; deleting the construction takes it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "../helpers/fake-db.mjs";

const require = createRequire(import.meta.url);
const store = require("../../data/store.js");
const m = require("../../rules/model.js");

/** A construction with two phases, the first built (one task done), the second not planned. */
async function built() {
  const { db } = fakeDb();
  const app = globalThis.crypto.randomUUID();
  await store.createConstruction(db, app);
  const input = (title) => ({ id: null, title, description: `${title}, described`, requirements: [{ text: `${title} works`, priority: "must" }] });
  const saved = await store.savePhases(db, app, [input("Contacts"), input("Reports")]);
  const [first, second] = saved.phases.map((p) => p.phase.id);
  const task = await store.insertTask(db, m.newTask(first, 1, "feature", "Contact list", "List them"));
  await store.updateTask(db, { ...task, status: "done" });
  return { db, app, first, second };
}

const plan = () =>
  m.parseFeedbackPlan({
    requirements: [{ text: "The list is sorted by name", priority: "must" }],
    tasks: [
      { kind: "feature", title: "Sort the list", brief: "Sort the contact list by name" },
      { kind: "feature", title: "Remember the order", brief: "Keep it", depends_on: [1] },
    ],
  });

test("feedback plans follow-up tasks after the phase's own", async () => {
  const { db, app, first, second } = await built();

  // Feedback is on what a phase built.
  await assert.rejects(store.addFeedback(db, app, second, "Charts", "Add charts"), /has not built anything/);
  await assert.rejects(store.addFeedback(db, app, first, " ", "x"), /no title/);
  await assert.rejects(store.addFeedback(db, app, first, "Sorting", " "), /no description/);

  const feedback = await store.addFeedback(db, app, first, " Sorting ", " Sort by name ");
  assert.deepEqual([feedback.title, feedback.description, feedback.run], ["Sorting", "Sort by name", null]);

  const saved = await store.planFeedback(db, app, feedback.id, plan());
  assert.equal(saved.length, 2);
  const construction = await store.requireConstruction(db, app);
  const phases = await store.phasesWithTasks(db, construction.id);
  const tasks = phases[0].tasks;
  // After the phase's own task, in order, naming the feedback and waiting.
  assert.deepEqual(
    tasks.map((t) => t.title),
    ["Contact list", "Sort the list", "Remember the order"],
  );
  assert.deepEqual(
    tasks.map((t) => t.position),
    [1, 2, 3],
  );
  assert.equal(tasks[0].feedback, null);
  assert.ok(tasks.slice(1).every((t) => t.feedback === feedback.id));
  assert.deepEqual(tasks[2].depends_on, [tasks[1].uid]);
  assert.equal(m.phaseStatus(tasks), "planned");
  assert.equal(m.feedbackTasks(phases, feedback.id).length, 2);
  const stored = (await store.feedbackOf(db, construction.id))[0];
  assert.equal(stored.requirements[0].text, "The list is sorted by name");

  // The runner reaches them before the next phase.
  assert.deepEqual(m.nextStep(phases, null), { kind: "run_task", task: tasks[1] });

  // Planned once.
  await assert.rejects(store.planFeedback(db, app, feedback.id, plan()), /planned already/);
});

test("a feedback's run is the agent run that planned it last", async () => {
  const { db, app, first } = await built();
  const construction = await store.requireConstruction(db, app);
  const feedback = await store.addFeedback(db, app, first, "Sorting", "Sort by name");
  const during = await store.insertConstructionRun(db, construction.id, "feedback", feedback.id);
  assert.equal((await store.requireFeedback(db, construction.id, feedback.id)).run, null);
  await store.setAgentRun(db, during, "run-1");
  assert.equal((await store.requireFeedback(db, construction.id, feedback.id)).run, "run-1");
  await assert.rejects(store.requireFeedback(db, construction.id, 99999), /no feedback 99999/);
});

test("feedback is deleted with its tasks until one has started", async () => {
  const { db, app, first } = await built();
  const construction = await store.requireConstruction(db, app);

  let feedback = await store.addFeedback(db, app, first, "Sorting", "Sort by name");
  await store.planFeedback(db, app, feedback.id, plan());
  await store.deleteFeedback(db, app, feedback.id);
  assert.deepEqual(await store.feedbackOf(db, construction.id), []);
  let phases = await store.phasesWithTasks(db, construction.id);
  assert.equal(phases[0].tasks.length, 1);
  assert.equal(m.phaseStatus(phases[0].tasks), "done");

  feedback = await store.addFeedback(db, app, first, "Sorting", "Sort by name");
  const [started] = await store.planFeedback(db, app, feedback.id, plan());
  await store.updateTask(db, { ...started, status: "done" });
  await assert.rejects(store.deleteFeedback(db, app, feedback.id), /has started/);

  // Deleting the construction takes the feedback and its tasks, children first.
  await store.deleteConstruction(db, app);
  assert.deepEqual(await store.feedbackOf(db, construction.id), []);
  phases = await store.phasesWithTasks(db, construction.id);
  assert.deepEqual(phases, []);
});
