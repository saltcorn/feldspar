// The webhook handler's processing (TODO-payments §6.5, P2.3), over the fake provider and an
// in-memory `db`: dedupe, fetch-and-upsert, normalised events, and what a failure leaves.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "./fake-db.mjs";

const require = createRequire(import.meta.url);
const fake = require("../adapters/fake.js");
const { events, deliverTo } = require("../webhooks.js");

const account = { id: 1, name: "shop", adapter: "fake", mode: "test", enabled: true, webhook_secret: "whsec_test", settings: { webhooks: "hold" } };

let store;
let ctx;
beforeEach(() => {
  fake.reset();
  store = fakeDb({ pay_accounts: [{ ...account }] });
  ctx = { db: store.db, log: Object.assign(() => {}, { warn: () => {}, error: () => {} }) };
});

/** A checkout the shop made for its order 12, mirrored as `pay_checkout` mirrors it. */
async function orderCheckout() {
  const created = await fake.createCheckout(account, {
    currency: "EUR",
    lines: [{ description: "Tea", quantity: 2, unit_amount: 450 }],
    metadata: { sc_ref_table: "orders", sc_ref_key: "12", sc_user_id: "7" },
  });
  await store.db.pay_objects.insert({
    account: 1, kind: "checkout", provider_ref: created.id, ref_table: "orders", ref_key: 12, user_id: 7, status: "open", amount: 900, currency: "EUR",
  });
  return created;
}

const deliver = () => deliverTo(account, ctx);
const kinds = () => store.tables.pay_events.map((e) => e.kind);

test("each event is logged once, with the application row it is for, however often it arrives", async () => {
  const created = await orderCheckout();
  await fake.settle(account, created.id, "succeeded", deliver());
  const [completed, succeeded] = fake.events(account);
  for (const id of [completed.id, succeeded.id, succeeded.id, completed.id, succeeded.id]) {
    assert.equal(((await fake.resend(account, id, deliver())) || {}).status, 200);
  }
  assert.deepEqual(kinds(), ["checkout.completed", "payment.succeeded"]);
  const paid = store.tables.pay_events[1];
  assert.equal(paid.ref_table, "orders");
  assert.equal(paid.ref_key, 12);
  assert.equal(paid.user_id, 7);
  assert.equal(paid.amount, 900);
  assert.equal(paid.status, "succeeded");
  assert.equal(paid.provider_type, "payment.succeeded");
  assert.equal(paid.raw.id, succeeded.id);
  // The payment's own mirror row inherits the checkout's application row.
  const payment = store.tables.pay_objects.find((o) => o.kind === "payment");
  assert.equal(payment.ref_key, 12);
  assert.equal(paid.object, payment.id);
  assert.equal(store.tables.pay_objects.find((o) => o.kind === "checkout").status, "complete");
});

test("events out of order end in the provider's current state", async () => {
  const created = await orderCheckout();
  await fake.settle(account, created.id, "failed", deliver());
  await fake.settle(account, created.id, "succeeded", deliver());
  const [failed, completed, succeeded] = fake.events(account);
  // The success first, the decline last: the mirror says what the provider says now.
  for (const e of [succeeded, completed, failed]) await fake.resend(account, e.id, deliver());
  assert.deepEqual(kinds(), ["payment.succeeded", "checkout.completed", "payment.failed"]);
  const byRef = Object.fromEntries(store.tables.pay_objects.map((o) => [o.provider_ref, o.status]));
  assert.equal(byRef[created.id], "complete");
  assert.equal(byRef[succeeded.ref.id], "succeeded");
  assert.equal(byRef[failed.ref.id], "failed");
});

test("an object the mirror has never seen takes its row from the provider's metadata", async () => {
  const { fromMetadata } = require("../processing.js");
  assert.deepEqual(fromMetadata({ sc_ref_table: "orders", sc_ref_key: "12", sc_user_id: "7" }), { ref_table: "orders", ref_key: 12, user_id: 7 });
  assert.deepEqual(fromMetadata({ sc_ref_table: "invoices", sc_ref_key: '"12"' }), { ref_table: "invoices", ref_key: "12" });
  assert.deepEqual(fromMetadata({ test: true }), {});
});

test("an event the module does not understand is logged as other, with the raw event", async () => {
  const body = Buffer.from(JSON.stringify({ id: "evt_odd", type: "payout.paid", account: "shop", data: { object: null } }));
  const answer = await events({ rawBody: body, headers: { "fake-signature": fake.sign("whsec_test", body) }, rest: "shop" }, ctx);
  assert.equal(answer.status, 200);
  assert.deepEqual(store.tables.pay_events.map((e) => [e.kind, e.provider_type, e.raw.id, e.object]), [["other", "payout.paid", "evt_odd", undefined]]);
});

test("a bad signature stores nothing; an object the provider cannot read is a failure the retry finishes", async () => {
  const created = await orderCheckout();
  await fake.settle(account, created.id, "succeeded", deliver());
  const [completed, succeeded] = fake.events(account);
  const forged = await events(
    { rawBody: Buffer.from(JSON.stringify({ id: "evt_x", type: "payment.succeeded", account: "shop" })), headers: { "fake-signature": "t=1,v1=00" }, rest: "shop" },
    ctx,
  );
  assert.equal(forged.status, 400);
  assert.equal(store.tables.pay_events.length, 0);

  // The event row's insert fails once: the delivery fails (a 500), and its retry logs it once.
  store.failInsert("pay_events");
  const failed = await fake.resend(account, completed.id, deliver());
  assert.equal(failed.status, 500);
  assert.match(failed.error, /insert into pay_events failed/);
  assert.equal(store.tables.pay_events.length, 0);
  assert.equal((await fake.resend(account, completed.id, deliver())).status, 200);
  assert.equal((await fake.resend(account, succeeded.id, deliver())).status, 200);
  assert.deepEqual(kinds(), ["checkout.completed", "payment.succeeded"]);

  // An event about an object the provider does not have (a restarted fake) cannot be processed.
  const body = Buffer.from(JSON.stringify({ id: "evt_gone", type: "payment.succeeded", account: "shop", data: { object: { kind: "payment", id: "pay_gone" } } }));
  await assert.rejects(
    events({ rawBody: body, headers: { "fake-signature": fake.sign("whsec_test", body) }, rest: "shop" }, ctx),
    /evt_gone is about payment pay_gone, which could not be read/,
  );
  assert.equal(store.tables.pay_events.length, 2);
});

test("a refund is an event about the payment", async () => {
  const created = await orderCheckout();
  await fake.settle(account, created.id, "succeeded", deliver());
  for (const e of fake.events(account)) await fake.resend(account, e.id, deliver());
  const payment = fake.events(account)[1].ref.id;
  const first = await fake.refund(account, payment, { amount: 300, key: "r1" }, deliver());
  assert.deepEqual(await fake.refund(account, payment, { amount: 300, key: "r1" }, deliver()), first);
  await assert.rejects(fake.refund(account, payment, { amount: 700 }, deliver()), /more than the 6.00 EUR left/);
  await fake.refund(account, payment, {}, deliver());
  for (const e of fake.events(account).slice(2)) await fake.resend(account, e.id, deliver());
  assert.deepEqual(kinds(), ["checkout.completed", "payment.succeeded", "payment.refunded", "payment.refunded"]);
  assert.equal(store.tables.pay_objects.find((o) => o.provider_ref === payment).status, "refunded");
});
