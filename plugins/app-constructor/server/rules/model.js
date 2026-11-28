// The App Constructor's rules, with no database (TODO-app-constructor AC13.1; ported from
// crates/sc-construct): the statuses, a phase's status from its tasks, what the agent's tools
// and the admin's edits may save, what the runner does next, and the texts the agent is told.
//
// Pure functions, so the rules are tested without a server, and the agent's tool and the
// admin's endpoint share them: both are allowed the same lists and told the same reasons.
//
// Shapes, as the store reads them (snake_case, as the columns):
// - phase `{ id, construction, position, title, description, requirements: [{ text, priority }] }`;
// - task `{ id, uid, phase, position, kind, title, brief, depends_on: [uid], status, runs: [run id],
//   summary, created: [{ kind, name }], feedback }`. `depends_on` names tasks by `uid`, made when
//   the plan is built: a plan's tasks are written in one batch, before any has an id;
// - a phase with its tasks `{ phase, tasks }`, which is what a phase's status is read from.

"use strict";

// Web Crypto's, which both the module worker and node have.
const randomUUID = () => globalThis.crypto.randomUUID();
const utf8 = new TextEncoder();
const { check } = require("./check.js");

// --- statuses -----------------------------------------------------------------------------

/** Where a construction is. The runner moves it between these; the specification and the
 * phases are edited in `draft` and `planned`. */
const CONSTRUCTION_STATUSES = [
  "draft",
  "planned",
  "running",
  "stopped",
  "done",
  "failed",
];
/** The kind of artefact a task makes, which decides the agent that runs it: `data_model` the
 * construction's own agent, `feature` the application's builder agent. */
const TASK_KINDS = ["data_model", "feature"];
const TASK_STATUSES = ["todo", "running", "done", "failed", "skipped"];
/** A phase's status. Not stored: it follows from the phase's tasks (`phaseStatus`). */
const PHASE_STATUSES = ["unplanned", "planned", "running", "failed", "done"];
const PRIORITIES = ["must", "should", "could"];
/** One of a construction's mockups at most is `chosen`. */
const MOCKUP_STATUSES = ["proposed", "chosen"];
/** What failed, for the error log; `runner` is what is no one step's, such as tasks that wait
 * on each other, or an admin who is no longer one. */
const ERROR_SOURCES = [
  "phases",
  "tasks",
  "task",
  "mockups",
  "questions",
  "feedback",
  "runner",
];
/** What a construction run is for; `build` is a session of the runner, whose tasks keep their
 * own runs. */
const RUN_PURPOSES = ["phases", "mockups", "questions", "feedback", "build"];

// --- limits -------------------------------------------------------------------------------

/** v1's plans had three to six phases; a longer list is a sign the phases are tasks. */
const MAX_PHASES = 12;
const MAX_TITLE = 200;
/** v1's phases had two to eight tasks; a longer list is a sign the phase should be split. */
const MAX_TASKS = 20;
const MAX_TASK_TITLE = 200;
const MAX_MOCKUPS = 3;
const DEFAULT_MOCKUPS = 2;
/** The screens that decide the application's shape, not every page of it. */
const MAX_SCREENS = 8;
/** In bytes. */
const MAX_SCREEN_HTML = 100000;
const MAX_NAME = 120;
/** Fewer is better. */
const MAX_QUESTIONS = 10;
const MAX_QUESTION = 500;
const MAX_FEEDBACK_TITLE = 200;
/** Feedback asks for a change, not a new phase. */
const MAX_FEEDBACK_REQUIREMENTS = 10;
const MAX_FEEDBACK_TASKS = 5;
/** The newest errors, which are the ones that matter. */
const MAX_ERRORS_SHOWN = 100;

// --- errors -------------------------------------------------------------------------------

/** A refusal the routes answer with 400 (`kind` is `invalid`) or 404 (`not_found`). */
class ConstructError extends Error {
  constructor(kind, message) {
    super(message);
    this.name = "ConstructError";
    this.kind = kind;
  }
}
/** An error for input that is not acceptable. */
const invalid = (message) => new ConstructError("invalid", message);
/** An error for something that does not exist. */
const notFound = (message) => new ConstructError("not_found", message);

// --- input shapes and parsing -------------------------------------------------------------
//
// The JSON Schemas of what the agent's tools and the admin's edits save. The tools declare them
// as their parameters (`trait.js`), and the agent loop checks a call against them before the
// tool runs; a route checks the same input with `check` (`check.js`). What a schema cannot say
// (no blank titles, ids that exist, limits in characters) the `parse…` and `validate…`
// functions below check after it.

