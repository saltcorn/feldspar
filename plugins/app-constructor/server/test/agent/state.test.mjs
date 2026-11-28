// The runner's wake-up: a wake while the runner is busy (not sleeping) is not lost.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const state = require("../../agent/state.js");

/** Whether `promise` settles within `ms`. */
const within = (promise, ms) =>
  Promise.race([promise.then(() => true), new Promise((r) => setTimeout(() => r(false), ms))]);

test("a wake while no one sleeps ends the next sleep at once, and only that one", async () => {
  state.reset();
  state.wake();
  assert.equal(await within(state.sleep(10000), 100), true, "the next sleep ends at once");
  const next = state.sleep(10000);
  assert.equal(await within(next, 100), false, "the wake was used up");
  state.wake();
  assert.equal(await within(next, 100), true, "a wake ends a sleep in progress");
});

test("a sleep that times out is not woken later", async () => {
  state.reset();
  let ended = 0;
  await state.sleep(1).then(() => (ended += 1));
  // Its waker is gone, so this wake is kept for the next sleep rather than spent on it.
  state.wake();
  assert.equal(await within(state.sleep(10000), 100), true);
  assert.equal(ended, 1);
});
