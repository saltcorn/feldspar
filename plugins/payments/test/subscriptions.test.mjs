// Subscriptions on the fake provider (TODO-payments P4.1–P4.4), over an in-memory `db` and a
// `roles` that records what the module granted: the plan binding, subscription checkouts, the
// grant through a trial, renewals, a failed renewal and its recovery, plan changes (by action and
// in the portal), cancellation, an unbound price, a refused grant, and the sweep.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "./fake-db.mjs";

const require = createRequire(import.meta.url);
const fake = require("../adapters/fake.js");
const { actionsFor } = require("../actions.js");
const { deliverTo, fakeProvider } = require("../webhooks.js");
const { sweepAll } = require("../service.js");

const account = { id: 1, name: "shop", adapter: "fake", mode: "test", enabled: true, webhook_secret: "whsec_test", settings: {} };
const held = { ...account, id: 2, name: "held", settings: { webhooks: "hold" } };
const configuration = { plans_table: "plans", plan_price_field: "stripe_price", plan_role_field: "role", grace_days: 3 };
const actions = actionsFor(configuration);

const DAY = 86400000;
const plusDays = (iso, days) => new Date(Date.parse(iso) + days * DAY).toISOString();

/** A `roles` that keeps what it was asked, refusing (as the server does) a role above its
 * ceiling. */
function recordingRoles(ceiling = 50) {
  const grants = new Map();
  return {
    grants,
    ceiling,
    async grant({ user, role, ref, expires_at }) {
      if (role === 1 || role < this.ceiling) throw new Error(`module asked to grant role ${role}, but it may only grant roles ${this.ceiling} to 99`);
      grants.set(ref, { user, role, expires_at: expires_at || null, revoked: false });
      return grants.get(ref);
    },
    async revoke(ref) {
      const g = grants.get(ref);
      if (g) g.revoked = true;
      return { revoked: Boolean(g) };
    },
    async get(ref) {
      return grants.get(ref) || null;
    },
  };
}

let store;
let roles;
let ctx;
beforeEach(() => {
  fake.reset();
  store = fakeDb({
    pay_accounts: [{ ...account }, { ...held }],
    plans: [
      { id: 1, name: "Basic", stripe_price: "price_basic", role: 70 },
      { id: 2, name: "Pro", stripe_price: "price_pro", role: 60 },
      { id: 3, name: "Staff", stripe_price: "price_staff", role: 40 },
    ],
    orders: [{ id: 12, total: 900 }],
  });
  roles = recordingRoles();
  ctx = { db: store.db, roles, configuration, log: Object.assign(() => {}, { warn: () => {}, error: () => {} }) };
});

const ann = { id: 7, email: "ann@example.com" };
const argument = (extra) => ({ row: null, user: ann, app: { id: "a", url: "https://saas.example.com" }, db: store.db, roles, ...extra });

/** Ann subscribes to `plan` through `pay_checkout`, and pays on the fake's page. */
async function subscribe(plan, settings = {}, onAccount = account) {
  const started = await actions.pay_checkout.run(
    argument({
      configuration: { account: onAccount.name, mode: "subscription", table: "plans", key_field: "plan", currency: "EUR", ...settings },
      payload: { plan },
    }),
  );
  await fake.settle(onAccount, started.checkout, "succeeded", deliverTo(onAccount, ctx));
  const sub = store.tables.pay_objects.find((o) => o.kind === "subscription" && o.ref_key === plan && o.account === onAccount.id);
  return { started, sub };
}

const mirror = (sub) => store.tables.pay_objects.find((o) => o.id === sub.id);
const grantOf = (sub, on = account) => roles.grants.get(`${on.name}:${sub.provider_ref}`);
const kinds = () => store.tables.pay_events.map((e) => e.kind);

