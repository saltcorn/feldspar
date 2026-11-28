// Saving a construction's phases and tasks (ported from crates/sc-construct/tests/phases.rs): a
// list arrives as rows in its order and makes the construction `planned`; a phase named by id is
// updated in place and one left out is deleted, unless it has tasks; an invalid list is refused
// whole, with the reason. A task inserted while the phases are saved is either seen by the save,
// which keeps its phase, or refused because its phase is gone.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb, settled } from "../helpers/fake-db.mjs";

const require = createRequire(import.meta.url);
const store = require("../../data/store.js");
const m = require("../../rules/model.js");

async function setup() {
  const fake = fakeDb();
  const app = globalThis.crypto.randomUUID();
  await store.createConstruction(fake.db, app);
  return { ...fake, app };
}

const input = (title, id = null) => ({
  id,
  title,
  description: `  ${title}, described  `,
  requirements: [{ text: `${title} works`, priority: "should" }],
});
const titles = (saved) => saved.phases.map((p) => p.phase.title);
const task = (title) => ({ kind: "data_model", title, brief: `make ${title}`, depends_on: [] });

test("a phase list arrives as rows and is replaced by the next", async () => {
  const { db, app } = await setup();
  let saved = await store.savePhases(db, app, [input("Data model"), input("Customers"), input("Invoices")]);
  assert.equal(saved.construction.status, "planned");
  assert.deepEqual(titles(saved), ["Data model", "Customers", "Invoices"]);
  assert.ok(saved.phases.every((p) => m.phaseStatus(p.tasks) === "unplanned"));
  // Trimmed on the way in.
  assert.equal(saved.phases[0].phase.description, "Data model, described");
  const list = m.phaseList(saved.phases);
  assert.ok(list.startsWith("1. Data model [unplanned] (id "), list);
  assert.ok(list.includes("   - (should) Customers works"), list);

  // Reordered and retitled by id; `Invoices` left out, so deleted; one new.
  const data = saved.phases[0].phase.id;
  const customers = saved.phases[1].phase.id;
  saved = await store.savePhases(db, app, [input("Customers and contacts", customers), input("Data model", data), input("Reports")]);
  const rows = await store.phasesOf(db, saved.construction.id);
  assert.deepEqual(
    rows.map((p) => [p.id, p.title]).slice(0, 2),
    [
      [customers, "Customers and contacts"],
      [data, "Data model"],
    ],
  );
  assert.equal(rows[2].title, "Reports");
  assert.equal(rows.length, 3);
  assert.deepEqual(saved.kept, []);
});

test("a phase with tasks is kept when the list leaves it out", async () => {
  const { db, app } = await setup();
  let saved = await store.savePhases(db, app, [input("Data model"), input("Customers")]);
  const started = saved.phases[0].phase.id;
  await store.insertTask(db, m.newTask(started, 1, "data_model", "Tables", "Create them"));

  saved = await store.savePhases(db, app, [input("Customers"), input("Invoices")]);
  assert.deepEqual(saved.kept, ["Data model"]);
  // Kept ahead of the list: it was reached first.
  assert.deepEqual(titles(saved), ["Data model", "Customers", "Invoices"]);
  assert.equal(saved.phases[0].phase.id, started);
  assert.equal(m.phaseStatus(saved.phases[0].tasks), "planned");
});

test("an invalid list is refused whole with the reason", async () => {
  const { db, app } = await setup();
  const saved = await store.savePhases(db, app, [input("Data model")]);
  const construction = saved.construction.id;

  const refused = (list, needle) => assert.rejects(store.savePhases(db, app, list), needle);
  await refused([], /at least one phase/);
  await refused([input("Fine"), input("  ")], /phase 2 has no title/);
  await refused([input("Elsewhere", 99999)], /not a phase of this construction/);
  await refused([input("Twice", saved.phases[0].phase.id), input("Again", saved.phases[0].phase.id)], /a second time/);
  const empty = input("Requirement-less");
  empty.requirements[0].text = " ";
  await refused([empty], /requirement 1 has no text/);
  await refused(
    Array.from({ length: 13 }, (_, i) => input(`P${i}`)),
    /too many/,
  );

  // Nothing of a refused list was written, and the construction is still `planned`.
  const rows = await store.phasesOf(db, construction);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].title, "Data model");
  assert.equal((await store.requireConstruction(db, app)).status, "planned");
});

