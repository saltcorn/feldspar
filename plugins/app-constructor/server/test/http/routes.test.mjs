// The Plan & build routes (ported from crates/sc-server/tests/constructions_api.rs), over fake
// agents and applications: admins only and React only; the construction agent made with both
// traits on the chosen provider; planning, questions, mockups and feedback launched as runs the
// runner follows and judges; the chosen mockup written into the project; deleting a construction,
// or its application, takes its rows and its agent.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { world, call, STAFF, fakeRes } from "../helpers/fakes.mjs";

const require = createRequire(import.meta.url);
const store = require("../../data/store.js");
const { constructor } = require("../../agent/trait.js");
const routes = require("../../http/routes.js");
const state = require("../../agent/state.js");

const phase = (title) => ({ title, description: `${title}, described`, requirements: [{ text: `${title} works`, priority: "must" }] });
/** The trait's tool `name`, called as the construction agent's model would. */
const tool = (env, name, args) => constructor.tools[name].run(args, { config: { application: env.app.subdomain }, db: env.db });

async function created(env, body = {}) {
  const answer = await call(env, "POST", env.base, body);
  assert.equal(answer.status, 201, JSON.stringify(answer.body));
  return answer.body;
}

test("only an admin constructs, only a React application, with an agent on the builder's provider", async () => {
  const env = await world();
  const refused = await call(env, "POST", env.base, {}, STAFF);
  assert.equal(refused.status, 403);
  assert.match(refused.body.error, /only an administrator/);
  assert.equal((await call(env, "GET", "/constructions", {}, null)).status, 403);
  // None yet: not found, which the screen reads as "none yet".
  const none = await call(env, "GET", env.base);
  assert.equal(none.status, 404);
  assert.equal(none.body.error, "this application has no construction");
  assert.equal((await call(env, "GET", "/applications/nowhere/construction")).status, 404);

  const construction = await created(env);
  assert.equal(construction.status, "draft");
  assert.equal(construction.agent, "constructor-crm");
  for (const flag of ["planning", "designing", "asking", "plan_only"]) assert.equal(construction[flag], false);
  assert.deepEqual(construction.phases, []);
  const agent = await env.agents.api.get("constructor-crm");
  assert.equal(agent.provider, "other", "the builder agent's");
  assert.deepEqual(agent.traits, [
    { trait: "constructor", config: { application: "crm" } },
    { trait: "data_model", config: {} },
  ]);
  const again = await call(env, "POST", env.base, {});
  assert.equal(again.status, 400);
  assert.match(again.body.error, /already has a construction/);
  const list = await call(env, "GET", "/constructions");
  assert.deepEqual(list.body, [{ id: construction.id, application: env.app.id, status: "draft", agent: "constructor-crm", phases: 0, phases_done: 0 }]);

  // Another framework is refused.
  const vue = await world({ app: { framework: "vue" } });
  const refusedVue = await call(vue, "POST", vue.base, {});
  assert.equal(refusedVue.status, 400);
  assert.match(refusedVue.body.error, /builds React applications/);
});

test("the provider is the request's, else the application's setting, which applies before each run", async () => {
  const named = await world();
  const bare = await call(named, "POST", named.base, { provider: "bare" });
  assert.equal(bare.status, 400);
  assert.match(bare.body.error, /LLM provider `bare` has no default model/);
  await created(named, { provider: "house" });
  assert.equal((await named.agents.api.get("constructor-crm")).provider, "house");

  const chosen = await world({ app: { settings: { constructor_provider: "house" } } });
  await created(chosen);
  assert.equal((await chosen.agents.api.get("constructor-crm")).provider, "house");
  // A change on the form switches the agent when it next plans; one that does not exist is
  // refused by name.
  chosen.app.settings.constructor_provider = "other";
  await call(chosen, "POST", `${chosen.base}/specification`, { specification: "A CRM." });
  chosen.script = () => tool(chosen, "save_phases", { phases: [phase("Data model")] }).then(() => ({ answer: "One phase." }));
  assert.equal((await call(chosen, "POST", `${chosen.base}/plan`, {})).status, 202);
  await chosen.work();
  assert.equal((await chosen.agents.api.get("constructor-crm")).provider, "other");
  chosen.app.settings.constructor_provider = "gone";
  const gone = await call(chosen, "POST", `${chosen.base}/plan`, {});
  assert.equal(gone.status, 404);
  assert.match(gone.body.error, /gone/);
});