/** One requirement of a phase or a feedback. */
const REQUIREMENT = {
  type: "object",
  title: "requirement",
  properties: {
    text: { type: "string" },
    priority: { type: "string", enum: PRIORITIES },
  },
  required: ["text", "priority"],
  additionalProperties: false,
};

/** The phase list `save_phases` and the admin's edit save. */
const PHASES = {
  type: "array",
  items: {
    type: "object",
    title: "phase",
    properties: {
      id: {
        type: "integer",
        minimum: 1,
        nullable: true,
        description: "An existing phase's id",
      },
      title: { type: "string" },
      description: { type: "string" },
      requirements: { type: "array", items: REQUIREMENT },
    },
    required: ["title", "description", "requirements"],
    additionalProperties: false,
  },
};

/** The tasks of one phase, or of one feedback's plan. */
const TASKS = {
  type: "array",
  items: {
    type: "object",
    title: "task",
    properties: {
      kind: { type: "string", enum: TASK_KINDS },
      title: { type: "string" },
      brief: {
        type: "string",
        description: "What to make; all the task's agent reads",
      },
      depends_on: {
        type: "array",
        items: { type: "integer", minimum: 1 },
        description: "Earlier tasks of this list, by number",
      },
    },
    required: ["kind", "title", "brief"],
    additionalProperties: false,
  },
};

/** One round of mockups. */
const MOCKUPS = {
  type: "array",
  minItems: 1,
  maxItems: MAX_MOCKUPS,
  items: {
    type: "object",
    title: "mockup",
    properties: {
      title: { type: "string" },
      rationale: {
        type: "string",
        description: "What sets this mockup apart",
      },
      screens: {
        type: "array",
        minItems: 1,
        maxItems: MAX_SCREENS,
        items: {
          type: "object",
          title: "screen",
          properties: {
            name: {
              type: "string",
              description: "What the screen is, e.g. `Orders list`",
            },
            html: {
              type: "string",
              description:
                "A complete HTML page, CSS in a `<style>` element, no scripts or external resources",
            },
          },
          required: ["name", "html"],
          additionalProperties: false,
        },
      },
    },
    required: ["title", "rationale", "screens"],
    additionalProperties: false,
  },
};

/** The clarifying questions the agent asks. */
const QUESTIONS = {
  type: "array",
  minItems: 1,
  maxItems: MAX_QUESTIONS,
  description: "The questions, one idea each; fewer is better",
  items: { type: "string", title: "question" },
};

/** The requirements a feedback adds to its phase. */
const FEEDBACK_REQUIREMENTS = {
  type: "array",
  minItems: 1,
  maxItems: MAX_FEEDBACK_REQUIREMENTS,
  items: REQUIREMENT,
};

/** The follow-up tasks that meet a feedback. */
const FEEDBACK_TASKS = {
  ...TASKS,
  minItems: 1,
  maxItems: MAX_FEEDBACK_TASKS,
  description: "`depends_on` numbers tasks of this list only",
};

/** Throw `invalid` for the first way `value` (called `name`) does not match `schema`. */
const checkShape = (schema, value, name) => check(schema, value, name, invalid);

/** A phase list from JSON (`PHASES`), naming what is wrong with it. An `id` names an existing
 * phase, updated in place; none is a new one. */
function parsePhases(value) {
  checkShape(PHASES, value, "`phases`");
  return value.map((p) => ({ ...p, id: p.id ?? null }));
}

/** A list of tasks from a tool's JSON arguments — `[{ kind, title, brief, depends_on? }]`,
 * `depends_on` naming earlier tasks of the list by number (1 is the first). */
function parseTasks(value) {
  checkShape(TASKS, value, "`tasks`");
  return value.map((t) => ({ ...t, depends_on: t.depends_on ?? [] }));
}

/** A round of mockups from the tool's JSON, checked, naming what is wrong: one to
 * `MAX_MOCKUPS` mockups, each with a title and one to `MAX_SCREENS` screens of distinct names
 * (as files) and some HTML no longer than `MAX_SCREEN_HTML` bytes. */