test("a task inserted while the phases are saved is seen by the save", async () => {
  const { db, app, hold } = await setup();
  const saved = await store.savePhases(db, app, [input("Data model")]);
  const phase = saved.phases[0].phase.id;

  // A task is being inserted for the phase, under the construction's lock.
  const insert = hold({ op: "insert", table: "ac_tasks" });
  const adding = store.insertTask(db, m.newTask(phase, 1, "data_model", "Tables", "Create them"));
  await insert.reached;
  const saving = store.savePhases(db, app, [input("Customers")]);
  assert.equal(await settled(saving), false, "the save waits for the task's insert");
  insert.release();
  await adding;

  // The save then sees the task, so the phase stays.
  assert.deepEqual((await saving).kept, ["Data model"]);
  assert.equal((await store.tasksOfPhase(db, phase)).length, 1);
});

test("a task for a phase the save deletes waits and is refused", async () => {
  const { db, app, hold } = await setup();
  const saved = await store.savePhases(db, app, [input("Data model")]);
  const phase = saved.phases[0].phase.id;

  // A save of the phases that deletes it is about to write.
  const write = hold({ op: "batch" });
  const saving = store.savePhases(db, app, [input("Customers")]);
  await write.reached;
  const inserting = store.insertTask(db, m.newTask(phase, 1, "data_model", "Tables", "Create them"));
  assert.equal(await settled(inserting), false, "the insert waits for the save");
  write.release();
  await saving;

  // The insert then finds no phase, and no task is left without one.
  await assert.rejects(inserting, /does not exist/);
  assert.deepEqual(await store.tasksOfPhase(db, phase), []);
});

test("while running, a phase with tasks keeps them and one without is planned", async () => {
  const { db, app } = await setup();
  await store.savePhases(db, app, [input("Data model"), input("Screens")]);
  const phases = await store.phasesOf(db, (await store.requireConstruction(db, app)).id);
  await store.saveTasks(db, app, phases[0].id, [task("Tables")]);
  await store.setConstructionStatus(db, app, "running");

  // The runner may have picked `Tables`: replacing it is refused.
  await assert.rejects(store.saveTasks(db, app, phases[0].id, [task("Other")]), /stop it to plan them again/);
  assert.equal((await store.tasksOfPhase(db, phases[0].id))[0].title, "Tables");
  // So is changing the phases.
  await assert.rejects(store.savePhases(db, app, [input("Other")]), /stop it before changing its phases/);

  // The runner planning the phase it has reached is what running allows.
  await store.saveTasks(db, app, phases[1].id, [task("List")]);
  assert.equal((await store.tasksOfPhase(db, phases[1].id)).length, 1);
});

test("saved tasks replace the phase's, keep their order and dependencies, and refuse once started", async () => {
  const { db, app } = await setup();
  const saved = await store.savePhases(db, app, [input("Data model")]);
  const phase = saved.phases[0].phase.id;
  await store.saveTasks(db, app, phase, [task("Old")]);
  const tasks = await store.saveTasks(db, app, String(phase), m.parseTasks([task("Tables"), { ...task("Queries"), depends_on: [1] }]));
  assert.deepEqual(
    tasks.map((t) => [t.title, t.position, t.status]),
    [
      ["Tables", 1, "todo"],
      ["Queries", 2, "todo"],
    ],
  );
  assert.ok(tasks.every((t) => Number.isInteger(t.id)));
  assert.deepEqual(tasks[1].depends_on, [tasks[0].uid]);
  assert.deepEqual(await store.tasksOfPhase(db, phase), tasks);
  assert.equal(m.taskList(tasks), "1. Tables [data_model, todo]\n2. Queries [data_model, todo] after 1\n");

  // An invalid list changes nothing; another construction's phase is not this one's.
  await assert.rejects(store.saveTasks(db, app, phase, []), /at least one task/);
  assert.equal((await store.tasksOfPhase(db, phase)).length, 2);
  await assert.rejects(store.saveTasks(db, app, 99999, [task("X")]), /not a phase of this construction/);

  await store.updateTask(db, { ...tasks[0], status: "done" });
  await assert.rejects(store.saveTasks(db, app, phase, [task("New")]), /has started, so its tasks cannot be planned again/);
});

test("a phase's tasks are cleared to plan them again, until one has started", async () => {
  const { db, app } = await setup();
  const saved = await store.savePhases(db, app, [input("Data model"), input("Screens")]);
  const [first, second] = saved.phases.map((p) => p.phase.id);
  const tasks = await store.saveTasks(db, app, first, [task("Tables")]);

  await assert.rejects(store.clearTasks(db, app, second), /`Data model` is not done yet/);
  const cleared = await store.clearTasks(db, app, first);
  assert.deepEqual(cleared, tasks);
  assert.deepEqual(await store.tasksOfPhase(db, first), []);

  const again = await store.saveTasks(db, app, first, [task("Tables")]);
  await store.updateTask(db, { ...again[0], runs: ["run-1"] });
  await assert.rejects(store.clearTasks(db, app, first), /has started/);
  assert.equal((await store.tasksOfPhase(db, first)).length, 1);
});
