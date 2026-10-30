// The dialog language's engine: following choices, running `run` steps,
// going back, filling placeholders, and finding a broken graph.

import { describe, expect, it } from "vitest";

import {
  ask,
  canGoBack,
  checkFlow,
  choose,
  fill,
  finish,
  flow,
  form,
  goBack,
  run,
  say,
  settle,
  startFlow,
  submit,
  type FlowState,
} from "./flow";

type Fx = { lookup(): Promise<string | null> };

const definition = flow<Fx>({
  start: "first",
  steps: {
    first: ask({
      title: "First",
      options: [
        { label: "Look", go: "look" },
        { label: "Hidden", go: "end", when: (ctx) => ctx.vars.seen === "yes" },
        { label: "Leave", go: finish({ to: "/x/{name}" }) },
      ],
    }),
    look: run({
      exits: ["found", "named"],
      do: async (_ctx, fx) => {
        const found = await fx.lookup();
        return found ? { go: "found", set: { name: found } } : "named";
      },
    }),
    named: form({
      title: "Name it",
      fields: () => [],
      exits: ["found"],
      submit: async (values) => ({ go: "found", set: { name: values.name } }),
    }),
    found: say({ title: "Found {name}", buttons: [{ label: "OK", go: "end" }] }),
    end: say({ title: "End", buttons: [{ label: "Done", go: finish({ to: "/x/{name}" }) }] }),
  },
});

const at = (state: FlowState) => ("done" in state ? "done" : state.at);

describe("a flow", () => {
  it("is sound", () => {
    expect(checkFlow(definition)).toEqual([]);
  });

  it("runs a lookup and follows what it found", async () => {
    let state = choose(definition, startFlow(definition), 0);
    expect(at(state)).toBe("look");
    state = await settle(definition, state, { lookup: async () => "Ada" });
    expect(at(state)).toBe("found");
    expect(state.ctx.vars.name).toBe("Ada");
  });

  it("asks when the lookup finds nothing, and carries the answer on", async () => {
    let state = await settle(definition, choose(definition, startFlow(definition), 0), {
      lookup: async () => null,
    });
    expect(at(state)).toBe("named");
    state = await submit(definition, state, { name: "a b" }, { lookup: async () => null });
    state = choose(definition, state, 0);
    state = choose(definition, state, 0);
    // The path is filled with the value URL-encoded.
    expect(state).toEqual({ done: { to: "/x/a%20b", href: undefined }, ctx: expect.anything() });
  });

  it("goes back past a run step to the dialog before it, with that dialog's context", async () => {
    const fx = { lookup: async () => "Ada" };
    let state = await settle(definition, choose(definition, startFlow(definition), 0), fx);
    expect(canGoBack(state)).toBe(true);
    state = goBack(state);
    expect(at(state)).toBe("first");
    expect(state.ctx.vars).toEqual({});
    expect(canGoBack(state)).toBe(false);
  });

  it("offers a choice only while its condition holds", () => {
    // The second option is hidden, so index 1 is "Leave".
    const state = choose(definition, startFlow(definition), 1);
    expect("done" in state).toBe(true);
  });

  it("refuses a run step going somewhere it did not declare", async () => {
    const bad = flow<Fx>({
      start: "a",
      steps: {
        a: run({ exits: ["b"], do: async () => "c" }),
        b: say({ title: "B", buttons: [] }),
        c: say({ title: "C", buttons: [] }),
      },
    });
    await expect(settle(bad, startFlow(bad), { lookup: async () => null })).rejects.toThrow(
      /"c" is not one of this step's exits/,
    );
  });

  it("names a missing step and an unreachable one", () => {
    const broken = flow<Fx>({
      start: "a",
      steps: {
        a: ask({ title: "A", options: [{ label: "x", go: "nowhere" }] }),
        orphan: say({ title: "O", buttons: [] }),
      },
    });
    expect(checkFlow(broken)).toEqual([
      'step "a" goes to "nowhere", which does not exist',
      'step "orphan" is never reached',
    ]);
  });
});

describe("fill", () => {
  it("fills known placeholders and leaves the rest", () => {
    expect(fill("{a} and {b}", { a: "x" })).toBe("x and {b}");
  });
});
