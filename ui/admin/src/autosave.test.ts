import { describe, expect, it } from "vitest";

import { Autosaver, type SaveState, type Timers } from "./autosave";

/** Timers a test advances by hand. */
function manualTimers() {
  let now = 0;
  let next = 1;
  const due = new Map<number, { at: number; fn: () => void }>();
  const timers: Timers = {
    setTimeout: (fn, ms) => {
      const id = next++;
      due.set(id, { at: now + ms, fn });
      return id;
    },
    clearTimeout: (id) => {
      due.delete(id as number);
    },
  };
  const advance = async (ms: number) => {
    now += ms;
    for (const [id, t] of [...due.entries()].sort((a, b) => a[1].at - b[1].at)) {
      if (t.at <= now) {
        due.delete(id);
        t.fn();
      }
    }
    // Let the saves' promise chains settle.
    for (let i = 0; i < 10; i++) await Promise.resolve();
  };
  return { timers, advance };
}

/** A save the test resolves or rejects itself. */
function controlledSave() {
  const calls: { body: unknown; resolve: () => void; reject: (e: unknown) => void }[] = [];
  const save = (body: unknown) =>
    new Promise<void>((resolve, reject) => calls.push({ body, resolve, reject }));
  return { calls, save };
}

function setup() {
  const { timers, advance } = manualTimers();
  const { calls, save } = controlledSave();
  const states: SaveState[] = [];
  const saver = new Autosaver<{ v: string }>(save, (s) => states.push(s), String, timers);
  saver.baseline({ v: "a" });
  return { saver, calls, states, advance, last: () => states[states.length - 1] };
}

const settle = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};

describe("Autosaver", () => {
  it("waits out typing and saves the last value once", async () => {
    const { saver, calls, advance, last } = setup();
    saver.change({ v: "ab" }, 700);
    await advance(300);
    saver.change({ v: "abc" }, 700);
    await advance(600);
    expect(calls).toHaveLength(0);
    expect(last()).toEqual({ kind: "pending" });
    await advance(100);
    expect(calls.map((c) => c.body)).toEqual([{ v: "abc" }]);
    expect(last()).toEqual({ kind: "saving" });
    calls[0].resolve();
    await settle();
    expect(last()).toEqual({ kind: "saved" });
  });

  it("sends nothing for the stored value, or for a change undone", async () => {
    const { saver, calls, advance, last } = setup();
    saver.change({ v: "a" }, 0);
    saver.change({ v: "ab" }, 700);
    saver.change({ v: "a" }, 700);
    await advance(1000);
    expect(calls).toHaveLength(0);
    expect(last()).toEqual({ kind: "saved" });
  });

  it("never overlaps saves, and sends what changed meanwhile in one", async () => {
    const { saver, calls, advance } = setup();
    saver.change({ v: "b" }, 0);
    await advance(0);
    expect(calls).toHaveLength(1);
    saver.change({ v: "c" }, 0);
    saver.change({ v: "d" }, 0);
    await advance(0);
    expect(calls).toHaveLength(1); // still waiting for the first
    calls[0].resolve();
    await settle();
    expect(calls.map((c) => c.body)).toEqual([{ v: "b" }, { v: "d" }]);
  });

  it("saves a change back to the old value made while a save is in flight", async () => {
    const { saver, calls, advance } = setup();
    saver.change({ v: "b" }, 0);
    await advance(0);
    saver.change({ v: "a" }, 0);
    await advance(0);
    calls[0].resolve();
    await settle();
    // The server holds "b" now, so the form's "a" has to follow it.
    expect(calls.map((c) => c.body)).toEqual([{ v: "b" }, { v: "a" }]);
  });

  it("keeps a refused save's reason and tries again on the next change", async () => {
    const { saver, calls, advance, last } = setup();
    saver.change({ v: "" }, 0);
    await advance(0);
    calls[0].reject("a name is required");
    await settle();
    expect(last()).toEqual({ kind: "error", message: "a name is required" });
    await advance(10_000);
    expect(calls).toHaveLength(1); // no retry loop
    saver.change({ v: "x" }, 0);
    await advance(0);
    expect(calls.map((c) => c.body)).toEqual([{ v: "" }, { v: "x" }]);
  });

  it("flushes what is waiting at once", async () => {
    const { saver, calls } = setup();
    saver.change({ v: "typed" }, 700);
    const done = saver.flush();
    await settle();
    expect(calls.map((c) => c.body)).toEqual([{ v: "typed" }]);
    calls[0].resolve();
    await done;
  });

  it("does nothing before the form is loaded", async () => {
    const { timers, advance } = manualTimers();
    const { calls, save } = controlledSave();
    const saver = new Autosaver<{ v: string }>(save, () => {}, String, timers);
    saver.change({ v: "early" }, 0);
    await advance(0);
    expect(calls).toHaveLength(0);
  });
});
