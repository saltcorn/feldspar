// The webhook handler's processing (TODO-payments §6.5, P2.3), over the fake provider and an
// in-memory `db`: the row an object was for from the provider's metadata, an object that cannot
// be read, and the fake's refunds. Dedupe, events out of order, `other`, a forged signature and
// a delivery failing half way are the shared suite's (`shared.test.mjs`), against every provider.

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

test("an object the mirror has never seen takes its row from the provider's metadata", async () => {
  const { fromMetadata } = require("../processing.js");
  assert.deepEqual(fromMetadata({ sc_ref_table: "orders", sc_ref_key: "12", sc_user_id: "7" }), { ref_table: "orders", ref_key: 12, user_id: 7 });
  assert.deepEqual(fromMetadata({ sc_ref_table: "invoices", sc_ref_key: '"12"' }), { ref_table: "invoices", ref_key: "12" });
  assert.deepEqual(fromMetadata({ test: true }), {});
});

test("an event about an object the provider cannot read fails, naming the event", async () => {
  // A restarted fake has forgotten its objects: the delivery is a 500, and nothing is logged.
  const body = Buffer.from(JSON.stringify({ id: "evt_gone", type: "payment.succeeded", account: "shop", data: { object: { kind: "payment", id: "pay_gone" } } }));
  await assert.rejects(
    events({ rawBody: body, headers: { "fake-signature": fake.sign("whsec_test", body) }, rest: "shop" }, ctx),
    /evt_gone is about payment pay_gone, which could not be read/,
  );
  assert.equal(store.tables.pay_events.length, 0);
});

test("the fake refunds once per key, never more than is left, and reports each refund", async () => {
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