function parseMockups(value) {
  checkShape(MOCKUPS, value, "`mockups`");
  return value.map((item, i) => {
    const n = i + 1;
    const mockup = {
      title: item.title.trim(),
      rationale: item.rationale.trim(),
      screens: item.screens.map((s) => ({ name: s.name, html: s.html })),
    };
    if (!mockup.title) throw invalid(`mockup ${n} has no title`);
    if (chars(mockup.title) > MAX_NAME)
      throw invalid(
        `mockup ${n}'s title is longer than ${MAX_NAME} characters`
      );
    const files = new Set();
    for (const s of mockup.screens) {
      s.name = s.name.trim();
      if (!s.name) throw invalid(`mockup ${n} has a screen with no name`);
      if (chars(s.name) > MAX_NAME)
        throw invalid(
          `mockup ${n}: the screen name \`${s.name}\` is longer than ${MAX_NAME} characters`
        );
      const file = screenFile(s.name);
      if (files.has(file))
        throw invalid(`mockup ${n} has two screens named \`${s.name}\``);
      files.add(file);
      if (!s.html.trim())
        throw invalid(`mockup ${n}: the screen \`${s.name}\` has no HTML`);
      if (utf8.encode(s.html).length > MAX_SCREEN_HTML) {
        throw invalid(
          `mockup ${n}: the screen \`${s.name}\` is longer than ${MAX_SCREEN_HTML} bytes; keep a mockup to the layout and a few rows of sample data`
        );
      }
    }
    return mockup;
  });
}

/** The questions the agent asks, checked: one to `MAX_QUESTIONS` distinct (ignoring case),
 * non-blank texts of at most `MAX_QUESTION` characters. Answers them trimmed. */
function parseQuestions(value) {
  checkShape(QUESTIONS, value, "`questions`");
  const seen = new Set();
  return value.map((item, i) => {
    const n = i + 1;
    const t = item.trim();
    if (!t) throw invalid(`question ${n} is blank`);
    if (chars(t) > MAX_QUESTION)
      throw invalid(
        `question ${n} is longer than ${MAX_QUESTION} characters; ask one thing at a time`
      );
    if (seen.has(t.toLowerCase()))
      throw invalid(`question ${n} is asked twice`);
    seen.add(t.toLowerCase());
    return t;
  });
}

/** A feedback's plan from the tool's JSON arguments — `{ requirements, tasks }` — naming what
 * is wrong: one to `MAX_FEEDBACK_REQUIREMENTS` requirements with text, and one to
 * `MAX_FEEDBACK_TASKS` tasks. */
function parseFeedbackPlan(args) {
  checkShape(FEEDBACK_REQUIREMENTS, args?.requirements ?? null, "`requirements`");
  const requirements = args.requirements.map((r, i) => {
    const t = r.text.trim();
    if (!t) throw invalid(`requirement ${i + 1} has no text`);
    return { text: t, priority: r.priority };
  });
  checkShape(FEEDBACK_TASKS, args.tasks ?? null, "`tasks`");
  return { requirements, tasks: parseTasks(args.tasks) };
}

/** The length of `s` in characters, not UTF-16 units. */
const chars = (s) => [...s].length;

// --- phases and tasks ---------------------------------------------------------------------

/** A phase's status, from its tasks: none is unplanned, any running is running, any failed is
 * failed, all done or skipped is done, and anything else (some still to do) is planned. */
function phaseStatus(tasks) {
  if (tasks.length === 0) return "unplanned";
  if (tasks.some((t) => t.status === "running")) return "running";
  if (tasks.some((t) => t.status === "failed")) return "failed";
  if (tasks.every((t) => t.status === "done" || t.status === "skipped"))
    return "done";
  return "planned";
}

/** A new task to do, not stored yet. */
function newTask(phase, position, kind, title, brief) {
  return {
    uid: randomUUID(),
    phase,
    position,
    kind,
    title,
    brief,
    depends_on: [],
    status: "todo",
    runs: [],
    summary: null,
    created: [],
    feedback: null,
  };
}

/** Refuse a phase list that cannot be saved over `existing` (the construction's phases), with
 * the reason; answer it trimmed. */
