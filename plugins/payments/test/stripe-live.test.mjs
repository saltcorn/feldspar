// The Stripe adapter against Stripe itself, in test mode (TODO-payments P2.5, P4.2). It runs only
// when a test-mode secret key is given — `STRIPE_TEST_SECRET_KEY=sk_test_… npm test` — as the
// CmdStan tests run only where CmdStan is installed. Nothing it makes is ever paid. The
// subscription tests run on a Stripe **test clock**, which moves a customer's time forward so a
// trial ends and a renewal falls due in seconds.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const stripe = require("../adapters/stripe.js");
const plans = require("../plans.js");

const key = process.env.STRIPE_TEST_SECRET_KEY;
const skip = key ? false : "set STRIPE_TEST_SECRET_KEY to a Stripe test-mode secret key to run";
const account = { id: 1, name: "live_test", adapter: "stripe", mode: "test", enabled: true, secret_key: key, settings: {} };

test("a Checkout Session is made, read back and listed in Stripe's test mode", { skip }, async () => {
  const created = await stripe.createCheckout(account, {
    currency: "EUR",
    lines: [
      { description: "Tea", quantity: 2, unit_amount: 450 },
      { description: "Cup", quantity: 1, unit_amount: 250 },
    ],
    customer: { email: "ann@example.com" },
    success_url: "https://shop.example.com/thanks",
    cancel_url: "https://shop.example.com/basket",
    client_reference: "orders:12",
    metadata: { sc_ref_table: "orders", sc_ref_key: "12" },
  });
  assert.match(created.id, /^cs_test_/);
  assert.match(created.url, /^https:\/\/checkout\.stripe\.com\//);
  const read = await stripe.fetch(account, { kind: "checkout", id: created.id });
  assert.equal(read.status, "open");
  assert.equal(read.amount, 1150);
  assert.equal(read.currency, "EUR");
  assert.deepEqual(read.lines.map((l) => [l.description, l.quantity, l.unit_amount]), [["Tea", 2, 450], ["Cup", 1, 250]]);
  assert.equal(read.metadata.sc_ref_key, "12");
  const since = Math.floor(Date.now() / 1000) - 3600;
  const listed = await stripe.list(account, "checkout", { since });
  assert.ok(listed.some((c) => c.ref === created.id));
  assert.ok(Array.isArray(await stripe.listEvents(account, { since })));
});

/** A Stripe test clock with a customer on it, paying by a test Visa card, and a monthly price; deleted
 * (with the customer) when `body` is done. */
async function onATestClock(body) {
  const api = stripe.client(account);
  const now = Math.floor(Date.now() / 1000);
  const clock = await api.post("/v1/test_helpers/test_clocks", { frozen_time: now, name: "feldspar P4.2" });
  try {
    const price = await api.post("/v1/prices", {
      currency: "eur",
      unit_amount: 1500,
      recurring: { interval: "month" },
      product_data: { name: "Feldspar test plan" },
    });
    const customer = await api.post("/v1/customers", {
      email: "ann@example.com",
      test_clock: clock.id,
      payment_method: "pm_card_visa",
      invoice_settings: { default_payment_method: "pm_card_visa" },
    });
    /** Move the clock to `to` (unix seconds), and wait for Stripe to have caught up. */
    const advance = async (to) => {
      await api.post(`/v1/test_helpers/test_clocks/${clock.id}/advance`, { frozen_time: to });
      for (let i = 0; i < 60; i += 1) {
        const state = await api.get(`/v1/test_helpers/test_clocks/${clock.id}`);
        if (state.status === "ready") return;
        await new Promise((r) => setTimeout(r, 2000));
      }
      throw new Error("the test clock did not finish advancing in two minutes");
    };
    await body({ api, price, customer, advance });
  } finally {
    await api.del(`/v1/test_helpers/test_clocks/${clock.id}`).catch(() => {});
  }
}

const read = (ref) => stripe.fetch(account, { kind: "subscription", id: ref });
const secondsOf = (iso) => Math.floor(Date.parse(iso) / 1000);

test("on a test clock: a trial converts, a renewal fails and then recovers within grace", { skip, timeout: 600000 }, async () => {
  await onATestClock(async ({ api, price, customer, advance }) => {
    const created = await api.post("/v1/subscriptions", {
      customer: customer.id,
      items: [{ price: price.id }],
      trial_period_days: 7,
      metadata: { sc_ref_table: "plans", sc_ref_key: "1", sc_user_id: "7" },
    });
    let sub = await read(created.id);
    assert.equal(sub.status, "trialing");
    assert.equal(sub.price_ref, price.id);
    assert.equal(sub.amount, 1500);
    assert.deepEqual(plans.subscriptionGrant(sub, 3), { op: "grant", expires_at: new Date(Date.parse(sub.trial_end) + 3 * 86400000).toISOString() });

    // The trial ends, and the card pays: active for a month.
    await advance(secondsOf(sub.trial_end) + 3600);
    sub = await read(created.id);
    assert.equal(sub.status, "active");
    const paidTo = sub.current_period_end;
    assert.ok(Date.parse(paidTo) > Date.parse(sub.trial_end));
    assert.equal(plans.subscriptionGrant(sub, 3).op, "grant");

    // The card starts declining: the renewal fails, and the subscription is past due — the
    // grant is left alone.
    await api.post(`/v1/customers/${customer.id}`, { invoice_settings: { default_payment_method: (await api.post("/v1/payment_methods/pm_card_chargeCustomerFail/attach", { customer: customer.id })).id } });
    await advance(secondsOf(paidTo) + 3600);
    sub = await read(created.id);
    assert.equal(sub.status, "past_due");
    assert.deepEqual(plans.subscriptionGrant(sub, 3), { op: "keep" });

    // A working card pays the open invoice: active again, the grant extended.
    const visa = await api.post("/v1/payment_methods/pm_card_visa/attach", { customer: customer.id });
    await api.post(`/v1/customers/${customer.id}`, { invoice_settings: { default_payment_method: visa.id } });
    const open = await api.get("/v1/invoices", { subscription: created.id, status: "open", limit: 1 });
    assert.equal(open.data.length, 1);
    await api.post(`/v1/invoices/${open.data[0].id}/pay`, { payment_method: visa.id });
    sub = await read(created.id);
    assert.equal(sub.status, "active");
    assert.ok(Date.parse(plans.subscriptionGrant(sub, 3).expires_at) > Date.parse(paidTo));
  });
});

test("on a test clock: a trial canceled before it ends runs to its end, then ends", { skip, timeout: 600000 }, async () => {
  await onATestClock(async ({ api, price, customer, advance }) => {
    const created = await api.post("/v1/subscriptions", { customer: customer.id, items: [{ price: price.id }], trial_period_days: 7 });
    const canceled = await stripe.cancel(account, created.id, { at_period_end: true });
    assert.equal(canceled.cancel_at_period_end, true);
    let sub = await read(created.id);
    assert.deepEqual(plans.subscriptionGrant(sub, 3), { op: "grant", expires_at: sub.trial_end });
    await advance(secondsOf(sub.trial_end) + 3600);
    sub = await read(created.id);
    assert.equal(sub.status, "canceled");
    assert.deepEqual(plans.subscriptionGrant(sub, 3), { op: "revoke" });
  });
});

test("a saved card is charged off-session, and one that asks to authenticate falls back to checkout (P7.2)", { skip }, async () => {
  const api = stripe.client(account);
  const customer = await api.post("/v1/customers", { email: "ann@example.com" });
  try {
    // No saved card yet.
    const none = await stripe.chargeSavedMethod(account, { customer: customer.id, amount: 200, currency: "EUR", key: `p7-none-${customer.id}` });
    assert.deepEqual([none.status, none.reason], ["requires_checkout", "the customer has no saved card"]);
    // Stripe's test cards: one that pays, one whose bank always asks the buyer to authenticate.
    const visa = await api.post("/v1/payment_methods/pm_card_visa/attach", { customer: customer.id });
    await api.post(`/v1/customers/${customer.id}`, { invoice_settings: { default_payment_method: visa.id } });
    const key = `p7-paid-${customer.id}`;
    const charge = { customer: customer.id, amount: 200, currency: "EUR", description: "100 credits", key, metadata: { sc_ref_table: "credit_packs", sc_ref_key: "1" } };
    const paid = await stripe.chargeSavedMethod(account, charge);
    assert.equal(paid.status, "paid");
    assert.deepEqual([paid.object.amount, paid.object.currency, paid.object.metadata.sc_ref_table], [200, "EUR", "credit_packs"]);
    const again = await stripe.chargeSavedMethod(account, charge);
    assert.equal(again.object.ref, paid.object.ref, "the same key is the same PaymentIntent");
    const sca = await api.post("/v1/payment_methods/pm_card_authenticationRequired/attach", { customer: customer.id });
    await api.post(`/v1/customers/${customer.id}`, { invoice_settings: { default_payment_method: sca.id } });
    const asked = await stripe.chargeSavedMethod(account, { ...charge, key: `p7-sca-${customer.id}` });
    assert.equal(asked.status, "requires_checkout");
    assert.ok(asked.object, "the PaymentIntent Stripe made is answered with the refusal");
  } finally {
    await api.del(`/v1/customers/${customer.id}`);
  }
});
