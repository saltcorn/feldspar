// The agent trait `constructor` (TODO-app-constructor AC13.2; ported from
// crates/sc-agent-builtins/src/app_constructor.rs): the planning tools of one application's
// construction, carried by its construction agent `constructor-<subdomain>`.
//
// - `save_phases` replaces the construction's phases, through the validation the admin's edits go
//   through too. Its result ends with the phase list, so the freshest copy is the last thing the
//   model read.
// - `ask_questions` asks the admin what the specification leaves open, before the mockups.
// - `save_mockups` saves a round of mockups for the admin to choose from before the phases.
// - `save_tasks` plans the tasks of one phase, when the runner reaches it, so they are written
//   against what the earlier phases actually made.
// - `plan_feedback` turns the admin's feedback on a built phase into requirements and follow-up
//   tasks after the phase's own.
//
// The session header carries the specification and the phases as they stand, so a conversation
// with the agent starts from the plan.
//
// The data-model tools (schema, queries, code libraries, the documentation) are not here: the
// construction agent carries the built-in trait `data_model` beside this one.
//
// **Admins only** (`admin_only`): the plan decides what the agent later builds with the
// console's authority, so the tools are offered only to an administrator at the console, never to
// a run a trigger or an application's page started. A tool runs as the run's caller, and reaches
// the construction tables through `ctx.db` as them.
//
// One application per instance, named by its subdomain. The trait has no way to look an
// application up, so it finds the construction by the agent name the routes gave it
// (`constructor-<subdomain>`), and names the application by its subdomain.

"use strict";

const model = require("../rules/model.js");
const store = require("../data/store.js");

const { invalid, notFound } = model;

/** What the agent is told about planning. */
const PROMPT =
  "You plan the construction of an application in phases. A phase delivers a part of the application someone can use and check; its tasks are planned later, when the phase is reached, so a phase says what, not how. The first phase is always the data model: the tables, their fields and relations, and the queries the application needs. Three to six phases suit most applications. Give each phase a short title, a description, and requirements, each with a priority (`must`, `should` or `could`). Save the phases with `save_phases`; to change them, call it again with the whole list, naming each phase you keep by its `id`. A phase that already has tasks stays even when you leave it out.\n\n" +
  "When a phase is reached you are asked to plan its tasks with `save_tasks`. A `data_model` task makes tables, fields or queries, and you run it yourself; a `feature` task makes part of the code, and the application's builder agent runs it. Plan few tasks, each a sizeable piece of work: every task starts a fresh session that reads the schema and the code again before it does anything, so two small tasks cost far more than one that does both. One to three tasks suit most phases: the phase's tables and fields in one `data_model` task, the queries over them in one more (or the same one when there are few), and a whole screen or flow in one `feature` task. Never plan a task that only builds, checks, tests, publishes or reviews: after every feature task the application is built and checked anyway, and the task fails if it does not build. A task's brief is all its agent reads, so say what to make and name the tables and queries it uses. Never plan what exists already. `depends_on` lists earlier tasks of the same list by number.\n\n" +
  "Before anything is drawn or planned, you may be asked to ask the admin clarifying questions with `ask_questions`. Ask only what the specification leaves open and the answer would change what gets built: who uses the application and with which rights, what is recorded, what is calculated, what must be seen together. Fewer is better; ten is a limit, not a target. One idea per question, short and direct, in plain language with no abbreviation left unexplained. Do not ask what the specification or an answer already says. The answers count as part of the specification.\n\n" +
  "Before the phases, you may be asked to propose mockups with `save_mockups`: a few different looks of the application, for the admin to choose from. Each mockup is the handful of screens that decide the application's shape (its main list, a form, a detail page, a dashboard), each a complete, self-contained HTML page with its CSS in a `<style>` element: no scripts, no external stylesheets, fonts or images (draw icons with inline SVG). Fill the screens with a few rows of realistic sample data, and show every field a person would see or enter, since the data model is built from them. Make the mockups differ in layout and navigation, not only in colour, and say in each one's rationale what sets it apart. When a mockup has been chosen, the phases and their tasks follow it: its screens are the features, and its fields are the data model.\n\n" +
  "Once a phase is built, the admin may give feedback on it, and you are asked to plan it with `plan_feedback`: the requirements the feedback adds to the phase, each one sentence on what must change, and the follow-up tasks that meet them, after the phase's own. A follow-up task changes what exists: its brief names the tables, queries, screens or code it changes and how. Plan only what the feedback asks, in as few tasks as the tasks above.";