function validatePhases(inputs, existing) {
  if (inputs.length === 0)
    throw invalid("a construction needs at least one phase; the list is empty");
  if (inputs.length > MAX_PHASES) {
    throw invalid(
      `${inputs.length} phases are too many: at most ${MAX_PHASES}. A phase delivers a usable part of the application; smaller steps are its tasks`
    );
  }
  const seen = new Set();
  return inputs.map((input, i) => {
    const n = i + 1;
    const title = input.title.trim();
    if (!title) throw invalid(`phase ${n} has no title`);
    if (chars(title) > MAX_TITLE)
      throw invalid(
        `phase ${n}'s title is longer than ${MAX_TITLE} characters; put the detail in its description`
      );
    if (input.id !== null && input.id !== undefined) {
      if (!existing.some((p) => sameId(p.id, input.id))) {
        throw invalid(
          `phase ${n} names id ${input.id}, which is not a phase of this construction; leave \`id\` out for a new phase`
        );
      }
      if (seen.has(String(input.id)))
        throw invalid(`phase ${n} names id ${input.id} a second time`);
      seen.add(String(input.id));
    }
    const requirements = (input.requirements || []).map((r, j) => {
      const t = r.text.trim();
      if (!t) throw invalid(`phase ${n}'s requirement ${j + 1} has no text`);
      return { text: t, priority: r.priority };
    });
    return {
      id: input.id ?? null,
      title,
      description: (input.description || "").trim(),
      requirements,
    };
  });
}

/** The phases as the agent reads them: one numbered line each, with its id, status and
 * requirements. What every `save_phases` result ends with, and the session header shows. */
function phaseList(phases) {
  if (phases.length === 0) return "No phases yet.";
  let out = "";
  phases.forEach(({ phase, tasks }, i) => {
    out += `${i + 1}. ${phase.title} [${phaseStatus(tasks)}] (id ${
      phase.id
    })\n`;
    if (phase.description) out += `   ${phase.description}\n`;
    for (const r of phase.requirements)
      out += `   - (${r.priority}) ${r.text}\n`;
  });
  return out.trimEnd();
}

/** Check `inputs` and turn them into tasks of `phase`, numbered in order (positions 1..n). A
 * task depends only on earlier ones, so a list cannot depend on itself in a circle; the tasks
 * of earlier phases are all done before a phase starts, so they need no naming. */
function buildTasks(phase, inputs) {
  if (inputs.length === 0)
    throw invalid("a phase needs at least one task; the list is empty");
  if (inputs.length > MAX_TASKS)
    throw invalid(
      `${inputs.length} tasks are too many for one phase: at most ${MAX_TASKS}`
    );
  const tasks = [];
  inputs.forEach((input, i) => {
    const n = i + 1;
    const title = input.title.trim();
    if (!title) throw invalid(`task ${n} has no title`);
    if (chars(title) > MAX_TASK_TITLE)
      throw invalid(
        `task ${n}'s title is longer than ${MAX_TASK_TITLE} characters; put the detail in its brief`
      );
    const brief = input.brief.trim();
    if (!brief) throw invalid(`task ${n} has no brief; say what it makes`);
    const dependsOn = [];
    for (const earlier of input.depends_on || []) {
      if (earlier === 0 || earlier >= n) {
        throw invalid(
          `task ${n} depends on task ${earlier}, which is not before it; a task can only depend on earlier tasks of the list`
        );
      }
      const uid = tasks[earlier - 1].uid;
      if (!dependsOn.includes(uid)) dependsOn.push(uid);
    }
    const task = newTask(phase, n, input.kind, title, brief);
    task.depends_on = dependsOn;
    tasks.push(task);
  });
  return tasks;
}

/** The tasks as the agent reads them: one numbered line each, with its kind, status and what
 * it depends on. */
function taskList(tasks) {
  if (tasks.length === 0) return "No tasks yet.";
  let out = "";
  tasks.forEach((task, i) => {
    out += `${i + 1}. ${task.title} [${task.kind}, ${task.status}]`;
    const after = task.depends_on
      .map((uid) => tasks.findIndex((t) => t.uid === uid))
      .filter((k) => k >= 0)
      .map((k) => k + 1);
    if (after.length) out += ` after ${after.join(", ")}`;
    out += "\n";
  });
  return out;
}

/** What the runner does next, from the construction's phases in order (each `{ phase, tasks }`)
 * and `until`, the id of the phase the run stops after (`null` runs them all):
 * - `{ kind: "plan_tasks", phase }`: the phase has no tasks yet; plan them;
 * - `{ kind: "run_task", task }`: the first `todo` task whose dependencies are done, or the one
 *   still `running` when the server stopped;
 * - `{ kind: "failed", task }`: a task failed, and the construction stops here;
 * - `{ kind: "blocked", phase }`: tasks to do, but each waits on another that is not done;
 * - `{ kind: "finished" }`: every phase is done, or the one the run ends after is.
 *
 * Phases run one after another, and a phase's tasks are only planned once every earlier phase is
 * done, so a feature task is written against the tables the data-model task actually made. A
 * skipped task counts as done, so the tasks that depend on it still run. */
