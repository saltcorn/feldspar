// The construction store (TODO-app-constructor AC13.1; ported from crates/sc-construct): every
// read and write of the module's tables (`tables.js`), over the `db` handle it is given — a
// route's, a service's or a hook's, which all reach the module's own tables.
//
// Two rules the Rust store kept with transactions and row locks, kept here another way:
// - **A change that reads before it writes holds its construction's lock** (`mutex.js`), so
//   nothing changes in between: a phase is never deleted while a task is being added to it, two
//   rounds of mockups saved at once are two rounds. Reads alone take no lock.
// - **A change of several rows is one `db.atomically`**, so it lands whole or not at all. That
//   can only write, so the lock is what makes the reads before it still true when it lands.
//
// There is no ON DELETE on the keys between the tables, so a delete takes the children first.
// Rows are read back as plain objects with the columns' names (`model.js` lists the shapes);
// "newest first" is by id, which numbers itself in insert order and never ties.

"use strict";

const model = require("../rules/model.js");
const { keyedMutex } = require("./mutex.js");

const { invalid, notFound, sameId } = model;

const locks = keyedMutex();

/** Run `fn` holding construction `id`'s lock. Not re-entrant: `fn` must not call another
 * function of this file that takes it. */
const forConstruction = (id, fn) => locks.run(`construction:${id}`, fn);

const now = () => new Date().toISOString();
/** An optional text column: absent, null and empty are all "none". */
const optText = (value) =>
  value === null || value === undefined || value === "" ? null : value;
const opt = (value) => (value === undefined ? null : value);

// --- rows ---------------------------------------------------------------------------------

function constructionFromRow(row) {
  return {
    id: row.id,
    application: row.application,
    specification: row.specification,
    status: row.status,
    agent: optText(row.agent),
    started_by: opt(row.started_by),
    authority_run: optText(row.authority_run),
    // From its runs (`withRuns`).
    plan_run: null,
    error: null,
    error_entry: null,
    until_phase: opt(row.until_phase),
    plan_only: row.plan_only === true,
    plan_note: optText(row.plan_note),
    updated_at: opt(row.updated_at),
  };
}

function phaseFromRow(row) {
  return {
    id: row.id,
    construction: row.construction,
    position: Number(row.position),
    title: row.title,
    description: row.description,
    requirements: row.requirements || [],
  };
}

function taskFromRow(row) {
  return {
    id: row.id,
    uid: row.uid,
    phase: row.phase,
    position: Number(row.position),
    kind: row.kind,
    title: row.title,
    brief: row.brief,
    depends_on: row.depends_on || [],
    status: row.status,
    runs: row.runs || [],
    summary: optText(row.summary),
    created: row.created || [],
    feedback: opt(row.feedback),
  };
}

function mockupFromRow(row) {
  return {
    id: row.id,
    construction: row.construction,
    round: Number(row.round),
    position: Number(row.position),
    title: row.title,
    rationale: row.rationale,
    screens: row.screens || [],
    status: row.status,
    feedback: optText(row.feedback),
  };
}

function questionFromRow(row) {
  return {
    id: row.id,
    construction: row.construction,
    position: Number(row.position),
    text: row.question,
    answer: optText(row.answer),
  };
}

function feedbackFromRow(row) {
  return {
    id: row.id,
    phase: row.phase,
    position: Number(row.position),
    title: row.title,
    description: row.description,
    requirements: row.requirements || [],
    // From its construction runs (`feedbackOf`).
    run: null,
  };
}

function errorFromRow(row) {
  return {
    id: row.id,
    construction: row.construction,
    at: row.at,
    source: row.source,
    phase: opt(row.phase),
    task: opt(row.task),
    feedback: opt(row.feedback),
    run: optText(row.run),
    construction_run: opt(row.construction_run),
    message: row.message,
  };
}

function runFromRow(row) {
  return {
    id: row.id,
    construction: row.construction,
    purpose: row.purpose,
    run: optText(row.run),
    feedback: opt(row.feedback),
    started_at: row.started_at,
  };
}

/** The columns of a new task, `uid` made here when the task has none. */
function taskRow(task) {
  return {
    phase: task.phase,
    uid: task.uid || globalThis.crypto.randomUUID(),
    position: task.position,
    kind: task.kind,
    title: task.title,
    brief: task.brief,
    depends_on: task.depends_on || [],
    status: task.status || "todo",
    runs: task.runs || [],
    summary: opt(task.summary),
    created: task.created || [],
    feedback: opt(task.feedback),
  };
}

