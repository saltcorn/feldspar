// The module's tables (TODO-app-constructor AC13.1): one construction per application, its
// phases and their tasks, and what the construction gathered on the way — mockups, questions,
// feedback, the errors it met and the runs it started.
//
// Every module table gets an int `id` key of its own (crates/sc-module/src/tables.rs), so the
// links between them are `references` to it. They are foreign keys with no ON DELETE: a row is
// never left pointing at nothing, and so the store deletes children before their parents.
// What a log names but must outlive (an error's phase, task or run) is a plain column.
//
// A phase has no status column: its status follows from its tasks (`model.phaseStatus`), so the
// two cannot disagree. Likewise a construction's current error and planning run, and a
// feedback's run, are read from `ac_runs` and `ac_errors` rather than stored twice.

"use strict";

const tables = [
  {
    name: "ac_constructions",
    label: "Constructions",
    description:
      "The App Constructor's construction of one application each: its specification and where the run is.",
    fields: [
      {
        name: "application",
        type: "uuid",
        required: true,
        unique: true,
        label: "Application",
        description: "The application it builds; one construction each",
      },
      {
        name: "specification",
        type: "text",
        required: true,
        label: "Specification",
        description: "The application described in a few paragraphs",
      },
      {
        name: "status",
        type: "text",
        required: true,
        label: "Status",
        description: "draft, planned, running, stopped, done or failed",
      },
      {
        name: "agent",
        type: "text",
        label: "Agent",
        description: "The agent that plans and builds the data model, by name",
      },
      {
        name: "started_by",
        type: "uuid",
        label: "Started by",
        description: "The admin who last started it: the runner works for them",
      },
      {
        name: "authority_run",
        type: "text",
        label: "Authority run",
        description:
          "The agent run the admin's request started, on whose behalf the runner starts later runs",
      },
      {
        name: "until_phase",
        type: "int",
        label: "Until phase",
        description: "The phase a run stops after; empty runs them all",
      },
      {
        name: "plan_only",
        type: "bool",
        label: "Plan only",
        description: "The run only plans the tasks of the phase it stops after",
      },
      {
        name: "plan_note",
        type: "text",
        label: "Plan note",
        description: "What that planning run is told besides the phase",
      },
      { name: "updated_at", type: "timestamp", label: "Updated" },
    ],
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "ac_phases",
    label: "Construction phases",
    description:
      "A construction's phases, in position order. A phase's status follows from its tasks.",
    fields: [
      {
        name: "construction",
        references: "ac_constructions",
        required: true,
        label: "Construction",
      },
      { name: "position", type: "float", required: true, label: "Position" },
      { name: "title", type: "text", required: true, label: "Title" },
      {
        name: "description",
        type: "text",
        required: true,
        label: "Description",
      },
      {
        name: "requirements",
        type: "json",
        required: true,
        label: "Requirements",
        description:
          "What the phase must deliver: [{ text, priority: must|should|could }]",
      },
    ],
    indexes: [["construction"]],
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "ac_feedback",
    label: "Construction feedback",
    description:
      "The admin's feedback on what a phase built, planned as requirements and follow-up tasks once approved.",
    fields: [
      {
        name: "phase",
        references: "ac_phases",
        required: true,
        label: "Phase",
        description: "The phase it is about; the construction is the phase's",
      },
      { name: "position", type: "float", required: true, label: "Position" },
      { name: "title", type: "text", required: true, label: "Title" },
      {
        name: "description",
        type: "text",
        required: true,
        label: "Description",
      },
      {
        name: "requirements",
        type: "json",
        required: true,
        label: "Requirements",
        description: "What the feedback asks of the phase, once planned",
      },
    ],
    indexes: [["phase"]],
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "ac_tasks",
    label: "Construction tasks",
    description: "A phase's tasks, in position order, each run by an agent.",
    fields: [
      {
        name: "phase",
        references: "ac_phases",
        required: true,
        label: "Phase",
      },
      {
        name: "uid",
        type: "text",
        required: true,
        unique: true,
        label: "Uid",
        description:
          "Made when the task is planned, before it has an id: what depends_on names",
      },
      { name: "position", type: "float", required: true, label: "Position" },
      {
        name: "kind",
        type: "text",
        required: true,
        label: "Kind",
        description:
          "data_model (the construction agent) or feature (the builder agent)",
      },
      { name: "title", type: "text", required: true, label: "Title" },
      {
        name: "brief",
        type: "text",
        required: true,
        label: "Brief",
        description: "What to make, for the agent that runs the task",
      },
      {
        name: "depends_on",
        type: "json",
        required: true,
        label: "Depends on",
        description: "The uids of earlier tasks of the same plan",
      },
      {
        name: "status",
        type: "text",
        required: true,
        label: "Status",
        description: "todo, running, done, failed or skipped",
      },
      {
        name: "runs",
        type: "json",
        required: true,
        label: "Runs",
        description: "The agent run of each attempt, oldest first",
      },
      {
        name: "summary",
        type: "text",
        label: "Summary",
        description: "The closing summary of the last attempt",
      },
      {
        name: "created",
        type: "json",
        required: true,
        label: "Created",
        description: "The tables and queries its runs made",
      },
      {
        name: "feedback",
        references: "ac_feedback",
        label: "Feedback",
        description:
          "The feedback it was planned for; empty for the phase's own",
      },
    ],
    indexes: [["phase"]],
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "ac_mockups",
    label: "Construction mockups",
    description:
      "Screens of the application as self-contained HTML, proposed in rounds before the phases are planned; one at most is chosen.",
    fields: [
      {
        name: "construction",
        references: "ac_constructions",
        required: true,
        label: "Construction",
      },
      {
        name: "round",
        type: "int",
        required: true,
        label: "Round",
        description: "1 for the first proposal, one more per revision",
      },
      { name: "position", type: "float", required: true, label: "Position" },
      { name: "title", type: "text", required: true, label: "Title" },
      { name: "rationale", type: "text", required: true, label: "Rationale" },
      {
        name: "screens",
        type: "json",
        required: true,
        label: "Screens",
        description: "[{ name, html }]",
      },
      {
        name: "status",
        type: "text",
        required: true,
        label: "Status",
        description: "proposed or chosen",
      },
      {
        name: "feedback",
        type: "text",
        label: "Feedback",
        description: "What the admin said about it, for the next round",
      },
    ],
    indexes: [["construction"]],
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "ac_questions",
    label: "Construction questions",
    description:
      "What the construction agent asked about the specification, and the admin's answers.",
    fields: [
      {
        name: "construction",
        references: "ac_constructions",
        required: true,
        label: "Construction",
      },
      { name: "position", type: "float", required: true, label: "Position" },
      { name: "question", type: "text", required: true, label: "Question" },
      {
        name: "answer",
        type: "text",
        label: "Answer",
        description: "Empty until answered",
      },
    ],
    indexes: [["construction"]],
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "ac_runs",
    label: "Construction runs",
    description:
      "The runs a construction started: one per run of the construction agent and per session of the runner.",
    fields: [
      {
        name: "construction",
        references: "ac_constructions",
        required: true,
        label: "Construction",
      },
      {
        name: "purpose",
        type: "text",
        required: true,
        label: "Purpose",
        description: "phases, mockups, questions, feedback or build",
      },
      // No key: an agent run can be deleted on its own while this stays recorded.
      {
        name: "run",
        type: "text",
        label: "Agent run",
        description: "The agent run, once launched; none for a runner session",
      },
      // No key: feedback can be deleted (declined) while its run stays recorded.
      {
        name: "feedback",
        type: "int",
        label: "Feedback",
        description: "The feedback a feedback run plans",
      },
      {
        name: "started_at",
        type: "timestamp",
        required: true,
        label: "Started",
      },
    ],
    indexes: [["construction"]],
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "ac_errors",
    label: "Construction errors",
    description:
      "Every error a construction met, with what failed and the run it happened in.",
    fields: [
      {
        name: "construction",
        references: "ac_constructions",
        required: true,
        label: "Construction",
      },
      { name: "at", type: "timestamp", required: true, label: "At" },
      {
        name: "source",
        type: "text",
        required: true,
        label: "Source",
        description:
          "phases, tasks, task, mockups, questions, feedback or runner",
      },
      // No keys on what failed: the log outlives the phases, tasks, feedback and runs it names.
      { name: "phase", type: "int", label: "Phase" },
      { name: "task", type: "int", label: "Task" },
      { name: "feedback", type: "int", label: "Feedback" },
      { name: "run", type: "text", label: "Agent run" },
      { name: "message", type: "text", required: true, label: "Message" },
      {
        name: "construction_run",
        references: "ac_runs",
        label: "Construction run",
        description:
          "The construction run it happened in, whose current error it is",
      },
    ],
    indexes: [["construction"], ["construction_run"]],
    min_role_read: 1,
    min_role_write: 1,
  },
];

module.exports = { tables };