function nextStep(phases, until = null) {
  const finished = new Set();
  for (const { tasks } of phases) {
    for (const t of tasks)
      if (t.status === "done" || t.status === "skipped") finished.add(t.uid);
  }
  for (const { phase, tasks } of phases) {
    if (tasks.length === 0) return { kind: "plan_tasks", phase };
    const failed = tasks.find((t) => t.status === "failed");
    if (failed) return { kind: "failed", task: failed };
    const running = tasks.find((t) => t.status === "running");
    if (running) return { kind: "run_task", task: running };
    if (!tasks.every((t) => finished.has(t.uid))) {
      const ready = tasks.find(
        (t) => t.status === "todo" && t.depends_on.every((d) => finished.has(d))
      );
      return ready
        ? { kind: "run_task", task: ready }
        : { kind: "blocked", phase };
    }
    if (until !== null && until !== undefined && sameId(until, phase.id))
      return { kind: "finished" };
  }
  return { kind: "finished" };
}

/** The phase `phase` (an id) with its tasks, if its tasks may be planned (again) on their own,
 * before a run; else why not. Every phase before it must be done, since its tasks are planned
 * against what those made, and none of its tasks may have started (even one a stop put back to
 * `todo` has runs), since what ran stays recorded. */
function requirePlannable(phases, phase) {
  const index = phases.findIndex((p) => sameId(p.phase.id, phase));
  if (index < 0) throw notFound(`no phase ${phase} in this construction`);
  const target = phases[index];
  const before = phases
    .slice(0, index)
    .find((p) => phaseStatus(p.tasks) !== "done");
  if (before) {
    throw invalid(
      `phase \`${before.phase.title}\` is not done yet, and \`${target.phase.title}\`'s tasks are planned against what the phases before it made; run up to \`${before.phase.title}\` first`
    );
  }
  if (target.tasks.some((t) => t.status !== "todo" || t.runs.length > 0)) {
    throw invalid(
      `phase \`${target.phase.title}\` has started, so its tasks cannot be planned again`
    );
  }
  return target;
}

/** What the run that plans a phase's tasks again is told besides the phase: the plan it
 * replaces, and the admin's `note` on it. `null` when there is neither. */
function replanNote(previous, note) {
  const trimmed = typeof note === "string" ? note.trim() : "";
  let out = "";
  if (previous.length) {
    out += "This phase was planned before, as:\n";
    previous.forEach((task, i) => {
      out += `${i + 1}. (${task.kind}) ${task.title}: ${task.brief}\n`;
    });
    out +=
      "The admin asked for the tasks to be planned again, so that plan is gone. Plan the phase anew; keep what was right in it.\n";
  }
  if (trimmed)
    out += `The admin's note on how to plan this phase: ${trimmed}\n`;
  return out ? out : null;
}

// --- mockups ------------------------------------------------------------------------------

/** The file a screen is written to in the application's source, under `mockups/`: its name in
 * lower case, letters and digits kept and the rest one dash ("Orders list" → `orders-list.html`). */
function screenFile(name) {
  let slug = "";
  for (const c of name.trim().toLowerCase()) {
    if (/[\p{L}\p{N}]/u.test(c)) slug += c;
    else if (slug && !slug.endsWith("-")) slug += "-";
  }
  slug = slug.replace(/-+$/, "");
  return `${slug || "screen"}.html`;
}

/** The last round's mockups, in order: what the admin is choosing from. */
function latestRound(mockups) {
  if (mockups.length === 0) return [];
  const round = Math.max(...mockups.map((m) => m.round));
  return mockups.filter((m) => m.round === round);
}

/** The chosen mockup as the phase plan, the task plans and the feature tasks are told it: its
 * title and rationale, the admin's feedback, and each screen's file and HTML. The screens show
 * the fields the data model needs. */
function mockupText(mockup) {
  let out = `## The chosen mockup: ${mockup.title}\n\n${
    mockup.rationale || "(no rationale given)"
  }\n`;
  if (mockup.feedback)
    out += `\nThe admin's feedback on it: ${mockup.feedback}\n`;
  for (const s of mockup.screens) {
    out += `\n### ${s.name} (\`mockups/${screenFile(
      s.name
    )}\`)\n\n\`\`\`html\n${s.html.trim()}\n\`\`\`\n`;
  }
  return out;
}

