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

const account = { id: 1, name: "shop", adapter: "fake", mode: "test", enabled: true, webhook_secret: "whsec_test", settings: { webhooks: "hold", grants_in_test_mode: true } };

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

// Security review #33: a test-mode account takes no real money, so it grants nothing unless the
// admin opted it in; a live account grants.
test("a test-mode account grants no role or credits unless its settings say so", async () => {
  const { applyEffects } = require("../processing.js");
  const granted = [];
  const grantCtx = {
    ...ctx,
    roles: { grant: async (g) => granted.push(["role", g.role]), revoke: async () => ({ revoked: false }) },
    credits: { grant: async (g) => granted.push(["credits", g.amount]) },
  };
  const current = { kind: "payment", ref: "pay_test_1", status: "succeeded", created: new Date().toISOString() };
  const effects = { role: { role: 70, days: 30 }, credits: { amount: 1000 } };
  const effectsOf = async (acct) => {
    const row = await store.db.pay_objects.insert({ account: 1, kind: "payment", provider_ref: `${acct.mode}-${granted.length}-${Math.random()}`, user_id: 7, effects });
    return applyEffects(grantCtx, acct, { ...current, ref: row.provider_ref }, row);
  };

  const plain = await effectsOf({ ...account, settings: {} });
  assert.deepEqual(granted, []);
  assert.match(plain.issue, /is in test mode, so it grants no roles or LLM credits/);

  await effectsOf(account);
  assert.deepEqual(granted, [["role", 70], ["credits", 1000]]);

  granted.length = 0;
  const live = await effectsOf({ ...account, adapter: "stripe", mode: "live", settings: {} });
  assert.deepEqual(granted, [["role", 70], ["credits", 1000]]);
  assert.ok(!live.issue, live.issue);
});

// Security review #34: a payment refunded in full, or charged back, takes back its role and what is
// left of its credits; a partial refund takes back nothing.
test("a refunded or charged-back payment takes back its role and its credits", async () => {
  const { applyEffects } = require("../processing.js");
  const log = [];
  const takeCtx = {
    ...ctx,
    roles: { grant: async (g) => log.push(["grant role", g.ref]), revoke: async (ref) => log.push(["revoke role", ref]) && { revoked: true } },
    credits: { grant: async (g) => log.push(["grant credits", g.key]), revoke: async (r) => log.push(["revoke credits", r.key]) && { found: true } },
  };
  const effects = { role: { role: 70, days: 30 }, credits: { amount: 1000 } };
  for (const [status, expected] of [
    ["partially_refunded", ["grant role", "grant credits"]],
    ["refunded", ["revoke role", "revoke credits"]],
    ["charged_back", ["revoke role", "revoke credits"]],
  ]) {
    log.length = 0;
    const ref = `pay_${status}`;
    const row = await store.db.pay_objects.insert({ account: 1, kind: "payment", provider_ref: ref, user_id: 7, effects });
    await applyEffects(takeCtx, account, { kind: "payment", ref, status, created: new Date().toISOString() }, row);
    assert.deepEqual(log.map((l) => l[0]), expected, status);
    assert.deepEqual(log.map((l) => l[1]), [`shop:${ref}`, `payment:1:${ref}`], status);
  }
});

// Security review #35: a provider customer is bound to the user who paid only when the provider
// says it is theirs. Polar may attach a checkout to an existing customer found by email; a sign-up
// with somebody else's address must not get that customer's portal and saved card.
test("a customer the provider says is somebody else's is not bound to whoever paid", async () => {
  const { applyEffects } = require("../processing.js");
  const customerOf = (ref) => store.tables.pay_objects.find((o) => o.kind === "customer" && o.provider_ref === ref);
  const pay = async (ref, customer, external) => {
    const row = await store.db.pay_objects.insert({ account: 1, kind: "payment", provider_ref: ref, user_id: 7, email: "victim@example.com" });
    const current = { kind: "payment", ref, status: "open", customer_ref: customer };
    if (external !== undefined) current.customer_external_id = external;
    await applyEffects(ctx, account, current, row);
  };
  // The victim's customer (external id 8), reached by the attacker's (user 7) checkout.
  await pay("pay_a", "cus_victim", "8");
  assert.equal(customerOf("cus_victim").user_id ?? null, null);
  // A customer that never had an external id is nobody's either.
  await pay("pay_b", "cus_unnamed", null);
  assert.equal(customerOf("cus_unnamed").user_id ?? null, null);
  // The payer's own customer is theirs.
  await pay("pay_c", "cus_own", "7");
  assert.equal(customerOf("cus_own").user_id, 7);
  // An adapter that does not say (Stripe creates its customers per user) binds as before.
  await pay("pay_d", "cus_stripe", undefined);
  assert.equal(customerOf("cus_stripe").user_id, 7);
});