function errorRow(construction, failure) {
  return {
    construction,
    at: now(),
    source: failure.source,
    phase: opt(failure.phase),
    task: opt(failure.task),
    feedback: opt(failure.feedback),
    run: opt(failure.run),
    message: failure.message,
    construction_run: opt(failure.construction_run),
  };
}

// --- constructions ------------------------------------------------------------------------

const alreadyHasOne = () =>
  invalid("this application already has a construction");

/** Start the construction of `application`, as a draft with no specification. An application
 * has one construction at most. */
async function createConstruction(db, application, agent = null) {
  // Keyed by the application, as there is no construction yet to key it by. The unique column
  // stops what the lock cannot see (an insert from elsewhere), told in the same sentence.
  return locks.run(`application:${application}`, async () => {
    if (await loadConstruction(db, application)) throw alreadyHasOne();
    try {
      await db.ac_constructions.insert({
        application,
        specification: "",
        status: "draft",
        agent: opt(agent),
        started_by: null,
        plan_only: false,
        updated_at: now(),
      });
    } catch (e) {
      if (await loadConstruction(db, application)) throw alreadyHasOne();
      throw e;
    }
    return requireConstruction(db, application);
  });
}

/** The construction of `application`, if it has one, with what its runs say: its current
 * error (`error`, `error_entry`) and the agent run that planned its phases last (`plan_run`). */
async function loadConstruction(db, application) {
  const row = await db.ac_constructions.where({ application }).first();
  return row ? withRuns(db, constructionFromRow(row)) : null;
}

async function withRuns(db, construction) {
  const entry = await currentError(db, construction.id);
  if (entry) {
    construction.error = entry.message;
    construction.error_entry = entry.id;
  }
  construction.plan_run = await latestAgentRun(db, construction.id, "phases");
  return construction;
}

/** `loadConstruction`, or a not-found refusal. */
async function requireConstruction(db, application) {
  const construction = await loadConstruction(db, application);
  if (!construction) throw notFound("this application has no construction");
  return construction;
}

/** Every construction, oldest first. */
async function listConstructions(db) {
  const rows = await db.ac_constructions.orderBy("id", "asc").rows();
  const out = [];
  for (const row of rows)
    out.push(await withRuns(db, constructionFromRow(row)));
  return out;
}

/** The constructions whose status is `running`: what the runner resumes when it starts. */
async function runningConstructions(db) {
  return (await listConstructions(db)).filter((c) => c.status === "running");
}

/** Run `fn(construction)` holding the lock of `application`'s construction, with the row as it
 * is once the lock is held: a change that waited may find it deleted. */
async function withConstruction(db, application, fn) {
  const construction = await requireConstruction(db, application);
  return forConstruction(construction.id, async () => {
    const row = await db.ac_constructions.get(construction.id);
    if (!row) throw notFound("the construction was deleted meanwhile");
    return fn(constructionFromRow(row));
  });
}

/** Replace the specification. Not while it runs: a running construction is building from the
 * one it has. */
async function saveSpecification(db, application, specification) {
  await withConstruction(db, application, async (construction) => {
    if (construction.status === "running")
      throw invalid(
        "the construction is running; stop it before changing its specification"
      );
    await db.ac_constructions
      .where({ id: construction.id })
      .update({ specification, updated_at: now() });
  });
  return requireConstruction(db, application);
}

/** Set construction `id`'s columns as `values` say, and log `failure` when there is one, in one
 * write. */
async function updateLogging(db, id, values, failure) {
  await db.atomically((db) => {
    db.ac_constructions.where({ id }).update({ ...values, updated_at: now() });
    if (failure) db.ac_errors.insert(errorRow(id, failure));
  });
}

/** Log why a run of the construction agent failed (planning the phases, mockups, questions or
 * feedback), leaving the status as it is: a failed plan changes nothing else. It is the current
 * error while `failure.construction_run` is the construction's latest run. */
async function setPlanError(db, application, failure) {
  const construction = await requireConstruction(db, application);
  await updateLogging(db, construction.id, {}, failure);
  return requireConstruction(db, application);
}

