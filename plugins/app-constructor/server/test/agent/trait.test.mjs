// The agent trait `constructor` (ported from app_constructor.rs): admins only, one application
// named by subdomain, its tools saving through the store with the answers the Rust gave, and a
// session header with the specification, the answers, the mockup and the phases.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "../helpers/fake-db.mjs";

const require = createRequire(import.meta.url);
const store = require("../../data/store.js");
const { constructor } = require("../../agent/trait.js");
const plugin = require("../../index.js");
const m = require("../../rules/model.js");

/** A construction of `crm` made by the routes (its agent named for it), and a tool context. */
async function made() {
  const { db } = fakeDb();
  const app = globalThis.crypto.randomUUID();
  await store.createConstruction(db, app, "constructor-crm");
  await store.saveSpecification(db, app, "A CRM for a small team.");
  const ctx = { config: { application: "crm" }, db, run: { id: "r", agent: "constructor-crm", mode: "act" }, state: null };
  return { db, app, ctx };
}

const tool = (name) => constructor.tools[name].run;
const phase = (title) => ({ title, description: `${title}, described`, requirements: [{ text: `${title} works`, priority: "must" }] });

test("the trait is an admin's, configured with a subdomain, and offers the planning tools only", () => {
  assert.equal(plugin.agent_traits.constructor, constructor);
  assert.equal(constructor.admin_only, true);
  assert.deepEqual(constructor.config_fields, [{ name: "application", label: "Application (subdomain)", type: "String", required: true }]);
  assert.deepEqual(Object.keys(constructor.tools).sort(), ["ask_questions", "plan_feedback", "save_mockups", "save_phases", "save_tasks"]);
  for (const t of Object.values(constructor.tools)) assert.equal(t.mutates, true);
  // The data-model tools are the built-in `data_model` trait's.
  assert.equal(constructor.tools.describe_schema, undefined);
  assert.match(constructor.prompt, /`save_phases`/);
});

test("phases and tasks are saved with the lists the agent reads back", async () => {
  const { db, app, ctx } = await made();
  const saved = await tool("save_phases")({ phases: [phase("Data model"), phase("Contacts")] }, ctx);
  assert.match(saved, /^Saved 2 phase\(s\)\.\n\nThe phases now:\n1\. Data model \[unplanned\] \(id \d+\)/);
  const [first] = await store.phasesOf(db, (await store.requireConstruction(db, app)).id);
  const tasks = await tool("save_tasks")(
    {
      phase: String(first.id),
      tasks: [
        { kind: "data_model", title: "Tables", brief: "Make them" },
        { kind: "feature", title: "List", brief: "List them", depends_on: [1] },
      ],
    },
    ctx,
  );
  assert.equal(tasks, "Saved 2 task(s).\n\n1. Tables [data_model, todo]\n2. List [feature, todo] after 1\n");
  // A phase that is not a number, and an invalid list, are refused with the reason.
  await assert.rejects(tool("save_tasks")({ phase: "first", tasks: [] }, ctx), /`phase` must be the phase's id/);
  await assert.rejects(tool("save_phases")({ phases: [] }, ctx), /at least one phase/);
  // Keeping a phase with tasks is said.
  const again = await tool("save_phases")({ phases: [phase("Contacts only")] }, ctx);
  assert.match(again, /Kept, because they have tasks: `Data model`\./);
});

test("questions, mockups and a feedback plan are saved and said", async () => {
  const { db, app, ctx } = await made();
  assert.equal(await tool("ask_questions")({ questions: ["Which currency?", "Who signs in?"] }, ctx), "Asked 2 question(s) for the admin to answer.");
  const c = await store.requireConstruction(db, app);
  const [q] = await store.questionsOf(db, c.id);
  await store.answerQuestions(db, app, [{ question: q.id, text: "Euro" }]);
  const mockup = { title: "Table", rationale: "Dense", screens: [{ name: "Orders list", html: "<table></table>" }] };
  assert.equal(await tool("save_mockups")({ mockups: [mockup] }, ctx), "Saved 1 mockup(s) as round 1, for the admin to choose from:\n- Table: Orders list");
  assert.equal(
    await tool("ask_questions")({ questions: ["Any discounts?"] }, ctx),
    "Asked 1 question(s) for the admin to answer. 1 answered earlier stay as they are. The 1 mockup(s) drawn before were discarded: new ones are drawn from the answers.",
  );

  await tool("save_phases")({ phases: [phase("Contacts")] }, ctx);
  const [p] = await store.phasesOf(db, c.id);
  const [t] = await store.saveTasks(db, app, p.id, [{ kind: "feature", title: "List", brief: "List them", depends_on: [] }]);
  await store.updateTask(db, { ...t, status: "done" });
  const feedback = await store.addFeedback(db, app, p.id, "Sort it", "By name");
  const planned = await tool("plan_feedback")(
    { feedback: feedback.id, requirements: [{ text: "Sorted by name", priority: "must" }], tasks: [{ kind: "feature", title: "Sort", brief: "Sort the list" }] },
    ctx,
  );
  assert.match(planned, /^Planned the feedback: 1 requirement\(s\), and 1 follow-up task\(s\) for the admin to review and run:\n\n1\. Sort \[feature, todo\]/);
  await assert.rejects(tool("plan_feedback")({ feedback: "x", requirements: [], tasks: [] }, ctx), /`feedback` must be the feedback's id/);
});

test("the session header carries the plan, and an application without a construction says so", async () => {
  const { db, app, ctx } = await made();
  await tool("ask_questions")({ questions: ["Which currency?"] }, ctx);
  const c = await store.requireConstruction(db, app);
  const [q] = await store.questionsOf(db, c.id);
  await store.answerQuestions(db, app, [{ question: q.id, text: "Euro" }]);
  await tool("save_mockups")({ mockups: [{ title: "Cards", rationale: "", screens: [{ name: "Home", html: "<p>x</p>" }] }] }, ctx);
  await tool("save_phases")({ phases: [phase("Data model")] }, ctx);
  let header = await constructor.session_header(ctx);
  assert.match(header, /^## The construction of `crm`\n\nSpecification:\nA CRM for a small team\./);
  assert.match(header, /Question: Which currency\?\nAnswer: Euro/);
  assert.match(header, /Mockups proposed; none chosen yet\./);
  assert.match(header, /Phases:\n1\. Data model \[unplanned\]/);
  const [mockup] = await store.mockupsOf(db, c.id);
  await store.chooseMockup(db, app, mockup.id);
  header = await constructor.session_header(ctx);
  assert.match(header, /Chosen mockup: Cards/);

  const other = { ...ctx, config: { application: "shop" } };
  assert.equal(await constructor.session_header(other), "`shop` has no construction yet.");
  await assert.rejects(tool("save_phases")({ phases: [phase("x")] }, other), /this application has no construction/);
  await assert.rejects(tool("save_phases")({ phases: [phase("x")] }, { ...ctx, config: {} }), /`application` must name an application by its subdomain/);
});

test("the tools declare the shapes the routes check, not copies of them", () => {
  const { tools } = constructor;
  assert.equal(tools.save_phases.parameters.properties.phases, m.PHASES);
  assert.equal(tools.save_tasks.parameters.properties.tasks, m.TASKS);
  assert.equal(tools.ask_questions.parameters.properties.questions, m.QUESTIONS);
  assert.equal(tools.save_mockups.parameters.properties.mockups, m.MOCKUPS);
  assert.equal(tools.plan_feedback.parameters.properties.requirements, m.FEEDBACK_REQUIREMENTS);
  assert.equal(tools.plan_feedback.parameters.properties.tasks, m.FEEDBACK_TASKS);
});
