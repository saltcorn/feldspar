// The checks a handler runs before it changes a construction: is the agent free, is the
// runner idle, does the path name a row of this construction.

"use strict";

const store = require("../data/store.js");
const state = require("../agent/state.js");
const { invalid, notFound, sameId } = require("../rules/model.js");

/** Refuse while the construction agent plans the phases, proposes mockups, asks its questions
 * or plans feedback. */
function requireAgentFree(construction) {
  const entry = state.busy.get(state.key(construction.id));
  if (entry) throw invalid(state.busyMessage(entry));
}

/** Refuse while the runner drives the construction, even one that is stopping: a task the
 * runner writes is never written here too. */
function requireNoDriver(construction) {
  if (state.driverOf(construction.id))
    throw invalid(
      "the construction is still running or stopping; stop it, or try again in a moment"
    );
}

/** Refuse while the agent plans `construction` or the runner runs it. */
function requireIdle(construction) {
  requireAgentFree(construction);
  if (construction.status === "running")
    throw invalid("the construction is running; stop it first");
}

/** The construction of `app`, refused unless the agent can plan it now: not running, and with a
 * specification. */
async function requirePlannable(env, app) {
  const construction = await store.requireConstruction(env.db, app.id);
  if (construction.status === "running")
    throw invalid(
      "the construction is running; stop it before planning it again"
    );
  if (!construction.specification.trim())
    throw invalid(
      "the construction has no specification yet; describe the application first"
    );
  return construction;
}

/** The construction of `app`, refused unless its approved feedback can change now: it neither
 * runs nor is being run down, and the agent is free. */
async function requireFeedbackIdle(env, app) {
  const construction = await store.requireConstruction(env.db, app.id);
  requireAgentFree(construction);
  if (construction.status === "running")
    throw invalid(
      "the construction is running; stop it before you change its feedback"
    );
  requireNoDriver(construction);
  return construction;
}

/** A row id from a path: a malformed one is not found, as an id of no row is. */
function pathId(raw, what) {
  if (!/^\d+$/.test(String(raw)))
    throw notFound(`no ${what} ${raw} in this construction`);
  return Number(raw);
}

/** The task `raw` of `construction`, or not found. */
async function requireTask(env, construction, raw) {
  const id = pathId(raw, "task");
  const task = await store.loadTask(env.db, id);
  const phases = await store.phasesOf(env.db, construction.id);
  if (!task || !phases.some((p) => sameId(p.id, task.phase)))
    throw notFound(`no task ${raw} in this construction`);
  return task;
}

module.exports = {
  requireAgentFree,
  requireNoDriver,
  requireIdle,
  requirePlannable,
  requireFeedbackIdle,
  pathId,
  requireTask,
};