/** Mark `application`'s construction running for `user`, up to phase `untilPhase` (all when
 * `null`), and start a runner session (a `build` run), which has no error yet — that is what
 * clears the last one. Only the rows: the runner that does the work is started by its caller.
 * `authorityRun`, when given, is the agent run the runner's later runs are started on behalf of. */
async function markRunning(
  db,
  application,
  user,
  untilPhase = null,
  authorityRun = undefined
) {
  return markStarted(
    db,
    application,
    {
      started_by: user,
      until_phase: opt(untilPhase),
      plan_only: false,
      plan_note: null,
    },
    authorityRun
  );
}

/** Mark `application`'s construction running for `user` to plan the tasks of `phase` only, the
 * planning run told `note` besides the phase. The runner stops once they are planned, for the
 * admin to review before they run. A runner session, as `markRunning`'s. */
async function markPlanning(
  db,
  application,
  user,
  phase,
  note = null,
  authorityRun = undefined
) {
  return markStarted(
    db,
    application,
    {
      started_by: user,
      until_phase: phase,
      plan_only: true,
      plan_note: optText(note),
    },
    authorityRun
  );
}

async function markStarted(db, application, values, authorityRun) {
  const construction = await requireConstruction(db, application);
  const set = { ...values, status: "running", updated_at: now() };
  if (authorityRun !== undefined) set.authority_run = opt(authorityRun);
  await db.atomically((db) => {
    db.ac_constructions.where({ id: construction.id }).update(set);
    db.ac_runs.insert({
      construction: construction.id,
      purpose: "build",
      run: null,
      feedback: null,
      started_at: now(),
    });
  });
  return requireConstruction(db, application);
}

/** Record the agent run the admin's request started, on whose behalf the runner starts the
 * construction's later runs (a service can start a run only for an earlier module run). */
async function setAuthorityRun(db, application, run) {
  const construction = await requireConstruction(db, application);
  await db.ac_constructions
    .where({ id: construction.id })
    .update({ authority_run: opt(run), updated_at: now() });
  return requireConstruction(db, application);
}

/** Set the status of `application`'s construction, logging `failure` when it stopped or failed.
 * `null` logs nothing and clears nothing, as starting a run is what clears the current error. */
async function setConstructionStatus(db, application, status, failure = null) {
  if (!model.CONSTRUCTION_STATUSES.includes(status))
    throw invalid(`\`${status}\` is not a valid construction status`);
  const construction = await requireConstruction(db, application);
  await updateLogging(db, construction.id, { status }, failure);
  return requireConstruction(db, application);
}

/** Delete `application`'s construction with its phases, tasks, mockups, questions, feedback,
 * errors and runs, answering what it was; `null` when it had none. One write, so a failure
 * deletes nothing, under the lock, so a task being added is either deleted with its phase or
 * refused (`insertTask`). */
async function deleteConstruction(db, application) {
  const found = await db.ac_constructions.where({ application }).first();
  if (!found) return null;
  return forConstruction(found.id, async () => {
    const row = await db.ac_constructions.get(found.id);
    if (!row) return null;
    const id = row.id;
    const phases = (
      await db.ac_phases.where({ construction: id }).select("id").rows()
    ).map((p) => p.id);
    await db.atomically((db) => {
      // Children first: the keys have no ON DELETE. Tasks point at feedback, errors at runs.
      if (phases.length) {
        db.ac_tasks.where({ phase: { in: phases } }).delete();
        db.ac_feedback.where({ phase: { in: phases } }).delete();
      }
      db.ac_mockups.where({ construction: id }).delete();
      db.ac_questions.where({ construction: id }).delete();
      db.ac_errors.where({ construction: id }).delete();
      db.ac_runs.where({ construction: id }).delete();
      db.ac_phases.where({ construction: id }).delete();
      db.ac_constructions.where({ id }).delete();
    });
    return constructionFromRow(row);
  });
}

// --- phases -------------------------------------------------------------------------------

/** Add `phase` (`{ construction, position, title, description, requirements }`) to its
 * construction, answering it as stored. */
async function insertPhase(db, phase) {
  const row = await db.ac_phases.insert({
    construction: phase.construction,
    position: phase.position,
    title: phase.title,
    description: phase.description || "",
    requirements: phase.requirements || [],
  });
  return phaseFromRow(row);
}

