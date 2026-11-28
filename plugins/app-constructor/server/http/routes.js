// The Plan & build screen's routes (TODO-app-constructor AC13.3; ported from
// crates/sc-server/src/constructions.rs): an application's construction, created with the agent
// that plans it and builds its data model, read with its phases and tasks, planned, run, stopped
// and deleted. Served under `/_feldspar/routes/app-constructor`, GET for reads and POST for
// every change (module routes answer nothing else), each answering the JSON the Rust handler
// answered, so the screen ports with its calls renamed.
//
// **Admins only.** Every route but the screen's script answers 403 to anyone else: a
// construction decides what agents build with the console's authority.
//
// **Runs are started here, followed by the service.** A route's asks end when it answers, so a
// route that starts a run of the construction agent (`launch`) records it as the construction's
// `busy` entry and answers 202; the runner (`runner.js`) waits on it and records how it ended.
// A route that starts a construction running starts its first step's run itself, as the admin:
// the runner, which has no caller, starts every later run on behalf of that one
// (`authority_run`).
//
// Errors are `{ error }`: a refusal (`ConstructError`) is 400, or 404 for something that is not
// there, as the Rust's `invalid` and `not_found`; anything else the server refused is 400 with
// its words.

"use strict";

const store = require("../data/store.js");
const state = require("../agent/state.js");
const {
  CONSTRUCTOR_TRAIT,
  CONSTRUCTOR_PROVIDER,
  constructionAgentName,
  deleteConstructionAgent,
} = require("./agents.js");
const { constructionJson } = require("./json.js");
const { HANDLERS } = require("./handlers.js");

const ADMIN_ROLE = 1;

/** The status a failure is answered with. */
function statusOf(e) {
  if (e && e.name === "ConstructError")
    return e.kind === "not_found" ? 404 : 400;
  if (e && Number.isInteger(e.status)) return e.status;
  return 400;
}

/** What a route's `req` lends a handler. */
const envOf = (req) => ({
  db: req.feldspar.db,
  agents: req.feldspar.agents,
  apps: req.feldspar.apps,
  user: req.user,
});

/** Run `handler` for an admin's request and answer what it answers, or `{ error }`. */
async function respond(req, res, handler) {
  try {
    if (!req.user || Number(req.user.role_id) !== ADMIN_ROLE) {
      return res.status(403).json({
        error: "only an administrator may plan and build applications",
      });
    }
    const { status, body } = await handler(envOf(req), req);
    return res.status(status).json(body);
  } catch (e) {
    return res
      .status(statusOf(e))
      .json({ error: (e && e.message) || String(e) });
  }
}

/** Where the screen is built to (`npm run build` in `ui/`): a CommonJS module whose export is
 * the script's text. `require`d, not read with `fs`: the module's sandbox lets `require` load
 * the package's own files, while `fs` reads only what a `read` permission lists. */
const SCREEN = "../../screen/ui.cjs";

/** The screen's script, for the admin to import. Not held to admins: it is code, the same for
 * everyone, and every route it calls is. */
function serveScreen(req, res) {
  let script;
  try {
    script = require(SCREEN);
  } catch (_) {
    res.status(404).set("content-type", "text/plain; charset=utf-8");
    return res.send(
      "The Plan & build screen has not been built: run `npm run build` in plugins/app-constructor/ui."
    );
  }
  res.set("content-type", "text/javascript; charset=utf-8");
  return res.send(script);
}

const routes = [
  { method: "get", url: "/ui.js", callback: serveScreen },
  ...HANDLERS.map(([method, url, handler]) => ({
    method,
    url,
    callback: (req, res) => respond(req, res, handler),
  })),
];

/** `on_application_deleted`: deletes the application's construction and the agent made for it.
 * If the construction is still being worked on, that work is stopped first (and its current
 * agent run aborted). */
async function applicationDeleted(app, ctx) {
  const construction = await store.loadConstruction(ctx.db, app.id);
  if (!construction) return;
  const slot = state.driverOf(construction.id);
  if (slot) {
    slot.stop = true;
    if (slot.run) {
      try {
        await ctx.agents.abort(slot.run);
      } catch (_) {
        // Ended meanwhile.
      }
    }
  }
  state.busy.delete(state.key(construction.id));
  state.pendingPlans.delete(state.key(construction.id));
  const deleted = await store.deleteConstruction(ctx.db, app.id);
  if (deleted) await deleteConstructionAgent(ctx.agents, app, deleted);
}

module.exports = {
  routes,
  HANDLERS,
  respond,
  statusOf,
  constructionJson,
  constructionAgentName,
  applicationDeleted,
  CONSTRUCTOR_TRAIT,
  CONSTRUCTOR_PROVIDER,
};
