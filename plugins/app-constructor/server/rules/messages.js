// What the agents are told (TODO-app-constructor AC13.3, AC13.4; ported from
// crates/sc-server/src/constructions.rs and construction_runner.rs): the message of every run
// the constructor starts, and what a task made, read back from its runs' transcripts.
//
// Pure functions over the shapes `store.js` reads, and the application as `apps.get` answers it
// (`{ id, name, subdomain, description, framework, builder_agent }`), so the texts are tested
// without a server and the routes and the runner share them.
//
// One thing the Rust told and this cannot: the names of the tables the catalog has. A module
// reaches only its own tables, so a run is told the tables the construction's own tasks made, and
// to read the schema (`describe_schema`, which the construction agent's `data_model` trait has)
// before it makes any.

"use strict";

const model = require("./model.js");

/** The data-model tool that reads the schema, which every planning run is told to start with. */
const TOOL_DESCRIBE = "describe_schema";
const TOOL_EDIT = "edit_schema";
const TOOL_SAVE_QUERY = "save_api_query";
const TOOL_DELETE_QUERY = "delete_api_query";

const trim = (s) => (typeof s === "string" ? s.trim() : "");

/** The specification as a run is told it: with the admin's answers to the clarifying questions
 * after it, which belong to it. */
function specificationWith(specification, answers) {
  return answers ? `${trim(specification)}\n\n${answers}` : trim(specification);
}

const requirementLines = (requirements) =>
  (requirements || []).map((r) => `- (${r.priority}) ${r.text}\n`).join("");

/** What the phase planning run is asked: the specification; the chosen mockup if there is one,
 * whose screens are the features and whose fields are the data model; and, when phases were
 * planned before (`replan`), to plan them anew, as the specification or the chosen mockup may
 * have changed since. */
function planMessage(app, specification, mockup, replan) {
  const description = trim(app.description)
    ? `\n\nThe application's description: ${trim(app.description)}`
    : "";
  const chosen = mockup
    ? `\n\nThe admin chose this mockup. Plan the phases to build it: its screens are the features, and the data model has every field they show.\n\n${model.mockupText(
        mockup
      )}`
    : "";
  const again = replan
    ? "\n\nThe construction has phases already, which may follow an earlier specification or another mockup. Plan the phases anew from what is above, as if there were none: keep a phase, by its `id`, only where it still fits as it is, and leave out the others, so `save_phases` deletes them. A phase with tasks is kept whatever the list says."
    : "";
  return (
    `Plan the construction of the \`${
      app.name
    }\` application (subdomain \`${trim(
      app.subdomain
    )}\`) in phases.${description}\n\n` +
    `The specification:\n${trim(specification)}${chosen}${again}\n\n` +
    `First look at the data model there is with \`${TOOL_DESCRIBE}\`, so the phases build on it rather than repeat it. Then save the phases with \`save_phases\`. Only plan in this conversation: nothing is built until the phases run.`
  );
}

/** What the mockup run is asked, `{ text, revision }`: `count` new mockups from the
 * specification, or, when the last round has the admin's feedback or there is a `note`, a
 * revision of that round, told each mockup with what was said about it. */
function mockupMessage(app, specification, count, mockups, note) {
  const latest = model.latestRound(mockups);
  const said = trim(note) || null;
  const revision =
    latest.length > 0 && (said !== null || latest.some((m) => m.feedback));
  const intro =
    `Propose mockups of the \`${app.name}\` application (subdomain \`${trim(
      app.subdomain
    )}\`) for the admin to choose from, before its construction is planned.\n\n` +
    `The specification:\n${trim(specification)}`;
  let ask;
  if (revision) {
    ask =
      "\n\nThe admin reviewed the last mockups. Revise them as the feedback asks: keep what was not criticised, and drop a mockup the admin rejected.\n";
    if (said) ask += `\nThe admin's note on all of them: ${said}\n`;
    for (const m of latest) {
      ask += `\n## Mockup "${m.title}"\n\n${
        m.rationale
      }\n\nThe admin's feedback: ${m.feedback || "none"}\n`;
      for (const s of m.screens)
        ask += `\n### ${s.name}\n\n\`\`\`html\n${trim(s.html)}\n\`\`\`\n`;
    }
    ask += "\nSave the revised mockups with `save_mockups`, as one new round.";
  } else {
    ask = `\n\nPropose ${count} different mockups and save them with \`save_mockups\`, in one call. Only propose in this conversation: nothing is built yet.`;
  }
  return { text: `${intro}${ask}`, revision };
}

