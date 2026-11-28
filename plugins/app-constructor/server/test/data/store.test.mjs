// The construction store over the in-memory `db` (ported from crates/sc-construct/tests/store.rs):
// a construction keeps what is written to it, a phase's status follows from its tasks, a phase
// with tasks stays, and a deleted construction leaves nothing behind. Deleting a phase or a
// construction while a task is being added, or creating a construction twice at once, leaves the
// tables consistent: the construction's lock serializes them. The links rows never outlive are
// keys.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb, settled } from "../helpers/fake-db.mjs";

const require = createRequire(import.meta.url);
const store = require("../../data/store.js");
const m = require("../../rules/model.js");
const { tables } = require("../../data/tables.js");

const app = () => globalThis.crypto.randomUUID();

const phase = (construction, position, title) => ({
  construction,
  position,
  title,
  description: `${title}, described`,
  requirements: [{ text: "Customers can be listed", priority: "must" }],
});

test("a construction keeps what is written to it", async () => {
  const { db } = fakeDb();
  const a = app();
  assert.equal(await store.loadConstruction(db, a), null);
  const created = await store.createConstruction(db, a, "construct-crm");
  assert.equal(created.status, "draft");
  assert.equal(created.agent, "construct-crm");
  assert.equal(created.specification, "");
  assert.equal(created.plan_only, false);

  // One per application.
  await assert.rejects(store.createConstruction(db, a, null), /already has a construction/);

  const saved = await store.saveSpecification(db, a, "A CRM for a small agency.");
  assert.equal(saved.specification, "A CRM for a small agency.");
  assert.equal(saved.id, created.id);
  assert.equal((await store.listConstructions(db)).length, 1);

  // Not while it runs.
  await store.setConstructionStatus(db, a, "running");
  await assert.rejects(store.saveSpecification(db, a, "Other"), /stop it before changing its specification/);
  assert.deepEqual((await store.runningConstructions(db)).map((c) => c.id), [created.id]);
  await assert.rejects(store.requireConstruction(db, app()), (e) => e.kind === "not_found" && /has no construction/.test(e.message));
});

test("phases and tasks come back in order and a phase's status follows its tasks", async () => {
  const { db } = fakeDb();
  const construction = await store.createConstruction(db, app());
  const second = await store.insertPhase(db, phase(construction.id, 2, "Contacts"));
  const first = await store.insertPhase(db, phase(construction.id, 1, "Data model"));
  const phases = await store.phasesOf(db, construction.id);
  assert.deepEqual(
    phases.map((p) => p.title),
    ["Data model", "Contacts"],
  );
  assert.deepEqual(phases[0], first);

  // No tasks: not planned yet.
  assert.equal(m.phaseStatus(await store.tasksOfPhase(db, first.id)), "unplanned");

  const tables = m.newTask(first.id, 1, "data_model", "Tables", "Make the tables");
  const queries = { ...m.newTask(first.id, 2, "data_model", "Queries", "Add queries"), depends_on: [tables.uid] };
  await store.insertTask(db, queries);
  const storedTables = await store.insertTask(db, tables);
  const tasks = await store.tasksOfPhase(db, first.id);
  assert.deepEqual(tasks[0], storedTables);
  assert.deepEqual({ ...tasks[0], id: undefined }, { ...tables, id: undefined });
  assert.deepEqual(tasks[1].depends_on, [tables.uid]);
  assert.equal(m.phaseStatus(tasks), "planned");
  assert.deepEqual(await store.loadTask(db, tasks[0].id), tasks[0]);

  // What the runner writes on a task comes back.
  await store.updateTask(db, { ...tasks[0], status: "done", runs: ["run-1"], summary: "made", created: [{ kind: "table", name: "contacts" }] });
  const updated = await store.loadTask(db, tasks[0].id);
  assert.equal(updated.status, "done");
  assert.deepEqual(updated.runs, ["run-1"]);
  assert.equal(updated.summary, "made");
  assert.deepEqual(updated.created, [{ kind: "table", name: "contacts" }]);
  const withTasks = await store.phasesWithTasks(db, construction.id);
  assert.deepEqual(
    withTasks.map((p) => [p.phase.title, p.tasks.length]),
    [
      ["Data model", 2],
      ["Contacts", 0],
    ],
  );

  // A phase with tasks stays; one without goes.
  await assert.rejects(store.deletePhase(db, first.id), /has tasks/);
  assert.equal(await store.deletePhase(db, second.id), true);
  assert.equal((await store.phasesOf(db, construction.id)).length, 1);
});

