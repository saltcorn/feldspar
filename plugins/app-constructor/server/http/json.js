// A construction as the screen reads it.

"use strict";

const model = require("../rules/model.js");
const store = require("../data/store.js");
const state = require("../agent/state.js");

const { sameId } = model;

const statusJson = (c) => ({
  id: c.id,
  application: c.application,
  status: c.status,
  agent: c.agent,
});

/** A task as the screen reads it: as the Rust serialized it, with its `uid`, which
 * `depends_on` names the tasks by (the screen matches it against `uid`). */
function taskJson(task) {
  return {
    id: task.id,
    uid: task.uid,
    phase: task.phase,
    position: task.position,
    kind: task.kind,
    title: task.title,
    brief: task.brief,
    depends_on: task.depends_on,
    status: task.status,
    runs: task.runs,
    summary: task.summary,
    created: task.created,
    feedback: task.feedback,
  };
}

/** A construction with its phases, each with its status and tasks, and what it gathered.
 *
 * What the agent is busy with is read first, and the construction after it: a run records its
 * outcome before it stops being busy, so a reader that sees it finished sees its outcome too. */
async function constructionJson(env, construction) {
  const db = env.db;
  const busy = state.busy.get(state.key(construction.id)) || null;
  const c =
    (await store.loadConstruction(db, construction.application)) ||
    construction;
  // The feedback before the phases: feedback planned in between reads as not planned yet,
  // never as tasks without their feedback.
  const feedback = await store.feedbackOf(db, c.id);
  const phases = await store.phasesWithTasks(db, c.id);
  const questions = await store.questionsOf(db, c.id);
  const mockups = await store.mockupsOf(db, c.id);
  const errors = await store.errorsOf(db, c.id);
  return {
    ...statusJson(c),
    specification: c.specification,
    started_by: c.started_by,
    plan_run: c.plan_run,
    planning: busy?.activity === "planning",
    error: c.error,
    until_phase: c.until_phase,
    plan_only: c.plan_only,
    designing: busy?.activity === "designing",
    asking: busy?.activity === "asking",
    questions: questions.map((q) => ({
      id: q.id,
      text: q.text,
      answer: q.answer,
    })),
    // Without their HTML, which the screens route answers.
    mockups: mockups.map((m) => ({
      id: m.id,
      round: m.round,
      title: m.title,
      rationale: m.rationale,
      screens: m.screens.map((s) => ({
        name: s.name,
        file: model.screenFile(s.name),
      })),
      status: m.status,
      feedback: m.feedback,
    })),
    feedback: feedback.map((f) => {
      const tasks = model.feedbackTasks(phases, f.id);
      // Given, then approved by starting the run that plans it.
      const status = tasks.length
        ? model.phaseStatus(tasks)
        : f.run === null
        ? "given"
        : "open";
      return {
        id: f.id,
        phase: f.phase,
        title: f.title,
        description: f.description,
        requirements: f.requirements,
        run: f.run,
        planning: busy?.activity === "feedback" && sameId(busy.feedback, f.id),
        status,
      };
    }),
    // Marked by id: the one the construction's latest run failed with.
    errors: errors.map((e) => ({
      ...e,
      current: c.error_entry !== null && sameId(c.error_entry, e.id),
    })),
    updated_at: c.updated_at,
    phases: phases.map(({ phase, tasks }) => ({
      id: phase.id,
      title: phase.title,
      description: phase.description,
      requirements: phase.requirements,
      status: model.phaseStatus(tasks),
      tasks: tasks.map(taskJson),
    })),
  };
}

module.exports = { statusJson, taskJson, constructionJson };