/** What the questions run is asked: the specification, the questions the admin has answered (so
 * they are not asked again), and to ask what is still open with `ask_questions`. */
function questionsMessage(app, specification, questions) {
  const answers = model.answersText(questions);
  const answered = answers
    ? `\n\n${answers}\n\nDo not ask these again, nor what their answers settle. The questions still open are replaced by what you ask now.`
    : "";
  return (
    `Before mockups of the \`${app.name}\` application (subdomain \`${trim(
      app.subdomain
    )}\`) are drawn and its construction is planned, ask the admin what its specification leaves open.\n\n` +
    `The specification:\n${trim(specification)}${answered}\n\n` +
    "Ask your questions with `ask_questions`, in one call: only those whose answer would change what gets built. Only ask in this conversation: nothing is drawn or built yet."
  );
}

/** What the feedback run is asked: the phase as planned and as built (each task with what its
 * run said and made), the admin's feedback on it, and to plan it with `plan_feedback`. */
function feedbackMessage(app, phase, feedback) {
  let built = "";
  phase.tasks.forEach((task, i) => {
    built += `${i + 1}. ${task.title} [${task.kind}, ${
      task.status
    }]\n   Brief: ${trim(task.brief)}\n`;
    if (trim(task.summary)) built += `   What it did: ${trim(task.summary)}\n`;
    if (task.created && task.created.length)
      built += `   It created: ${JSON.stringify(task.created)}\n`;
  });
  return (
    `The admin gave feedback on phase \`${phase.phase.title}\` of the \`${
      app.name
    }\` application (subdomain \`${trim(
      app.subdomain
    )}\`), which has been built.\n\n` +
    `## The phase\n\n${trim(
      phase.phase.description
    )}\n\nRequirements:\n${requirementLines(phase.phase.requirements)}\n` +
    `Tasks:\n${built}\n## The feedback (id ${feedback.id})\n\n${
      feedback.title
    }\n\n${trim(feedback.description)}\n\n` +
    `Look at what exists with \`${TOOL_DESCRIBE}\` and the application's code where you need to. Then plan the feedback with \`plan_feedback\`, in one call: the requirements it adds to the phase, and the follow-up tasks that meet them. Only plan in this conversation: the tasks run when the admin starts them.`
  );
}

/** The tables the construction's tasks made so far, by name, from what each recorded: what the
 * runs are told in place of the catalog's table list, which a module cannot read. */
function tablesMade(phases) {
  const names = [];
  for (const { tasks } of phases) {
    for (const t of tasks) {
      for (const c of t.created || []) {
        if (
          c &&
          c.kind === "table" &&
          typeof c.name === "string" &&
          !names.includes(c.name)
        )
          names.push(c.name);
      }
    }
  }
  return names.sort();
}

/** What exists already, so a task does not make it again: the tables the construction made, and
 * what each finished task said it made. */
function existsAlready(phases) {
  const tables = tablesMade(phases);
  let out = "Exists already, do not recreate:\n";
  out += tables.length
    ? `- tables the construction made: ${tables.join(", ")}\n`
    : "- no tables made by the construction yet\n";
  for (const { phase, tasks } of phases) {
    for (const t of tasks.filter((t) => t.status === "done"))
      out += `- ${phase.title} / ${t.title}: ${t.summary || "done"}\n`;
  }
  out += `Read the schema with \`${TOOL_DESCRIBE}\` for every table there is.\n`;
  return out;
}

/** What the run that plans a phase's tasks is told: the phase, what exists already, the chosen
 * mockup and the admin's `note` (the plan it replaces, when it is planned again). */
function planTasksMessage(app, phase, phases, note, mockup) {
  const noted = note ? `${note.replace(/\s+$/, "")}\n\n` : "";
  const chosen = mockup
    ? `The application follows the mockup the admin chose. A \`data_model\` task makes the fields its screens show; a \`feature\` task's brief names the screens it builds by their files under \`mockups/\`, which the builder agent can read.\n\n${model.mockupText(
        mockup
      )}\n`
    : "";
  return (
    `The construction of \`${app.name}\` has reached the phase \`${
      phase.title
    }\` (id ${phase.id}).\n\n${
      phase.description
    }\n\nRequirements:\n${requirementLines(phase.requirements)}\n` +
    `${existsAlready(phases)}\n` +
    chosen +
    noted +
    "Plan this phase's tasks now with `save_tasks`, giving the phase id above: few tasks, each a sizeable piece of work, and none that only builds or checks. Only plan in this conversation: the tasks run afterwards."
  );
}