test("planning is a run the runner follows: phases saved, or why not on the construction", async () => {
  const env = await world();
  await created(env);
  const early = await call(env, "POST", `${env.base}/plan`, {});
  assert.match(early.body.error, /no specification yet/);
  await call(env, "POST", `${env.base}/specification`, { specification: "A CRM." });

  // An answer without phases is a failure said on the construction.
  env.script = () => ({ answer: "Nothing to plan." });
  let planning = await call(env, "POST", `${env.base}/plan`, {});
  assert.equal(planning.status, 202);
  assert.equal(planning.body.planning, true);
  const busy = await call(env, "POST", `${env.base}/questions`, {});
  assert.match(busy.body.error, /the construction agent is planning the phases; wait for it to finish/);
  await env.work();
  let read = (await call(env, "GET", env.base)).body;
  assert.equal(read.planning, false);
  assert.equal(read.error, "the construction agent finished without saving any phases; see its run");
  assert.equal(read.errors[0].current, true);
  assert.equal(read.errors[0].source, "phases");
  // A run that fails says why.
  env.script = () => ({ state: "failed", error: "invalid x-api-key" });
  await call(env, "POST", `${env.base}/plan`, {});
  await env.work();
  read = (await call(env, "GET", env.base)).body;
  assert.equal(read.error, "planning the phases failed: invalid x-api-key");

  // The plan the agent saves arrives; a new run clears the last error.
  env.script = (opts) => {
    assert.match(opts.prompt, /Plan the construction of the `CRM` application/);
    return tool(env, "save_phases", { phases: [phase("Data model"), phase("Contacts")] }).then(() => ({ answer: "Two phases." }));
  };
  planning = await call(env, "POST", `${env.base}/plan`, {});
  const run = env.agents.started[env.agents.started.length - 1];
  await env.work();
  read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "planned");
  assert.equal(read.error, null);
  assert.equal(read.plan_run, run.id);
  assert.deepEqual(read.phases.map((p) => [p.title, p.status]), [["Data model", "unplanned"], ["Contacts", "unplanned"]]);
  assert.equal((await store.requireConstruction(env.db, env.app.id)).authority_run, run.id);

  // The admin edits them.
  const edited = await call(env, "POST", `${env.base}/phases`, { phases: [{ ...phase("Data model"), id: read.phases[0].id }] });
  assert.deepEqual(edited.body.phases.map((p) => p.title), ["Data model"]);
  const invalid = await call(env, "POST", `${env.base}/phases`, { phases: [] });
  assert.equal(invalid.status, 400);
  // The route checks the shape the agent's tool declares, with the same message.
  const misshapen = await call(env, "POST", `${env.base}/phases`, { phases: [{ ...phase("A"), requirements: [{ text: "x", priority: "maybe" }] }] });
  assert.equal(misshapen.status, 400);
  assert.match(misshapen.body.error, /phase 1: requirement 1: `priority` must be one of `must`, `should`, `could`/);
});

