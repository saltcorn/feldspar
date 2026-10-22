// The Stripe adapter (TODO-payments P2.5) against Stripe's responses, with no network: each
// request is checked as Stripe would receive it, and answered from `fixtures/stripe/`, which are
// responses in the shape Stripe's API documents. `stripe-live.test.mjs` makes the same calls
// against Stripe's test mode when a key is given.

import { test, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import crypto from "node:crypto";
import { fakeDb } from "./fake-db.mjs";

const require = createRequire(import.meta.url);
const stripe = require("../adapters/stripe.js");
const { processEvents } = require("../processing.js");

const fixture = (name) => JSON.parse(readFileSync(new URL(`./fixtures/stripe/${name}.json`, import.meta.url), "utf8"));
const account = {
  id: 3,
  name: "stripe_test",
  adapter: "stripe",
  mode: "test",
  enabled: true,
  secret_key: "sk_test_51ExampleKey",
  webhook_secret: "whsec_example_secret",
  settings: {},
};

/** The requests made, and the answers queued for them. */
let sent;
let answers;
const send = stripe.transport.send;
beforeEach(() => {
  sent = [];
  answers = [];
  stripe.transport.send = async (url, init) => {
    const u = new URL(url);
    sent.push({ method: init.method, host: u.host, path: u.pathname, query: u.searchParams, headers: init.headers, form: new URLSearchParams(init.body || "") });
    const [status, body] = answers.shift() || [500, { error: { message: "no answer queued" } }];
    return { ok: status < 300, status, text: async () => JSON.stringify(body) };
  };
});
afterEach(() => {
  stripe.transport.send = send;
});

const signed = (body, secret = account.webhook_secret, t = Math.floor(Date.now() / 1000)) => {
  const mac = crypto.createHmac("sha256", secret).update(`${t}.`).update(body).digest("hex");
  return `t=${t},v1=${mac}`;
};

test("a checkout is a Checkout Session with a price made for each line", async () => {
  answers.push([200, fixture("checkout_session")]);
  const created = await stripe.createCheckout(account, {
    currency: "EUR",
    lines: [
      { description: "Tea", quantity: 2, unit_amount: 450 },
      { description: "Cup", quantity: 1, unit_amount: 250 },
    ],
    customer: { email: "ann@example.com" },
    tax: "provider",
    shipping: { countries: ["GB", "IE"] },
    success_url: "https://shop.example.com/orders/12/thanks",
    cancel_url: "https://shop.example.com/basket",
    client_reference: "orders:12",
    metadata: { sc_ref_table: "orders", sc_ref_key: "12" },
  });
  assert.equal(created.id, "cs_test_a1B2c3D4e5F6g7H8i9J0");
  assert.match(created.url, /^https:\/\/checkout\.stripe\.com\//);
  assert.equal(created.object.amount, 1150);
  assert.equal(created.object.currency, "EUR");

  const [req] = sent;
  assert.deepEqual([req.method, req.host, req.path], ["POST", "api.stripe.com", "/v1/checkout/sessions"]);
  assert.equal(req.headers.authorization, "Bearer sk_test_51ExampleKey");
  assert.equal(req.headers["content-type"], "application/x-www-form-urlencoded");
  const form = Object.fromEntries(req.form);
  assert.equal(form.mode, "payment");
  assert.equal(form["line_items[0][price_data][currency]"], "eur");
  assert.equal(form["line_items[0][price_data][unit_amount]"], "450");
  assert.equal(form["line_items[0][price_data][product_data][name]"], "Tea");
  assert.equal(form["line_items[0][quantity]"], "2");
  assert.equal(form["line_items[1][price_data][unit_amount]"], "250");
  assert.equal(form["automatic_tax[enabled]"], "true");
  assert.equal(form["shipping_address_collection[allowed_countries][0]"], "GB");
  assert.equal(form["shipping_address_collection[allowed_countries][1]"], "IE");
  assert.equal(form.customer_email, "ann@example.com");
  assert.equal(form.client_reference_id, "orders:12");
  // The row it is for travels on the session and on its payment, which Stripe does not link.
  assert.equal(form["metadata[sc_ref_key]"], "12");
  assert.equal(form["payment_intent_data[metadata][sc_ref_table]"], "orders");
  assert.equal(form["managed_payments[enabled]"], undefined);
});

test("Managed Payments is the account's choice, and never ships anything", async () => {
  const mor = { currency: "USD", lines: [{ unit_amount: 500 }], tax: "merchant_of_record", success_url: "https://a.example/", cancel_url: "https://a.example/" };
  await assert.rejects(stripe.createCheckout(account, mor), /does not use Managed Payments/);
  const managed = { ...account, settings: { managed_payments: true } };
  await assert.rejects(stripe.createCheckout(managed, { ...mor, shipping: { countries: ["US"] } }), /digital goods only/);
  answers.push([200, fixture("checkout_session")]);
  await stripe.createCheckout(managed, mor);
  assert.equal(sent[0].form.get("managed_payments[enabled]"), "true");
  assert.equal(sent[0].form.get("automatic_tax[enabled]"), null);
});

test("what Stripe refuses, and what is refused before asking it", async () => {
  answers.push([400, fixture("error_invalid_request")]);
  await assert.rejects(
    stripe.createCheckout(account, { currency: "EUR", lines: [{ unit_amount: 100 }], success_url: "https://a.example/x", cancel_url: "https://a.example/" }),
    /Stripe refused POST \/v1\/checkout\/sessions: Not a valid URL \(url_invalid\)/,
  );
  // A return address is a full one, made from the application the checkout came through.
  await assert.rejects(stripe.createCheckout(account, { currency: "EUR", lines: [{ unit_amount: 100 }], success_url: "/thanks", cancel_url: "/" }), /start the checkout through an application/);
  await assert.rejects(stripe.fetch({ ...account, secret_key: "sk_live_x" }, { kind: "payment", id: "pi_1" }), /in test mode, but its secret key is a live key/);
  await assert.rejects(stripe.fetch({ ...account, secret_key: "" }, { kind: "payment", id: "pi_1" }), /has no secret key/);
  await assert.rejects(stripe.fetch(account, { kind: "payment", id: "../v1/customers" }), /is not a Stripe id/);
  assert.equal(sent.length, 1);
});

test("amounts follow Stripe's own counting of ISK, UGX and the three-decimal currencies", () => {
  assert.equal(stripe.toStripe(500, "ISK"), 50000);
  assert.equal(stripe.fromStripe(50000, "isk"), 500);
  assert.equal(stripe.toStripe(1999, "EUR"), 1999);
  assert.equal(stripe.toStripe(500, "JPY"), 500);
  assert.equal(stripe.toStripe(1250, "KWD"), 1250);
  assert.throws(() => stripe.toStripe(1255, "KWD"), /steps of 0.010/);
});

test("a webhook verifies over the exact bytes with any of its v1 signatures", () => {
  const body = Buffer.from(JSON.stringify(fixture("event_payment_intent_succeeded")));
  const [event] = stripe.verifyWebhook(account, { rawBody: body, headers: { "stripe-signature": signed(body) } });
  assert.deepEqual([event.id, event.type, event.ref], ["evt_3Q9zYxWvUt1Abc2D0kLmNoPq", "payment_intent.succeeded", { kind: "payment", id: "pi_3Q9zYxWvUt1Abc2D0aBcDeFg"}]);
  assert.deepEqual(stripe.kinds(event), ["payment.succeeded"]);
  // While a secret is rolled, Stripe signs with both; either one verifies.
  const t = Math.floor(Date.now() / 1000);
  const both = `t=${t},v1=${"0".repeat(64)},${signed(body, account.webhook_secret, t).split(",")[1]}`;
  assert.equal(stripe.verifyWebhook(account, { rawBody: body, headers: { "stripe-signature": both } }).length, 1);
  for (const [headers, refusal] of [
    [{}, /no stripe-signature header/],
    [{ "stripe-signature": signed(body, "whsec_other") }, /does not match/],
    [{ "stripe-signature": signed(body, account.webhook_secret, t - 600) }, /too old/],
    [{ "stripe-signature": "v1=abc" }, /not t=…,v1=…/],
  ]) {
    assert.throws(() => stripe.verifyWebhook(account, { rawBody: body, headers }), refusal);
  }
  assert.throws(() => stripe.verifyWebhook(account, { rawBody: Buffer.from(JSON.stringify(fixture("event_payment_intent_succeeded"), null, 1)), headers: { "stripe-signature": signed(body) } }), /does not match/);
  const live = { ...account, mode: "live", secret_key: "sk_live_x" };
  assert.throws(() => stripe.verifyWebhook(live, { rawBody: body, headers: { "stripe-signature": signed(body) } }), /from Stripe's test mode/);
});

test("objects are read back and normalised: a payment, a declined one, a refund", async () => {
  answers.push([200, fixture("payment_intent_succeeded")]);
  const paid = await stripe.fetch(account, { kind: "payment", id: "pi_3Q9zYxWvUt1Abc2D0aBcDeFg" });
  assert.equal(sent[0].path, "/v1/payment_intents/pi_3Q9zYxWvUt1Abc2D0aBcDeFg");
  assert.equal(sent[0].query.get("expand[0]"), "latest_charge");
  assert.deepEqual([paid.status, paid.amount, paid.currency, paid.email, paid.metadata.sc_ref_key], ["succeeded", 1150, "EUR", "ann@example.com", "12"]);

  answers.push([200, fixture("payment_intent_failed")]);
  const declined = await stripe.fetch(account, { kind: "payment", id: "pi_3Q9zYxWvUt1Abc2D1declined" });
  assert.deepEqual([declined.status, declined.amount, declined.currency], ["failed", 5000, "ISK"]);

  answers.push([200, { ...fixture("payment_intent_succeeded"), latest_charge: { ...fixture("payment_intent_succeeded").latest_charge, amount_refunded: 300 } }]);
  assert.equal((await stripe.fetch(account, { kind: "payment", id: "pi_3Q9zYxWvUt1Abc2D0aBcDeFg" })).status, "partially_refunded");

  answers.push([200, fixture("checkout_session_complete")]);
  const session = await stripe.fetch(account, { kind: "checkout", id: "cs_test_a1B2c3D4e5F6g7H8i9J0" });
  assert.deepEqual(session.lines.map((l) => [l.description, l.quantity, l.unit_amount]), [["Tea", 2, 450], ["Cup", 1, 250]]);
  assert.equal(session.payment_ref, "pi_3Q9zYxWvUt1Abc2D0aBcDeFg");
});

test("a refund is asked for once per key, and Stripe's reasons are its own", async () => {
  answers.push([200, fixture("payment_intent_succeeded")], [200, fixture("refund")]);
  const refund = await stripe.refund(account, "pi_3Q9zYxWvUt1Abc2D0aBcDeFg", { amount: 300, key: "refund:pi_3Q9:1", reason: "the tea was cold" });
  assert.deepEqual([refund.kind, refund.amount, refund.payment_ref], ["refund", 300, "pi_3Q9zYxWvUt1Abc2D0aBcDeFg"]);
  const post = sent[1];
  assert.deepEqual([post.method, post.path], ["POST", "/v1/refunds"]);
  assert.equal(post.headers["idempotency-key"], "refund:pi_3Q9:1");
  assert.equal(post.form.get("payment_intent"), "pi_3Q9zYxWvUt1Abc2D0aBcDeFg");
  assert.equal(post.form.get("amount"), "300");
  assert.equal(post.form.get("reason"), null);
  assert.equal(post.form.get("metadata[reason]"), "the tea was cold");
});

test("a Stripe event processed: the payment is mirrored from Stripe's current state, against its row", async () => {
  const store = fakeDb({ pay_accounts: [{ ...account }] });
  const body = Buffer.from(JSON.stringify(fixture("event_payment_intent_succeeded")));
  const events = stripe.verifyWebhook(account, { rawBody: body, headers: { "stripe-signature": signed(body) } });
  answers.push([200, fixture("payment_intent_succeeded")]);
  await processEvents(store.db, account, stripe, events);
  // A refund arrives as a charge event, about the same payment.
  const refunded = Buffer.from(JSON.stringify(fixture("event_charge_refunded")));
  answers.push([200, { ...fixture("payment_intent_succeeded"), latest_charge: { ...fixture("payment_intent_succeeded").latest_charge, amount_refunded: 300 } }]);
  await processEvents(store.db, account, stripe, stripe.verifyWebhook(account, { rawBody: refunded, headers: { "stripe-signature": signed(refunded) } }));
  assert.deepEqual(
    store.tables.pay_events.map((e) => [e.kind, e.ref_table, e.ref_key, e.amount, e.status]),
    [
      ["payment.succeeded", "orders", 12, 1150, "succeeded"],
      ["payment.refunded", "orders", 12, 1150, "partially_refunded"],
    ],
  );
  assert.equal(store.tables.pay_objects.length, 1);
});

test("the sweep reads Stripe's event log, oldest first", async () => {
  answers.push([200, fixture("events_page")]);
  const listed = await stripe.listEvents(account, { since: 1792650000 });
  assert.deepEqual(listed.map((e) => e.id), ["evt_old", "evt_new"]);
  assert.deepEqual(listed[1].ref, { kind: "checkout", id: "cs_test_a1B2c3D4e5F6g7H8i9J0" });
  assert.equal(sent[0].path, "/v1/events");
  assert.equal(sent[0].query.get("created[gte]"), "1792650000");
  assert.equal(sent[0].query.get("types[0]"), "checkout.session.completed");
});