/** What a feature task is told about the chosen mockup: where its screens are, to read before
 * building them. A data-model task's brief carries the fields already. */
function mockupHint(task, mockup) {
  if (!mockup || task.kind !== "feature") return "";
  const files = mockup.screens.map(
    (s) => `\`mockups/${model.screenFile(s.name)}\` (${s.name})`
  );
  return `The admin chose a mockup of the application ("${
    mockup.title
  }"); its screens are in the application's files: ${files.join(
    ", "
  )}. Read the ones this task builds first, and match their layout, fields and look, turning their CSS into the application's own styles.\n\n`;
}

/** What the run of a task is told: its brief and what exists already, and for a feature task of
 * a construction with a chosen mockup, where its screens are. */
function taskMessage(app, task, phases, mockup) {
  const tables = tablesMade(phases);
  return (
    `A task of the construction of the \`${
      app.name
    }\` application (subdomain \`${trim(app.subdomain)}\`):\n\n## ${
      task.title
    }\n\n${task.brief}\n\n` +
    `Tables the construction made already: ${
      tables.length ? tables.join(", ") : "none yet"
    }. Read the schema with \`${TOOL_DESCRIBE}\` before you change it, and do not recreate what exists.\n\n` +
    mockupHint(task, mockup) +
    "Do this one task, then answer with a short summary of what you made."
  );
}

/** The tables and custom queries a task made, read from the tool results in its runs'
 * transcripts (oldest run first): each transcript the run's messages (`agents.messages`), or a
 * stored context `{ messages }`. Answers `[{ kind: "table" | "query", name }]`.
 *
 * A table counts when `edit_schema` created it for real (not in a dry run), and a query when
 * `save_api_query` saved it as new. Something the task later dropped or deleted is left out, and
 * a renamed table is listed under its new name. A retry's first run may have made a table the
 * retry then found there: it was still made by this task, which is why every run is read. */
function createdBy(transcripts) {
  const made = [];
  const add = (kind, name) => {
    if (!made.some((m) => m.kind === kind && m.name === name))
      made.push({ kind, name });
  };
  const drop = (kind, name) => {
    for (let i = made.length - 1; i >= 0; i--)
      if (made[i].kind === kind && made[i].name === name) made.splice(i, 1);
  };
  const names = (value) =>
    Array.isArray(value) ? value.filter((n) => typeof n === "string") : [];
  for (const transcript of transcripts || []) {
    const messages = Array.isArray(transcript)
      ? transcript
      : transcript && Array.isArray(transcript.messages)
      ? transcript.messages
      : null;
    if (!messages) continue;
    for (const message of messages) {
      if (!message || message.role !== "tool_result") continue;
      // A failed call's result is its error as text, not JSON: it made nothing.
      let result;
      try {
        result =
          typeof message.content === "string"
            ? JSON.parse(message.content)
            : null;
      } catch (_) {
        result = null;
      }
      if (!result || typeof result !== "object") continue;
      if (message.name === TOOL_EDIT) {
        if (result.applied !== true) continue;
        for (const name of names(result.tables_created)) add("table", name);
        for (const name of names(result.tables_dropped)) drop("table", name);
        for (const renamed of Array.isArray(result.tables_renamed)
          ? result.tables_renamed
          : []) {
          if (
            renamed &&
            typeof renamed.from === "string" &&
            typeof renamed.to === "string"
          ) {
            for (const m of made)
              if (m.kind === "table" && m.name === renamed.from)
                m.name = renamed.to;
          }
        }
      } else if (message.name === TOOL_SAVE_QUERY) {
        if (result.created === true && typeof result.saved === "string")
          add("query", result.saved);
      } else if (message.name === TOOL_DELETE_QUERY) {
        if (typeof result.deleted === "string") drop("query", result.deleted);
      }
    }
  }
  return made;
}

module.exports = {
  TOOL_DESCRIBE,
  specificationWith,
  planMessage,
  mockupMessage,
  questionsMessage,
  feedbackMessage,
  tablesMade,
  existsAlready,
  planTasksMessage,
  mockupHint,
  taskMessage,
  createdBy,
};
