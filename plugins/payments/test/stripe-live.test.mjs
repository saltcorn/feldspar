// The Stripe adapter against Stripe itself, in test mode (TODO-payments P2.5). It runs only when
// a test-mode secret key is given — `STRIPE_TEST_SECRET_KEY=sk_test_… npm test` — as the CmdStan
// tests run only where CmdStan is installed. Nothing it makes is ever paid.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const stripe = require("../adapters/stripe.js");

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
