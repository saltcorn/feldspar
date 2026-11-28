// The runner: the module's service `runner` (TODO-app-constructor AC13.4). It does two jobs.
//
// **1. Follows runs a route started.** A route can't wait for an agent run, so it marks the
// construction as busy (`state.js`) and returns. The runner waits for the run, checks it saved
// what it was asked to, logs a failure, and marks the agent free again.
//
// **2. Runs constructions.** For each construction with status `running`, a driver does the next
// step (`model.nextStep`) until none is left:
// - a phase without tasks: the construction agent plans them;
// - a `data_model` task: the construction agent runs it;
// - a `feature` task: the builder agent runs it, then the app is built. It's done only if the
//   build succeeds.
// A failed task fails the construction. When all is done it's `done`, or `planned` if it was
// only asked to go up to a phase.
//
// **As the admin.** A service has no user, so its runs act for the admin whose request started
// the construction (via `authority_run`). If that person is no longer an admin, the construction
// stops.
//
// **Never two runs for one task.** A task's run is saved on the task before waiting. After a
// restart the runner picks up that run instead of starting a new one.
//
// **Stopping.** The stop route aborts the current run; the driver puts the task back to `todo`
// and marks the construction `stopped`. When the service itself stops, everything is left as is
// and continues on the next start.
//
// Every wait is at most a minute per call (`agents.wait`, `apps.waitBuild`), repeated in a loop.

"use strict";

const model = require("../rules/model.js");
const store = require("../data/store.js");
const messages = require("../rules/messages.js");
const state = require("./state.js");

const { failure, sameId, invalid } = model;

/** How long one wait holds its call: the most the server allows. */
const WAIT_MS = 60000;
/** How often the service looks for running constructions nothing drives, besides being woken. */
const SCAN_MS = 30000;

/** A run started for the construction came back without the console's authority: the admin it
 * acts for is no longer an administrator, or the module was not granted `agents: console`. */
class ConsoleLost extends Error {
  constructor(message) {
    super(message);
    this.name = "ConsoleLost";
  }
}

const NOT_ADMIN = "the admin who started it is no longer an admin";
const GONE = "the admin who started it no longer exists";

/** The construction agent's name. */
function constructionAgent(construction) {
  if (!construction.agent)
    throw invalid("the construction has no agent; delete it and start again");
  return construction.agent;
}

/** Start a run with `opts` (on behalf of `onBehalfOf` when given, as a service must), refused —
 * and the run aborted — unless it has the console's authority, which the data-model tools and
 * the planning tools need. */
async function startRun(env, opts, onBehalfOf) {
  const run = await env.agents.start(
    onBehalfOf ? { ...opts, on_behalf_of: onBehalfOf } : opts
  );
  if (run.authority !== "console") {
    try {
      await env.agents.abort(run.id);
    } catch (_) {
      // Refused either way; the run not stopping is not what the admin needs to read.
    }
    throw new ConsoleLost(NOT_ADMIN);
  }
  return run;
}

/** The run a `running` task already has, if it still exists: that run is followed, never a
 * second one started. `null` otherwise. */
async function existingRun(env, task) {
  if (task.status !== "running" || !task.runs.length) return null;
  const last = task.runs[task.runs.length - 1];
  try {
    return (await env.agents.run(last)) ? last : null;
  } catch (_) {
    // Gone (deleted, or not this module's): a new attempt is started.
    return null;
  }
}

/** Undo the work a stopped construction still has under way. Its running tasks go back to `todo`
 * and their runs are aborted, and so is a plan run a route started for it. Used when a
 * construction is stopped before a driver has started any run of its own, by the stop route or
 * by the driver itself, which would otherwise leave a task `running` with its run still live. */
async function stopRunning(env, construction) {
  const abortRun = async (id) => {
    try {
      const run = await env.agents.run(id);
      if (run && run.live) await env.agents.abort(id);
    } catch (_) {
      // A run deleted since, or not this module's, is not running for it.
    }
  };
  for (const { tasks } of await store.phasesWithTasks(env.db, construction.id)) {
    for (const task of tasks.filter((t) => t.status === "running")) {
      if (task.runs.length) await abortRun(task.runs[task.runs.length - 1]);
      task.status = "todo";
      await store.updateTask(env.db, task);
    }
  }
  const k = state.key(construction.id);
  const pending = state.pendingPlans.get(k);
  if (pending) {
    state.pendingPlans.delete(k);
    await abortRun(pending.run);
  }
}

