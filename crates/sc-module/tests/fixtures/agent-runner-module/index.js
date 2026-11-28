// A test module for the `agents` surface (TODO-app-constructor AC12.2): routes that make an agent
// and start, read, wait on and abort its runs as their caller, and a service that does what the
// routes hand it with its own `ctx.agents`, to show what a service may do (it has no caller).
"use strict";

// Work the routes hand the service: `{ work, resolve, reject }`.
const queue = [];
let running = false;

const perform = (agents, { op, ...args }) =>
  op === "start"
    ? agents.start(args)
    : op === "drive"
      ? agents.drive(args.id)
      : op === "save"
        ? agents.save(args.agent)
        : Promise.reject(new Error(`no op ${op}`));

const answer = (res, work) =>
  work.then(
    (value) => res.json(value),
    (e) => res.status(400).json({ error: e.message }),
  );

module.exports = {
  sc_plugin_api_version: 1,
  routes: [
    {
      method: "post",
      url: "/agents",
      callback: (req, res) => answer(res, req.feldspar.agents.save(req.body)),
    },
    {
      method: "get",
      url: "/agents/:name",
      callback: (req, res) => answer(res, req.feldspar.agents.get(req.params.name)),
    },
    {
      method: "post",
      url: "/agents/:name/delete",
      callback: (req, res) => answer(res, req.feldspar.agents.remove(req.params.name)),
    },
    {
      method: "get",
      url: "/providers",
      callback: (req, res) => answer(res, req.feldspar.agents.providers()),
    },
    {
      method: "post",
      url: "/start",
      callback: (req, res) => answer(res, req.feldspar.agents.start(req.body)),
    },
    {
      method: "get",
      url: "/runs/:id",
      callback: (req, res) => answer(res, req.feldspar.agents.run(req.params.id)),
    },
    {
      method: "get",
      url: "/runs/:id/messages",
      callback: (req, res) => answer(res, req.feldspar.agents.messages(req.params.id)),
    },
    {
      method: "post",
      url: "/runs/:id/wait",
      callback: (req, res) =>
        answer(res, req.feldspar.agents.wait(req.params.id, { timeout_ms: req.body.timeout_ms })),
    },
    {
      method: "post",
      url: "/runs/:id/abort",
      callback: (req, res) => answer(res, req.feldspar.agents.abort(req.params.id)),
    },
    // What the service's own `ctx.agents` does with `body`, `{ op, ...args }`: handed to the
    // service, which asks from its own call, and answered when it has.
    {
      method: "post",
      url: "/service",
      callback: (req, res) => {
        if (!running) return res.status(400).json({ error: "the service is not running" });
        return answer(
          res,
          new Promise((resolve, reject) => queue.push({ work: req.body, resolve, reject })),
        );
      },
    },
  ],
  services: {
    keeper: {
      description: "Does the work the routes hand it with its own `ctx.agents`",
      start: (ctx) =>
        new Promise((resolve) => {
          running = true;
          const timer = setInterval(() => {
            while (queue.length) {
              const { work, resolve: done, reject } = queue.shift();
              perform(ctx.agents, work).then(done, reject);
            }
          }, 10);
          ctx.signal.addEventListener("abort", () => {
            clearInterval(timer);
            running = false;
            resolve();
          });
        }),
    },
  },
};
