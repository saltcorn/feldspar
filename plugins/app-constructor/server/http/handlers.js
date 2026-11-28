// The route handlers, and the table of which URL runs which (`HANDLERS`).

"use strict";

const model = require("../rules/model.js");
const store = require("../data/store.js");
const messages = require("../rules/messages.js");
const state = require("../agent/state.js");
const {
  requireApp,
  requireConstructible,
  chosenProvider,
  createConstructionAgent,
  useChosenProvider,
  deleteConstructionAgent,
} = require("./agents.js");
const {
  requireAgentFree,
  requireNoDriver,
  requireIdle,
  requirePlannable,
  requireFeedbackIdle,
  pathId,
  requireTask,
} = require("./checks.js");
const {
  claim,
  unclaim,
  launch,
  beginRun,
  stopWithoutRunner,
} = require("./launch.js");
const { statusJson, constructionJson } = require("./json.js");
const { bodyOf, text, optionalText, pairs } = require("./body.js");

const { invalid, notFound, sameId } = model;

/** Write `mockup`'s screens into `app`'s project as `mockups/<screen>.html`, where the builder
 * agent reads them, removing the screens of a mockup chosen before. */
async function writeMockupFiles(env, app, mockup) {
  const keep = new Set(mockup.screens.map((s) => model.screenFile(s.name)));
  for (const entry of await env.apps.files(app.id, "mockups")) {
    if (!entry.dir && entry.name.endsWith(".html") && !keep.has(entry.name))
      await env.apps.remove(app.id, entry.path);
  }
  for (const s of mockup.screens)
    await env.apps.write(app.id, `mockups/${model.screenFile(s.name)}`, s.html);
}

//
// Each is `(env, req)` → `{ status, body }`, where `env` is `{ db, agents, apps, user }`: what
// the route's `req.feldspar` and `req.user` are, so a test hands in fakes.

const ok = (body, status = 200) => ({ status, body });

async function listConstructions(env) {
  const out = [];
  for (const c of await store.listConstructions(env.db)) {
    const phases = await store.phasesWithTasks(env.db, c.id);
    out.push({
      ...statusJson(c),
      phases: phases.length,
      phases_done: phases.filter((p) => model.phaseStatus(p.tasks) === "done")
        .length,
    });
  }
  return ok(out);
}

async function getConstruction(env, req) {
  const app = await requireApp(env, req.params.app);
  return ok(
    await constructionJson(env, await store.requireConstruction(env.db, app.id))
  );
}

async function createConstruction(env, req) {
  const app = await requireApp(env, req.params.app);
  requireConstructible(app);
  if (await store.loadConstruction(env.db, app.id))
    throw invalid(`\`${app.name}\` already has a construction`);
  const body = bodyOf(req);
  const named =
    (typeof body.provider === "string" && body.provider.trim()) ||
    chosenProvider(app);
  const agent = await createConstructionAgent(env, app, named || null);
  const construction = await store.createConstruction(env.db, app.id, agent);
  return ok(await constructionJson(env, construction), 201);
}

async function saveSpecification(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.saveSpecification(
    env.db,
    app.id,
    text(bodyOf(req), "specification")
  );
  return ok(await constructionJson(env, construction));
}

async function plan(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await requirePlannable(env, app);
  const entry = claim(construction, "planning", { purpose: "phases" });
  let message;
  try {
    const chosen = await store.chosenMockup(env.db, construction.id);
    const replan = (await store.phasesOf(env.db, construction.id)).length > 0;
    const answers = model.answersText(
      await store.questionsOf(env.db, construction.id)
    );
    message = messages.planMessage(
      app,
      messages.specificationWith(construction.specification, answers),
      chosen,
      replan
    );
  } catch (e) {
    unclaim(entry);
    throw e;
  }
  await launch(
    env,
    app,
    construction,
    entry,
    message,
    `Plan the construction of ${app.name}`
  );
  return ok(await constructionJson(env, construction), 202);
}

