// Stand-ins for what the server lends the module — `agents`, `apps`, a route's `req` and `res` —
// so the routes, the trait and the runner are tested without a server. They keep the shapes
// crates/sc-module/src/js/module-host.mjs documents: a run is `{ id, agent, state, live, driven,
// conclusion, answer, error, authority, … }`, an application `{ id, name, subdomain, framework,
// settings, builder_agent }`.

import { createRequire } from "node:module";
import { fakeDb } from "./fake-db.mjs";

const require = createRequire(import.meta.url);
const routes = require("../../http/routes.js");
const state = require("../../agent/state.js");
const { createRunner } = require("../../agent/runner.js");

const copy = (v) => JSON.parse(JSON.stringify(v));
const tick = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Agents and runs. `script(opts, run)` is what a started run does: it may act (save tasks, as
 * the model's tool call would) and answers how the run ends — `{ answer, conclusion, state,
 * error, messages }`, an answered run by default; a promise it returns keeps the run going until
 * it settles. `authority` is what a started run is lent. */
export function fakeAgents({ script = () => ({ answer: "Done." }), providers, authority = "console" } = {}) {
  const agents = new Map();
  const runs = new Map();
  const started = [];
  let n = 0;
  const view = (r) => {
    const { done, abort, prompt, on_behalf_of, messages, ...out } = r;
    return copy(out);
  };
  const end = (r, outcome) => {
    if (!r.live) return;
    r.state = outcome.state || "done";
    r.conclusion = outcome.conclusion !== undefined ? outcome.conclusion : r.state === "done" ? "answered" : null;
    r.answer = outcome.answer ?? null;
    r.error = outcome.error ?? null;
    r.messages = outcome.messages || [];
    r.live = false;
    r.driven = false;
  };
  const own = (id) => {
    const r = runs.get(id);
    if (!r) throw new Error(`module \`app-constructor\` started no run ${id}`);
    return r;
  };
  const api = {
    providers: async () => copy(providers || [{ name: "house", default: true, model: "claude-sonnet-4-5" }, { name: "other", default: false, model: "claude-sonnet-4-5" }, { name: "bare", default: false, model: null }]),
    get: async (name) => (agents.has(name) ? copy(agents.get(name)) : null),
    save: async (agent) => {
      const current = agents.get(agent.name) || { name: agent.name, traits: [], model: null, attributes: { module: "app-constructor" } };
      const next = { ...current, ...copy(agent) };
      agents.set(agent.name, next);
      return copy(next);
    },
    remove: async (name) => ({ deleted: agents.delete(name) }),
    start: async (opts) => {
      if (!agents.has(opts.agent)) throw new Error(`there is no agent \`${opts.agent}\``);
      if (opts.on_behalf_of && !runs.has(opts.on_behalf_of)) throw new Error(`module \`app-constructor\` started no run ${opts.on_behalf_of}`);
      n += 1;
      const r = {
        id: `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`,
        agent: opts.agent,
        description: opts.description || "",
        state: "running",
        live: true,
        driven: true,
        conclusion: null,
        answer: null,
        error: null,
        authority: typeof authority === "function" ? authority(opts) : authority,
        prompt: opts.prompt,
        on_behalf_of: opts.on_behalf_of || null,
        messages: [],
      };
      const aborted = new Promise((resolve) => (r.abort = resolve));
      const work = Promise.resolve()
        .then(() => script(opts, view(r)))
        .then(
          (outcome) => end(r, outcome || {}),
          (e) => end(r, { state: "failed", error: e.message }),
        );
      r.done = Promise.race([work, aborted]);
      runs.set(r.id, r);
      started.push(r);
      return view(r);
    },
    run: async (id) => view(own(id)),
    messages: async (id) => copy(own(id).messages),
    wait: async (id) => {
      const r = own(id);
      if (r.driven) await Promise.race([r.done, tick(20)]);
      return view(r);
    },
    abort: async (id) => {
      const r = own(id);
      if (r.live) {
        r.state = "aborted";
        r.conclusion = "aborted";
        r.live = false;
        r.driven = false;
        r.abort();
      }
      return view(r);
    },
    drive: async (id) => {
      const r = own(id);
      if (r.live) r.driven = true;
      return view(r);
    },
  };
  return {
    api,
    agents,
    runs,
    started,
    /** A run as a restart leaves it: live, nothing driving it, its work done once driven on. */
    leave(id, outcome) {
      const r = own(id);
      r.driven = false;
      r.done = new Promise((resolve) => {
        const check = setInterval(() => {
          if (r.driven || !r.live) {
            clearInterval(check);
            end(r, outcome);
            resolve();
          }
        }, 5);
      });
    },
  };
}