test("questions are asked and answered, mockups proposed and the chosen one written to the project", async () => {
  const env = await world();
  await created(env);
  await call(env, "POST", `${env.base}/specification`, { specification: "A shop." });
  env.script = () => tool(env, "ask_questions", { questions: ["Which currency?"] }).then(() => ({ answer: "Asked." }));
  const asking = await call(env, "POST", `${env.base}/questions`, {});
  assert.equal(asking.body.asking, true);
  const early = await call(env, "POST", `${env.base}/questions/answer`, { answers: [] });
  assert.match(early.body.error, /asking its questions/);
  await env.work();
  let read = (await call(env, "GET", env.base)).body;
  assert.deepEqual(read.questions.map((q) => [q.text, q.answer]), [["Which currency?", null]]);
  const answered = await call(env, "POST", `${env.base}/questions/answer`, { answers: [{ question: read.questions[0].id, text: "Euro" }] });
  assert.equal(answered.body.questions[0].answer, "Euro");
  assert.match((await call(env, "POST", `${env.base}/questions/answer`, { answers: "Euro" })).body.error, /`answers` must be a list of `\{question, text\}`/);

  // Asking again with nothing new is a failure.
  env.script = () => ({ answer: "Nothing to ask." });
  await call(env, "POST", `${env.base}/questions`, {});
  await env.work();
  assert.equal((await call(env, "GET", env.base)).body.error, "the construction agent finished without asking any questions; see its run");

  const screens = (title) => ({ title, rationale: `${title}!`, screens: [{ name: "Orders list", html: `<p>${title}</p>` }, { name: "Order", html: "<p>one</p>" }] });
  env.script = (opts) => {
    assert.match(opts.prompt, /Answer: Euro/);
    return tool(env, "save_mockups", { mockups: [screens("Table"), screens("Cards")] }).then(() => ({ answer: "Two." }));
  };
  assert.match((await call(env, "POST", `${env.base}/mockups`, { count: 9 })).body.error, /`count` must be between 1 and 3/);
  const designing = await call(env, "POST", `${env.base}/mockups`, {});
  assert.equal(designing.body.designing, true);
  await env.work();
  read = (await call(env, "GET", env.base)).body;
  assert.deepEqual(read.mockups.map((m) => [m.round, m.title]), [[1, "Table"], [1, "Cards"]]);
  assert.deepEqual(read.mockups[0].screens, [{ name: "Orders list", file: "orders-list.html" }, { name: "Order", file: "order.html" }]);
  const html = await call(env, "GET", `${env.base}/mockups/${read.mockups[0].id}/screens`);
  assert.deepEqual(html.body[0], { name: "Orders list", file: "orders-list.html", html: "<p>Table</p>" });
  assert.equal((await call(env, "GET", `${env.base}/mockups/999/screens`)).status, 404);

  // A revision that saves no new round fails.
  env.script = () => ({ answer: "Kept them." });
  await call(env, "POST", `${env.base}/mockups`, { feedback: [{ mockup: read.mockups[0].id, text: "Too plain" }] });
  await env.work();
  read = (await call(env, "GET", env.base)).body;
  assert.equal(read.mockups[0].feedback, "Too plain");
  assert.equal(read.error, "the construction agent finished without saving any mockups; see its run");

  // Choosing writes the screens into the project, and another choice replaces them.
  await env.apps.api.write(env.app.id, "mockups/old.html", "<p>old</p>");
  const chosen = await call(env, "POST", `${env.base}/mockups/${read.mockups[1].id}/choose`, {});
  assert.equal(chosen.status, 200, JSON.stringify(chosen.body));
  assert.deepEqual(chosen.body.mockups.map((m) => m.status), ["proposed", "chosen"]);
  assert.equal(await env.apps.api.read(env.app.id, "mockups/orders-list.html"), "<p>Cards</p>");
  assert.equal(await env.apps.api.read(env.app.id, "mockups/old.html"), null);
});

/** A construction with its phases planned and saved, ready to run. */
async function ready(env, titles) {
  await created(env);
  await call(env, "POST", `${env.base}/specification`, { specification: "A CRM." });
  const saved = await call(env, "POST", `${env.base}/phases`, { phases: titles.map(phase) });
  return saved.body.phases.map((p) => p.id);
}

/** The model of a run: plans each phase's tasks with `tasks(phaseId)`, and answers each task. */
function planner(env, tasks, answers = (opts) => `Did ${opts.description}.`) {
  return async (opts) => {
    const planning = /^Plan the tasks of/.test(opts.description);
    if (planning) {
      const id = Number(/\(id (\d+)\)/.exec(opts.prompt)[1]);
      await tool(env, "save_tasks", { phase: id, tasks: tasks(id) });
      return { answer: "Planned." };
    }
    return { answer: answers(opts) };
  };
}

