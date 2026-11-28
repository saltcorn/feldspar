// A construction's mockups (ported from crates/sc-construct/tests/mockups.rs): each proposal is a
// new round, feedback is kept per mockup, one mockup at most is chosen, and deleting the
// construction takes them. Two rounds saved at once are two rounds: the lock serializes them.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb, settled } from "../helpers/fake-db.mjs";

const require = createRequire(import.meta.url);
const store = require("../../data/store.js");
const m = require("../../rules/model.js");

const round = (titles) =>
  m.parseMockups(
    titles.map((t) => ({
      title: t,
      rationale: `${t}, explained`,
      screens: [
        { name: "Orders list", html: "<table><th>Customer</th></table>" },
        { name: "Order form", html: "<form><label>Customer</label></form>" },
      ],
    })),
  );

test("proposals come in rounds and one mockup is chosen", async () => {
  const { db } = fakeDb();
  const app = globalThis.crypto.randomUUID();
  const construction = await store.createConstruction(db, app);

  const first = await store.saveMockups(db, app, round(["Table", "Cards"]));
  assert.deepEqual(
    first.map((x) => x.round),
    [1, 1],
  );
  assert.equal(first[0].status, "proposed");
  assert.equal(await store.chosenMockup(db, construction.id), null);

  // Feedback is kept on the mockup it is about; blank clears it.
  await store.setMockupFeedback(db, app, [
    { mockup: first[0].id, text: " denser rows " },
    { mockup: first[1].id, text: "" },
  ]);
  const stored = await store.mockupsOf(db, construction.id);
  assert.equal(stored[0].feedback, "denser rows");
  assert.equal(stored[1].feedback, null);
  // A mockup of no construction of this application is refused, and nothing is saved.
  await assert.rejects(
    store.setMockupFeedback(db, app, [
      { mockup: first[1].id, text: "x" },
      { mockup: 99999, text: "x" },
    ]),
    (e) => e.kind === "not_found",
  );
  assert.equal((await store.mockupsOf(db, construction.id))[1].feedback, null);

  // A revision is the next round; the first stays readable.
  const second = await store.saveMockups(db, app, round(["Dense table"]));
  assert.equal(second[0].round, 2);
  const all = await store.mockupsOf(db, construction.id);
  assert.equal(all.length, 3);
  assert.deepEqual(
    m.latestRound(all).map((x) => x.title),
    ["Dense table"],
  );
  assert.equal(all[2].screens[1].name, "Order form");

  // One chosen at most: choosing another unchooses the first.
  await store.chooseMockup(db, app, first[1].id);
  const chosen = await store.chooseMockup(db, app, second[0].id);
  assert.equal(chosen.status, "chosen");
  const marked = (await store.mockupsOf(db, construction.id)).filter((x) => x.status === "chosen");
  assert.deepEqual(
    marked.map((x) => x.id),
    [second[0].id],
  );
  assert.equal((await store.chosenMockup(db, construction.id)).id, second[0].id);
  await assert.rejects(store.chooseMockup(db, app, 99999), /no mockup 99999/);

  // Deleting the construction takes its mockups with it.
  await store.deleteConstruction(db, app);
  assert.deepEqual(await store.mockupsOf(db, construction.id), []);
});

test("two rounds saved at once are two rounds", async () => {
  const { db, hold } = fakeDb();
  const app = globalThis.crypto.randomUUID();
  const construction = await store.createConstruction(db, app);

  // The first save has counted the rounds and is about to write; the second waits for it and
  // takes the next round.
  const write = hold({ op: "batch" });
  const a = store.saveMockups(db, app, round(["Table", "Cards"]));
  await write.reached;
  const b = store.saveMockups(db, app, round(["Split view"]));
  assert.equal(await settled(b), false, "the second save waits for the first");
  write.release();
  assert.deepEqual([(await a)[0].round, (await b)[0].round], [1, 2]);
  // A round is saved whole.
  assert.equal((await store.mockupsOf(db, construction.id)).length, 3);
});

test("a round that fails half way saves none of it", async () => {
  const { db, failWrite } = fakeDb();
  const app = globalThis.crypto.randomUUID();
  const construction = await store.createConstruction(db, app);
  failWrite("ac_mockups", 2);
  await assert.rejects(store.saveMockups(db, app, round(["Table", "Cards"])), /failed/);
  assert.deepEqual(await store.mockupsOf(db, construction.id), []);
  assert.equal((await store.saveMockups(db, app, round(["Table"])))[0].round, 1);
});