/** A construction's phases, in order. */
async function phasesOf(db, construction) {
  return (
    await db.ac_phases.where({ construction }).orderBy("position", "asc").rows()
  ).map(phaseFromRow);
}

/** A phase's tasks, in order. */
async function tasksOfPhase(db, phase) {
  return (
    await db.ac_tasks.where({ phase }).orderBy("position", "asc").rows()
  ).map(taskFromRow);
}

/** A construction's phases, in order, each with its tasks in order — `{ phase, tasks }`: what a
 * phase's status is read from (`model.phaseStatus`). */
async function phasesWithTasks(db, construction) {
  const phases = await phasesOf(db, construction);
  if (!phases.length) return [];
  const tasks = (
    await db.ac_tasks
      .where({ phase: { in: phases.map((p) => p.id) } })
      .orderBy("position", "asc")
      .rows()
  ).map(taskFromRow);
  return phases.map((phase) => ({
    phase,
    tasks: tasks.filter((t) => sameId(t.phase, phase.id)),
  }));
}

/** Delete a phase that has no tasks, answering whether it existed. One with tasks is refused:
 * its tasks record what was built for it. */
async function deletePhase(db, id) {
  const found = await db.ac_phases.get(id);
  if (!found) return false;
  return forConstruction(found.construction, async () => {
    if (!(await db.ac_phases.get(id))) return false;
    if ((await db.ac_tasks.where({ phase: found.id }).count()) > 0)
      throw invalid("this phase has tasks, so it cannot be deleted");
    await db.ac_phases.where({ id: found.id }).delete();
    return true;
  });
}

/** Replace the phases of `application`'s construction with `inputs` (`model.parsePhases`), in
 * their order, answering `{ construction, phases (with tasks), kept }`:
 * - a phase with an `id` updates that phase, and its tasks stay;
 * - a phase without one is new;
 * - an existing phase the list leaves out is deleted, unless it has tasks: then it is kept,
 *   placed before the list's phases (it was reached first), and `kept` names it, so work that
 *   started is never lost.
 * A construction with phases is `planned`; the list may not be empty. Refused while it runs. */
async function savePhases(db, application, inputs) {
  const refuseRunning = (c) => {
    if (c.status === "running")
      throw invalid(
        "the construction is running; stop it before changing its phases"
      );
  };
  refuseRunning(await requireConstruction(db, application));
  const kept = await withConstruction(db, application, async (construction) => {
    refuseRunning(construction);
    const existing = await phasesOf(db, construction.id);
    const list = model.validatePhases(inputs, existing);
    const dropped = existing.filter(
      (p) => !list.some((i) => sameId(i.id, p.id))
    );
    const withTasks = new Set();
    if (dropped.length) {
      const rows = await db.ac_tasks
        .where({ phase: { in: dropped.map((p) => p.id) } })
        .select("phase")
        .rows();
      for (const r of rows) withTasks.add(String(r.phase));
    }
    const keep = dropped.filter((p) => withTasks.has(String(p.id)));
    const remove = dropped.filter((p) => !withTasks.has(String(p.id)));
    await db.atomically((db) => {
      for (const p of remove) db.ac_phases.where({ id: p.id }).delete();
      let position = 0;
      for (const p of keep)
        db.ac_phases.where({ id: p.id }).update({ position: ++position });
      for (const input of list) {
        position += 1;
        const values = {
          position,
          title: input.title,
          description: input.description,
          requirements: input.requirements,
        };
        if (input.id !== null)
          db.ac_phases.where({ id: input.id }).update(values);
        else db.ac_phases.insert({ construction: construction.id, ...values });
      }
      const set = { updated_at: now() };
      if (construction.status === "draft") set.status = "planned";
      db.ac_constructions.where({ id: construction.id }).update(set);
    });
    return keep;
  });
  const construction = await requireConstruction(db, application);
  return {
    construction,
    phases: await phasesWithTasks(db, construction.id),
    kept: kept.map((p) => p.title),
  };
}

// --- tasks --------------------------------------------------------------------------------

/** Add `task` (`model.newTask`'s shape) to its phase, answering it as stored. Refused when the
 * phase does not exist — it may have been deleted from the plan. */