/** The subdomain the trait's configuration names. */
function configured(config) {
  const subdomain =
    typeof (config && config.application) === "string"
      ? config.application.trim()
      : "";
  if (!subdomain)
    throw invalid("`application` must name an application by its subdomain");
  return subdomain;
}

/** The construction of the configured application, or `null`. */
async function constructionOf(ctx) {
  const subdomain = configured(ctx.config);
  const row = await ctx.db.ac_constructions
    .where({ agent: `constructor-${subdomain}` })
    .first();
  return row ? store.loadConstruction(ctx.db, row.application) : null;
}

async function requireConstruction(ctx) {
  const construction = await constructionOf(ctx);
  if (!construction) throw notFound("this application has no construction");
  return construction;
}

/** A row id a tool names: a number, or its text. */
function rowId(value, message) {
  if (Number.isInteger(value) && value > 0) return value;
  if (typeof value === "string" && /^\d+$/.test(value.trim()))
    return Number(value.trim());
  throw invalid(message);
}

async function savePhases(args, ctx) {
  const construction = await requireConstruction(ctx);
  const phases = model.parsePhases(args.phases ?? null);
  const saved = await store.savePhases(
    ctx.db,
    construction.application,
    phases
  );
  let text = `Saved ${phases.length} phase(s).`;
  if (saved.kept.length)
    text += ` Kept, because they have tasks: ${saved.kept
      .map((t) => `\`${t}\``)
      .join(", ")}.`;
  return `${text}\n\nThe phases now:\n${model.phaseList(saved.phases)}`;
}

async function saveTasks(args, ctx) {
  const construction = await requireConstruction(ctx);
  const phase = rowId(args.phase, "`phase` must be the phase's id");
  const tasks = model.parseTasks(args.tasks ?? null);
  const saved = await store.saveTasks(
    ctx.db,
    construction.application,
    phase,
    tasks
  );
  return `Saved ${saved.length} task(s).\n\n${model.taskList(saved)}`;
}

async function askQuestions(args, ctx) {
  const construction = await requireConstruction(ctx);
  const questions = model.parseQuestions(args.questions ?? null);
  const saved = await store.saveQuestions(
    ctx.db,
    construction.application,
    questions
  );
  const answered = saved.questions.filter((q) => q.answer !== null).length;
  let text = `Asked ${questions.length} question(s) for the admin to answer.`;
  if (answered) text += ` ${answered} answered earlier stay as they are.`;
  if (saved.discarded_mockups) {
    text += ` The ${saved.discarded_mockups} mockup(s) drawn before were discarded: new ones are drawn from the answers.`;
  }
  return text;
}

async function saveMockups(args, ctx) {
  const construction = await requireConstruction(ctx);
  const mockups = model.parseMockups(args.mockups ?? null);
  const saved = await store.saveMockups(
    ctx.db,
    construction.application,
    mockups
  );
  // The HTML is not repeated: the model has just written it.
  const list = saved.map(
    (m) => `- ${m.title}: ${m.screens.map((s) => s.name).join(", ")}`
  );
  return `Saved ${saved.length} mockup(s) as round ${
    saved.length ? saved[0].round : 1
  }, for the admin to choose from:\n${list.join("\n")}`;
}

async function planFeedback(args, ctx) {
  const construction = await requireConstruction(ctx);
  const feedback = rowId(args.feedback, "`feedback` must be the feedback's id");
  const plan = model.parseFeedbackPlan(args);
  const saved = await store.planFeedback(
    ctx.db,
    construction.application,
    feedback,
    plan
  );
  return `Planned the feedback: ${
    plan.requirements.length
  } requirement(s), and ${
    saved.length
  } follow-up task(s) for the admin to review and run:\n\n${model.taskList(
    saved
  )}`;
}

/** The specification and the phases as they stand. A construction not made yet is said so,
 * rather than failing the run. */
async function sessionHeader(ctx) {
  const subdomain = configured(ctx.config);
  const construction = await constructionOf(ctx);
  if (!construction) return `\`${subdomain}\` has no construction yet.`;
  const phases = await store.phasesWithTasks(ctx.db, construction.id);
  const specification = construction.specification.trim() || "(none yet)";
  // Which mockup was chosen, by title: its screens are in the messages that need them, not
  // repeated on every step.
  const mockups = await store.mockupsOf(ctx.db, construction.id);
  const chosen = mockups.find((m) => m.status === "chosen");
  const mockup = chosen
    ? `\n\nChosen mockup: ${chosen.title}`
    : mockups.length
    ? "\n\nMockups proposed; none chosen yet."
    : "";
  // The answers belong to the specification: every run is told them.
  const answers = model.answersText(
    await store.questionsOf(ctx.db, construction.id)
  );
  return `## The construction of \`${subdomain}\`\n\nSpecification:\n${specification}${
    answers ? `\n\n${answers}` : ""
  }${mockup}\n\nPhases:\n${model.phaseList(phases)}`;
}