const task = (kind, title, depends_on = []) => ({ kind, title, brief: `Make ${title}.`, depends_on });

test("running plans each phase when it is reached and runs its tasks in order, as the admin", async () => {
  const env = await world();
  const [first, second] = await ready(env, ["Data model", "Contacts"]);
  env.script = planner(env, (id) => (id === first ? [task("data_model", "Tables")] : [task("feature", "List contacts"), task("feature", "Edit a contact", [1])]));
  const started = await call(env, "POST", `${env.base}/run`, {});
  assert.equal(started.status, 202, JSON.stringify(started.body));
  assert.equal(started.body.status, "running");
  // The first step is started by the route, as the admin, and is what the runner acts for.
  const firstRun = env.agents.started[env.agents.started.length - 1];
  assert.equal(firstRun.on_behalf_of, null);
  assert.equal((await store.requireConstruction(env.db, env.app.id)).authority_run, firstRun.id);
  assert.match((await call(env, "POST", `${env.base}/run`, {})).body.error, /running already/);
  await env.work();

  const read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "done", JSON.stringify(read.errors));
  assert.equal(read.error, null);
  const contacts = read.phases[1];
  assert.equal(contacts.status, "done");
  assert.deepEqual(contacts.tasks[1].depends_on, [contacts.tasks[0].uid]);
  for (const t of contacts.tasks) assert.equal(t.runs.length, 1);
  assert.equal(contacts.tasks[0].summary, "Did Construct: List contacts.");
  const runs = env.agents.started.slice(-5);
  assert.deepEqual(
    runs.map((r) => [r.description, r.agent]),
    [
      ["Plan the tasks of `Data model`", "constructor-crm"],
      ["Construct: Tables", "constructor-crm"],
      ["Plan the tasks of `Contacts`", "constructor-crm"],
      ["Construct: List contacts", "build-crm"],
      ["Construct: Edit a contact", "build-crm"],
    ],
  );
  for (const r of runs.slice(1)) assert.equal(r.on_behalf_of, firstRun.id);
  // The second phase was planned after the first ran, told what it made.
  assert.match(runs[2].prompt, /Data model \/ Tables: Did Construct: Tables\./);
  assert.match(runs[4].prompt, /Make Edit a contact\./);
  assert.doesNotMatch(runs[4].prompt, /Make List contacts\./);
  // Each feature was built.
  assert.equal(env.apps.builds.length, 2);
  assert.equal(second, read.phases[1].id);
});

test("a red build fails the task and the construction, and retry carries on", async () => {
  const env = await world();
  await ready(env, ["Contacts"]);
  env.script = planner(env, () => [task("feature", "List contacts")]);
  env.build = () => "failed";
  await call(env, "POST", `${env.base}/run`, {});
  await env.work();
  let read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "failed");
  const failed = read.phases[0].tasks[0];
  assert.equal(failed.status, "failed");
  assert.match(failed.summary, /^the application does not build: .*TS2322/);
  assert.match(read.error, /task `List contacts` failed: the application does not build/);
  assert.match((await call(env, "POST", `${env.base}/tasks/${failed.id}/skip`, {}, STAFF)).body.error, /only an administrator/);
  assert.equal((await call(env, "POST", `${env.base}/tasks/999/retry`, {})).status, 404);

  env.build = () => "succeeded";
  const retried = await call(env, "POST", `${env.base}/tasks/${failed.id}/retry`, {});
  assert.equal(retried.status, 200, JSON.stringify(retried.body));
  assert.equal(retried.body.status, "running");
  await env.work();
  read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "done");
  assert.equal(read.phases[0].tasks[0].runs.length, 2);
  assert.match((await call(env, "POST", `${env.base}/tasks/${failed.id}/retry`, {})).body.error, /has not failed/);
});