async function insertTask(db, task) {
  const gone = () =>
    invalid(
      `phase ${task.phase} does not exist; it may have been deleted from the plan`
    );
  const phase = await db.ac_phases.get(task.phase);
  if (!phase) throw gone();
  // Under the lock a save of the phases that would delete it takes too: it either sees this
  // task, and keeps the phase, or has deleted the phase, and this is refused.
  return forConstruction(phase.construction, async () => {
    if (!(await db.ac_phases.get(task.phase))) throw gone();
    return taskFromRow(await db.ac_tasks.insert(taskRow(task)));
  });
}

/** Write what the runner changes on a task: its status, its runs, its summary and what it
 * created. */
async function updateTask(db, task) {
  await db.ac_tasks.where({ id: task.id }).update({
    status: task.status,
    runs: task.runs || [],
    summary: opt(task.summary),
    created: task.created || [],
  });
}

/** The task with `id`, or `null`. */
async function loadTask(db, id) {
  const row = await db.ac_tasks.get(id);
  return row ? taskFromRow(row) : null;
}

/** Replace the tasks of `phase` (an id), a phase of `application`'s construction, with `inputs`
 * (`model.parseTasks`), in order, answering them as stored.
 *
 * Refused when one of the phase's tasks has started: what ran stays recorded. Refused too while
 * the construction runs and the phase has tasks already: the runner may have picked one, and
 * would run a task that no longer exists. Planning a phase with none, which the runner does when
 * it reaches one, is what a running construction allows. An invalid list changes nothing. */
async function saveTasks(db, application, phase, inputs) {
  return withConstruction(db, application, async (construction) => {
    const target = (await phasesOf(db, construction.id)).find((p) =>
      sameId(p.id, phase)
    );
    if (!target)
      throw invalid(`phase ${phase} is not a phase of this construction`);
    const existing = await tasksOfPhase(db, target.id);
    if (existing.some((t) => t.status !== "todo"))
      throw invalid(
        `phase \`${target.title}\` has started, so its tasks cannot be planned again`
      );
    if (construction.status === "running" && existing.length) {
      throw invalid(
        `the construction is running and phase \`${target.title}\` has its tasks already; stop it to plan them again`
      );
    }
    const tasks = model.buildTasks(target.id, inputs);
    const answers = await db.atomically((db) => {
      db.ac_tasks.where({ phase: target.id }).delete();
      for (const t of tasks) db.ac_tasks.insert(taskRow(t));
    });
    return answers.slice(1).map(taskFromRow);
  });
}

/** Delete the tasks of `phase` (an id), a phase of `application`'s construction, so they can be
 * planned again, answering the deleted tasks. Checked with `model.requirePlannable` under the
 * lock, so a task that started in between is never deleted. */
async function clearTasks(db, application, phase) {
  return withConstruction(db, application, async (construction) => {
    const target = model.requirePlannable(
      await phasesWithTasks(db, construction.id),
      phase
    );
    await db.ac_tasks.where({ phase: target.phase.id }).delete();
    return target.tasks;
  });
}

/** Put back `tasks`, which `clearTasks` deleted from `phase`, as they were (same uids, so their
 * `depends_on` still holds): for a replan that could not start. Only while the phase has no tasks
 * again, so nothing planned in between is overwritten. */
async function restoreTasks(db, application, phase, tasks) {
  return withConstruction(db, application, async () => {
    if ((await tasksOfPhase(db, phase)).length || !tasks.length) return;
    await db.atomically((db) => {
      for (const t of tasks) db.ac_tasks.insert(taskRow(t));
    });
  });
}

// --- mockups ------------------------------------------------------------------------------

/** Save `mockups` (`model.parseMockups`) as the next round of `application`'s construction,
 * answering them as stored. One write, so a round is saved whole, under the lock, so two saves
 * at once take two rounds rather than the same one. */
async function saveMockups(db, application, mockups) {
  return withConstruction(db, application, async (construction) => {
    const rounds = await db.ac_mockups
      .where({ construction: construction.id })
      .select("round")
      .rows();
    const round = rounds.reduce((m, r) => Math.max(m, Number(r.round)), 0) + 1;
    const answers = await db.atomically((db) => {
      mockups.forEach((m, i) => {
        db.ac_mockups.insert({
          construction: construction.id,
          round,
          position: i + 1,
          title: m.title,
          rationale: m.rationale || "",
          screens: m.screens,
          status: "proposed",
          feedback: null,
        });
      });
    });
    return answers.map(mockupFromRow);
  });
}

