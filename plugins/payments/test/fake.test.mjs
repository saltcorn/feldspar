import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const fake = require("../adapters/fake.js");
const { page, pagePolicy } = require("../webhooks.js");

const account = { name: "shop", adapter: "fake", mode: "test", enabled: true, webhook_secret: "whsec_test" };

beforeEach(() => fake.reset());

const checkout = () =>
  fake.createCheckout(account, {
    currency: "eur",
    lines: [
      { description: "Tea", quantity: 2, unit_amount: 450 },
      { description: "Pot", unit_amount: 2500 },
    ],
    customer: { email: "ann@example.com" },
    base_url: "https://shop.example.com",
    success_url: "https://shop.example.com/thanks",
    metadata: { order: 12 },
  });

test("a checkout charges its lines in minor units, on a page under the shop's host", async () => {
  const created = await checkout();
  assert.match(created.id, /^cs_fake_/);
  assert.equal(created.url, `https://shop.example.com/_feldspar/hooks/payments/fake/shop/checkout/${created.id}`);
  assert.equal(created.object.amount, 3400);
  assert.equal(created.object.currency, "EUR");
  assert.equal(created.object.status, "open");
  await assert.rejects(
    fake.createCheckout(account, { currency: "EUR", lines: [{ unit_amount: 9.99 }] }),
    /whole number/,
  );
  await assert.rejects(fake.createCheckout(account, { mode: "subscription", currency: "EUR", lines: [] }), /one-off/);
  await assert.rejects(
    fake.createCheckout({ ...account, webhook_secret: "" }, { currency: "EUR", lines: [{ unit_amount: 1 }] }),
    /no webhook signing secret/,
  );
});

test("paying delivers signed events, in order, that verify as the provider's", async () => {
  const created = await checkout();
  const delivered = [];
  const deliver = async (body, headers) => {
    delivered.push(fake.verifyWebhook(account, { rawBody: body, headers }));
    return 200;
  };
  const settled = await fake.settle(account, created.id, "succeeded", deliver);
  assert.equal(settled.status, "complete");
  assert.deepEqual(
    delivered.map(([e]) => [e.type, e.ref.kind]),
    [["checkout.completed", "checkout"], ["payment.succeeded", "payment"]],
  );
  // The handler reads the current state back by reference (§3.6).
  const payment = await fake.fetch(account, delivered[1][0].ref);
  assert.equal(payment.status, "succeeded");
  assert.equal(payment.amount, 3400);
  assert.equal(payment.checkout_ref, created.id);
  const view = fake.view(account, created.id);
  assert.deepEqual(view.deliveries.map((d) => d.status), [200, 200]);
  assert.equal(view.total, "34.00 EUR");
  // Settling again does nothing: the checkout is no longer open.
  await fake.settle(account, created.id, "succeeded", deliver);
  assert.equal(delivered.length, 2);
});

test("a declined card leaves the checkout open and says payment.failed", async () => {
  const created = await checkout();
  const types = [];
  await fake.settle(account, created.id, "failed", async (body, headers) => {
    types.push(fake.verifyWebhook(account, { rawBody: body, headers })[0].type);
    return 500;
  });
  assert.deepEqual(types, ["payment.failed"]);
  const view = fake.view(account, created.id);
  assert.equal(view.checkout.status, "open");
  assert.equal(view.checkout.declined, 1);
  // What the handler answered is what the provider recorded.
  assert.deepEqual(view.deliveries.map((d) => d.status), [500]);
});

test("a signature is checked over the exact bytes, with the account's secret, and not too old", () => {
  const body = Buffer.from(JSON.stringify({ id: "evt_1", type: "payment.succeeded", account: "shop", data: { object: { kind: "payment", id: "p" } } }));
  const headers = (sig) => ({ [fake.SIGNATURE_HEADER]: sig });
  assert.equal(fake.verifyWebhook(account, { rawBody: body, headers: headers(fake.sign("whsec_test", body)) })[0].id, "evt_1");
  const tampered = Buffer.from(body.toString().replace("evt_1", "evt_2"));
  assert.throws(() => fake.verifyWebhook(account, { rawBody: tampered, headers: headers(fake.sign("whsec_test", body)) }), /does not match/);
  assert.throws(() => fake.verifyWebhook(account, { rawBody: body, headers: headers(fake.sign("other", body)) }), /does not match/);
  assert.throws(() => fake.verifyWebhook(account, { rawBody: body, headers: {} }), /carries no fake-signature/);
  const old = Math.floor(Date.now() / 1000) - 3600;
  assert.throws(() => fake.verifyWebhook(account, { rawBody: body, headers: headers(fake.sign("whsec_test", body, old)) }), /too old/);
  assert.throws(() => fake.verifyWebhook({ ...account, webhook_secret: null }, { rawBody: body, headers: headers("t=1,v1=00") }), /no webhook signing secret/);
  const other = { ...account, name: "blog" };
  assert.throws(() => fake.verifyWebhook({ ...other, webhook_secret: "whsec_test" }, { rawBody: body, headers: headers(fake.sign("whsec_test", body)) }), /another account/);
});

test("the checkout page escapes what it shows and runs no script", async () => {
  const created = await fake.createCheckout(account, {
    currency: "USD",
    lines: [{ description: "<script>alert(1)</script>", unit_amount: 100 }],
    success_url: "https://shop.example.com/thanks",
  });
  const html = page("/p", fake.view(account, created.id));
  assert.ok(!html.includes("<script>"));
  assert.ok(html.includes("&lt;script&gt;"));
  assert.ok(html.includes('action="/p/pay"'));
  const policy = pagePolicy(fake.view(account, created.id).checkout);
  assert.match(policy, /default-src 'none'/);
  assert.match(policy, /form-action 'self' https:\/\/shop\.example\.com/);
});