test("a subscription to a plan row grants its role to the period's end plus grace, and each renewal extends it", async () => {
  const { started, sub } = await subscribe(1);
  assert.equal(started.amount, 1000, "the fake's price: 10.00");
  assert.deepEqual(kinds(), ["checkout.completed", "subscription.started", "payment.succeeded"]);
  // The mirror: Ann's, for plan 1, with its renewal date.
  assert.deepEqual(
    [sub.user_id, sub.ref_table, sub.ref_key, sub.status, sub.price_ref, sub.cancel_at_period_end, sub.issue || null],
    [7, "plans", 1, "active", "price_basic", false, null],
  );
  assert.ok(sub.current_period_end);
  // Which provider customer Ann is.
  const customer = store.tables.pay_objects.find((o) => o.kind === "customer");
  assert.deepEqual([customer.user_id, customer.provider_ref], [7, fake.view(account, started.checkout).checkout.customer]);
  // The grant: Basic's role, to the period's end plus three days.
  assert.deepEqual(grantOf(sub), { user: 7, role: 70, expires_at: plusDays(sub.current_period_end, 3), revoked: false });

  const firstEnd = sub.current_period_end;
  await fake.advance(account, sub.provider_ref, "succeeded", deliverTo(account, ctx));
  const renewed = mirror(sub);
  assert.equal(Date.parse(renewed.current_period_end) - Date.parse(firstEnd), 30 * DAY);
  assert.equal(grantOf(sub).expires_at, plusDays(renewed.current_period_end, 3));
  assert.deepEqual(kinds().slice(3), ["payment.succeeded", "subscription.renewed"]);
  // The renewal's payment is for the same plan row, and Ann's.
  const renewal = store.tables.pay_events.filter((e) => e.kind === "payment.succeeded")[1];
  assert.deepEqual([renewal.ref_table, renewal.ref_key, renewal.user_id], ["plans", 1, 7]);
});

test("a failed renewal leaves the grant to run out on its own; a recovery within grace extends it", async () => {
  const { sub } = await subscribe(1);
  const paidTo = grantOf(sub).expires_at;
  await fake.advance(account, sub.provider_ref, "failed", deliverTo(account, ctx));
  assert.equal(mirror(sub).status, "past_due");
  // The period moved on at the provider, but the grant did not: unpaid, it lapses on time.
  assert.equal(grantOf(sub).expires_at, paidTo);
  assert.deepEqual(kinds().slice(3), ["payment.failed", "subscription.past_due"]);
  await fake.retry(account, sub.provider_ref, "failed", deliverTo(account, ctx));
  assert.equal(grantOf(sub).expires_at, paidTo);
  await fake.retry(account, sub.provider_ref, "succeeded", deliverTo(account, ctx));
  assert.equal(mirror(sub).status, "active");
  assert.equal(grantOf(sub).expires_at, plusDays(mirror(sub).current_period_end, 3));
});