async function savePhases(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  requireAgentFree(construction);
  const phases = model.parsePhases(bodyOf(req).phases ?? null);
  const saved = await store.savePhases(env.db, app.id, phases);
  return ok(await constructionJson(env, saved.construction));
}

async function run(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  requireAgentFree(construction);
  if (construction.status === "running")
    throw invalid("the construction is running already");
  const phases = await store.phasesOf(env.db, construction.id);
  if (!phases.length)
    throw invalid("the construction has no phases yet; plan them first");
  const raw = bodyOf(req).until_phase;
  let until = null;
  if (raw !== undefined && raw !== null) {
    if (!Number.isInteger(raw) && typeof raw !== "string")
      throw invalid(
        `\`until_phase\` must be a phase id, got ${JSON.stringify(raw)}`
      );
    const found = phases.find((p) => sameId(p.id, raw));
    if (!found) throw invalid(`\`${raw}\` is not a phase of this construction`);
    until = found.id;
  }
  requireNoDriver(construction);
  await useChosenProvider(env, app, construction);
  const marked = await beginRun(env, app, construction, { until });
  return ok(await constructionJson(env, marked), 202);
}

async function planPhaseTasks(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  requireAgentFree(construction);
  if (construction.status === "running")
    throw invalid(
      "the construction is running; stop it before you plan a phase's tasks"
    );
  requireNoDriver(construction);
  const phase = pathId(req.params.phase, "phase");
  const given = optionalText(bodyOf(req), "note");
  // Checked before anything changes; `clearTasks` checks again under the lock.
  const planned =
    model.requirePlannable(
      await store.phasesWithTasks(env.db, construction.id),
      phase
    ).tasks.length > 0;
  await useChosenProvider(env, app, construction);
  const previous = planned ? await store.clearTasks(env.db, app.id, phase) : [];
  const note = model.replanNote(previous, given);
  let marked;
  try {
    marked = await beginRun(env, app, construction, {
      until: phase,
      planOnly: true,
      note,
    });
  } catch (e) {
    // The planning run did not start: the phase keeps the tasks it had.
    await store.restoreTasks(env.db, app.id, phase, previous);
    throw e;
  }
  return ok(await constructionJson(env, marked), 202);
}

async function stop(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  if (construction.status !== "running")
    throw invalid("the construction is not running");
  // A driver here is asked to stop, and the run it waits on aborted at once: it puts the task
  // back and marks the construction stopped itself. With none, the construction was left
  // running by a restart the runner has not caught up with, and is stopped here.
  const slot = state.driverOf(construction.id);
  if (slot) {
    slot.stop = true;
    if (slot.run) {
      try {
        await env.agents.abort(slot.run);
      } catch (_) {
        // Ended meanwhile: the driver sees the stop before its next step.
      }
    }
  } else {
    await stopWithoutRunner(env, construction);
  }
  return ok(await constructionJson(env, construction));
}

/** Retry (`retry`) or skip a task; a construction the task failed carries on. */
async function changeTask(env, req, retry) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  // The runner writes the tasks while it runs, so a task is changed here only when it is not
  // running: neither by its status, nor by a runner still winding down after a stop.
  if (construction.status === "running")
    throw invalid(
      "the construction is running; stop it before you retry or skip a task"
    );
  requireNoDriver(construction);
  const task = await requireTask(env, construction, req.params.task);
  if (retry) {
    if (task.status !== "failed")
      throw invalid(
        `task \`${task.title}\` has not failed, so there is nothing to retry`
      );
    task.status = "todo";
    task.summary = null;
  } else {
    if (task.status !== "todo" && task.status !== "failed")
      throw invalid(
        `task \`${task.title}\` is not waiting or failed, so it cannot be skipped`
      );
    task.status = "skipped";
  }
  await store.updateTask(env.db, task);
  let current = construction;
  if (construction.status === "failed") {
    await useChosenProvider(env, app, construction);
    current = await beginRun(env, app, construction, {
      until: construction.until_phase,
    });
  }
  return ok(await constructionJson(env, current));
}