/** Start the run of the step `next` (`model.nextStep`'s) of `construction`, answering it, or
 * `null` for a step that needs no run. A plan run is noted in `state.pendingPlans`; a task's run
 * is recorded on the task, which is `running` from now on. Used by the runner, on behalf of the
 * construction's authority run, and by the routes that start a construction, as the admin. */
async function startStep(
  env,
  construction,
  app,
  phases,
  next,
  { note = null, onBehalfOf = null } = {}
) {
  if (next.kind === "plan_tasks") {
    const mockup = await store.chosenMockup(env.db, construction.id);
    const run = await startRun(
      env,
      {
        agent: constructionAgent(construction),
        prompt: messages.planTasksMessage(
          app,
          next.phase,
          phases,
          note,
          mockup
        ),
        description: `Plan the tasks of \`${next.phase.title}\``,
      },
      onBehalfOf
    );
    state.pendingPlans.set(state.key(construction.id), {
      phase: next.phase.id,
      run: run.id,
    });
    return run;
  }
  if (next.kind === "run_task") {
    const task = next.task;
    const agent =
      task.kind === "data_model"
        ? constructionAgent(construction)
        : app.builder_agent;
    const mockup = await store.chosenMockup(env.db, construction.id);
    const run = await startRun(
      env,
      {
        agent,
        prompt: messages.taskMessage(app, task, phases, mockup),
        description: `Construct: ${task.title}`,
      },
      onBehalfOf
    );
    task.runs = [...task.runs, run.id];
    task.status = "running";
    await store.updateTask(env.db, task);
    return run;
  }
  return null;
}

/** How often `followRun` drives one run on before it gives up. Once is normal (after a restart);
 * a run that keeps stopping without ending would otherwise be driven on for ever. */
const MAX_DRIVES = 3;

/** Follow run `id` until it has ended, driving it on when nothing drives it (a restart left it),
 * answering it as it ended — or `null` once `stopping()` says stop, the run then aborted. */
async function followRun(env, id, stopping = () => false) {
  let drives = 0;
  for (;;) {
    let run = await env.agents.wait(id, { timeout_ms: WAIT_MS });
    if (env.signal && env.signal.aborted)
      throw new Error("the service is stopping");
    if (stopping()) {
      if (run.live) run = await env.agents.abort(id);
      return null;
    }
    // A run waiting on something (a person, a timer) has stopped being driven: it ended as far
    // as the construction can tell, with no answer.
    if (!run.live || (run.state !== "running" && !run.driven)) return run;
    if (!run.driven) {
      drives += 1;
      if (drives > MAX_DRIVES)
        throw new Error(
          `the run ${id} keeps stopping without ending (driven on ${MAX_DRIVES} times)`
        );
      const driven = await env.agents.drive(id);
      if (driven.live && !driven.driven)
        throw new Error(
          `the run ${id} is not being driven and could not be driven on`
        );
    }
  }
}

/** Build `app` and wait for the build to finish, answering it. */
async function buildApp(env, app) {
  let build = await env.apps.build(app.id);
  while (build && build.state === "running") {
    if (env.signal && env.signal.aborted)
      throw new Error("the service is stopping");
    build = await env.apps.waitBuild(app.id, { timeout_ms: WAIT_MS });
  }
  return build;
}

/** What a launch is about, as its failures say it. */
const LAUNCH_WHAT = {
  phases: "the phases",
  mockups: "the mockups",
  questions: "its questions",
  feedback: "the feedback's plan",
};

/** Why the launch `entry` failed, now its run has ended as `run`, or `null`. An answered run must
 * have saved what it was asked to: phases, a round of mockups after the last, a question not
 * asked before, or tasks for the feedback. */