test("deleting a construction takes its phases and tasks", async () => {
  const { db, tables: rows } = fakeDb();
  const a = app();
  const other = await store.createConstruction(db, app());
  const construction = await store.createConstruction(db, a);
  const mine = await store.insertPhase(db, phase(construction.id, 1, "Data model"));
  const theirs = await store.insertPhase(db, phase(other.id, 1, "Data model"));
  await store.insertTask(db, m.newTask(mine.id, 1, "data_model", "T", "B"));
  await store.insertTask(db, m.newTask(theirs.id, 1, "feature", "T", "B"));

  const deleted = await store.deleteConstruction(db, a);
  assert.equal(deleted.id, construction.id);
  assert.equal(await store.loadConstruction(db, a), null);
  assert.deepEqual(await store.phasesOf(db, construction.id), []);
  assert.deepEqual(await store.tasksOfPhase(db, mine.id), []);
  // Another application's construction is untouched.
  assert.equal((await store.phasesOf(db, other.id)).length, 1);
  assert.equal((await store.tasksOfPhase(db, theirs.id)).length, 1);
  // A second delete finds nothing.
  assert.equal(await store.deleteConstruction(db, a), null);
  assert.ok(rows.ac_constructions.length === 1);
});

test("the links that rows never outlive are keys", async () => {
  const target = (table, column) => tables.find((t) => t.name === table).fields.find((f) => f.name === column);
  for (const [table, column, to] of [
    ["ac_phases", "construction", "ac_constructions"],
    ["ac_tasks", "phase", "ac_phases"],
    ["ac_tasks", "feedback", "ac_feedback"],
    ["ac_feedback", "phase", "ac_phases"],
    ["ac_mockups", "construction", "ac_constructions"],
    ["ac_questions", "construction", "ac_constructions"],
    ["ac_runs", "construction", "ac_constructions"],
    ["ac_errors", "construction", "ac_constructions"],
    ["ac_errors", "construction_run", "ac_runs"],
  ]) {
    assert.equal(target(table, column).references, to, `${table}.${column}`);
  }
  // The log outlives what it names, so those stay plain.
  for (const column of ["phase", "task", "feedback", "run"]) assert.equal(target("ac_errors", column).references, undefined, `errors.${column}`);
  assert.equal(target("ac_runs", "feedback").references, undefined);
  // What the runs record is not stored again.
  for (const [table, column] of [
    ["ac_constructions", "error"],
    ["ac_constructions", "plan_run"],
    ["ac_feedback", "run"],
    ["ac_feedback", "construction"],
  ]) {
    assert.equal(target(table, column), undefined, `${table}.${column}`);
  }
  // A table is declared after the tables it points at, and every one is admin-only.
  tables.forEach((t, i) => {
    for (const f of t.fields.filter((f) => f.references)) {
      assert.ok(tables.slice(0, i).some((u) => u.name === f.references), `${t.name}.${f.name}`);
    }
    assert.equal(t.min_role_read, 1);
    assert.equal(t.min_role_write, 1);
    assert.ok(t.name.startsWith("ac_"));
  });

  // A phase of no construction is refused.
  const { db } = fakeDb();
  await assert.rejects(store.insertPhase(db, phase(999, 1, "Orphan")), /foreign key/);
});

test("deleting a construction waits for a task being added and takes it", async () => {
  const { db, hold } = fakeDb();
  const a = app();
  const construction = await store.createConstruction(db, a);
  const first = await store.insertPhase(db, phase(construction.id, 1, "Data model"));

  // The task is being added: its insert is about to land, under the construction's lock.
  const insert = hold({ op: "insert", table: "ac_tasks" });
  const adding = store.insertTask(db, m.newTask(first.id, 1, "data_model", "T", "B"));
  await insert.reached;
  const deleting = store.deleteConstruction(db, a);
  assert.equal(await settled(deleting), false, "the delete waits for the task");
  insert.release();

  // The task went in first, and went with its phase.
  assert.equal((await adding).phase, first.id);
  assert.equal((await deleting).id, construction.id);
  assert.deepEqual(await store.tasksOfPhase(db, first.id), []);
  assert.deepEqual(await store.phasesOf(db, construction.id), []);
});

test("deleting a phase waits for a task being added and is then refused", async () => {
  const { db, hold } = fakeDb();
  const construction = await store.createConstruction(db, app());
  const first = await store.insertPhase(db, phase(construction.id, 1, "Data model"));

  const insert = hold({ op: "insert", table: "ac_tasks" });
  const adding = store.insertTask(db, m.newTask(first.id, 1, "data_model", "T", "B"));
  await insert.reached;
  const deleting = store.deletePhase(db, first.id);
  assert.equal(await settled(deleting), false, "the delete waits for the task");
  insert.release();

  await adding;
  await assert.rejects(deleting, /has tasks/);
  assert.equal((await store.phasesOf(db, construction.id)).length, 1);
  // A phase that is not there answers false.
  assert.equal(await store.deletePhase(db, 424242), false);
});

test("two creates at once make one construction and one clear refusal", async () => {
  const { db } = fakeDb({ delay: 3 });
  const a = app();
  const results = await Promise.allSettled([store.createConstruction(db, a), store.createConstruction(db, a)]);
  assert.equal(results.filter((r) => r.status === "fulfilled").length, 1);
  const refused = results.find((r) => r.status === "rejected");
  assert.match(refused.reason.message, /already has a construction/);
  assert.equal((await store.listConstructions(db)).length, 1);
});

