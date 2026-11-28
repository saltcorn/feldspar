// What the routes and the runner share in this worker (TODO-app-constructor AC13.3, AC13.4):
// the Rust kept it in process-wide maps (`BUSY` in constructions.rs, `RUNNERS` in
// construction_runner.rs), and one worker runs this module, so module-level maps do the same.
//
// A route's asks stop working once it has answered, so a route never waits on a run: it starts
// the run, notes it here and answers; the service (`runner.js`) waits on it and finishes it.
// Nothing here is stored: a worker that restarts forgets it, as a server restart forgot `BUSY`.
// The runner picks running constructions up again from the tables; what a planning run was
// doing is lost with it, and the admin can start another (a run left `running` then reads idle).

"use strict";

const key = (id) => String(id);

/** What the construction agent is busy with, by construction id: one launch at a time, as the
 * phases are not edited under a plan run and a mockup run reads the answers. Each is
 * `{ activity, construction, application, run, during, purpose, round, known, feedback }`:
 * `activity` is `planning`, `designing`, `asking` or `feedback`, `run` the agent run, `during`
 * its `ac_runs` row, and the rest what the end of the run is judged by. The runner removes it
 * once it has recorded how the run ended. */
const busy = new Map();

/** Why another run, or an edit, has to wait while the agent is busy with `entry`. */
function busyMessage(entry) {
  const what = {
    planning: "planning the phases",
    designing: "proposing mockups",
    asking: "asking its questions",
    feedback: "planning feedback",
  }[entry.activity];
  return `the construction agent is ${what}; wait for it to finish`;
}

/** The constructions the runner drives, by id: `{ stop, run, owner }`. `stop` is set by the stop
 * route, `run` is the agent run it waits on (which the stop route aborts), and `owner` the
 * service that drives it — one whose signal is aborted has been stopped and no longer counts. */
const drivers = new Map();

/** The driver of construction `id`, if a live service drives it. */
function driverOf(id) {
  const slot = drivers.get(key(id));
  return slot && !slot.owner.signal.aborted ? slot : null;
}

/** The run a route started to plan a phase's tasks, by construction id: `{ phase, run }`. The
 * runner waits on it instead of starting a second one. */
const pendingPlans = new Map();

/** Constructions a route marked running, for the runner to drive, by id. */
const wanted = new Set();

let wakers = [];
/** Set when `wake` is called while the runner isn't sleeping (it's busy scanning). The next
 * `sleep` then returns at once, so the wake isn't lost. */
let woken = false;

/** Tell the runner there is work: a launch to follow or a construction to drive. */
function wake() {
  if (!wakers.length) {
    woken = true;
    return;
  }
  const waiting = wakers;
  wakers = [];
  for (const resolve of waiting) resolve();
}

/** Wait until `wake`, `ms` or `signal`, whichever comes first. Ends at once if a wake came while
 * no one was sleeping. */
function sleep(ms, signal) {
  return new Promise((resolve) => {
    if (woken || (signal && signal.aborted)) {
      woken = false;
      return resolve();
    }
    const done = () => {
      clearTimeout(timer);
      if (signal) signal.removeEventListener("abort", done);
      wakers = wakers.filter((w) => w !== done);
      resolve();
    };
    const timer = setTimeout(done, ms);
    if (signal) signal.addEventListener("abort", done, { once: true });
    wakers.push(done);
  });
}

/** Forget everything: for tests, which share one module between cases. */
function reset() {
  busy.clear();
  drivers.clear();
  pendingPlans.clear();
  wanted.clear();
  wakers = [];
  woken = false;
}

module.exports = {
  key,
  busy,
  busyMessage,
  drivers,
  driverOf,
  pendingPlans,
  wanted,
  wake,
  sleep,
  reset,
};
