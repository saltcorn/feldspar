// Starting runs from a route: claim the construction agent, start its run as the admin, and
// hand it to the runner; start a construction running; stop one no runner drives.

"use strict";

const model = require("../rules/model.js");
const store = require("../data/store.js");
const state = require("../agent/state.js");
const {
  startStep,
  startRun,
  existingRun,
  stopRunning,
  ConsoleLost,
} = require("../agent/runner.js");
const {
  CONSTRUCTOR_TRAIT,
  NEEDS_CONSOLE,
  useChosenProvider,
} = require("./agents.js");

const { invalid, notFound, sameId } = model;

/** Claim the agent for `activity`, refused while it is busy with this or another: checked and
 * marked at once. Answers the entry, completed by `launch`. */
function claim(construction, activity, extra = {}) {
  const k = state.key(construction.id);
  const current = state.busy.get(k);
  if (current) throw invalid(state.busyMessage(current));
  const entry = {
    activity,
    construction: construction.id,
    application: construction.application,
    run: null,
    ...extra,
  };
  state.busy.set(k, entry);
  return entry;
}

/** Give up a claim whose run never started. */
function unclaim(entry) {
  const k = state.key(entry.construction);
  if (state.busy.get(k) === entry) state.busy.delete(k);
}

/** Start a run of the construction agent told `message`, as the admin, for `entry` (claimed),
 * and hand it to the runner. Recorded as the construction's latest run once it has started, and
 * not before: a launch refused (no agent, no provider) leaves the current error as it was. */
async function launch(env, app, construction, entry, message, description) {
  let run = null;
  try {
    await useChosenProvider(env, app, construction);
    if (!construction.agent)
      throw invalid(
        "the construction has no agent; delete it and start it again"
      );
    const agent = await env.agents.get(construction.agent);
    if (!agent) throw notFound(`there is no agent \`${construction.agent}\``);
    if (!(agent.traits || []).some((t) => t.trait === CONSTRUCTOR_TRAIT)) {
      throw invalid(
        `agent \`${construction.agent}\` has no \`${CONSTRUCTOR_TRAIT}\` trait, so it cannot plan the construction; add it on the agent's page`
      );
    }
    try {
      run = await startRun(env, {
        agent: construction.agent,
        prompt: message,
        description,
      });
    } catch (e) {
      throw e instanceof ConsoleLost ? invalid(NEEDS_CONSOLE) : e;
    }
    const during = await store.insertConstructionRun(
      env.db,
      construction.id,
      entry.purpose,
      entry.feedback ?? null
    );
    await store.setAgentRun(env.db, during, run.id);
    // The runner starts the construction's later runs on behalf of this one.
    await store.setAuthorityRun(env.db, app.id, run.id);
    entry.run = run.id;
    entry.during = during;
  } catch (e) {
    // Started, but not recorded: nothing would follow it, so it is stopped.
    if (run) {
      try {
        await env.agents.abort(run.id);
      } catch (_) {
        // Ended meanwhile.
      }
    }
    unclaim(entry);
    throw e;
  }
  state.wake();
}

/** Start a construction running — up to `until`, or planning only that phase with `note` — with
 * its first step's run started here, as the admin, and hand it to the runner. */
async function beginRun(
  env,
  app,
  construction,
  { until = null, planOnly = false, note = null } = {}
) {
  const phases = await store.phasesWithTasks(env.db, construction.id);
  const next = model.nextStep(phases, until);
  // A task still running with its run: the runner follows that run, so none is started here.
  const resuming = next.kind === "run_task" && (await existingRun(env, next.task)) !== null;
  let first = null;
  if (
    !resuming &&
    (!planOnly || (next.kind === "plan_tasks" && sameId(next.phase.id, until)))
  ) {
    try {
      first = await startStep(env, construction, app, phases, next, {
        note: planOnly ? note : null,
      });
    } catch (e) {
      throw e instanceof ConsoleLost ? invalid(NEEDS_CONSOLE) : e;
    }
  }
  const user = env.user.id;
  const authority = first ? first.id : undefined;
  const marked = planOnly
    ? await store.markPlanning(env.db, app.id, user, until, note, authority)
    : await store.markRunning(env.db, app.id, user, until, authority);
  state.wanted.add(app.id);
  state.wake();
  return marked;
}

/** The owner of the entry `stopWithoutRunner` adds to `state.drivers` while it works. A route
 * can't be stopped the way a service can, so its signal is never aborted and `driverOf` always
 * counts the construction as driven. */
const STOPPING = { signal: { aborted: false } };

/** Stop a construction that no runner drives yet, such as one a restart left `running` before
 * the runner picked it up. Its running tasks go back to `todo` and their runs are aborted, just
 * as the runner does when it stops a construction.
 *
 * While this works, it adds an entry for the construction to `state.drivers`, so the
 * construction counts as driven and a runner scan won't start a driver for it in the meantime.
 * Such a driver would follow a run this function just aborted, and would mark its task as
 * failed. */
async function stopWithoutRunner(env, construction) {
  const k = state.key(construction.id);
  const slot = { stop: true, run: null, owner: STOPPING };
  state.drivers.set(k, slot);
  try {
    await stopRunning(env, construction);
    state.wanted.delete(construction.application);
    await store.setConstructionStatus(env.db, construction.application, "stopped");
  } finally {
    if (state.drivers.get(k) === slot) state.drivers.delete(k);
  }
}

module.exports = { claim, unclaim, launch, beginRun, stopWithoutRunner };
