// A lock per key, for the store's read-then-write changes (TODO-app-constructor AC13.1).
//
// The Rust store locked rows (`SELECT … FOR UPDATE`) so that, say, a phase was never deleted
// while a task was being added to it. A module's `db` has no row locks, and `db.atomically`
// can only write, so a change reads what it needs first and then writes atomically. Between the
// read and the write another change could slip in; this lock stops that. One worker runs the
// module, so serializing the changes in this process is enough: nothing else writes the
// module's tables.

"use strict";

/** A lock per key. `run(key, fn)` runs `fn` once every earlier `run` of the same key has
 * finished, and resolves to what `fn` resolved to. Different keys do not wait for each other. */
function keyedMutex() {
  // The last change queued per key: a new one waits for it. Settled either way, so one that
  // failed does not stop the next.
  const tails = new Map();
  const run = (key, fn) => {
    const before = tails.get(key) || Promise.resolve();
    const result = before.then(() => fn());
    const tail = result.then(
      () => {},
      () => {}
    );
    tails.set(key, tail);
    // Forget a key once nothing waits on it, so the map holds only keys in use.
    tail.then(() => {
      if (tails.get(key) === tail) tails.delete(key);
    });
    return result;
  };
  return { run, held: () => tails.size };
}

module.exports = { keyedMutex };