/** A construction's mockups, oldest round first and in order within it. */
async function mockupsOf(db, construction) {
  return (
    await db.ac_mockups
      .where({ construction })
      .orderBy("round", "asc")
      .orderBy("position", "asc")
      .rows()
  ).map(mockupFromRow);
}

/** The chosen mockup of a construction, or `null`. */
async function chosenMockup(db, construction) {
  return (
    (await mockupsOf(db, construction)).find((m) => m.status === "chosen") ||
    null
  );
}

/** Record the admin's feedback on mockups of `application`'s construction — `[{ mockup, text }]`
 * — blank text clearing it. A mockup of another construction is refused, before anything is
 * saved. */
async function setMockupFeedback(db, application, feedback) {
  await withConstruction(db, application, async (construction) => {
    const mockups = await mockupsOf(db, construction.id);
    const unknown = feedback.find(
      (f) => !mockups.some((m) => sameId(m.id, f.mockup))
    );
    if (unknown)
      throw notFound(`no mockup ${unknown.mockup} in this construction`);
    await db.atomically((db) => {
      for (const f of feedback) {
        const id = mockups.find((m) => sameId(m.id, f.mockup)).id;
        db.ac_mockups
          .where({ id })
          .update({ feedback: optText((f.text || "").trim()) });
      }
    });
  });
}

/** Choose `mockup` (an id) for `application`'s construction, answering it. Any other chosen one
 * goes back to proposed, in the same write: one is chosen at most. */
async function chooseMockup(db, application, mockup) {
  return withConstruction(db, application, async (construction) => {
    const chosen = (await mockupsOf(db, construction.id)).find((m) =>
      sameId(m.id, mockup)
    );
    if (!chosen) throw notFound(`no mockup ${mockup} in this construction`);
    await db.atomically((db) => {
      db.ac_mockups
        .where({ construction: construction.id })
        .update({ status: "proposed" });
      db.ac_mockups.where({ id: chosen.id }).update({ status: "chosen" });
    });
    return { ...chosen, status: "chosen" };
  });
}

// --- questions ----------------------------------------------------------------------------

/** Save `questions` (`model.parseQuestions`) for `application`'s construction, answering
 * `{ questions, discarded_mockups }`: the questions as they now stand — the answered ones first,
 * as they were, then the new ones — and how many mockups were discarded.
 *
 * Asking again replaces the open questions and keeps the answered ones, so nothing the admin
 * wrote is lost. It discards every mockup, the chosen one too: they were drawn without what is
 * asked now. One write under the lock, so an answer saved meanwhile is either kept or its
 * question replaced, and a round of mockups saved meanwhile is either discarded or saved after. */
async function saveQuestions(db, application, questions) {
  return withConstruction(db, application, async (construction) => {
    const id = construction.id;
    const answered = (await questionsOf(db, id)).filter(
      (q) => q.answer !== null
    );
    const discarded = await db.ac_mockups.where({ construction: id }).count();
    const last = answered.reduce((m, q) => Math.max(m, q.position), 0);
    const answers = await db.atomically((db) => {
      db.ac_questions
        .where({ construction: id })
        .where({ answer: { is_null: true } })
        .delete();
      db.ac_mockups.where({ construction: id }).delete();
      questions.forEach((text, i) => {
        db.ac_questions.insert({
          construction: id,
          position: last + i + 1,
          question: text,
          answer: null,
        });
      });
    });
    return {
      questions: [...answered, ...answers.slice(2).map(questionFromRow)],
      discarded_mockups: discarded,
    };
  });
}

/** A construction's questions, in order. */
async function questionsOf(db, construction) {
  return (
    await db.ac_questions
      .where({ construction })
      .orderBy("position", "asc")
      .rows()
  ).map(questionFromRow);
}

/** Record the admin's answers to questions of `application`'s construction —
 * `[{ question, text }]` — blank text leaving a question open. A question of another
 * construction is refused, before anything is saved. */
async function answerQuestions(db, application, answers) {
  await withConstruction(db, application, async (construction) => {
    const questions = await questionsOf(db, construction.id);
    const unknown = answers.find(
      (a) => !questions.some((q) => sameId(q.id, a.question))
    );
    if (unknown)
      throw notFound(`no question ${unknown.question} in this construction`);
    await db.atomically((db) => {
      for (const a of answers) {
        const id = questions.find((q) => sameId(q.id, a.question)).id;
        db.ac_questions
          .where({ id })
          .update({ answer: optText((a.text || "").trim()) });
      }
    });
  });
}

