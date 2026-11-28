// The runner's steps (ported from the runner tests of constructions_api.rs): a restart resumes
// the running task without a second run; an admin no longer one stops the construction before
// its next run; a run with no summary fails its task; a task records what its runs made; the
// service's start resumes running constructions and its stop leaves them for the next.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { world, call } from "../helpers/fakes.mjs";

const require = createRequire(import.meta.url);
const store = require("../../data/store.js");
const runner = require("../../agent/runner.js");
const state = require("../../agent/state.js");
const m = require("../../rules/model.js");

const phase = (title) => ({ title, description: "", requirements: [] });

/** A construction of `crm` with one phase whose task `kind` is planned, left `running` by the
 * admin's run `authority` (started as a route would). */
async function running(env, kind = "data_model") {
  await call(env, "POST", env.base, {});
  const saved = await call(env, "POST", `${env.base}/phases`, { phases: [phase("Data model")] });
  const phaseId = saved.body.phases[0].id;
  const [t] = await store.saveTasks(env.db, env.app.id, phaseId, [{ kind, title: "Tables", brief: "Make them", depends_on: [] }]);
  const authority = await env.agents.api.start({ agent: "constructor-crm", prompt: "plan", description: "the admin's" });
  await store.markRunning(env.db, env.app.id, "11111111-1111-4111-8111-111111111111", null, authority.id);
  return { task: t, authority };
}

test("a restart resumes the running task, driving its run on, without starting a second", async () => {
  const env = await world();
  const { task, authority } = await running(env);
  // Its model never answers in this life: what it does once driven on is `leave`'s.
  env.script = () => new Promise(() => {});
  const left = await env.agents.api.start({ agent: "constructor-crm", prompt: "the task", on_behalf_of: authority.id });
  env.script = null;
  env.agents.leave(left.id, { answer: "Made the tables.", messages: [{ role: "tool_result", name: "edit_schema", content: JSON.stringify({ applied: true, tables_created: ["contacts"] }) }] });
  await store.updateTask(env.db, { ...task, status: "running", runs: [left.id] });
  const before = env.agents.started.length;
  await env.work();
  assert.equal(env.agents.started.length, before, "no second run");
  const read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "done", JSON.stringify(read.errors));
  assert.deepEqual(read.phases[0].tasks[0].runs, [left.id]);
  assert.equal(read.phases[0].tasks[0].summary, "Made the tables.");
  assert.deepEqual(read.phases[0].tasks[0].created, [{ kind: "table", name: "contacts" }]);
});

test("an admin who is no longer one stops the construction before its next run", async () => {
  const env = await world({ authority: (opts) => (opts.on_behalf_of ? "user" : "console") });
  await running(env);
  await env.work();
  const read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "stopped");
  assert.equal(read.error, "the admin who started it is no longer an admin");
  assert.equal(read.errors[0].source, "runner");
  assert.equal(read.phases[0].tasks[0].status, "todo");
  // The run it would have started went no further.
  assert.equal(env.agents.started[env.agents.started.length - 1].state, "aborted");
});

test("a route refuses to start a construction without the console's authority", async () => {
  const env = await world({ authority: "user" });
  await call(env, "POST", env.base, {});
  await call(env, "POST", `${env.base}/phases`, { phases: [phase("Data model")] });
  const refused = await call(env, "POST", `${env.base}/run`, {});
  assert.equal(refused.status, 400);
  assert.match(refused.body.error, /"Agents": console/);
  assert.equal((await call(env, "GET", env.base)).body.status, "planned");
});