async function deleteConstruction(env, req) {
  const app = await requireApp(env, req.params.app);
  const existing = await store.loadConstruction(env.db, app.id);
  if (existing) {
    requireIdle(existing);
    requireNoDriver(existing);
  }
  const deleted = await store.deleteConstruction(env.db, app.id);
  const agent = deleted
    ? await deleteConstructionAgent(env.agents, app, deleted)
    : null;
  return ok({ deleted: deleted !== null, agent });
}

async function askQuestions(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await requirePlannable(env, app);
  const entry = claim(construction, "asking", { purpose: "questions" });
  let message;
  try {
    const questions = await store.questionsOf(env.db, construction.id);
    entry.known = questions.map((q) => q.id);
    message = messages.questionsMessage(
      app,
      construction.specification,
      questions
    );
  } catch (e) {
    unclaim(entry);
    throw e;
  }
  await launch(
    env,
    app,
    construction,
    entry,
    message,
    `Ask about the specification of ${app.name}`
  );
  return ok(await constructionJson(env, construction), 202);
}

async function answerQuestions(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  // The run replaces the open questions: an answer to one of them could land on a question
  // that is gone.
  const entry = state.busy.get(state.key(construction.id));
  if (entry && entry.activity === "asking")
    throw invalid(state.busyMessage(entry));
  const answers = pairs(
    bodyOf(req).answers,
    "question",
    "`answers` must be a list of `{question, text}`"
  );
  await store.answerQuestions(env.db, app.id, answers);
  return ok(await constructionJson(env, construction));
}

async function giveFeedback(env, req) {
  const app = await requireApp(env, req.params.app);
  // Recorded only, so not held back by a run: the runner and the agent leave feedback that is
  // not approved alone.
  const construction = await store.requireConstruction(env.db, app.id);
  const body = bodyOf(req);
  const phase = body.phase;
  if (!Number.isInteger(phase) && typeof phase !== "string")
    throw invalid("`phase` must be text");
  await store.addFeedback(
    env.db,
    app.id,
    phase,
    text(body, "title"),
    text(body, "description")
  );
  return ok(await constructionJson(env, construction), 201);
}

async function planFeedback(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await requireFeedbackIdle(env, app);
  const feedback = await store.requireFeedback(
    env.db,
    construction.id,
    pathId(req.params.feedback, "feedback")
  );
  const phases = await store.phasesWithTasks(env.db, construction.id);
  if (model.feedbackTasks(phases, feedback.id).length) {
    throw invalid(
      `the feedback \`${feedback.title}\` is planned already; delete it and give it again to plan it anew`
    );
  }
  const entry = claim(construction, "feedback", {
    purpose: "feedback",
    feedback: feedback.id,
  });
  let message;
  try {
    const phase = model.requireFeedbackPhase(phases, feedback.phase);
    message = messages.feedbackMessage(app, phase, feedback);
  } catch (e) {
    unclaim(entry);
    throw e;
  }
  await launch(
    env,
    app,
    construction,
    entry,
    message,
    `Plan feedback on ${app.name}: ${feedback.title}`
  );
  return ok(await constructionJson(env, construction), 202);
}

async function deleteFeedback(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  const feedback = await store.requireFeedback(
    env.db,
    construction.id,
    pathId(req.params.feedback, "feedback")
  );
  // Feedback not approved yet has no run and no tasks: declining it touches nothing a run uses.
  if (feedback.run !== null) await requireFeedbackIdle(env, app);
  await store.deleteFeedback(env.db, app.id, feedback.id);
  return ok(await constructionJson(env, construction));
}