// --- feedback -----------------------------------------------------------------------------

/** Record feedback on `phase` (an id) of `application`'s construction, which must be able to
 * take it (`model.requireFeedbackPhase`); not approved yet. Giving it starts nothing. */
async function addFeedback(db, application, phase, title, description) {
  title = (title || "").trim();
  if (!title) throw invalid("the feedback has no title");
  if ([...title].length > model.MAX_FEEDBACK_TITLE) {
    throw invalid(
      `the feedback's title is longer than ${model.MAX_FEEDBACK_TITLE} characters; put the detail in its description`
    );
  }
  description = (description || "").trim();
  if (!description)
    throw invalid("the feedback has no description; say what should change");
  return withConstruction(db, application, async (construction) => {
    const phases = await phasesWithTasks(db, construction.id);
    const target = model.requireFeedbackPhase(phases, phase);
    const given = await feedbackRows(
      db,
      phases.map((p) => p.phase.id)
    );
    const last = given.reduce((m, f) => Math.max(m, f.position), 0);
    const row = await db.ac_feedback.insert({
      phase: target.phase.id,
      position: last + 1,
      title,
      description,
      requirements: [],
    });
    return feedbackFromRow(row);
  });
}

async function feedbackRows(db, phases) {
  if (!phases.length) return [];
  return (
    await db.ac_feedback
      .where({ phase: { in: phases } })
      .orderBy("position", "asc")
      .rows()
  ).map(feedbackFromRow);
}

/** A construction's feedback, in the order it was given, each with `run`: the agent run that
 * planned it last, `null` until the admin approves it. */
async function feedbackOf(db, construction) {
  const phases = (
    await db.ac_phases.where({ construction }).select("id").rows()
  ).map((p) => p.id);
  const feedback = await feedbackRows(db, phases);
  if (!feedback.length) return feedback;
  const runs = await constructionRunsFor(db, construction, "feedback");
  for (const f of feedback) {
    const planned = runs.find(
      (r) => sameId(r.feedback, f.id) && r.run !== null
    );
    f.run = planned ? planned.run : null;
  }
  return feedback;
}

/** Feedback `id` of `construction`, or a not-found refusal. */
async function requireFeedback(db, construction, id) {
  const found = (await feedbackOf(db, construction)).find((f) =>
    sameId(f.id, id)
  );
  if (!found) throw notFound(`no feedback ${id} in this construction`);
  return found;
}

/** Save the plan (`model.parseFeedbackPlan`) for `feedback` (an id) of `application`'s
 * construction: its requirements, and its tasks after the phase's last one, as `todo`, naming
 * the feedback; the runner reaches them first, as they belong to the earliest unfinished phase.
 * Answers the tasks as stored. Refused when the feedback is planned already or its phase can no
 * longer take feedback. One write, so an invalid plan changes nothing. */
async function planFeedback(db, application, feedback, plan) {
  const construction = await requireConstruction(db, application);
  const found = await requireFeedback(db, construction.id, feedback);
  return withConstruction(db, application, async (construction) => {
    const phases = await phasesWithTasks(db, construction.id);
    if (model.feedbackTasks(phases, found.id).length)
      throw invalid(`the feedback \`${found.title}\` is planned already`);
    const target = model.requireFeedbackPhase(phases, found.phase);
    const last = target.tasks.reduce((m, t) => Math.max(m, t.position), 0);
    const tasks = model.buildTasks(target.phase.id, plan.tasks);
    const answers = await db.atomically((db) => {
      for (const t of tasks)
        db.ac_tasks.insert(
          taskRow({ ...t, position: t.position + last, feedback: found.id })
        );
      db.ac_feedback
        .where({ id: found.id })
        .update({ requirements: plan.requirements });
    });
    return answers.slice(0, tasks.length).map(taskFromRow);
  });
}

/** Delete `feedback` (an id) of `application`'s construction with its tasks, which declines it
 * when it is not approved. Refused once one of its tasks has started: what ran stays recorded. */