// --- questions ----------------------------------------------------------------------------

/** The answered questions as the agent is told them, beside the specification; `null` when
 * none is answered. A question is `{ text, answer }` as the store reads it. */
function answersText(questions) {
  const pairs = questions
    .filter((q) => q.answer !== null && q.answer !== undefined)
    .map((q) => `Question: ${q.text}\nAnswer: ${q.answer.trim()}`);
  return pairs.length
    ? `## The admin's answers to the clarifying questions\n\n${pairs.join(
        "\n\n"
      )}`
    : null;
}

// --- feedback -----------------------------------------------------------------------------

/** The phase `phase` (an id) with its tasks, if it can take feedback; else why not. It must
 * have built something (a task done), and have no failed task, which would stop the runner
 * before the follow-up tasks. */
function requireFeedbackPhase(phases, phase) {
  const target = phases.find((p) => sameId(p.phase.id, phase));
  if (!target) throw notFound(`no phase ${phase} in this construction`);
  if (!target.tasks.some((t) => t.status === "done")) {
    throw invalid(
      `phase \`${target.phase.title}\` has not built anything yet; feedback is on what a phase built`
    );
  }
  const failed = target.tasks.find((t) => t.status === "failed");
  if (failed)
    throw invalid(
      `task \`${failed.title}\` of phase \`${target.phase.title}\` failed; retry or skip it before giving feedback`
    );
  return target;
}

/** The tasks planned for feedback `feedback` (an id), in order. */
function feedbackTasks(phases, feedback) {
  return phases
    .flatMap((p) => p.tasks)
    .filter(
      (t) =>
        t.feedback !== null &&
        t.feedback !== undefined &&
        sameId(t.feedback, feedback)
    );
}

// --- errors -------------------------------------------------------------------------------

/** An error to log: what failed (`source`, one of ERROR_SOURCES), why, and optionally where —
 * `{ phase, task, feedback, run, construction_run }`. It is the construction's current error
 * while `construction_run` is its latest run. */
function failure(source, message, where = {}) {
  if (!ERROR_SOURCES.includes(source))
    throw new Error(`\`${source}\` is not an error source`);
  return {
    source,
    message,
    phase: where.phase ?? null,
    task: where.task ?? null,
    feedback: where.feedback ?? null,
    run: where.run ?? null,
    construction_run: where.construction_run ?? null,
  };
}

/** Whether two ids are one: a row's id is a number, and one from a URL or a tool is often its
 * text. */
function sameId(a, b) {
  return (
    a !== null &&
    a !== undefined &&
    b !== null &&
    b !== undefined &&
    String(a) === String(b)
  );
}

module.exports = {
  CONSTRUCTION_STATUSES,
  TASK_KINDS,
  TASK_STATUSES,
  PHASE_STATUSES,
  PRIORITIES,
  MOCKUP_STATUSES,
  ERROR_SOURCES,
  RUN_PURPOSES,
  MAX_PHASES,
  MAX_TITLE,
  MAX_TASKS,
  MAX_TASK_TITLE,
  MAX_MOCKUPS,
  DEFAULT_MOCKUPS,
  MAX_SCREENS,
  MAX_SCREEN_HTML,
  MAX_NAME,
  MAX_QUESTIONS,
  MAX_QUESTION,
  MAX_FEEDBACK_TITLE,
  MAX_FEEDBACK_REQUIREMENTS,
  MAX_FEEDBACK_TASKS,
  MAX_ERRORS_SHOWN,
  REQUIREMENT,
  PHASES,
  TASKS,
  MOCKUPS,
  QUESTIONS,
  FEEDBACK_REQUIREMENTS,
  FEEDBACK_TASKS,
  ConstructError,
  invalid,
  notFound,
  phaseStatus,
  newTask,
  parsePhases,
  validatePhases,
  phaseList,
  parseTasks,
  buildTasks,
  taskList,
  nextStep,
  requirePlannable,
  replanNote,
  parseMockups,
  screenFile,
  latestRound,
  mockupText,
  parseQuestions,
  answersText,
  requireFeedbackPhase,
  feedbackTasks,
  parseFeedbackPlan,
  failure,
  sameId,
};