async function proposeMockups(env, req) {
  const app = await requireApp(env, req.params.app);
  const body = bodyOf(req);
  let count = model.DEFAULT_MOCKUPS;
  if (body.count !== undefined && body.count !== null) {
    if (
      !Number.isInteger(body.count) ||
      body.count < 1 ||
      body.count > model.MAX_MOCKUPS
    ) {
      throw invalid(`\`count\` must be between 1 and ${model.MAX_MOCKUPS}`);
    }
    count = body.count;
  }
  const feedback =
    body.feedback === undefined || body.feedback === null
      ? []
      : pairs(
          body.feedback,
          "mockup",
          "`feedback` must be a list of `{mockup, text}`"
        );
  const note = typeof body.note === "string" ? body.note : null;
  const construction = await requirePlannable(env, app);
  const entry = claim(construction, "designing", { purpose: "mockups" });
  let message;
  try {
    // Stored first, so the screen shows the feedback while the run works.
    await store.setMockupFeedback(env.db, app.id, feedback);
    const mockups = await store.mockupsOf(env.db, construction.id);
    entry.round = mockups.reduce((m, x) => Math.max(m, x.round), 0);
    const answers = model.answersText(
      await store.questionsOf(env.db, construction.id)
    );
    message = messages.mockupMessage(
      app,
      messages.specificationWith(construction.specification, answers),
      count,
      mockups,
      note
    );
  } catch (e) {
    unclaim(entry);
    throw e;
  }
  const description = message.revision
    ? `Revise the mockups of ${app.name}`
    : `Propose mockups of ${app.name}`;
  await launch(env, app, construction, entry, message.text, description);
  return ok(await constructionJson(env, construction), 202);
}

async function mockupScreens(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  const raw = req.params.mockup;
  const mockup = (await store.mockupsOf(env.db, construction.id)).find((m) =>
    sameId(m.id, raw)
  );
  if (!mockup) throw notFound(`no mockup ${raw} in this construction`);
  return ok(
    mockup.screens.map((s) => ({
      name: s.name,
      file: model.screenFile(s.name),
      html: s.html,
    }))
  );
}

async function chooseMockup(env, req) {
  const app = await requireApp(env, req.params.app);
  const construction = await store.requireConstruction(env.db, app.id);
  requireAgentFree(construction);
  const chosen = await store.chooseMockup(
    env.db,
    app.id,
    pathId(req.params.mockup, "mockup")
  );
  await writeMockupFiles(env, app, chosen);
  return ok(await constructionJson(env, construction));
}

const BASE = "/applications/:app/construction";

/** Every route, as `[method, url, handler]`. */
const HANDLERS = [
  ["get", "/constructions", listConstructions],
  ["get", BASE, getConstruction],
  ["post", BASE, createConstruction],
  ["post", `${BASE}/specification`, saveSpecification],
  ["post", `${BASE}/plan`, plan],
  ["post", `${BASE}/phases`, savePhases],
  ["post", `${BASE}/run`, run],
  ["post", `${BASE}/phases/:phase/plan`, planPhaseTasks],
  ["post", `${BASE}/stop`, stop],
  [
    "post",
    `${BASE}/tasks/:task/retry`,
    (env, req) => changeTask(env, req, true),
  ],
  [
    "post",
    `${BASE}/tasks/:task/skip`,
    (env, req) => changeTask(env, req, false),
  ],
  ["post", `${BASE}/delete`, deleteConstruction],
  ["post", `${BASE}/questions`, askQuestions],
  ["post", `${BASE}/questions/answer`, answerQuestions],
  ["post", `${BASE}/feedback`, giveFeedback],
  ["post", `${BASE}/feedback/:feedback/plan`, planFeedback],
  ["post", `${BASE}/feedback/:feedback/delete`, deleteFeedback],
  ["post", `${BASE}/mockups`, proposeMockups],
  ["get", `${BASE}/mockups/:mockup/screens`, mockupScreens],
  ["post", `${BASE}/mockups/:mockup/choose`, chooseMockup],
];

module.exports = { HANDLERS };