async function deleteFeedback(db, application, feedback) {
  const construction = await requireConstruction(db, application);
  const found = await requireFeedback(db, construction.id, feedback);
  await withConstruction(db, application, async (construction) => {
    const started = model
      .feedbackTasks(await phasesWithTasks(db, construction.id), found.id)
      .find((t) => t.status !== "todo" || t.runs.length > 0);
    if (started)
      throw invalid(
        `task \`${started.title}\` of this feedback has started, so the feedback stays`
      );
    await db.atomically((db) => {
      db.ac_tasks.where({ feedback: found.id }).delete();
      db.ac_feedback.where({ id: found.id }).delete();
    });
  });
}

// --- errors -------------------------------------------------------------------------------

/** The errors `construction` met, newest first, at most `MAX_ERRORS_SHOWN`. */
async function errorsOf(db, construction) {
  return (
    await db.ac_errors
      .where({ construction })
      .orderBy("id", "desc")
      .limit(model.MAX_ERRORS_SHOWN)
      .rows()
  ).map(errorFromRow);
}

/** The construction's current error: the newest one logged for its latest construction run, if
 * that run has one. A new run has none until it fails, which is what clears it. */
async function currentError(db, construction) {
  const latest = await latestConstructionRun(db, construction);
  if (!latest) return null;
  const row = await db.ac_errors
    .where({ construction_run: latest.id })
    .orderBy("id", "desc")
    .first();
  return row ? errorFromRow(row) : null;
}

// --- construction runs --------------------------------------------------------------------

/** Record a run of `construction` for `purpose` (one of `RUN_PURPOSES`), starting now, answering
 * its id. Being the latest, it leaves the construction with no current error. */
async function insertConstructionRun(
  db,
  construction,
  purpose,
  feedback = null
) {
  if (!model.RUN_PURPOSES.includes(purpose))
    throw invalid(`\`${purpose}\` is not a construction run purpose`);
  const row = await db.ac_runs.insert({
    construction,
    purpose,
    run: null,
    feedback: opt(feedback),
    started_at: now(),
  });
  return row.id;
}

/** Record the agent run that construction run `id` launched. */
async function setAgentRun(db, id, agentRun) {
  await db.ac_runs.where({ id }).update({ run: agentRun });
}

/** The construction's newest run, or `null`. */
async function latestConstructionRun(db, construction) {
  const row = await db.ac_runs
    .where({ construction })
    .orderBy("id", "desc")
    .first();
  return row ? runFromRow(row) : null;
}

/** The runs `construction` started for `purpose`, newest first. */
async function constructionRunsFor(db, construction, purpose) {
  return (
    await db.ac_runs
      .where({ construction, purpose })
      .orderBy("id", "desc")
      .rows()
  ).map(runFromRow);
}

/** The agent run that `construction`'s newest launched run for `purpose` started, or `null`. */
async function latestAgentRun(db, construction, purpose) {
  const row = await db.ac_runs
    .where({ construction, purpose })
    .where({ run: { is_null: false } })
    .orderBy("id", "desc")
    .first();
  return row ? optText(row.run) : null;
}

/** The runner session the running `construction` is in: its newest `build` run, or one started
 * now when it has none. */
async function currentBuild(db, construction) {
  const row = await db.ac_runs
    .where({ construction, purpose: "build" })
    .orderBy("id", "desc")
    .first();
  return row ? row.id : insertConstructionRun(db, construction, "build");
}

module.exports = {
  createConstruction,
  loadConstruction,
  requireConstruction,
  listConstructions,
  runningConstructions,
  saveSpecification,
  setPlanError,
  markRunning,
  markPlanning,
  setAuthorityRun,
  setConstructionStatus,
  deleteConstruction,
  insertPhase,
  phasesOf,
  tasksOfPhase,
  phasesWithTasks,
  deletePhase,
  savePhases,
  insertTask,
  updateTask,
  loadTask,
  saveTasks,
  clearTasks,
  restoreTasks,
  saveMockups,
  mockupsOf,
  chosenMockup,
  setMockupFeedback,
  chooseMockup,
  saveQuestions,
  questionsOf,
  answerQuestions,
  addFeedback,
  feedbackOf,
  requireFeedback,
  planFeedback,
  deleteFeedback,
  errorsOf,
  currentError,
  insertConstructionRun,
  setAgentRun,
  latestConstructionRun,
  constructionRunsFor,
  latestAgentRun,
  currentBuild,
};
