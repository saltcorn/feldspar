// `pay_refund`, `pay_provider_code`, `pay_api` and the reconcile sweep (TODO-payments P2.6,
// P2.7), over the fake provider and an in-memory `db`.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "./fake-db.mjs";

const require = createRequire(import.meta.url);
const fake = require("../adapters/fake.js");
const plugin = require("../index.js");
const { actions } = plugin;
const { keepAccounts } = require("../accounts.js");
const { sweepAll } = require("../service.js");

const shop = { id: 1, name: "shop", adapter: "fake", mode: "test", enabled: true, webhook_secret: "whsec_test", settings: {} };
const held = { ...shop, id: 2, name: "held", settings: { webhooks: "hold" } };

let store;
beforeEach(() => {
  fake.reset();
  store = fakeDb(
    {
      pay_accounts: [{ ...shop }, { ...held }],
      orders: [
        { id: 12, owner: 7, total: 900 },
        { id: 13, owner: 8, total: 500 },
      ],
    },
    { canRead: (table, row) => table !== "orders" || row.owner === 7 },
  );
});

const base = { row: null, user: { id: 7, email: "ann@example.com" }, app: { id: "a", url: "https://shop.example.com" } };
const checkout = (account, order) =>
  actions.pay_checkout.run({
    ...base,
    db: store.db,
    configuration: { account, table: "orders", key_field: "order", amount: "total", currency: "EUR", read_check: false },
    payload: { order },
  });
const ctx = () => ({ db: store.db, log: Object.assign(() => {}, { warn: () => {}, error: () => {} }) });
const kinds = () => store.tables.pay_events.map((e) => [e.kind, e.ref_key]);

async function paid(account, order) {
  const { deliverTo } = require("../webhooks.js");
  const started = await checkout(account.name, order);
  await fake.settle(account, started.checkout, "succeeded", deliverTo(account, ctx()));
  return started;
}

test("a refund is found from the row it was for, asked for once, and logged when the provider says so", async () => {
  await paid(shop, 12);
  const refund = (configuration, payload) => actions.pay_refund.run({ ...base, db: store.db, configuration: { source: "row", table: "orders", ...configuration }, payload });
  const part = await refund({ amount: "total" }, { id: 12 });
  assert.deepEqual([part.amount, part.currency, part.status], [900, "EUR", "succeeded"]);
  assert.deepEqual(kinds(), [["checkout.completed", 12], ["payment.succeeded", 12], ["payment.refunded", 12]]);
  assert.equal(store.tables.pay_objects.find((o) => o.kind === "payment").status, "refunded");
  for (const [configuration, payload, refusal] of [
    [{}, { id: 12 }, /there is no payment for orders 12 to refund/],
    [{}, { id: 13 }, /there is no orders row 13 that you can read/],
    [{ table: "" }, { id: 12 }, /needs the table setting/],
  ]) {
    await assert.rejects(refund(configuration, payload), refusal);
  }
  // Given the mirror row instead.
  await paid(shop, 13);
  const mirror = store.tables.pay_objects.find((o) => o.kind === "payment" && o.ref_key === 13);
  const whole = await actions.pay_refund.run({ ...base, user: { id: 8 }, db: store.db, configuration: { source: "payment", read_check: false }, payload: { id: mirror.id } });
  assert.equal(whole.amount, 500);
});

test("pay_provider_code runs with the account's client, and sees no keys", async () => {
  const started = await paid(shop, 12);
  const run = (code) => actions.pay_provider_code.run({ ...base, db: store.db, configuration: { account: "shop", code }, payload: { n: 2 } });
  const seen = await run("return { account, same: client === fake, n: payload.n };");
  assert.deepEqual(seen, { account: { name: "shop", adapter: "fake", mode: "test" }, same: true, n: 2 });
  const read = await actions.pay_provider_code.run({
    ...base,
    db: store.db,
    configuration: { account: "shop", code: "return (await client.checkouts.retrieve(payload.id)).status;" },
    payload: { id: started.checkout },
  });
  assert.equal(read, "complete");
  await assert.rejects(run("return ("), /does not parse/);
});

test("pay_api reads the accounts the service keeps, with no db of its own", async () => {
  const started = await paid(shop, 12);
  const { pay_api } = plugin.functions;
  keepAccounts([]);
  await assert.rejects(pay_api.run("shop", "GET", `/checkouts/${started.checkout}`), /there is no payment account "shop"; there are none yet/);
  keepAccounts(store.tables.pay_accounts);
  const object = await pay_api.run("shop", "GET", `/checkouts/${started.checkout}`);
  assert.equal(object.status, "complete");
  await assert.rejects(pay_api.run("shop", "POST", "/checkouts"), /answers GET/);
});

test("the sweep processes what was never delivered, and nothing twice", async () => {
  await paid(held, 12);
  assert.equal(store.tables.pay_events.length, 0);
  const swept = await sweepAll(store.db);
  assert.deepEqual(swept.map((s) => [s.account, s.events, s.logged]), [["shop", 0, 0], ["held", 2, 2]]);
  assert.deepEqual(kinds(), [["checkout.completed", 12], ["payment.succeeded", 12]]);
  assert.ok(store.tables.pay_accounts.every((a) => a.synced_at));
  // Again: the same events, all of them seen.
  const again = await sweepAll(store.db, { only: "held" });
  assert.deepEqual(again.map((s) => s.logged), [0]);
  assert.equal(store.tables.pay_events.length, 2);
  await assert.rejects(sweepAll(store.db, { only: "nope" }), /there is no payment account "nope"/);
});