const constructor = {
  description:
    "Plans the construction of one application in phases (the App Constructor)",
  admin_only: true,
  config_fields: [
    {
      name: "application",
      label: "Application (subdomain)",
      type: "String",
      required: true,
    },
  ],
  prompt: PROMPT,
  tools: {
    save_phases: {
      description:
        "Save the construction's phases, in order, replacing the list there is. Name a phase you keep by its `id` to update it in place; leave `id` out for a new one. A phase left out is deleted, unless it already has tasks. Returns the phases as they now stand.",
      parameters: {
        type: "object",
        properties: { phases: model.PHASES },
        required: ["phases"],
        additionalProperties: false,
      },
      mutates: true,
      run: savePhases,
    },
    save_tasks: {
      description:
        "Save the tasks of one phase, in order, replacing the tasks it has. Only for a phase whose tasks have not started. Returns the tasks as saved.",
      parameters: {
        type: "object",
        properties: {
          phase: { type: "integer", description: "The phase's id" },
          tasks: model.TASKS,
        },
        required: ["phase", "tasks"],
        additionalProperties: false,
      },
      mutates: true,
      run: saveTasks,
    },
    ask_questions: {
      description:
        "Ask the admin clarifying questions about the specification, for them to answer before the mockups are drawn. Replaces the questions still open; the answered ones stay. Discards the mockups there are.",
      parameters: {
        type: "object",
        properties: { questions: model.QUESTIONS },
        required: ["questions"],
        additionalProperties: false,
      },
      mutates: true,
      run: askQuestions,
    },
    save_mockups: {
      description:
        "Save a round of mockups for the admin to choose from: each a title, a one-sentence rationale, and its screens as self-contained HTML pages. A new round leaves the earlier ones as they are.",
      parameters: {
        type: "object",
        properties: { mockups: model.MOCKUPS },
        required: ["mockups"],
        additionalProperties: false,
      },
      mutates: true,
      run: saveMockups,
    },
    plan_feedback: {
      description:
        "Plan the admin's feedback on a built phase: the requirements it adds to the phase, and the follow-up tasks that meet them, which come after the phase's own tasks. Once per feedback. The tasks wait for the admin to review and run them.",
      parameters: {
        type: "object",
        properties: {
          feedback: { type: "integer", description: "The feedback's id" },
          requirements: model.FEEDBACK_REQUIREMENTS,
          tasks: model.FEEDBACK_TASKS,
        },
        required: ["feedback", "requirements", "tasks"],
        additionalProperties: false,
      },
      mutates: true,
      run: planFeedback,
    },
  },
  session_header: sessionHeader,
};

module.exports = { constructor, PROMPT, sessionHeader };
