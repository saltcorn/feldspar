// A notebook's `nb` in JavaScript (analytics TODO A10.4): run once in a
// session's isolate, it defines `__nbMake(db)`, which every cell's body calls
// for the `nb` it writes. Each function is a plan sent over the cell's own
// `db` (`{ op: "nb", what, … }`), answered by `sc_analytics::nb::answer` and
// the server; the outputs a cell shows and prints are collected here, in
// order, and handed back with its value in one envelope.
//
// A dataset — or any value a cell keeps for the next — outlives the run that
// made it, and so does not hold that run's `db`: what it sends goes through
// the cell running now (`__nbSend`, set by each cell's `__nbMake`). A session
// runs one cell at a time, so "now" is one cell.
(() => {
  const send = (what, extra) => {
    const current = globalThis.__nbSend;
    if (typeof current !== "function") {
      throw new Error("nb is only reachable while a cell is running");
    }
    return current(Object.assign({ op: "nb", what: what }, extra || {}));
  };
  // A value as it crosses: plain JSON, a dataset as its chain.
  const lower = (v) => {
    if (v === undefined) return null;
    const text = JSON.stringify(v, (_, x) => (typeof x === "bigint" ? Number(x) : x));
    return text === undefined ? null : JSON.parse(text);
  };

  class Dataset {
    constructor(base, steps, name) {
      this.base = base;
      this.steps = steps;
      this.name = name;
      Object.freeze(this);
    }
    step(method, args) {
      return new Dataset(this.base, this.steps.concat([{ method: method, args: args }]), this.name);
    }
    filter(condition) { return this.step("filter", [condition]); }
    mutate(columns) {
      let out = this;
      for (const [name, formula] of Object.entries(columns || {})) {
        out = out.step("mutate", [{ [name]: formula }]);
      }
      return out;
    }
    select(...columns) { return this.step("select", columns); }
    sort(...keys) { return this.step("sort", keys); }
    aggregate(by, summaries) {
      return this.step("aggregate", summaries === undefined ? [by] : [by, summaries]);
    }
    limit(n) { return this.step("limit", [n]); }
    sample(n, seed) { return this.step("sample", seed === undefined ? [n] : [n, seed]); }
    then(kind, params) { return this.step("then", [kind, params]); }
    named(name) { return new Dataset(this.base, this.steps, String(name)); }
    rows(limit) { return send("rows", { data: lower(this), limit: limit === undefined ? null : limit }); }
    columns() { return send("columns", { data: lower(this) }); }
    toJSON() { return { __nb: "dataset", base: this.base, steps: this.steps, name: this.name || null }; }
  }

  globalThis.__nbMake = (db) => {
    globalThis.__nbSend = (plan) => db.__scSend(plan);
    const outputs = [];

    const panel = (answer) =>
      Object.freeze({ __nb: "panel", kind: answer.panel.kind, panel: answer.panel });
    const fitHandle = (got) => {
      const byName = new Map(got.outputs.map((o) => [o.name, o]));
      return Object.freeze({
        __nb: "fit",
        fit: got.fit,
        model: got.model,
        status: got.status,
        outputs: got.outputs.map((o) => ({ name: o.name, label: o.label, kind: o.kind, panel: o.panel })),
        output: (name) => {
          const o = byName.get(name);
          if (o === undefined) {
            throw new Error(
              "this fit has no output `" + name + "`; it has " +
              got.outputs.map((x) => "`" + x.name + "`").join(", ")
            );
          }
          return Object.freeze({ __nb: "panel", kind: o.panel.kind, panel: o.panel });
        },
      });
    };
    const text = (v) => {
      if (typeof v === "string") return v;
      if (v instanceof Error) return String(v);
      if (v === undefined) return "undefined";
      try {
        const t = JSON.stringify(v);
        return t === undefined ? String(v) : t;
      } catch (_) {
        return String(v);
      }
    };
    const print = (...args) => {
      outputs.push({ __nb: "text", text: args.map(text).join(" ") });
    };
    const describe = (e) => {
      if (e instanceof Error) return e.name === "Error" ? e.message : e.name + ": " + e.message;
      return text(e);
    };
    // What was shown, resolved: a shown promise (`nb.show(nb.plot(…))`) is
    // what it resolves to.
    const settled = async () => {
      const out = [];
      for (const o of outputs) out.push(lower(await o));
      return out;
    };

    return Object.freeze({
      dataset: (name) => {
        if (typeof name !== "string" || name === "") {
          throw new Error('nb.dataset() takes a dataset\'s or a table\'s name, as in nb.dataset("houses")');
        }
        return new Dataset(name, [], null);
      },
      plot: async (spec) => panel(await send("plot", { spec: lower(spec) })),
      table: async (data, spec) => panel(await send("table", { data: lower(data), spec: lower(spec) })),
      test: async (data, spec) => panel(await send("test", { data: lower(data), spec: lower(spec) })),
      card: async (data, spec) => panel(await send("card", { data: lower(data), spec: lower(spec) })),
      map: async (spec) => panel(await send("map", { spec: lower(spec) })),
      markdown: async (markdown) => panel(await send("text", { markdown: String(markdown) })),
      fit: async (model, options) =>
        fitHandle(await send("fit", { model: model, options: lower(options) })),
      show: (...values) => {
        for (const v of values) outputs.push(typeof v === "string" ? { __nb: "text", text: v } : v);
      },
      __console: Object.freeze({
        log: print, info: print, warn: print, error: print, debug: print, trace: print,
      }),
      __finish: async (value, line) => {
        try {
          const out = await settled();
          const v = await value;
          return { __nb: "finish", outputs: out, value: lower(v), has_value: v !== undefined && v !== null };
        } catch (e) {
          return { __nb: "fail", outputs: [], message: describe(e), line: line };
        }
      },
      __fail: async (e, line) => {
        let out = [];
        try { out = await settled(); } catch (_) { out = []; }
        return { __nb: "fail", outputs: out, message: describe(e), line: line };
      },
    });
  };
})();