test("a trial with the card up front: granted to the trial's end, then converted — or not", async () => {
  const { started, sub } = await subscribe(1, { trial_days: "14" });
  assert.equal(started.amount, 0, "nothing is charged today");
  assert.equal(sub.status, "trialing");
  assert.ok(Math.abs(Date.parse(sub.trial_end) - Date.parse(sub.created_at) - 14 * DAY) < 2000, "fourteen days from now");
  assert.equal(grantOf(sub).expires_at, plusDays(sub.trial_end, 3));
  assert.deepEqual(kinds(), ["checkout.completed", "subscription.started"]);
  // The trial ends and the card is charged: active, for a full period.
  await fake.advance(account, sub.provider_ref, "succeeded", deliverTo(account, ctx));
  assert.equal(mirror(sub).status, "active");
  assert.equal(grantOf(sub).expires_at, plusDays(mirror(sub).current_period_end, 3));

  // Bob's trial, canceled before it ends: kept to the trial's end exactly, then revoked.
  const bob = { id: 8, email: "bob@example.com" };
  const second = await actions.pay_checkout.run(
    argument({ user: bob, configuration: { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR", trial_days: "14" }, payload: { plan: 2 } }),
  );
  await fake.settle(account, second.checkout, "succeeded", deliverTo(account, ctx));
  const bobs = store.tables.pay_objects.find((o) => o.kind === "subscription" && o.user_id === 8);
  await actions.pay_cancel_subscription.run(argument({ user: bob, configuration: { account: "shop" }, payload: {} }));
  assert.equal(mirror(bobs).cancel_at_period_end, true);
  assert.deepEqual(grantOf(bobs), { user: 8, role: 60, expires_at: bobs.trial_end, revoked: false });
  await fake.advance(account, bobs.provider_ref, "succeeded", deliverTo(account, ctx));
  assert.equal(mirror(bobs).status, "canceled");
  assert.equal(grantOf(bobs).revoked, true);
  assert.deepEqual(kinds().slice(-2), ["subscription.canceled", "subscription.ended"]);
});

test("a plan change, by action or in the portal, moves the role", async () => {
  const { sub } = await subscribe(1);
  const changed = await actions.pay_change_subscription.run(argument({ configuration: { account: "shop" }, payload: { plan: 2 } }));
  assert.deepEqual([changed.subscription, changed.price_ref], [sub.id, "price_pro"]);
  assert.equal(grantOf(sub).role, 60);
  assert.equal(kinds().at(-1), "subscription.changed");
  await assert.rejects(actions.pay_change_subscription.run(argument({ configuration: { account: "shop" }, payload: { plan: 2 } })), /on that plan already/);
  await assert.rejects(actions.pay_change_subscription.run(argument({ configuration: { account: "shop" }, payload: { plan: 99 } })), /no plans row 99 that you can read/);
  // Somebody else has nothing to change.
  await assert.rejects(
    actions.pay_change_subscription.run(argument({ user: { id: 8 }, configuration: { account: "shop" }, payload: { plan: 1 } })),
    /you have no subscription on "shop" to change/,
  );
  await assert.rejects(
    actions.pay_change_subscription.run(argument({ user: { id: 8 }, configuration: { account: "shop" }, payload: { plan: 1, subscription: sub.id } })),
    /you have no subscription/,
  );

  // The portal: Ann's own, offering the plans' prices, and a change there moves the role back.
  const { url } = await actions.pay_portal.run(argument({ configuration: { account: "shop", return_path: "/account" } }));
  assert.match(url, /^https:\/\/saas\.example\.com\/_feldspar\/hooks\/payments\/fake\/shop\/portal\/bps_fake_/);
  const path = new URL(url).pathname;
  const rest = path.replace("/_feldspar/hooks/payments/fake/", "");
  const page = await fakeProvider({ method: "GET", rest, query: {}, headers: {}, rawBody: Buffer.alloc(0) }, ctx);
  assert.equal(page.status, 200);
  assert.match(page.body, /<option selected>price_pro<\/option>/);
  assert.match(page.body, /href="https:\/\/saas\.example\.com\/account"/);
  const posted = await fakeProvider(
    {
      method: "POST",
      rest: `${rest}/${sub.provider_ref}/change`,
      query: {},
      headers: { "content-type": "application/x-www-form-urlencoded" },
      rawBody: Buffer.from("price=price_basic"),
    },
    ctx,
  );
  assert.equal(posted.status, 303);
  assert.equal(grantOf(sub).role, 70);
  // Bob is no customer, so has no portal.
  await assert.rejects(actions.pay_portal.run(argument({ user: { id: 8 }, configuration: { account: "shop" } })), /not a customer of "shop" yet/);
});

test("cancelling now revokes; cancelling at the period's end keeps it to that end, then revokes", async () => {
  const { sub } = await subscribe(1);
  const later = await actions.pay_cancel_subscription.run(argument({ configuration: { account: "shop", at_period_end: true }, payload: {} }));
  assert.deepEqual([later.status, later.cancel_at_period_end], ["active", true]);
  assert.equal(grantOf(sub).expires_at, mirror(sub).current_period_end);
  const now = await actions.pay_cancel_subscription.run(argument({ configuration: { account: "shop", at_period_end: false }, payload: {} }));
  assert.equal(now.status, "canceled");
  assert.equal(grantOf(sub).revoked, true);
  await assert.rejects(actions.pay_cancel_subscription.run(argument({ configuration: { account: "shop" }, payload: {} })), /no subscription on "shop" to change/);
});

test("a price that is no plan's grants nothing and is reported on the mirror row", async () => {
  const { sub } = await subscribe(1, { price_ref: '"price_mystery"' });
  assert.equal(sub.price_ref, "price_mystery");
  assert.equal(grantOf(sub), undefined);
  assert.match(mirror(sub).issue, /its price price_mystery is no plan's: no row of plans has it as stripe_price, so it grants no role/);
  // The plan it should have been is added: the next sync grants, and clears the issue.
  store.tables.plans.push({ id: 4, name: "Mystery", stripe_price: "price_mystery", role: 80 });
  await sweepAll(ctx, { only: "shop" });
  assert.equal(grantOf(sub).role, 80);
  assert.equal(mirror(sub).issue, null);
  // A change to an unbound price leaves the grant to lapse by itself, and says why.
  await fake.changePlan(account, sub.provider_ref, "price_gone", {}, deliverTo(account, ctx));
  assert.equal(grantOf(sub).role, 80);
  assert.match(mirror(sub).issue, /price_gone is no plan's/);
});

test("a role above the module's ceiling is an issue, and granted by the sweep once allowed", async () => {
  const { sub } = await subscribe(3);
  assert.equal(grantOf(sub), undefined);
  assert.match(mirror(sub).issue, /its role could not be granted: module asked to grant role 40, but it may only grant roles 50 to 99/);
  roles.ceiling = 40;
  const [swept] = await sweepAll(ctx, { only: "shop" });
  assert.equal(swept.subscriptions, 1);
  assert.equal(grantOf(sub).role, 40);
  assert.equal(mirror(sub).issue, null);
});

test("the sweep reads live subscriptions again: a renewal never delivered still extends the grant", async () => {
  const { sub } = await subscribe(1, {}, held);
  // Held: nothing reached the module, so nothing is mirrored yet.
  assert.equal(sub, undefined);
  await sweepAll(ctx, { only: "held" });
  const mirrored = store.tables.pay_objects.find((o) => o.kind === "subscription");
  const first = grantOf(mirrored, held).expires_at;
  await fake.advance(held, mirrored.provider_ref, "succeeded", deliverTo(held, ctx));
  assert.equal(grantOf(mirrored, held).expires_at, first, "held: not delivered");
  await sweepAll(ctx, { only: "held" });
  assert.equal(grantOf(mirrored, held).expires_at, plusDays(mirror(mirrored).current_period_end, 3));
});

test("a subscription is a signed-in user's; effects belong to one-off payments", async () => {
  await assert.rejects(
    actions.pay_checkout.run(argument({ user: null, configuration: { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR" }, payload: { plan: 1 } })),
    /taken out by a signed-in user/,
  );
  await assert.rejects(
    actions.pay_checkout.run(
      argument({ configuration: { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR", grant_role: 70 }, payload: { plan: 1 } }),
    ),
    /effects are for one-off payments/,
  );
  // A second subscription checkout reuses the customer Ann already is.
  await subscribe(1);
  const again = await actions.pay_checkout.run(
    argument({ configuration: { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR" }, payload: { plan: 2 } }),
  );
  const customers = store.tables.pay_objects.filter((o) => o.kind === "customer");
  assert.equal(customers.length, 1);
  assert.equal(fake.view(account, again.checkout).checkout.customer, customers[0].provider_ref);
});

test("a one-off payment's role effect is a pass for its days, revoked when refunded", async () => {
  const started = await actions.pay_checkout.run(
    argument({ configuration: { account: "shop", table: "orders", key_field: "order", amount: "total", currency: "EUR", read_check: false, grant_role: 70, role_days: 30 }, payload: { order: 12 } }),
  );
  await fake.settle(account, started.checkout, "succeeded", deliverTo(account, ctx));
  const payment = store.tables.pay_objects.find((o) => o.kind === "payment");
  const pass = roles.grants.get(`shop:${payment.provider_ref}`);
  const made = fake.view(account, started.checkout).payment.created * 1000;
  assert.deepEqual(pass, { user: 7, role: 70, expires_at: new Date(made + 30 * DAY).toISOString(), revoked: false });
  await actions.pay_refund.run(argument({ configuration: { source: "row", table: "orders", key_field: "order", read_check: false }, payload: { order: 12 } }));
  assert.equal(roles.grants.get(`shop:${payment.provider_ref}`).revoked, true);
});
