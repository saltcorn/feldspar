// The lock that replaces the Rust store's row locks, and the fake `db`'s batch, which the store's
// several-row changes rely on: one key's changes run one at a time, in the order they came, and a
// failed one does not stop the next; a batch lands whole or not at all, holds writes only, and
// takes no async function.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "../helpers/fake-db.mjs";

const require = createRequire(import.meta.url);
const { keyedMutex } = require("../../data/mutex.js");

const tick = (ms = 5) => new Promise((resolve) => setTimeout(resolve, ms));

test("two changes of one key at once run one after the other, in order", async () => {
  const lock = keyedMutex();
  const events = [];
  const change = (name) => async () => {
    events.push(`${name} starts`);
    await tick();
    events.push(`${name} ends`);
    return name;
  };
  const results = await Promise.all([lock.run("a", change("first")), lock.run("a", change("second"))]);
  assert.deepEqual(results, ["first", "second"]);
  assert.deepEqual(events, ["first starts", "first ends", "second starts", "second ends"]);
  assert.equal(lock.held(), 0, "a key nothing waits on is forgotten");
});

test("different keys do not wait for each other, and a failure does not stop the next", async () => {
  const lock = keyedMutex();
  const events = [];
  const slow = lock.run("a", async () => {
    await tick(20);
    events.push("a");
  });
  await lock.run("b", async () => events.push("b"));
  await slow;
  assert.deepEqual(events, ["b", "a"]);

  const failing = lock.run("a", async () => {
    throw new Error("boom");
  });
  const next = lock.run("a", async () => "ran");
  await assert.rejects(failing, /boom/);
  assert.equal(await next, "ran");
});

test("a batch lands whole or not at all, and answers each write", async () => {
  const { db, tables, failWrite } = fakeDb();
  const [made] = await db.atomically((db) => {
    db.ac_constructions.insert({ application: globalThis.crypto.randomUUID(), specification: "", status: "draft" });
  });
  assert.equal(made.id, 1);

  failWrite("ac_phases", 2);
  await assert.rejects(
    db.atomically((db) => {
      db.ac_constructions.where({ id: 1 }).update({ specification: "changed" });
      db.ac_phases.insert({ construction: 1, position: 1, title: "A", description: "", requirements: [] });
      db.ac_phases.insert({ construction: 1, position: 2, title: "B", description: "", requirements: [] });
    }),
    /failed/,
  );
  assert.equal(tables.ac_constructions[0].specification, "");
  assert.deepEqual(tables.ac_phases, []);
  // The ids the rolled-back inserts took are given again, as nothing of them stayed.
  const [phase] = await db.atomically((db) => {
    db.ac_phases.insert({ construction: 1, position: 1, title: "A", description: "", requirements: [] });
  });
  assert.equal(phase.id, 1);

  // A parent still pointed at cannot go before its children.
  await assert.rejects(db.ac_constructions.where({ id: 1 }).delete(), /foreign key/);
});

test("a batch holds writes only and takes no async function", async () => {
  const { db } = fakeDb();
  assert.throws(() => db.atomically((db) => db.ac_phases.where({ id: 1 }).rows()), /holds writes/);
  assert.throws(() => db.atomically(async (db) => db.ac_phases.where({ id: 1 }).delete()), /must not be async/);
  assert.throws(() => db.ac_phases.update({ title: "x" }), /without a .where\(\)/);
});
