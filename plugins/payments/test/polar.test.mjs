// The Polar adapter (TODO-payments P5.1, P5.3) with no network: requests checked as Polar would
// receive them, answers in the shapes of Polar's OpenAPI description (2026-10). What subscriptions
// and the shop do on Polar end to end is the shared suite's (`shared.test.mjs`, over the
// simulation in `providers/polar-sim.mjs`); this is what that suite does not see.

import { test, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import crypto from "node:crypto";
import { polarSimulation } from "./providers/polar-sim.mjs";
import { fakeDb } from "./fake-db.mjs";

const require = createRequire(import.meta.url);
const polar = require("../adapters/polar.js");
const { prepareCheckout } = require("../checkout.js");

const PRODUCT = "11111111-1111-4111-8111-111111111111";
const PLAN = "22222222-2222-4222-8222-222222222222";
const ORDER = "33333333-3333-4333-8333-333333333333";
const CUSTOMER = "44444444-4444-4444-8444-444444444444";
const SUB = "55555555-5555-4555-8555-555555555555";
const account = { id: 4, name: "mor", adapter: "polar", mode: "test", enabled: true, secret_key: "polar_oat_example", webhook_secret: "whsec_dGhlIHNlY3JldA==", settings: { product: PRODUCT, grants_in_test_mode: true } };

let sent;
let answers;
const send = polar.transport.send;
beforeEach(() => {
  sent = [];
  answers = [];
  polar.transport.send = async (url, init) => {
    const u = new URL(url);
    sent.push({ method: init.method, host: u.host, path: u.pathname, query: u.searchParams, headers: init.headers, body: init.body ? JSON.parse(init.body) : null });
    const [status, body] = answers.shift() || [500, { detail: "no answer queued" }];
    return { ok: status < 300, status, text: async () => JSON.stringify(body) };
  };
});
afterEach(() => {
  polar.transport.send = send;
});

const checkoutAnswer = (extra = {}) => ({ id: "66666666-6666-4666-8666-666666666666", status: "open", url: "https://sandbox.polar.sh/checkout/polar_c_x", total_amount: 1150, currency: "eur", metadata: {}, created_at: "2026-10-23T10:00:00.000Z", ...extra });
const order = (extra = {}) => ({ id: ORDER, status: "paid", net_amount: 1000, tax_amount: 200, total_amount: 1200, refundable_amount: 1000, refundable_tax_amount: 200, refunded_amount: 0, refunded_tax_amount: 0, currency: "eur", customer_id: CUSTOMER, metadata: {}, items: [], created_at: "2026-10-23T10:00:00.000Z", ...extra });

/** A delivery signed as Polar signs it: with `key` over `id.timestamp.body`. */
function delivery(payload, key, { id = "msg_1", t = Math.floor(Date.now() / 1000) } = {}) {
  const body = Buffer.from(JSON.stringify(payload));
  return { rawBody: body, headers: { "webhook-id": id, "webhook-timestamp": String(t), "webhook-signature": polar.sign(key, id, t, body) } };
}

test("a one-off checkout charges the lines' total as an ad-hoc price of the account's product, on the sandbox", async () => {
  answers.push([201, checkoutAnswer()]);
  const created = await polar.createCheckout(account, {
    currency: "EUR",
    lines: [
      { description: "Tea", quantity: 2, unit_amount: 450 },
      { description: "Cup", quantity: 1, unit_amount: 250 },
    ],
    customer: { email: "ann@example.com", user_id: 7 },
    success_url: "https://shop.example.com/thanks",
    cancel_url: "https://shop.example.com/basket",
    metadata: { sc_ref_table: "orders", sc_ref_key: "12" },
  });
  assert.equal(created.url, "https://sandbox.polar.sh/checkout/polar_c_x");
  assert.deepEqual([created.object.amount, created.object.currency, created.object.status], [1150, "EUR", "open"]);
  const [req] = sent;
  assert.deepEqual([req.method, req.host, req.path], ["POST", "sandbox-api.polar.sh", "/v1/checkouts/"]);
  assert.equal(req.headers.authorization, "Bearer polar_oat_example");
  assert.equal(req.headers["polar-version"], "2026-10");
  assert.deepEqual(req.body, {
    success_url: "https://shop.example.com/thanks",
    return_url: "https://shop.example.com/basket",
    metadata: { sc_ref_table: "orders", sc_ref_key: "12" },
    products: [PRODUCT],
    currency: "eur",
    prices: { [PRODUCT]: [{ amount_type: "fixed", price_amount: 1150, price_currency: "eur" }] },
    customer_email: "ann@example.com",
    // The user's id, which is what binds the customer to them (security review #35).
    external_customer_id: "7",
  });
  // A live account reaches the production API, pinned to the version its settings name.
  answers.push([201, checkoutAnswer()]);
  await polar.createCheckout({ ...account, mode: "live", settings: { product: PRODUCT, api_version: "2027-01" } }, { currency: "EUR", lines: [{ unit_amount: 100 }], success_url: "https://a.example/" });
  assert.deepEqual([sent[1].host, sent[1].headers["polar-version"]], ["api.polar.sh", "2027-01"]);
});

test("a subscription checkout is for one product, its trial in days, and the customer the user already is", async () => {
  answers.push([201, checkoutAnswer({ total_amount: 0 })]);
  await polar.createCheckout(account, {
    mode: "subscription",
    currency: "EUR",
    lines: [{ price_ref: PLAN, quantity: 1, trial_days: 14 }],
    customer: { ref: CUSTOMER, email: "ann@example.com" },
    success_url: "https://saas.example.com/welcome",
  });
  const body = sent[0].body;
  assert.deepEqual([body.products, body.trial_interval, body.trial_interval_count, body.customer_id, body.customer_email, body.prices], [[PLAN], "day", 14, CUSTOMER, undefined, undefined]);
  await assert.rejects(polar.createCheckout(account, { mode: "subscription", currency: "EUR", lines: [{ price_ref: PLAN, quantity: 3 }], success_url: "https://a.example/" }), /one of a product, not 3/);
  await assert.rejects(polar.createCheckout(account, { mode: "subscription", currency: "EUR", lines: [{ price_ref: "price_basic" }], success_url: "https://a.example/" }), /not a Polar product id/);
  await assert.rejects(polar.createCheckout({ ...account, settings: {} }, { currency: "EUR", lines: [{ unit_amount: 100 }], success_url: "https://a.example/" }), /no product for one-off payments/);
  await assert.rejects(polar.createCheckout(account, { currency: "EUR", lines: [{ unit_amount: 100 }], shipping: { countries: ["GB"] }, success_url: "https://a.example/" }), /digital goods only/);
  await assert.rejects(polar.createCheckout(account, { currency: "EUR", lines: [{ unit_amount: 100 }], success_url: "/thanks" }), /start the checkout through an application/);
  assert.equal(sent.length, 1);
});

test("a webhook verifies with either key a secret stands for, over the exact bytes, and not too old", () => {
  const payload = { type: "order.refunded", timestamp: "2026-10-23T10:00:00Z", data: order({ status: "partially_refunded" }) };
  // Secrets made since 8 September 2026: the standard's base64 key after whsec_.
  const standard = delivery(payload, Buffer.from("the secret"));
  assert.deepEqual(polar.verifyWebhook(account, standard), [{ id: "msg_1", type: "order.refunded", ref: { kind: "payment", id: ORDER }, raw: payload }]);
  // Older secrets: the UTF-8 bytes of the whole string.
  const older = { ...account, webhook_secret: "polar_whs_older" };
  assert.equal(polar.verifyWebhook(older, delivery(payload, Buffer.from("polar_whs_older"))).length, 1);
  assert.equal(polar.verifyWebhook(account, delivery(payload, Buffer.from(account.webhook_secret))).length, 1);
  // Several signatures, one of them right.
  const both = { ...standard, headers: { ...standard.headers, "webhook-signature": `v1,${Buffer.alloc(32).toString("base64")} ${standard.headers["webhook-signature"]}` } };
  assert.equal(polar.verifyWebhook(account, both).length, 1);
  for (const [d, refusal] of [
    [delivery(payload, Buffer.from("another secret")), /does not match/],
    [{ ...standard, rawBody: Buffer.from(JSON.stringify(payload, null, 1)) }, /does not match/],
    [{ ...standard, headers: { ...standard.headers, "webhook-id": "msg_2" } }, /does not match/],
    [delivery(payload, Buffer.from("the secret"), { t: Math.floor(Date.now() / 1000) - 600 }), /too old/],
    [{ ...standard, headers: { "webhook-id": "msg_1" } }, /lacks its webhook-id, webhook-timestamp or webhook-signature/],
  ]) {
    assert.throws(() => polar.verifyWebhook(account, d), refusal);
  }
  assert.throws(() => polar.verifyWebhook({ ...account, webhook_secret: "" }, standard), /no webhook signing secret/);
});

test("Polar's events in the module's words: a renewal order is two events, a checkout's success one per checkout", () => {
  const key = Buffer.from("the secret");
  const renewal = polar.verifyWebhook(account, delivery({ type: "order.paid", data: order({ billing_reason: "subscription_cycle", subscription_id: SUB }) }, key, { id: "msg_r" }));
  assert.deepEqual(renewal.map((e) => [e.id, e.ref]), [["msg_r", { kind: "payment", id: ORDER }], ["msg_r", { kind: "subscription", id: SUB }]]);
  assert.deepEqual(renewal.map((e) => polar.kinds(e)), [["payment.succeeded"], ["subscription.renewed"]]);
  const first = polar.verifyWebhook(account, delivery({ type: "order.paid", data: order({ billing_reason: "subscription_create", subscription_id: SUB }) }, key));
  assert.deepEqual(first.map((e) => polar.kinds(e)), [["payment.succeeded"]]);

  const done = (id) => polar.verifyWebhook(account, delivery({ type: "checkout.updated", data: { id: "77777777-7777-4777-8777-777777777777", status: "succeeded" } }, key, { id }));
  assert.deepEqual([done("msg_a")[0].id, done("msg_b")[0].id], ["77777777-7777-4777-8777-777777777777:succeeded", "77777777-7777-4777-8777-777777777777:succeeded"]);
  assert.deepEqual(polar.kinds(done("msg_a")[0]), ["checkout.completed"]);

  const event = (type, data = {}) => ({ type, ref: null, raw: { type, data } });
  assert.deepEqual(polar.kinds(event("checkout.updated", { status: "failed" })), ["payment.failed"]);
  assert.equal(polar.kinds(event("checkout.updated", { status: "confirmed" })), null);
  assert.deepEqual(polar.kinds(event("subscription.revoked")), ["subscription.ended"]);
  assert.deepEqual(polar.kinds(event("subscription.past_due")), ["subscription.past_due"]);
  assert.equal(polar.kinds(event("subscription.cycled")), null);
  assert.deepEqual(polar.kinds(event("benefit.created")), []);
  // A plan change is told from the mirror: Polar's catch-all event says no more than what is.
  const previous = { price_ref: PLAN, provider_data: { modified_at: "2026-10-23T10:00:00.000Z" } };
  const updated = (product, at) => event("subscription.updated", { product_id: product, modified_at: at });
  assert.deepEqual(polar.kinds(updated(PRODUCT, "2026-10-23T11:00:00.000Z"), { previous }), ["subscription.changed"]);
  assert.equal(polar.kinds(updated(PLAN, "2026-10-23T11:00:00.000Z"), { previous }), null, "a renewal, a cancellation: no change of plan");
  assert.equal(polar.kinds(updated(PRODUCT, "2026-10-23T09:00:00.000Z"), { previous }), null, "older than the mirror: no change");
  assert.equal(polar.kinds(updated(PRODUCT, "2026-10-23T11:00:00.000Z"), { previous: null }), null);
  for (const type of ["checkout.updated", "order.paid", "order.refunded", "subscription.created", "subscription.updated", "subscription.canceled", "subscription.revoked", "subscription.past_due"]) {
    assert.ok(polar.WEBHOOK_EVENTS.includes(type), type);
  }
});

test("objects are read back and normalised: an order is a payment of what the buyer paid, tax included", async () => {
  answers.push([200, order({ status: "partially_refunded", refunded_amount: 300, refunded_tax_amount: 60, checkout_id: "66666666-6666-4666-8666-666666666666", customer: { email: "ann@example.com" } })]);
  const paid = await polar.fetch(account, { kind: "payment", id: ORDER });
  assert.deepEqual(
    [paid.kind, paid.status, paid.amount, paid.currency, paid.amount_refunded, paid.email, paid.checkout_ref, paid.customer_ref],
    ["payment", "partially_refunded", 1200, "EUR", 360, "ann@example.com", "66666666-6666-4666-8666-666666666666", CUSTOMER],
  );
  assert.equal(sent[0].path, `/v1/orders/${ORDER}`);
  answers.push([200, { id: SUB, status: "active", amount: 1000, currency: "eur", product_id: PLAN, current_period_end: "2026-11-23T10:00:00.000Z", trial_end: null, cancel_at_period_end: true, ends_at: "2026-11-23T10:00:00.000Z", customer_id: CUSTOMER, checkout_id: null, metadata: {}, created_at: "2026-10-23T10:00:00.000Z" }]);
  const sub = await polar.fetch(account, { kind: "subscription", id: SUB });
  assert.deepEqual([sub.price_ref, sub.quantity, sub.cancel_at_period_end, sub.cancel_at, sub.current_period_end], [PLAN, 1, true, "2026-11-23T10:00:00.000Z", "2026-11-23T10:00:00.000Z"]);
  await assert.rejects(polar.fetch(account, { kind: "payment", id: "../customers" }), /not a Polar payment id/);
  answers.push([404, { error: "ResourceNotFound", detail: "Order not found" }]);
  await assert.rejects(polar.fetch(account, { kind: "payment", id: ORDER }), /Polar refused GET \/v1\/orders\/.*: Order not found \(ResourceNotFound\)/);
});

test("a refund of what the buyer paid is Polar's pre-tax amount, once per key, with Polar's reasons", async () => {
  answers.push([200, { items: [], pagination: { total_count: 0, max_page: 1 } }], [200, order()], [201, { id: "r1", order_id: ORDER, amount: 250, tax_amount: 50, currency: "eur", status: "succeeded", metadata: { sc_key: "k1" }, created_at: "2026-10-23T10:00:00Z" }]);
  const refund = await polar.refund(account, ORDER, { amount: 300, key: "k1", reason: "requested_by_customer" });
  assert.deepEqual([refund.amount, refund.currency, refund.payment_ref], [300, "EUR", ORDER]);
  assert.deepEqual(sent[2].body, { order_id: ORDER, amount: 250, reason: "customer_request", metadata: { sc_key: "k1" } });
  // Asked again with the key: the refund already made.
  answers.push([200, { items: [{ id: "r1", order_id: ORDER, amount: 250, tax_amount: 50, currency: "eur", status: "succeeded", metadata: { sc_key: "k1" } }], pagination: { total_count: 1, max_page: 1 } }]);
  assert.equal((await polar.refund(account, ORDER, { amount: 300, key: "k1" })).ref, "r1");
  assert.equal(sent.length, 4);
  // All that is left, with a reason of the application's own.
  answers.push([200, order({ refundable_amount: 750, refundable_tax_amount: 150 })], [201, { id: "r2", order_id: ORDER, amount: 750, tax_amount: 150, currency: "eur", status: "succeeded", metadata: {} }]);
  await polar.refund(account, ORDER, { reason: "the tea was cold" });
  assert.deepEqual(sent[5].body, { order_id: ORDER, amount: 750, reason: "other", comment: "the tea was cold", metadata: {} });
  answers.push([200, order()]);
  await assert.rejects(polar.refund(account, ORDER, { amount: 1300 }), /more than the 1200 left/);
});

test("the portal is a customer session; a plan change prorates or invoices, and a cancel now revokes", async () => {
  answers.push([201, { id: "s1", token: "t", customer_portal_url: "https://sandbox.polar.sh/portal?customer_session_token=t", customer_id: CUSTOMER }]);
  const portal = await polar.createPortal(account, { customer: CUSTOMER, return_url: "https://saas.example.com/account" });
  assert.equal(portal.url, "https://sandbox.polar.sh/portal?customer_session_token=t");
  assert.deepEqual([sent[0].path, sent[0].body], ["/v1/customer-sessions/", { customer_id: CUSTOMER, return_url: "https://saas.example.com/account" }]);
  const sub = { id: SUB, status: "active", product_id: PRODUCT, currency: "eur", metadata: {} };
  answers.push([200, sub]);
  await polar.changePlan(account, SUB, PRODUCT, { proration: "invoice" });
  assert.deepEqual([sent[1].method, sent[1].path, sent[1].body], ["PATCH", `/v1/subscriptions/${SUB}`, { product_id: PRODUCT, proration_behavior: "invoice" }]);
  await assert.rejects(polar.changePlan(account, SUB, PRODUCT, { proration: "none" }), /Polar cannot change a plan with proration "none"/);
  answers.push([200, { ...sub, cancel_at_period_end: true }], [200, { ...sub, status: "canceled" }]);
  await polar.cancel(account, SUB, { at_period_end: true });
  assert.deepEqual([sent[2].method, sent[2].body], ["PATCH", { cancel_at_period_end: true }]);
  await polar.cancel(account, SUB, { at_period_end: false });
  assert.deepEqual([sent[3].method, sent[3].path], ["DELETE", `/v1/subscriptions/${SUB}`]);
});

test("the sweep reads the endpoint's deliveries, and processes the events never delivered", async () => {
  const endpoint = "88888888-8888-4888-8888-888888888888";
  const withEndpoint = { ...account, settings: { ...account.settings, webhook_endpoint_id: endpoint } };
  const evt = (id, succeeded, payload, at) => ({ id: `d_${id}`, succeeded, http_code: succeeded ? 200 : null, webhook_event: { id, succeeded, created_at: at, payload: JSON.stringify(payload) } });
  answers.push([
    200,
    {
      items: [
        evt("e2", false, { type: "order.refunded", data: order() }, "2026-10-23T10:02:00Z"),
        evt("e1", false, { type: "subscription.created", data: { id: SUB } }, "2026-10-23T10:01:00Z"),
        evt("e1", false, { type: "subscription.created", data: { id: SUB } }, "2026-10-23T10:01:00Z"),
        evt("e0", true, { type: "order.paid", data: order() }, "2026-10-23T10:00:00Z"),
      ],
      pagination: { total_count: 4, max_page: 1 },
    },
  ]);
  const listed = await polar.listEvents(withEndpoint, { since: 1792750000 });
  assert.deepEqual(listed.map((e) => [e.id, e.type]), [["e1", "subscription.created"], ["e2", "order.refunded"]]);
  assert.equal(sent[0].path, "/v1/webhooks/deliveries");
  assert.equal(sent[0].query.get("endpoint_id"), endpoint);
  assert.equal(sent[0].query.get("start_timestamp"), new Date(1792750000 * 1000).toISOString());
  // With no endpoint named there is nothing to read.
  assert.deepEqual(await polar.listEvents(account, {}), []);
  assert.equal(sent.length, 1);
});

test("a checkout on a merchant of record is one whatever the tax setting says, and ships nothing", async () => {
  const store = fakeDb({ pay_accounts: [{ ...account }], orders: [{ id: 12, total: 900 }] });
  const base = { account: "mor", table: "orders", key_field: "order", amount: "total", currency: "EUR", read_check: false };
  const argument = (configuration) => ({ configuration, payload: { order: 12 }, user: null, app: { url: "https://shop.example.com" }, db: store.db });
  assert.equal((await prepareCheckout(argument({ ...base, tax: "none" }))).tax, "merchant_of_record");
  await assert.rejects(prepareCheckout(argument({ ...base, shipping: true, shipping_countries: "GB" })), /a merchant of record sells digital goods only/);
});

test("P5.3: a saved card is charged with an off-session order, or the buyer is sent to a checkout", async () => {
  // On the simulation, whose customers and finalize behave as Polar documents.
  polar.transport.send = send;
  const sim = polarSimulation();
  const acct = sim.account(1, "credits");
  acct._offSession = true;
  sim.install([acct], () => async () => 200);
  try {
    const c = sim.simulation;
    const customer = { id: crypto.randomUUID(), email: "ann@example.com", metadata: {}, _key: acct.secret_key, _card: "ok" };
    c.customers.set(customer.id, customer);
    const charge = (key) => polar.chargeSavedMethod(acct, { customer: customer.id, amount: 200, currency: "EUR", description: "100 credits", key });
    const paid = await charge("topup:1");
    assert.deepEqual([paid.status, paid.object.kind, paid.object.status, paid.object.amount], ["paid", "payment", "succeeded", 200]);
    // The same key again: the order already paid, not a second charge.
    const again = await charge("topup:1");
    assert.equal(again.object.ref, paid.object.ref);
    assert.equal([...c.orders.values()].length, 1);
    // The bank wants the buyer to authenticate: a checkout instead, and the draft is kept for the key.
    customer._card = "3ds";
    const sca = await charge("topup:2");
    assert.deepEqual([sca.status, sca.reason], ["requires_checkout", "The payment requires the customer to authenticate."]);
    customer._card = "ok";
    assert.equal((await charge("topup:2")).status, "paid");
    assert.equal([...c.orders.values()].length, 2, "the draft was finalized, not made again");
    // Off-session charges not enabled for the organisation: refused by name.
    acct._offSession = false;
    await assert.rejects(charge("topup:3"), /cannot charge a saved card: Off-session charges are not enabled .* in preview, for organisations on a paid plan/);
  } finally {
    sim.uninstall();
  }
});
