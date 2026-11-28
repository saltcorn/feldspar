// What the runs are told, and what a task made (ported from construction_runner.rs's unit
// tests): a feature task is told where the chosen mockup's screens are; the tables and queries a
// task made are read from its runs' tool results.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const messages = require("../../rules/messages.js");
const m = require("../../rules/model.js");

const app = { id: "a", name: "CRM", subdomain: "crm", description: "Contacts", framework: "react", builder_agent: "build-crm" };

test("a feature task is told where the chosen mockup's screens are", () => {
  const mockup = { title: "Dense table", rationale: "", screens: [{ name: "Orders list", html: "<table></table>" }], status: "chosen", feedback: null };
  const feature = m.newTask(1, 1, "feature", "Orders", "List them");
  const hint = messages.mockupHint(feature, mockup);
  assert.match(hint, /"Dense table"/);
  assert.match(hint, /`mockups\/orders-list\.html` \(Orders list\)/);
  // A data-model task's brief has the fields; no mockup, no hint.
  assert.equal(messages.mockupHint(m.newTask(1, 1, "data_model", "Tables", "Make them"), mockup), "");
  assert.equal(messages.mockupHint(feature, null), "");
  assert.match(messages.taskMessage(app, feature, [], mockup), /mockups\/orders-list\.html/);
});

/** A run's messages holding these tool results, as `[tool, result]`. */
const runOf = (results) =>
  results.map(([tool, result], i) => ({ role: "tool_result", tool_call_id: `c${i}`, name: tool, content: typeof result === "string" ? result : JSON.stringify(result) }));

const edit = (created, dropped, renamed) => ({
  applied: true,
  dry_run: false,
  tables_created: created,
  tables_dropped: dropped,
  tables_renamed: renamed.map(([from, to]) => ({ from, to })),
});

test("what a task created is read from its runs' tool results", () => {
  const first = runOf([
    // A dry run and a failed call made nothing.
    ["edit_schema", { applied: false, dry_run: true, tables_created: ["x"] }],
    ["edit_schema", "error: the batch was refused"],
    ["edit_schema", edit(["contacts", "notes", "tmp"], [], [])],
    ["edit_schema", edit([], ["tmp"], [["notes", "memos"]])],
    ["save_api_query", { saved: "listContacts", created: true }],
    // Saving over an existing query edits it: not made here.
    ["save_api_query", { saved: "topAuthors", created: false }],
    ["save_api_query", { saved: "scratch", created: true }],
    ["delete_api_query", { deleted: "scratch" }],
    // Other tools are not read.
    ["describe_schema", { tables_created: ["nope"] }],
  ]);
  // A retry: the second attempt saves the same query again, and adds one.
  const second = runOf([
    ["save_api_query", { saved: "listContacts", created: true }],
    ["save_api_query", { saved: "countContacts", created: true }],
  ]);
  assert.deepEqual(messages.createdBy([first, second]), [
    { kind: "table", name: "contacts" },
    { kind: "table", name: "memos" },
    { kind: "query", name: "listContacts" },
    { kind: "query", name: "countContacts" },
  ]);
  // A stored context `{ messages }` reads the same; one with none made nothing anyone can tell.
  assert.deepEqual(messages.createdBy([{ messages: second }]).length, 2);
  assert.deepEqual(messages.createdBy([{}]), []);
});

test("a task plan is told what exists, the mockup and the admin's note", () => {
  const phase = { id: 7, title: "Contacts", description: "People", requirements: [{ text: "List them", priority: "must" }] };
  const done = { ...m.newTask(6, 1, "data_model", "Tables", "Make them"), status: "done", summary: "Made contacts.", created: [{ kind: "table", name: "contacts" }] };
  const phases = [{ phase: { id: 6, title: "Data model" }, tasks: [done] }, { phase, tasks: [] }];
  const text = messages.planTasksMessage(app, phase, phases, "The admin's note on how to plan this phase: fewer.", null);
  assert.match(text, /reached the phase `Contacts` \(id 7\)/);
  assert.match(text, /- \(must\) List them/);
  assert.match(text, /tables the construction made: contacts/);
  assert.match(text, /Data model \/ Tables: Made contacts\./);
  assert.match(text, /fewer\./);
  assert.match(text, /`save_tasks`/);
});

test("the phase plan is told the answers, the chosen mockup and to replan", () => {
  const spec = messages.specificationWith(" A CRM. ", "## The admin's answers");
  assert.equal(spec, "A CRM.\n\n## The admin's answers");
  const mockup = { title: "Cards", rationale: "Cards", screens: [{ name: "Home", html: "<p>x</p>" }], feedback: null };
  const text = messages.planMessage(app, spec, mockup, true);
  assert.match(text, /The application's description: Contacts/);
  assert.match(text, /The chosen mockup: Cards/);
  assert.match(text, /Plan the phases anew/);
  assert.doesNotMatch(messages.planMessage(app, spec, null, false), /anew|mockup/);
});

test("a mockup run proposes, or revises the last round when it was reviewed", () => {
  const fresh = messages.mockupMessage(app, "A CRM.", 2, [], null);
  assert.equal(fresh.revision, false);
  assert.match(fresh.text, /Propose 2 different mockups/);
  const round = [{ round: 1, title: "Table", rationale: "Dense", screens: [{ name: "List", html: "<table/>" }], feedback: "Too dense" }];
  const revised = messages.mockupMessage(app, "A CRM.", 2, round, "  bigger  ");
  assert.equal(revised.revision, true);
  assert.match(revised.text, /The admin's note on all of them: bigger/);
  assert.match(revised.text, /The admin's feedback: Too dense/);
});