test("stop aborts the running task's run and puts it back; stopping one no runner drives too", async () => {
  const env = await world();
  await ready(env, ["Data model"]);
  let release;
  const held = new Promise((resolve) => (release = resolve));
  env.script = async (opts) => {
    if (/^Plan the tasks/.test(opts.description)) return planner(env, () => [task("data_model", "Tables")])(opts);
    await held;
    return { answer: "never read" };
  };
  await call(env, "POST", `${env.base}/run`, {});
  const working = env.work();
  let read;
  for (let i = 0; i < 200; i++) {
    read = (await call(env, "GET", env.base)).body;
    if (read.phases[0].tasks[0]?.status === "running") break;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  const runId = read.phases[0].tasks[0].runs[0];
  assert.match((await call(env, "POST", `${env.base}/delete`, {})).body.error, /running/);
  await call(env, "POST", `${env.base}/stop`, {});
  await working;
  release();
  read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "stopped");
  assert.equal(read.phases[0].tasks[0].status, "todo");
  assert.equal((await env.agents.api.run(runId)).state, "aborted");
  assert.match((await call(env, "POST", `${env.base}/stop`, {})).body.error, /not running/);

  // Left running by a restart, before the runner picked it up: stopped here, its run aborted.
  const left = await world();
  await ready(left, ["Data model"]);
  left.script = () => new Promise(() => {});
  await call(left, "POST", `${left.base}/run`, {});
  const pending = left.agents.started[left.agents.started.length - 1];
  const stopped = await call(left, "POST", `${left.base}/stop`, {});
  assert.equal(stopped.body.status, "stopped");
  assert.equal((await left.agents.api.run(pending.id)).state, "aborted");
});

test("deleting a construction takes its rows and its agent; deleting the application does too", async () => {
  const env = await world();
  await ready(env, ["Data model"]);
  const deleted = await call(env, "POST", `${env.base}/delete`, {});
  assert.deepEqual(deleted.body, { deleted: true, agent: "constructor-crm" });
  assert.equal(await env.agents.api.get("constructor-crm"), null);
  assert.ok(await env.agents.api.get("build-crm"), "the builder agent stays");
  assert.deepEqual((await call(env, "POST", `${env.base}/delete`, {})).body, { deleted: false, agent: null });

  await ready(env, ["Data model"]);
  await routes.applicationDeleted(env.app, { db: env.db, agents: env.agents.api, apps: env.apps.api });
  for (const rows of Object.values(env.tables)) assert.equal(rows.length, 0);
  assert.equal(await env.agents.api.get("constructor-crm"), null);
});

test("feedback on a built phase is planned as follow-up tasks, which run when asked", async () => {
  const env = await world();
  const [first] = await ready(env, ["Contacts"]);
  env.script = planner(env, () => [task("feature", "List contacts")]);
  await call(env, "POST", `${env.base}/run`, {});
  await env.work();
  const given = await call(env, "POST", `${env.base}/feedback`, { phase: first, title: "Sort it", description: "By name" });
  assert.equal(given.status, 201, JSON.stringify(given.body));
  const feedback = given.body.feedback[0];
  assert.equal(feedback.status, "given");
  env.script = (opts) => {
    assert.match(opts.prompt, /## The feedback \(id \d+\)\n\nSort it/);
    return tool(env, "plan_feedback", {
      feedback: feedback.id,
      requirements: [{ text: "Sorted by name", priority: "must" }],
      tasks: [task("feature", "Sort the list")],
    }).then(() => ({ answer: "Planned." }));
  };
  const planning = await call(env, "POST", `${env.base}/feedback/${feedback.id}/plan`, {});
  assert.equal(planning.status, 202);
  assert.equal(planning.body.feedback[0].planning, true);
  await env.work();
  let read = (await call(env, "GET", env.base)).body;
  assert.equal(read.feedback[0].status, "planned");
  assert.equal(read.phases[0].tasks[1].feedback, feedback.id);
  assert.match((await call(env, "POST", `${env.base}/feedback/${feedback.id}/plan`, {})).body.error, /planned already/);

  env.script = () => ({ answer: "Sorted." });
  await call(env, "POST", `${env.base}/run`, {});
  await env.work();
  read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "done");
  assert.equal(read.feedback[0].status, "done");
  assert.match((await call(env, "POST", `${env.base}/feedback/${feedback.id}/delete`, {})).body.error, /has started/);
});

