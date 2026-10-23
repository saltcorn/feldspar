// `pay_checkout`'s configuration (TODO-payments §6.1, P2.4), over the fake provider and an
// in-memory `db`. The Ⱶ-paths, the guest session and the real ownership rules are the Rust
// test's (crates/sc-server/tests/payments_module.rs): this one is the arithmetic and the refusals.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "./fake-db.mjs";

const require = createRequire(import.meta.url);
const fake = require("../adapters/fake.js");
const { actions } = require("../actions.js");

const account = { id: 1, name: "shop", adapter: "fake", mode: "test", enabled: true, webhook_secret: "whsec_test" };

let store;
beforeEach(() => {
  fake.reset();
  store = fakeDb(
    {
      pay_accounts: [{ ...account }],
      orders: [
        { id: 12, owner: 7, total: 900, email: "guest@example.com", currency: "EUR" },
        { id: 13, owner: 8, total: 500, email: "other@example.com", currency: "EUR" },
        { id: 14, owner: 7, total: 0, email: null, currency: "EUR" },
      ],
      order_lines: [
        { id: 1, order: 12, quantity: 2, price: 450, name: "Tea" },
        { id: 2, order: 12, quantity: 1, price: 250, name: "Cup" },
        { id: 3, order: 13, quantity: 1, price: 500, name: "Pot" },
      ],
    },
    { canRead: (table, row) => table !== "orders" || row.owner === 7 },
  );
});

const children = {
  account: "shop",
  table: "orders",
  key_field: "order",
  lines: "children",
  child_table: "order_lines",
  child_key: "order",
  quantity: "quantity",
  unit_amount: "price",
  line_description: "name",
  currency: "EUR",
  success_path: "/orders/{key}/thanks",
};

const run = (configuration, payload, extra = {}) =>
  actions.pay_checkout.run({
    configuration,
    payload,
    row: null,
    user: { id: 7, email: "ann@example.com" },
    app: { id: "a", url: "https://shop.example.com" },
    db: store.db,
    ...extra,
  });

test("a row's child lines are charged by the server's formulas, whatever the payload says", async () => {
  const answer = await run(children, { order: 12, amount: 1, total: 1 });
  assert.equal(answer.amount, 1150);
  assert.equal(answer.currency, "EUR");
  assert.match(answer.url, /^https:\/\/shop\.example\.com\/_feldspar\/hooks\/payments\/fake\/shop\/checkout\/cs_fake_/);
  const [mirror] = store.tables.pay_objects;
  assert.equal(mirror.id, answer.id);
  assert.deepEqual(
    [mirror.kind, mirror.ref_table, mirror.ref_key, mirror.user_id, mirror.email, mirror.amount, mirror.status],
    ["checkout", "orders", 12, 7, "ann@example.com", 1150, "open"],
  );
  assert.deepEqual(mirror.lines.map((l) => [l.description, l.quantity, l.unit_amount]), [["Tea", 2, 450], ["Cup", 1, 250]]);
  const view = fake.view(account, answer.checkout);
  assert.equal(view.checkout.success_url, "https://shop.example.com/orders/12/thanks");
  // Metadata is text: the key is kept as JSON, so an integer key comes back an integer.
  assert.deepEqual(view.checkout.metadata, { sc_ref_table: "orders", sc_ref_key: "12", sc_user_id: "7" });
});

test("one line from the row, a guest's email, and a currency formula", async () => {
  const answer = await run(
    { account: "shop", table: "orders", key_field: "order", amount: "total", currency: "currency", email: "email" },
    { order: "12" },
    { user: null },
  );
  assert.equal(answer.amount, 900);
  const [mirror] = store.tables.pay_objects;
  assert.equal(mirror.email, "guest@example.com");
  assert.equal(mirror.user_id, null);
});

test("each thing that cannot be paid for is refused by name", async () => {
  const one = { account: "shop", table: "orders", key_field: "order", amount: "total", currency: "EUR" };
  for (const [configuration, payload, refusal] of [
    [one, { order: 13 }, /there is no orders row 13 that you can read/],
    [one, { order: 99 }, /there is no orders row 99 that you can read/],
    [one, {}, /say which orders row to pay for: \{ order: <its key> \}/],
    [one, { order: 14 }, /nothing to pay for orders 14/],
    [{ ...one, account: "nope" }, { order: 12 }, /there is no payment account "nope"/],
    [{ ...one, mode: "subscription" }, { order: 12 }, /a subscription checkout needs the price setting/],
    [{ ...one, mode: "subscription", price_ref: "total" }, { order: 12 }, /has no price to subscribe to: its price formula gives 900/],
    [{ ...one, tax: "merchant_of_record" }, { order: 12 }, /cannot act as merchant of record/],
    [{ ...one, tax: "provider" }, { order: 12 }, /calculates no tax/],
    [{ ...one, shipping: true }, { order: 12 }, /needs the countries it ships to/],
    [{ ...one, success_path: "https://elsewhere.example" }, { order: 12 }, /a path on the application's host/],
    [{ ...one, amount: "total * owner", currency: "EURO" }, { order: 12 }, /is not a currency/],
  ]) {
    await assert.rejects(run(configuration, payload), refusal);
  }
  assert.equal(store.tables.pay_objects.length, 0);
  // Without the read check (a workflow's later step), somebody else's row can be paid for.
  const unchecked = await run({ ...one, read_check: false }, { order: 13 });
  assert.equal(unchecked.amount, 500);
});

test("a checkout paid is logged against the row it was for", async () => {
  const answer = await run({ ...children, effects: undefined, grant_credits: "quantity" }, { order: 12 }).catch((e) => e);
  // `quantity` is not a field of orders: the effect's formula is the row's.
  assert.ok(answer instanceof Error);
  const paid = await run(children, { order: 12 });
  const ctx = { db: store.db, log: Object.assign(() => {}, { warn: () => {} }) };
  const { deliverTo } = require("../webhooks.js");
  await fake.settle(account, paid.checkout, "succeeded", deliverTo(account, ctx));
  const succeeded = store.tables.pay_events.find((e) => e.kind === "payment.succeeded");
  assert.deepEqual([succeeded.ref_table, succeeded.ref_key, succeeded.user_id, succeeded.amount], ["orders", 12, 7, 1150]);
});