test("a run with no summary fails its task, and tasks that wait on each other fail the construction", async () => {
  const env = await world();
  await running(env);
  env.script = () => ({ answer: "  " });
  await env.work();
  let read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "failed");
  assert.equal(read.phases[0].tasks[0].summary, "its run ended without a summary (answered)");
  assert.equal(read.errors[0].source, "task");

  // Blocked: a task that waits on one that is not in the plan can never start.
  const blocked = await world();
  await call(blocked, "POST", blocked.base, {});
  const saved = await call(blocked, "POST", `${blocked.base}/phases`, { phases: [phase("Data model")] });
  const t = await store.insertTask(blocked.db, { ...m.newTask(saved.body.phases[0].id, 1, "data_model", "Loop", "x"), depends_on: ["nobody"] });
  assert.ok(t.id);
  await store.markRunning(blocked.db, blocked.app.id, "11111111-1111-4111-8111-111111111111");
  await blocked.work();
  read = (await call(blocked, "GET", blocked.base)).body;
  assert.equal(read.status, "failed");
  assert.match(read.error, /no task of phase `Data model` can start/);
});

test("a planning run that saves no tasks fails the construction, logged as the tasks' step", async () => {
  const env = await world();
  await call(env, "POST", env.base, {});
  await call(env, "POST", `${env.base}/phases`, { phases: [phase("Data model")] });
  env.script = () => ({ answer: "I will not." });
  await call(env, "POST", `${env.base}/run`, {});
  await env.work();
  const read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "failed");
  assert.match(read.error, /the construction agent planned no tasks for phase `Data model` \(answered\); its run is /);
  assert.equal(read.errors[0].source, "tasks");
});

test("the service resumes running constructions when it starts, and its stop leaves them running", async () => {
  const env = await world();
  await running(env);
  let release;
  const held = new Promise((resolve) => (release = resolve));
  env.script = async () => {
    await held;
    return { answer: "Made them." };
  };
  const controller = new AbortController();
  const stop = runner.start({ db: env.db, agents: env.agents.api, apps: env.apps.api, signal: controller.signal, log: { error: () => {} } });
  for (let i = 0; i < 200 && !state.driverOf((await store.requireConstruction(env.db, env.app.id)).id); i++) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  const c = await store.requireConstruction(env.db, env.app.id);
  assert.ok(state.driverOf(c.id), "a driver took it up");
  controller.abort();
  await stop();
  assert.equal(state.driverOf(c.id), null);
  release();
  await new Promise((resolve) => setTimeout(resolve, 50));
  // Still running, for the next start to follow its task's run on.
  assert.equal((await store.requireConstruction(env.db, env.app.id)).status, "running");
  await env.work();
  assert.equal((await store.requireConstruction(env.db, env.app.id)).status, "done");
});

test("a scan leaves a launch alone until its run has started", async () => {
  const env = await world();
  await call(env, "POST", env.base, {});
  const construction = await store.requireConstruction(env.db, env.app.id);
  // What a route holds while it is still starting the run: claimed, with no run yet.
  const { claim } = require("../../http/launch.js");
  const entry = claim(construction, "planning", { purpose: "phases" });
  await env.work();
  assert.equal(state.busy.get(state.key(construction.id)), entry, "the claim is still the route's");
});

test("run on a construction whose task still runs follows that run, without starting a second", async () => {
  const env = await world();
  const { task, authority } = await running(env);
  // The task's run is still going, but the construction was marked failed meanwhile.
  env.script = () => new Promise(() => {});
  const left = await env.agents.api.start({ agent: "constructor-crm", prompt: "the task", on_behalf_of: authority.id });
  env.script = null;
  env.agents.leave(left.id, { answer: "Made the tables." });
  await store.updateTask(env.db, { ...task, status: "running", runs: [left.id] });
  await store.setConstructionStatus(env.db, env.app.id, "failed");
  const before = env.agents.started.length;
  const ran = await call(env, "POST", `${env.base}/run`, {});
  assert.equal(ran.status, 202, JSON.stringify(ran.body));
  assert.equal(env.agents.started.length, before, "the route starts no second run");
  await env.work();
  assert.equal(env.agents.started.length, before, "nor does the runner");
  const read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "done", JSON.stringify(read.errors));
  assert.deepEqual(read.phases[0].tasks[0].runs, [left.id]);
});