async function launchFailure(env, entry, run) {
  const what = LAUNCH_WHAT[entry.purpose];
  if (run.state === "failed")
    return `planning ${what} failed: ${run.error || "the run failed"}`;
  if (run.conclusion === "max_steps") {
    return `the construction agent ran out of steps before it saved ${what}; raise its step limit on the agent's page, or shorten the specification`;
  }
  if (run.conclusion !== "answered") return null;
  const db = env.db;
  if (entry.purpose === "phases") {
    if ((await store.phasesOf(db, entry.construction)).length === 0)
      return "the construction agent finished without saving any phases; see its run";
  } else if (entry.purpose === "mockups") {
    if (
      (await store.mockupsOf(db, entry.construction)).every(
        (m) => m.round <= entry.round
      )
    ) {
      return "the construction agent finished without saving any mockups; see its run";
    }
  } else if (entry.purpose === "questions") {
    const known = (entry.known || []).map(String);
    if (
      (await store.questionsOf(db, entry.construction)).every((q) =>
        known.includes(String(q.id))
      )
    ) {
      return "the construction agent finished without asking any questions; see its run";
    }
  } else if (entry.purpose === "feedback") {
    if (
      model.feedbackTasks(
        await store.phasesWithTasks(db, entry.construction),
        entry.feedback
      ).length === 0
    ) {
      return "the construction agent finished without planning the feedback; see its run";
    }
  }
  return null;
}

