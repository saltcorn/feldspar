// A construction's clarifying questions (ported from crates/sc-construct/tests/questions.rs):
// asking again replaces the open questions and keeps the answered ones, asking discards the
// mockups, an answer is refused for another construction's question, and deleting the
// construction takes them.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "../helpers/fake-db.mjs";

const require = createRequire(import.meta.url);
const store = require("../../data/store.js");
const m = require("../../rules/model.js");

const texts = (questions) => questions.map((q) => q.text);

test("asking again keeps the answered questions and replaces the open ones", async () => {
  const { db } = fakeDb();
  const app = globalThis.crypto.randomUUID();
  const construction = await store.createConstruction(db, app);

  const first = (await store.saveQuestions(db, app, ["Who?", "How many?"])).questions;
  assert.deepEqual(texts(first), ["Who?", "How many?"]);

  await store.answerQuestions(db, app, [{ question: first[1].id, text: " A dozen " }]);
  const again = (await store.saveQuestions(db, app, ["Offline?", "Where?"])).questions;
  // The answered one first, as it was; the open one gone.
  assert.deepEqual(texts(again), ["How many?", "Offline?", "Where?"]);
  assert.equal(again[0].id, first[1].id);
  assert.equal(again[0].answer, "A dozen");
  assert.deepEqual(texts(await store.questionsOf(db, construction.id)), texts(again));
  assert.equal(m.answersText(again), "## The admin's answers to the clarifying questions\n\nQuestion: How many?\nAnswer: A dozen");

  // Blank text opens it again; another construction's question is refused.
  await store.answerQuestions(db, app, [{ question: again[0].id, text: "  " }]);
  assert.equal((await store.questionsOf(db, construction.id))[0].answer, null);
  const other = globalThis.crypto.randomUUID();
  await store.createConstruction(db, other);
  const theirs = (await store.saveQuestions(db, other, ["Theirs?"])).questions;
  await assert.rejects(store.answerQuestions(db, app, [{ question: theirs[0].id, text: "x" }]), /no question/);

  await store.deleteConstruction(db, app);
  assert.deepEqual(await store.questionsOf(db, construction.id), []);
  assert.equal((await store.questionsOf(db, theirs[0].construction)).length, 1);
});

test("asking discards the mockups drawn before the answers", async () => {
  const { db } = fakeDb();
  const app = globalThis.crypto.randomUUID();
  const construction = await store.createConstruction(db, app);
  const round = (title) => m.parseMockups([{ title, rationale: "", screens: [{ name: "List", html: "<main>x</main>" }] }]);
  const saved = await store.saveMockups(db, app, round("Table"));
  await store.saveMockups(db, app, round("Cards"));
  await store.chooseMockup(db, app, saved[0].id);

  const asked = await store.saveQuestions(db, app, ["Who?"]);
  assert.equal(asked.discarded_mockups, 2);
  assert.deepEqual(await store.mockupsOf(db, construction.id), []);
  // Nothing left to discard the next time; a new round starts over at 1.
  assert.equal((await store.saveQuestions(db, app, ["Where?"])).discarded_mockups, 0);
  assert.equal((await store.saveMockups(db, app, round("New")))[0].round, 1);
});