test("the current error is the latest run's and every error stays logged", async () => {
  const { db } = fakeDb();
  const a = app();
  const construction = await store.createConstruction(db, a);

  // An agent run fails: its error is the current one.
  const mockups = await store.insertConstructionRun(db, construction.id, "mockups");
  const read = await store.setPlanError(db, a, m.failure("mockups", "no mockups saved", { run: "run-1", construction_run: mockups }));
  assert.equal(read.error, "no mockups saved");

  // A runner session starts, which clears it, and fails.
  const user = globalThis.crypto.randomUUID();
  const running = await store.markRunning(db, a, user, null, "authority-run");
  assert.equal(running.error, null);
  assert.equal(running.status, "running");
  assert.equal(running.started_by, user);
  assert.equal(running.authority_run, "authority-run");
  const session = await store.currentBuild(db, construction.id);
  const failed = await store.setConstructionStatus(db, a, "failed", m.failure("task", "the build is red", { phase: 11, task: 12, construction_run: session }));
  assert.equal(failed.error, "the build is red");
  assert.equal(failed.status, "failed");
  const errors = await store.errorsOf(db, construction.id);
  assert.equal(failed.error_entry, errors[0].id);

  // The next run starts with no error; the log keeps both, newest first.
  await store.insertConstructionRun(db, construction.id, "phases");
  assert.equal((await store.requireConstruction(db, a)).error, null);
  assert.deepEqual(
    errors.map((e) => [e.source, e.message]),
    [
      ["task", "the build is red"],
      ["mockups", "no mockups saved"],
    ],
  );
  assert.deepEqual([errors[0].phase, errors[0].task], [11, 12]);
  assert.equal(errors[1].run, "run-1");
  assert.equal(errors[0].construction_run, session);

  await store.deleteConstruction(db, a);
  assert.deepEqual(await store.errorsOf(db, construction.id), []);
  assert.equal(await store.latestConstructionRun(db, construction.id), null);
});

test("the planning run is read from the runs and an error names its feedback", async () => {
  const { db } = fakeDb();
  const a = app();
  const construction = await store.createConstruction(db, a);
  assert.equal(construction.plan_run, null);

  // The latest `phases` run that launched is the planning run.
  for (const run of ["run-a", "run-b"]) {
    const during = await store.insertConstructionRun(db, construction.id, "phases");
    await store.setAgentRun(db, during, run);
    assert.equal((await store.requireConstruction(db, a)).plan_run, run);
  }
  // One that never launched, and runs for other purposes, do not count.
  await store.insertConstructionRun(db, construction.id, "phases");
  await store.insertConstructionRun(db, construction.id, "build");
  assert.equal((await store.requireConstruction(db, a)).plan_run, "run-b");

  // An error planning a feedback names it.
  const during = await store.insertConstructionRun(db, construction.id, "feedback", 77);
  await store.setPlanError(db, a, m.failure("feedback", "no plan saved", { feedback: 77, construction_run: during }));
  let errors = await store.errorsOf(db, construction.id);
  assert.equal(errors[0].feedback, 77);

  // Any other error names none.
  const session = await store.insertConstructionRun(db, construction.id, "build");
  await store.setPlanError(db, a, m.failure("runner", "the runner stopped", { construction_run: session }));
  errors = await store.errorsOf(db, construction.id);
  assert.equal(errors[0].message, "the runner stopped");
  assert.equal(errors[0].feedback, null);
  assert.equal(errors[1].feedback, 77);
  await assert.rejects(store.insertConstructionRun(db, construction.id, "nothing"), /not a construction run purpose/);
});

test("planning one phase marks the construction running for it, with the note", async () => {
  const { db } = fakeDb();
  const a = app();
  const construction = await store.createConstruction(db, a);
  const p = await store.insertPhase(db, phase(construction.id, 1, "Data model"));
  const planning = await store.markPlanning(db, a, globalThis.crypto.randomUUID(), p.id, " no audit log ");
  assert.equal(planning.status, "running");
  assert.equal(planning.until_phase, p.id);
  assert.equal(planning.plan_only, true);
  assert.equal(planning.plan_note, " no audit log ");
  // An authority run not given keeps the one there is; a later run clears the plan-only flags.
  assert.equal((await store.setAuthorityRun(db, a, "run-x")).authority_run, "run-x");
  const running = await store.markRunning(db, a, globalThis.crypto.randomUUID());
  assert.equal(running.authority_run, "run-x");
  assert.equal(running.plan_only, false);
  assert.equal(running.plan_note, null);
  assert.equal(running.until_phase, null);
  assert.deepEqual(
    (await store.constructionRunsFor(db, construction.id, "build")).length,
    2,
  );
});

test("an unknown status is refused", async () => {
  const { db } = fakeDb();
  const a = app();
  await store.createConstruction(db, a);
  await assert.rejects(store.setConstructionStatus(db, a, "paused"), /not a valid construction status/);
});