test("a phase is planned on its own, then again with a note, and stops before it runs", async () => {
  const env = await world();
  const [first] = await ready(env, ["Data model"]);
  env.script = planner(env, () => [task("data_model", "Tables")]);
  const planning = await call(env, "POST", `${env.base}/phases/${first}/plan`, {});
  assert.equal(planning.status, 202, JSON.stringify(planning.body));
  assert.equal(planning.body.plan_only, true);
  await env.work();
  let read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "planned");
  assert.deepEqual(read.phases[0].tasks.map((t) => [t.title, t.status]), [["Tables", "todo"]]);

  await call(env, "POST", `${env.base}/phases/${first}/plan`, { note: "One task only." });
  const replan = env.agents.started[env.agents.started.length - 1];
  assert.match(replan.prompt, /This phase was planned before, as:\n1\. \(data_model\) Tables: Make Tables\./);
  assert.match(replan.prompt, /The admin's note on how to plan this phase: One task only\./);
  await env.work();
  read = (await call(env, "GET", env.base)).body;
  assert.equal(read.status, "planned");
  assert.equal((await call(env, "POST", `${env.base}/phases/abc/plan`, {})).status, 404);
});

test("a replan that cannot start leaves the phase's tasks as they were", async () => {
  let console_ = true;
  const env = await world({ authority: () => (console_ ? "console" : "user") });
  const [first] = await ready(env, ["Data model"]);
  env.script = planner(env, () => [task("data_model", "Tables"), { ...task("data_model", "Indexes"), depends_on: [1] }]);
  await call(env, "POST", `${env.base}/phases/${first}/plan`, {});
  await env.work();
  const before = (await call(env, "GET", env.base)).body.phases[0].tasks;
  assert.equal(before.length, 2);

  // The module lost the console: the planning run is refused before it starts.
  console_ = false;
  env.script = () => new Promise(() => {});
  const refused = await call(env, "POST", `${env.base}/phases/${first}/plan`, { note: "One task only." });
  assert.equal(refused.status, 400);
  assert.match(refused.body.error, /"Agents": console/);
  const after = (await call(env, "GET", env.base)).body.phases[0].tasks;
  const shape = (ts) => ts.map((t) => [t.uid, t.title, t.depends_on, t.status]);
  assert.deepEqual(shape(after), shape(before));
});

test("the screen's script is served, or a sentence on how to build it", () => {
  const route = routes.routes.find((r) => r.url === "/ui.js");
  const res = fakeRes();
  route.callback({}, res);
  if (res.out.status === 200) {
    assert.match(res.out.headers["content-type"], /^text\/javascript/);
  } else {
    assert.equal(res.out.status, 404);
    assert.match(res.out.body, /npm run build/);
  }
  assert.equal(state.busy.size, 0);
});

test("a launch whose run cannot be recorded stops that run and frees the agent", async () => {
  const env = await world();
  await call(env, "POST", env.base, {});
  await call(env, "POST", `${env.base}/specification`, { specification: "A CRM." });
  env.script = () => new Promise(() => {});
  // The run starts, then writing it down fails.
  const original = store.insertConstructionRun;
  store.insertConstructionRun = async () => {
    throw new Error("the database went away");
  };
  let refused;
  try {
    refused = await call(env, "POST", `${env.base}/plan`, {});
  } finally {
    store.insertConstructionRun = original;
  }
  assert.equal(refused.status, 400);
  assert.match(refused.body.error, /database went away/);
  const run = env.agents.started[env.agents.started.length - 1];
  assert.equal(run.state, "aborted", "nothing follows it, so it does not go on");
  assert.equal((await call(env, "GET", env.base)).body.planning, false);
});
