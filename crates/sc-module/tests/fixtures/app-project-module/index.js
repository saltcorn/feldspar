// A test module for the `apps` surface (TODO-app-constructor AC12.3): routes that read
// applications, read and write the files of an application's project, and build it, each as
// the route's caller. It keeps notes per application in a table of its own, which its
// `on_application_deleted` hook (AC12.5) deletes with the application.
"use strict";

const answer = (res, work) =>
  Promise.resolve()
    .then(work)
    .then(
      (value) => res.json(value === undefined ? null : value),
      (e) => res.status(400).json({ error: e.message }),
    );

const route = (method, url, run) => ({
  method,
  url,
  callback: (req, res) => answer(res, () => run(req.feldspar.apps, req)),
});

module.exports = {
  sc_plugin_api_version: 1,
  tables: [
    {
      name: "app_notes",
      fields: [
        { name: "application", type: "text", required: true },
        { name: "note", type: "text" },
      ],
    },
  ],
  // Two fields on a React application's settings form, and one whose kind is
  // not a setting's, which is an issue on the card (AC12.9).
  application_settings: [
    { name: "notes_provider", label: "Notes provider", kind: "llm_provider", frameworks: ["react"] },
    { name: "notes_on", label: "Keep notes", kind: "switch" },
    { name: "notes_colour", kind: "colour" },
  ],
  // A screen in a React application's sidebar, its script served by a route of
  // the module's, and one whose script is no route, which is an issue (AC12.4).
  admin_screens: [
    { name: "notes", label: "Notes", script: "/ui.js", frameworks: ["react"] },
    { name: "lost", label: "Lost", script: "/missing.js" },
  ],
  // The notes go, and the deletion is noted, together (AC12.6).
  on_application_deleted: async (app, ctx) => {
    await ctx.db.atomically((db) => {
      db.app_notes.where({ application: app.id }).delete();
      db.app_notes.insert({ application: "deleted", note: app.subdomain });
    });
  },
  routes: [
    {
      method: "get",
      url: "/ui.js",
      callback: (req, res) => {
        res.set("content-type", "text/javascript; charset=utf-8");
        res.send("export function mount(element) { element.textContent = 'notes'; }\n");
      },
    },
    {
      method: "get",
      url: "/notes",
      callback: (req, res) =>
        answer(res, () => req.feldspar.db.app_notes.select("application", "note").rows()),
    },
    route("get", "/apps", (apps) => apps.list()),
    route("get", "/apps/:app", (apps, req) => apps.get(req.params.app)),
    route("post", "/apps/:app/read", (apps, req) => apps.read(req.params.app, req.body.path)),
    route("post", "/apps/:app/write", (apps, req) =>
      apps.write(req.params.app, req.body.path, req.body.content),
    ),
    route("post", "/apps/:app/files", (apps, req) => apps.files(req.params.app, req.body.path)),
    route("post", "/apps/:app/remove", (apps, req) => apps.remove(req.params.app, req.body.path)),
    route("post", "/apps/:app/build", (apps, req) => apps.build(req.params.app)),
    route("post", "/apps/:app/wait", (apps, req) =>
      apps.waitBuild(req.params.app, { timeout_ms: req.body.timeout_ms }),
    ),
    route("get", "/apps/:app/last", (apps, req) => apps.lastBuild(req.params.app)),
  ],
};