test("a run that keeps stopping without ending is given up on, with the reason shown", async () => {
  const env = await world();
  await call(env, "POST", env.base, {});
  await call(env, "POST", `${env.base}/specification`, { specification: "A CRM." });
  env.script = () => new Promise(() => {});
  assert.equal((await call(env, "POST", `${env.base}/plan`, {})).status, 202);
  // Its drive ends at once, every time (a provider deleted meanwhile, say).
  const run = env.agents.started[env.agents.started.length - 1];
  run.driven = false;
  let drives = 0;
  env.agents.api.drive = async () => {
    drives += 1;
    if (drives > 10) throw new Error("driven on for ever");
    return { ...(await env.agents.api.run(run.id)), driven: true };
  };
  await env.work();
  assert.ok(drives <= 3, `driven on ${drives} times`);
  const read = (await call(env, "GET", env.base)).body;
  assert.equal(read.planning, false, "the agent is free again");
  assert.match(read.error, /keeps stopping without ending/);
});

test("stop right after a restart puts the task back, even if the runner scans meanwhile", async () => {
  const env = await world();
  const { task, authority } = await running(env);
  // A restart left the task's run live with nothing driving it, and no driver for the construction.
  env.script = () => new Promise(() => {});
  const left = await env.agents.api.start({ agent: "constructor-crm", prompt: "the task", on_behalf_of: authority.id });
  env.agents.runs.get(left.id).driven = false;
  await store.updateTask(env.db, { ...task, status: "running", runs: [left.id] });
  // The runner scans while the stop is under way: just as the stop reads the task's run.
  const read = env.agents.api.run;
  let scanned = false;
  env.agents.api.run = async (id) => {
    if (!scanned) {
      scanned = true;
      await env.runner.scan({ all: true });
    }
    return read(id);
  };
  assert.equal((await call(env, "POST", `${env.base}/stop`, {})).status, 200);
  assert.ok(scanned);
  await env.runner.settled();
  const got = (await call(env, "GET", env.base)).body;
  assert.equal(got.status, "stopped", JSON.stringify(got.errors));
  assert.equal(got.phases[0].tasks[0].status, "todo");
  assert.equal(got.phases[0].tasks[0].summary, null);
});

test("stop just as a driver takes up a restarted task puts the task back and aborts its run", async () => {
  const env = await world();
  const { task, authority } = await running(env);
  // A restart left the task's run live with nothing driving it.
  env.script = () => new Promise(() => {});
  const left = await env.agents.api.start({ agent: "constructor-crm", prompt: "the task", on_behalf_of: authority.id });
  env.agents.runs.get(left.id).driven = false;
  await store.updateTask(env.db, { ...task, status: "running", runs: [left.id] });
  // The driver starts, and the stop comes before it has taken up the task's run.
  const currentBuild = store.currentBuild;
  let release;
  const held = new Promise((r) => (release = r));
  store.currentBuild = async (...args) => {
    await held;
    return currentBuild(...args);
  };
  try {
    const scan = env.runner.scan({ all: true });
    const c = await store.requireConstruction(env.db, env.app.id);
    for (let i = 0; i < 200 && !state.driverOf(c.id); i++) await new Promise((r) => setTimeout(r, 5));
    assert.ok(state.driverOf(c.id), "a driver took it up");
    assert.equal((await call(env, "POST", `${env.base}/stop`, {})).status, 200);
    release();
    await scan;
    await env.runner.settled();
  } finally {
    store.currentBuild = currentBuild;
  }
  const got = (await call(env, "GET", env.base)).body;
  assert.equal(got.status, "stopped", JSON.stringify(got.errors));
  assert.equal(got.phases[0].tasks[0].status, "todo");
  assert.equal((await env.agents.api.run(left.id)).live, false, "its run is aborted");
});