/** Applications and their projects. `build` decides each build: `"succeeded"` or `"failed"`. */
export function fakeApps(list = [], { build = () => "succeeded" } = {}) {
  const files = new Map();
  const builds = [];
  const find = (ref) => list.find((a) => a.id === ref || a.subdomain === ref) || null;
  const api = {
    get: async (ref) => (find(ref) ? copy(find(ref)) : null),
    list: async () => copy(list),
    build: async (ref) => {
      const outcome = build(find(ref));
      builds.push({ app: ref, outcome });
      return { state: "running", log: "", error: null };
    },
    waitBuild: async () => {
      const last = builds[builds.length - 1];
      return last.outcome === "succeeded" ? { state: "succeeded", log: "", error: null } : { state: "failed", log: "", error: "src/App.tsx(3,7): error TS2322" };
    },
    lastBuild: async () => null,
    read: async (ref, p) => files.get(`${find(ref).id}:${p}`) ?? null,
    write: async (ref, p, content) => {
      files.set(`${find(ref).id}:${p}`, String(content));
      return null;
    },
    files: async (ref, dir) => {
      const prefix = `${find(ref).id}:${dir}/`;
      return [...files.keys()]
        .filter((k) => k.startsWith(prefix))
        .map((k) => {
          const p = k.slice(k.indexOf(":") + 1);
          return { name: p.slice(dir.length + 1), path: p, dir: false };
        });
    },
    remove: async (ref, p) => ({ deleted: files.delete(`${find(ref).id}:${p}`) }),
  };
  return { api, files, builds };
}

/** A React application, with its builder agent's name. */
export function reactApp(subdomain = "crm", extra = {}) {
  return {
    id: globalThis.crypto.randomUUID(),
    name: subdomain.toUpperCase(),
    subdomain,
    description: "",
    framework: "react",
    settings: {},
    builder_agent: `build-${subdomain}`,
    ...extra,
  };
}

export const ADMIN = { id: "11111111-1111-4111-8111-111111111111", email: "admin@example.com", role_id: 1 };
export const STAFF = { id: "22222222-2222-4222-8222-222222222222", email: "staff@example.com", role_id: 80 };

/** The params `pattern` binds in `path`, or `null`. */
function bind(pattern, path) {
  const want = pattern.split("/").filter(Boolean);
  const got = path.split("/").filter(Boolean);
  if (want.length !== got.length) return null;
  const params = {};
  for (let i = 0; i < want.length; i++) {
    if (want[i].startsWith(":")) params[want[i].slice(1)] = decodeURIComponent(got[i]);
    else if (want[i] !== got[i]) return null;
  }
  return params;
}

/** What `res` records. */
export function fakeRes() {
  const out = { status: 200, headers: {}, body: undefined };
  const res = {
    out,
    status(code) {
      out.status = code;
      return res;
    },
    set(k, v) {
      out.headers[k.toLowerCase()] = v;
      return res;
    },
    json(v) {
      out.body = copy(v === undefined ? null : v);
      return res;
    },
    send(v) {
      out.body = v;
      return res;
    },
  };
  return res;
}

/** One request to the module's route at `path`, as `user`: `{ status, body }`. */
export async function call(env, method, path, body = {}, user = ADMIN) {
  for (const route of routes.routes) {
    if (route.method !== method.toLowerCase()) continue;
    const params = bind(route.url, path);
    if (!params) continue;
    const res = fakeRes();
    await route.callback({ params, body, user, feldspar: { db: env.db, agents: env.agents.api, apps: env.apps.api } }, res);
    return { status: res.out.status, body: res.out.body, headers: res.out.headers };
  }
  throw new Error(`no route ${method} ${path}`);
}

/** A world to construct in: a fake db, agents, one React application `crm` and its builder
 * agent, and a runner over them, as the service would have it. */
export async function world({ script, build, authority, app: extra } = {}) {
  state.reset();
  const { db, tables } = fakeDb();
  const agents = fakeAgents({ script: (opts, run) => (env.script ? env.script(opts, run) : { answer: "Done." }), authority });
  const app = reactApp("crm", extra);
  const apps = fakeApps([app], { build: (a) => (env.build ? env.build(a) : "succeeded") });
  await agents.api.save({ name: app.builder_agent, provider: "other", traits: [] });
  const controller = new AbortController();
  const env = { db, tables, agents, apps, app, controller, script, build };
  env.runner = createRunner({ db, agents: agents.api, apps: apps.api, signal: controller.signal, log: { error: () => {}, warn: () => {} } });
  env.base = `/applications/${app.id}/construction`;
  /** Let the runner do what there is to do, to the end. */
  env.work = async () => {
    await env.runner.scan({ all: true });
    await env.runner.settled();
  };
  return env;
}