/** The runner of one service start, over `ctx` (`db`, `agents`, `apps`, `signal`, `log`). */
function createRunner(ctx) {
  const env = {
    db: ctx.db,
    agents: ctx.agents,
    apps: ctx.apps,
    signal: ctx.signal,
  };
  const owner = { signal: ctx.signal };
  const log = ctx.log || { error: () => {}, warn: () => {} };
  const aborted = () => ctx.signal.aborted;
  /** The launches this runner follows, so a scan does not follow one twice. */
  const following = new Set();
  /** The drivers this runner has going, for a test to await. */
  const going = new Set();

  /** Follow the launch `entry` to its end, record a failure, and free the agent. Left busy when
   * the service stops meanwhile, for the next start to follow it on. */
  async function followLaunch(entry) {
    following.add(entry);
    try {
      const run = await followRun(env, entry.run);
      const why = await launchFailure(env, entry, run);
      if (why) {
        await store.setPlanError(
          env.db,
          entry.application,
          failure(entry.purpose, why, {
            run: entry.run,
            construction_run: entry.during,
            feedback: entry.feedback ?? null,
          })
        );
      }
    } catch (e) {
      if (aborted()) return;
      log.error(
        `app-constructor: following the run ${entry.run} failed: ${e.message}`
      );
      // Shown on the construction, as a run that ended badly is.
      try {
        await store.setPlanError(
          env.db,
          entry.application,
          failure(entry.purpose, e.message, {
            run: entry.run,
            construction_run: entry.during,
            feedback: entry.feedback ?? null,
          })
        );
      } catch (_) {
        // Deleted meanwhile, most likely: nothing is left to mark.
      }
    } finally {
      following.delete(entry);
      if (!aborted() && state.busy.get(state.key(entry.construction)) === entry)
        state.busy.delete(state.key(entry.construction));
    }
  }

  /** Drive the running `construction` until it is no longer running. One driver per
   * construction: a second call while a live one drives it does nothing. */
  async function drive(construction) {
    const k = state.key(construction.id);
    if (state.driverOf(construction.id)) return;
    const slot = { stop: false, run: null, owner };
    state.drivers.set(k, slot);
    const application = construction.application;
    const release = () => {
      if (state.drivers.get(k) === slot) state.drivers.delete(k);
    };
    // The runner session its errors belong to (the `build` run marking it running started),
    // which makes them the construction's current error.
    let session = null;
    // The step under way, as an error of it is logged: what a step that throws is recorded as.
    let step = null;
    // The status is written before the place is given up: a construction that still reads
    // `running` then never has no driver behind it, and a scan never starts a second one.
    const finish = async (status, fail = null) => {
      await store.setConstructionStatus(
        env.db,
        application,
        status,
        fail ? { ...fail, construction_run: session } : null
      );
      release();
      return status;
    };
    // Only the stop route stops a run: a service that stops leaves it to the server, which goes
    // on driving it, for the next start to follow.
    const stopping = () => slot.stop;
    try {
      try {
        session = await store.currentBuild(env.db, construction.id);
      } catch (e) {
        log.error(
          `app-constructor: finding a construction's runner session failed: ${e.message}`
        );
      }
      while (!aborted()) {
        step = null;
        const c = await store.loadConstruction(env.db, application);
        if (!c || c.status !== "running") return null;
        if (slot.stop) {
          // Stopped before this driver started a run, so the stop route had none to abort.
          // A task a restart left running still has its run, which is stopped here.
          await stopRunning(env, c);
          return await finish("stopped");
        }
        const app = await env.apps.get(application);
        if (!app)
          throw new Error("the application of this construction is gone");
        const phases = await store.phasesWithTasks(env.db, c.id);
        const next = model.nextStep(phases, c.until_phase);
        // Planning only: plan that one phase, and stop before anything runs.
        if (
          c.plan_only &&
          !(next.kind === "plan_tasks" && sameId(next.phase.id, c.until_phase))
        )
          return await finish("planned");
        let outcome;
        if (next.kind === "finished") {
          return await finish(
            phases.every((p) => model.phaseStatus(p.tasks) === "done")
              ? "done"
              : "planned"
          );
        } else if (next.kind === "failed") {
          const t = next.task;
          const reason = `task \`${t.title}\` failed: ${
            t.summary || "no reason was recorded"
          }`;
          return await finish(
            "failed",
            failure("task", reason, {
              phase: t.phase,
              task: t.id,
              run: t.runs[t.runs.length - 1] ?? null,
            })
          );
        } else if (next.kind === "blocked") {
          const reason = `no task of phase \`${next.phase.title}\` can start: each waits on another that is not done`;
          return await finish(
            "failed",
            failure("runner", reason, { phase: next.phase.id })
          );
        } else if (next.kind === "plan_tasks") {
          step = failure("tasks", "", { phase: next.phase.id });
          outcome = await planTasks(slot, c, app, next, phases, step, stopping);
        } else {
          step = failure("task", "", {
            phase: next.task.phase,
            task: next.task.id,
          });
          outcome = await runTask(slot, c, app, next, phases, step, stopping);
        }
        if (aborted()) return null;
        if (outcome === "stopped") return await finish("stopped");
        if (outcome && outcome.lost)
          return await finish("stopped", failure("runner", outcome.lost));
      }
      // The service is stopping: the construction stays `running`, for the next start.
      return null;
    } catch (e) {
      if (aborted()) return null;
      log.error(
        `app-constructor: a construction's runner failed: ${e.message}`
      );
      const fail = step
        ? { ...step, message: e.message }
        : failure("runner", e.message);
      try {
        return await finish("failed", fail);
      } catch (_) {
        // Deleted meanwhile, most likely: nothing is left to mark.
        return null;
      }
    } finally {
      slot.run = null;
      release();
    }
  }

  /** Start the run of `next` on behalf of the construction's admin, or say why it cannot be. */
  async function startOnBehalf(c, app, next, phases, note) {
    if (!c.authority_run)
      return { lost: "nobody is recorded as having started it" };
    try {
      return {
        run: await startStep(env, c, app, phases, next, {
          note,
          onBehalfOf: c.authority_run,
        }),
      };
    } catch (e) {
      if (e instanceof ConsoleLost) return { lost: e.message };
      // The run it would act for names nobody who is still a user.
      if (/acts for nobody/.test(e.message)) return { lost: GONE };
      throw e;
    }
  }

  /** Plan the phase `next.phase`'s tasks with a run of the construction agent — the one a route
   * started, when there is one. A run that saves none fails the construction. */
  async function planTasks(slot, c, app, next, phases, step, stopping) {
    const k = state.key(c.id);
    const pending = state.pendingPlans.get(k);
    let id;
    if (pending && sameId(pending.phase, next.phase.id)) {
      id = pending.run;
    } else {
      const started = await startOnBehalf(
        c,
        app,
        next,
        phases,
        c.plan_only ? c.plan_note : null
      );
      if (started.lost) return started;
      id = started.run.id;
    }
    step.run = id;
    slot.run = id;
    let run;
    try {
      run = await followRun(env, id, stopping);
    } finally {
      slot.run = null;
      if (
        state.pendingPlans.get(k) &&
        sameId(state.pendingPlans.get(k).run, id)
      )
        state.pendingPlans.delete(k);
    }
    if (!run) return "stopped";
    if (run.state === "failed")
      throw new Error(run.error || "the run that plans the tasks failed");
    if ((await store.tasksOfPhase(env.db, next.phase.id)).length === 0) {
      throw invalid(
        `the construction agent planned no tasks for phase \`${
          next.phase.title
        }\` (${run.conclusion || "ended"}); its run is ${id}`
      );
    }
    return "continue";
  }

  /** Run the task `next.task` to an end: done, failed, or back to `todo` when stopped. A task
   * still `running` was interrupted (or its run started by a route): its last run is followed on. */
  async function runTask(slot, c, app, next, phases, step, stopping) {
    let task = next.task;
    let id = await existingRun(env, task);
    if (!id) {
      const started = await startOnBehalf(c, app, next, phases, null);
      if (started.lost) return started;
      id = started.run.id;
      task = next.task;
    }
    step.run = id;
    slot.run = id;
    let run;
    try {
      run = await followRun(env, id, stopping);
    } finally {
      slot.run = null;
    }
    if (!run) {
      if (aborted()) return "stopped";
      task.status = "todo";
      await store.updateTask(env.db, task);
      return "stopped";
    }
    const failed = async (reason) => {
      task.status = "failed";
      task.summary = reason;
      await store.updateTask(env.db, task);
      return "continue";
    };
    if (run.state === "failed") return failed(run.error || "its run failed");
    const answer = typeof run.answer === "string" ? run.answer.trim() : "";
    if (!answer)
      return failed(
        `its run ended without a summary (${run.conclusion || "ended"})`
      );
    if (task.kind === "feature") {
      let build;
      try {
        build = await buildApp(env, app);
      } catch (e) {
        if (aborted()) throw e;
        return failed(`the application does not build: ${e.message}`);
      }
      if (!build || build.state !== "succeeded")
        return failed(
          `the application does not build: ${
            (build && build.error) || "the build failed"
          }`
        );
    }
    // Every attempt's transcript, not only the last: a retried task's first run may have made a
    // table the retry then found there, and it was still made by this task.
    const transcripts = [];
    for (const each of task.runs) {
      try {
        transcripts.push(await env.agents.messages(each));
      } catch (_) {
        // A run deleted since made nothing anyone can tell.
      }
    }
    task.created = messages.createdBy(transcripts);
    task.summary = answer;
    task.status = "done";
    await store.updateTask(env.db, task);
    return "continue";
  }

  /** Start a driver for every running construction nothing drives, and follow every launch
   * nothing follows. */
  async function scan({ all = false } = {}) {
    for (const entry of state.busy.values()) {
      // No run yet: the route is still starting it, and wakes the runner once it has.
      if (entry.run === null) continue;
      if (!following.has(entry)) followLaunch(entry);
    }
    const ids = [...state.wanted];
    state.wanted.clear();
    let running = [];
    if (all) running = await store.runningConstructions(env.db);
    for (const application of ids) {
      if (running.some((c) => sameId(c.application, application))) continue;
      const c = await store.loadConstruction(env.db, application);
      if (c && c.status === "running") running.push(c);
    }
    for (const c of running) {
      if (state.driverOf(c.id)) continue;
      const p = drive(c);
      going.add(p);
      p.finally(() => going.delete(p));
    }
  }

  /** The service's loop: scan when woken, and every `SCAN_MS` for what a restart left. */
  async function loop() {
    let last = 0;
    while (!aborted()) {
      const all = Date.now() - last >= SCAN_MS;
      if (all) last = Date.now();
      try {
        await scan({ all });
      } catch (e) {
        if (!aborted())
          log.error(`app-constructor: looking for work failed: ${e.message}`);
      }
      await state.sleep(SCAN_MS, ctx.signal);
    }
  }

  /** Until every driver this runner has going has ended: for tests. */
  async function settled() {
    while (going.size || following.size) {
      await Promise.allSettled([...going]);
      await new Promise((resolve) => setImmediate(resolve));
    }
  }

  return { loop, scan, drive, followLaunch, settled };
}

/** The service's `start`: the loop runs on its own, and the stop it answers waits for it. */
function start(ctx) {
  const runner = createRunner(ctx);
  const looping = runner.loop();
  return async () => {
    state.wake();
    await looping;
  };
}

module.exports = {
  start,
  createRunner,
  startStep,
  startRun,
  existingRun,
  stopRunning,
  followRun,
  ConsoleLost,
  constructionAgent,
  WAIT_MS,
};
