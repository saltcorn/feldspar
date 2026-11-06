// A module that serves v1 `routes`: who is calling, a POST with and without
// v1's `noCsrf`, one that needs an API token, one that answers from a promise
// it did not return, one that throws — and one that reads what
// `@saltcorn/heartbeat-monitor` reads, through the same requires.

const si = require("systeminformation");
const EventLog = require("@saltcorn/data/models/eventlog");
const MetaData = require("@saltcorn/data/models/metadata");
const { getState } = require("@saltcorn/data/db/state");

module.exports = {
  sc_plugin_api_version: 1,
  configuration_workflow: () => ({
    steps: [{ name: "Greeting", form: { fields: [{ name: "greeting", label: "Greeting", type: "String" }] } }],
  }),
  routes: (cfg) => [
    {
      url: "/whoami",
      callback: async (req, res) => {
        res.set("Cache-Control", "no-store");
        res.json({
          greeting: cfg.greeting || null,
          role_id: req.user ? req.user.role_id : null,
          email: req.user ? req.user.email : null,
          query: req.query,
          path: req.path,
          originalUrl: req.originalUrl,
          sameRequest: res.req === req && req.res === res,
          cookie: req.headers.cookie || null,
        });
      },
    },
    {
      method: "post",
      url: "/echo/:id",
      noCsrf: true,
      callback: (req, res) => {
        res.status(201).json({ id: req.params.id, body: req.body, user: req.user ? req.user.email : null });
      },
    },
    {
      method: "post",
      url: "/guarded",
      callback: (req, res) => res.send("posted"),
    },
    {
      url: "/token",
      apiToken: true,
      callback: (req, res) => res.json({ email: req.user.email }),
    },
    {
      url: "/policy",
      // Headers the admin origin states for itself, and one of its own.
      callback: (req, res) => {
        res.set("Content-Security-Policy", "default-src *");
        res.set("X-Frame-Options", "ALLOWALL");
        res.set("X-Module", "kept");
        res.send("<p>policy</p>");
      },
    },
    {
      url: "/later",
      // Answers from a promise the callback does not return.
      callback: (req, res) => {
        setTimeout(() => res.type("text").send("later"), 20);
      },
    },
    {
      url: "/fail",
      callback: async () => {
        throw new Error("the route broke");
      },
    },
    {
      url: "/render",
      callback: (req, res) => res.render("page"),
    },
    {
      url: "/facts",
      callback: async (req, res) => {
        const [fs] = await si.fsSize();
        const mem = await si.mem();
        const load = await si.currentLoad();
        const startups = await EventLog.find({
          event_type: "Startup",
          occur_at: { gt: new Date(Date.now() - 24 * 60 * 60 * 1000) },
        });
        const backups = await MetaData.find(
          { type: "Backup", name: "Success" },
          { orderBy: "written_at", orderDesc: true, limit: 1 },
        );
        res.json({
          disk: fs.used <= fs.size && fs.size > 0,
          mem: mem.active <= mem.total && mem.total > 0,
          load: typeof load.currentLoad === "number",
          startups: startups.length,
          startupIsDate: startups[0] ? startups[0].occur_at instanceof Date : null,
          lastBackup: backups[0] ? backups[0].written_at.toISOString() : null,
          frequency: getState().getConfig("auto_backup_frequency"),
        });
      },
    },
  ],
};
