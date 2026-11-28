// **App Constructor** — the bundled module `@feldspar/app-constructor` (TODO-app-constructor
// AC13). An application described in a few paragraphs is broken into phases, each phase into
// tasks, and each task is run by an agent: the construction agent for the data model, the
// application's builder agent for features. Before the phases are planned the agent may ask
// clarifying questions and propose mockups; after a phase is built the admin may give feedback,
// planned as follow-up tasks.
//
// The parts, under `server/` (the code the server runs; the screen's source is `ui/`, beside it):
// - `data/`: the database.
//   - `tables.js`: the module's tables, prefixed `ac_`;
//   - `store.js`: every read and write of the tables, over the `db` it is given;
//   - `mutex.js`: the per-construction lock that keeps the store's read-then-write changes whole.
// - `rules/`: pure code, no database.
//   - `model.js`: statuses, what may be saved, what the runner does next;
//   - `messages.js`: what each run is told, and what a task made, read from its runs' transcripts.
// - `agent/`: driving the agents.
//   - `trait.js`: the agent trait `constructor`, the construction agent's planning tools;
//   - `runner.js`: the service that follows the runs the routes start and runs the constructions;
//   - `state.js`: what the routes and the runner share in this worker (who is busy, who drives).
// - `http/`: the Plan & build screen's routes.
//   - `routes.js`: the route list, the screen's script, and `applicationDeleted`;
//   - `handlers.js`: one handler per route, and which URL runs which;
//   - `agents.js`: the application a route names, and making the construction agent;
//   - `checks.js`: what a handler checks before it changes a construction;
//   - `launch.js`: starting the construction agent's runs from a route;
//   - `json.js`: a construction as the screen reads it;
//   - `body.js`: reading request fields.
// - `test/`: the tests of all the above (`npm test` in the module's root).
//
// Beside `server/`: `ui/`, the screen's source, whose `npm run build` writes `screen/ui.cjs`.

"use strict";

const { tables } = require("./data/tables.js");
const { routes, applicationDeleted, CONSTRUCTOR_PROVIDER } = require("./http/routes.js");
const { constructor } = require("./agent/trait.js");
const runner = require("./agent/runner.js");

module.exports = {
  sc_plugin_api_version: 1,
  tables,
  agent_traits: { constructor },
  routes,
  services: {
    runner: {
      description: "Follows the construction agent's runs and runs each construction's tasks, as the admin who started it",
      start: (ctx) => runner.start(ctx),
    },
  },
  // A setting this module adds to a React application's Development settings. It picks the LLM
  // provider the construction agent uses to plan and build, and is applied to the agent before
  // each of its runs.
  application_settings: [
    {
      name: CONSTRUCTOR_PROVIDER,
      label: "LLM provider for Plan & build",
      kind: "llm_provider",
      frameworks: ["react"],
      description: "The LLM provider the App Constructor's agent plans and builds the data model with. Empty uses the builder agent's provider.",
    },
  ],
  admin_screens: [{ name: "construct", label: "Plan & build", script: "/ui.js", scope: "application", frameworks: ["react"] }],
  on_application_deleted: (app, ctx) => applicationDeleted(app, ctx),
};
