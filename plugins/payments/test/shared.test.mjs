// The shared suite (TODO-payments P5.2): the shop (P2), the subscriptions (P4) and the LLM
// credits (P7) as one set of tests, run against every provider — the fake, Stripe and Polar — through a driver each
// (`providers/index.mjs`). What a provider does differently is the driver's to play, and where it
// shows through to the module, the test says so by the driver's flags, not by a copy of the test.
//
// Each test runs the module as the server does: its actions (`pay_checkout`, `pay_refund`, …),
// its webhook handler, the processing behind it and the sweep, over an in-memory `db`, a `roles`
// that records what the module granted, and a `credits` that keeps a ledger as core does.

import { describe, test, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "./fake-db.mjs";
import { PROVIDERS } from "./providers/index.mjs";

const require = createRequire(import.meta.url);
const { actionsFor } = require("../actions.js");
const { events, deliverTo } = require("../webhooks.js");
const { sweepAll } = require("../service.js");
const { millis } = require("../plans.js");

const DAY = 86400000;
const plusDays = (iso, days) => new Date(Date.parse(iso) + days * DAY).toISOString();

/** A `roles` that keeps what it was asked, refusing (as the server does) a role above its ceiling. */
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
  };
}

/** A `credits` that keeps buckets as core's ledger does (crates/sc-auth/src/credits.rs, whose
 * own tests hold it to this): one per key, a replayed key granting nothing, a rollover emptying
 * the earlier bucket and carrying what it held up to its cap, and spending soonest-expiring
 * first. */
function recordingCredits() {
  const buckets = new Map();
  const live = (b) => !b.expires_at || Date.parse(b.expires_at) > Date.now();
  const mine = (user) => [...buckets.values()].filter((b) => String(b.user) === String(user) && live(b));
  return {
    buckets,
    async grant({ user, amount, key, expires_at, kind, rollover }) {
      if (!key) throw new Error("credits.grant needs `key`");
      if (!(amount > 0)) throw new Error(`a credit grant gives credits; ${amount} is not more than nothing`);
      if (buckets.has(key)) return { ...buckets.get(key), replayed: true, carried: 0 };
      if (expires_at && Date.parse(expires_at) <= Date.now()) throw new Error(`a credit grant that expires at ${expires_at} has already expired`);
      let carried = 0;
      const from = rollover && buckets.get(rollover.from);
      if (from) {
        carried = rollover.cap === null || rollover.cap === undefined ? from.remaining : Math.min(from.remaining, rollover.cap);
        from.remaining = 0;
      }
      const bucket = { user, amount: amount + carried, remaining: amount + carried, expires_at: expires_at || null, kind: kind || "grant" };
      buckets.set(key, bucket);
      return { ...bucket, replayed: false, carried };
    },
    async balance(user) {
      return { balance: this.of(user) };
    },
    /** A user's balance now. */
    of(user) {
      return mine(user).reduce((t, b) => t + b.remaining, 0);
    },
    /** What an agent run's model calls would take: soonest-expiring first. */
    spend(user, amount) {
      let owed = amount;
      for (const b of mine(user).sort((a, b) => (a.expires_at ? Date.parse(a.expires_at) : Infinity) - (b.expires_at ? Date.parse(b.expires_at) : Infinity))) {
        const take = Math.min(owed, b.remaining);
        b.remaining -= take;
        owed -= take;
      }
      if (owed > 0) throw new Error(`the test spent ${amount}, more than there was`);
    },
  };
}

const ann = { id: 7, email: "ann@example.com" };
const bob = { id: 8, email: "bob@example.com" };
const quiet = Object.assign(() => {}, { warn: () => {}, error: () => {} });

for (const makeDriver of PROVIDERS) {
  describe(`on ${makeDriver().name}`, () => {
    let provider;
    let shop;
    let held;
    let store;
    let roles;
    let credits;
    let ctx;
    let actions;
    /** The deliveries the module's handler failed (a 500 at the server). */
    let failed;
    beforeEach(() => {
      provider = makeDriver();
      shop = provider.account(1, "shop");
      held = provider.account(2, "held", { hold: true });
      const field = provider.priceField;
      store = fakeDb({
        pay_accounts: [shop, held].map((a) => ({ ...a })),
        plans: [
          { id: 1, name: "Basic", [field]: provider.prices.basic, role: 70 },
          { id: 2, name: "Pro", [field]: provider.prices.pro, role: 60 },
          { id: 3, name: "Staff", [field]: provider.prices.staff, role: 40 },
        ],
        orders: [{ id: 12, total: 900 }],
        credit_packs: [{ id: 1, name: "100 credits", price: 200, units: 100 }],
      });
      roles = recordingRoles();
      credits = recordingCredits();
      const configuration = {
        plans_table: "plans",
        plan_price_field: field,
        plan_role_field: "role",
        plan_allowance_field: "credits",
        plan_rollover_field: "rollover",
        plan_rollover_cap_field: "rollover_cap",
        grace_days: 3,
      };
      ctx = { db: store.db, roles, credits, configuration, log: quiet };
      actions = actionsFor(configuration);
      failed = [];
      provider.install([shop, held], (account) => {
        const deliver = deliverTo(account, ctx);
        return async (body, headers) => {
          try {
            return await deliver(body, headers);
          } catch (e) {
            failed.push(e);
            throw e;
          }
        };
      });
    });
    afterEach(() => provider.uninstall());

    const argument = (extra) => ({ row: null, user: ann, app: { id: "a", url: "https://saas.example.com" }, db: store.db, roles, credits, ...extra });
    const kinds = () => store.tables.pay_events.map((e) => e.kind);
    const mirror = (row) => store.tables.pay_objects.find((o) => o.id === row.id);
    const grantOf = (sub, on = shop) => roles.grants.get(`${on.name}:${sub.provider_ref}`);
    const eventKeys = () => store.tables.pay_events.map((e) => `${e.event_id}/${e.kind}`);

    /** Ann pays for order 12 on `on`'s checkout page. */
    async function payForOrder(settings = {}, on = shop, outcome = "succeeded") {
      const started = await actions.pay_checkout.run(
        argument({ configuration: { account: on.name, table: "orders", key_field: "order", amount: "total", currency: "EUR", read_check: false, ...settings }, payload: { order: 12 } }),
      );
      await provider.pay(on, started.checkout, outcome);
      return started;
    }

    /** Ann (or `user`) subscribes to `plan` through `pay_checkout`, and pays. */
    async function subscribe(plan, settings = {}, on = shop, user = ann) {
      const started = await actions.pay_checkout.run(
        argument({ user, configuration: { account: on.name, mode: "subscription", table: "plans", key_field: "plan", currency: "EUR", ...settings }, payload: { plan } }),
      );
      await provider.pay(on, started.checkout, "succeeded");
      const sub = store.tables.pay_objects.find((o) => o.kind === "subscription" && o.account === on.id && String(o.user_id) === String(user.id) && o.ref_key === plan);
      return { started, sub };
    }

    // ---- P2: the shop ----

    test("a row is paid for, and each event is logged once however often it arrives", async () => {
      const started = await payForOrder();
      assert.equal(started.amount, 900);
      assert.ok(kinds().includes("checkout.completed"), kinds().join());
      const paid = store.tables.pay_events.find((e) => e.kind === "payment.succeeded");
      assert.deepEqual([paid.ref_table, paid.ref_key, paid.user_id, paid.amount, paid.currency, paid.status], ["orders", 12, 7, 900, "EUR", "succeeded"]);
      assert.equal(store.tables.pay_objects.find((o) => o.kind === "checkout").status, "complete");
      const logged = eventKeys();
      for (const e of [...provider.events(shop)].reverse()) assert.equal(await provider.redeliver(shop, e.id), 200);
      for (const e of provider.events(shop)) assert.equal(await provider.redeliver(shop, e.id), 200);
      assert.deepEqual(eventKeys(), logged);
    });

    test("a declined card, then a payment: both logged, and the mirror ends as the provider says", async () => {
      const started = await payForOrder({}, shop, "failed");
      assert.ok(kinds().includes("payment.failed"), kinds().join());
      assert.ok(!kinds().includes("payment.succeeded"));
      await provider.pay(shop, started.checkout, "succeeded");
      // Everything again, newest first: the mirror still says what the provider says now.
      for (const e of [...provider.events(shop)].reverse()) await provider.redeliver(shop, e.id);
      assert.deepEqual(kinds().filter((k) => k.startsWith("payment.")).sort(), ["payment.failed", "payment.succeeded"]);
      assert.equal(store.tables.pay_objects.find((o) => o.kind === "checkout").status, "complete");
      const payment = store.tables.pay_objects.find((o) => o.id === store.tables.pay_events.find((e) => e.kind === "payment.succeeded").object);
      assert.equal(payment.status, "succeeded");
    });

    test("a forged delivery stores nothing; one the module does not understand is logged as other", async () => {
      const forged = await events({ ...provider.forged(shop), rest: shop.name, method: "POST", query: {} }, ctx);
      assert.equal(forged.status, 400);
      assert.match(forged.body.error, /signature does not match/);
      assert.equal(store.tables.pay_events.length, 0);
      const odd = await events({ ...provider.unknown(shop), rest: shop.name, method: "POST", query: {} }, ctx);
      assert.equal(odd.status, 200);
      assert.deepEqual(kinds(), ["other"]);
      assert.ok(store.tables.pay_events[0].raw);
    });

    test("a delivery that fails half way is a 500, and the provider's retry finishes it", async () => {
      store.failInsert("pay_events");
      await payForOrder();
      // The first event row's insert failed, and so did that delivery (the server's 500).
      assert.equal(failed.length, 1);
      assert.match(failed[0].message, /the insert into pay_events failed/);
      for (const e of provider.events(shop)) await provider.redeliver(shop, e.id);
      assert.ok(kinds().includes("checkout.completed"));
      assert.ok(kinds().includes("payment.succeeded"));
      assert.equal(new Set(eventKeys()).size, eventKeys().length, "nothing logged twice");
      assert.equal(kinds().filter((k) => k === "checkout.completed").length, 1);
    });

    test("pay_refund gives money back, and the provider reports it", async () => {
      await payForOrder();
      const refund = { source: "row", table: "orders", key_field: "order", read_check: false };
      const part = await actions.pay_refund.run(argument({ configuration: { ...refund, amount: "300" }, payload: { order: 12 } }));
      await provider.flush();
      assert.equal(part.amount, 300);
      const payment = () => store.tables.pay_objects.find((o) => o.kind === "payment" && o.status !== "failed" && o.ref_key === 12);
      assert.equal(payment().status, "partially_refunded");
      await actions.pay_refund.run(argument({ configuration: refund, payload: { order: 12 } }));
      await provider.flush();
      assert.equal(payment().status, "refunded");
      const refunded = store.tables.pay_events.filter((e) => e.kind === "payment.refunded");
      assert.equal(refunded.length, 2);
      assert.deepEqual([refunded[1].ref_table, refunded[1].ref_key], ["orders", 12]);
      await assert.rejects(actions.pay_refund.run(argument({ configuration: refund, payload: { order: 12 } })), /no payment for orders 12 to refund/);
    });

    test("the sweep processes what was never delivered, once", async () => {
      await payForOrder({}, held);
      assert.equal(store.tables.pay_events.length, 0, "held: nothing delivered");
      await sweepAll(ctx, { only: "held" });
      assert.ok(kinds().includes("payment.succeeded"), kinds().join());
      const logged = eventKeys();
      await sweepAll(ctx, { only: "held" });
      assert.deepEqual(eventKeys(), logged);
    });

    test("a one-off payment's role effect is a pass for its days, revoked when refunded", async () => {
      await payForOrder({ grant_role: 70, role_days: 30 });
      const payment = store.tables.pay_objects.find((o) => o.kind === "payment" && o.status === "succeeded");
      const pass = roles.grants.get(`shop:${payment.provider_ref}`);
      assert.deepEqual([pass.user, pass.role, pass.revoked], [7, 70, false]);
      // Thirty days from when the provider says the payment was made.
      const made = millis(payment.provider_data.created === undefined ? payment.provider_data.created_at : payment.provider_data.created);
      assert.equal(pass.expires_at, new Date(made + 30 * DAY).toISOString());
      await actions.pay_refund.run(argument({ configuration: { source: "row", table: "orders", key_field: "order", read_check: false }, payload: { order: 12 } }));
      await provider.flush();
      assert.equal(roles.grants.get(`shop:${payment.provider_ref}`).revoked, true);
    });

    // ---- P4: subscriptions ----

    test("a subscription grants its plan's role to the period's end plus grace, and each renewal extends it", async () => {
      const { started, sub } = await subscribe(1);
      assert.equal(started.amount, 1000);
      assert.ok(kinds().includes("subscription.started"), kinds().join());
      assert.deepEqual(
        [sub.user_id, sub.ref_table, sub.ref_key, sub.status, sub.price_ref, sub.cancel_at_period_end, sub.issue || null],
        [7, "plans", 1, "active", provider.prices.basic, false, null],
      );
      const customer = store.tables.pay_objects.find((o) => o.kind === "customer");
      assert.deepEqual([customer.user_id, customer.provider_ref], [7, provider.checkoutCustomer(shop, started.checkout)]);
      assert.deepEqual(grantOf(sub), { user: 7, role: 70, expires_at: plusDays(sub.current_period_end, 3), revoked: false });

      const firstEnd = sub.current_period_end;
      await provider.renew(shop, sub.provider_ref, "succeeded");
      assert.equal(Date.parse(mirror(sub).current_period_end) - Date.parse(firstEnd), 30 * DAY);
      assert.equal(grantOf(sub).expires_at, plusDays(mirror(sub).current_period_end, 3));
      assert.equal(kinds().filter((k) => k === "subscription.renewed").length, 1, kinds().join());
      assert.ok(!kinds().includes("subscription.changed"), "a renewal is no change of plan");
      if (provider.renewalPaymentsKnowTheirRow) {
        const renewal = store.tables.pay_events.filter((e) => e.kind === "payment.succeeded").at(-1);
        assert.deepEqual([renewal.ref_table, renewal.ref_key, renewal.user_id], ["plans", 1, 7]);
      }
    });

    test("a failed renewal leaves the grant to run out; a recovery within grace extends it", async () => {
      const { sub } = await subscribe(1);
      const paidTo = grantOf(sub).expires_at;
      await provider.renew(shop, sub.provider_ref, "failed");
      assert.equal(mirror(sub).status, "past_due");
      assert.equal(grantOf(sub).expires_at, paidTo);
      assert.equal(kinds().filter((k) => k === "subscription.past_due").length, 1, kinds().join());
      await provider.retry(shop, sub.provider_ref, "failed");
      assert.equal(grantOf(sub).expires_at, paidTo);
      await provider.retry(shop, sub.provider_ref, "succeeded");
      assert.equal(mirror(sub).status, "active");
      assert.equal(grantOf(sub).expires_at, plusDays(mirror(sub).current_period_end, 3));
      assert.ok(kinds().includes("subscription.renewed"));
    });

    test("a trial with the card up front is granted to its end, then converts — or is canceled", async () => {
      const { started, sub } = await subscribe(1, { trial_days: "14" });
      assert.equal(started.amount, 0, "nothing is charged today");
      assert.equal(sub.status, "trialing");
      assert.ok(Math.abs(Date.parse(sub.trial_end) - Date.now() - 14 * DAY) < 5000, sub.trial_end);
      assert.equal(grantOf(sub).expires_at, plusDays(sub.trial_end, 3));
      await provider.renew(shop, sub.provider_ref, "succeeded");
      assert.equal(mirror(sub).status, "active");
      assert.equal(grantOf(sub).expires_at, plusDays(mirror(sub).current_period_end, 3));

      // Bob's trial, canceled before it ends: kept to the trial's end exactly, then revoked.
      const bobs = (await subscribe(2, { trial_days: "14" }, shop, bob)).sub;
      await actions.pay_cancel_subscription.run(argument({ user: bob, configuration: { account: "shop" }, payload: {} }));
      await provider.flush();
      assert.equal(mirror(bobs).cancel_at_period_end, true);
      assert.deepEqual(grantOf(bobs), { user: 8, role: 60, expires_at: bobs.trial_end, revoked: false });
      await provider.renew(shop, bobs.provider_ref, "succeeded");
      assert.equal(mirror(bobs).status, "canceled");
      assert.equal(grantOf(bobs).revoked, true);
      assert.ok(kinds().includes("subscription.canceled") && kinds().includes("subscription.ended"), kinds().join());
    });

    test("a plan change, by action or in the provider's portal, moves the role", async () => {
      const { sub } = await subscribe(1);
      const changed = await actions.pay_change_subscription.run(argument({ configuration: { account: "shop" }, payload: { plan: 2 } }));
      await provider.flush();
      assert.deepEqual([changed.subscription, changed.price_ref], [sub.id, provider.prices.pro]);
      assert.equal(grantOf(sub).role, 60);
      assert.equal(kinds().filter((k) => k === "subscription.changed").length, 1, kinds().join());
      await assert.rejects(actions.pay_change_subscription.run(argument({ configuration: { account: "shop" }, payload: { plan: 2 } })), /on that plan already/);
      await assert.rejects(actions.pay_change_subscription.run(argument({ configuration: { account: "shop" }, payload: { plan: 99 } })), /no plans row 99 that you can read/);
      await assert.rejects(
        actions.pay_change_subscription.run(argument({ user: { id: 8 }, configuration: { account: "shop" }, payload: { plan: 1, subscription: sub.id } })),
        /you have no subscription/,
      );

      const { url } = await actions.pay_portal.run(argument({ configuration: { account: "shop", return_path: "/account" } }));
      assert.match(url, /^https:\/\//);
      await provider.portalChange(shop, sub.provider_ref, provider.prices.basic);
      assert.equal(grantOf(sub).role, 70);
      assert.equal(kinds().filter((k) => k === "subscription.changed").length, 2);
      await assert.rejects(actions.pay_portal.run(argument({ user: { id: 8 }, configuration: { account: "shop" } })), /not a customer of "shop" yet/);
    });

    test("cancelling at the period's end keeps the grant to that end; cancelling now revokes it", async () => {
      const { sub } = await subscribe(1);
      const later = await actions.pay_cancel_subscription.run(argument({ configuration: { account: "shop", at_period_end: true }, payload: {} }));
      await provider.flush();
      assert.deepEqual([later.status, later.cancel_at_period_end], ["active", true]);
      assert.equal(grantOf(sub).expires_at, mirror(sub).current_period_end);
      assert.ok(kinds().includes("subscription.canceled"));
      const now = await actions.pay_cancel_subscription.run(argument({ configuration: { account: "shop", at_period_end: false }, payload: {} }));
      await provider.flush();
      assert.equal(now.status, "canceled");
      assert.equal(grantOf(sub).revoked, true);
      assert.ok(kinds().includes("subscription.ended"));
      await assert.rejects(actions.pay_cancel_subscription.run(argument({ configuration: { account: "shop" }, payload: {} })), /no subscription on "shop" to change/);
    });

    test("a price that is no plan's grants nothing, is reported, and is granted once it is a plan's", async () => {
      const { sub } = await subscribe(1, { price_ref: JSON.stringify(provider.prices.mystery) });
      assert.equal(sub.price_ref, provider.prices.mystery);
      assert.equal(grantOf(sub), undefined);
      assert.equal(mirror(sub).issue, `its price ${provider.prices.mystery} is no plan's: no row of plans has it as ${provider.priceField}, so it grants no role`);
      store.tables.plans.push({ id: 4, name: "Mystery", [provider.priceField]: provider.prices.mystery, role: 80 });
      await sweepAll(ctx, { only: "shop" });
      assert.equal(grantOf(sub).role, 80);
      assert.equal(mirror(sub).issue, null);
      await provider.portalChange(shop, sub.provider_ref, provider.prices.gone);
      assert.equal(grantOf(sub).role, 80);
      assert.match(mirror(sub).issue, /is no plan's/);
    });

    test("a role above the module's ceiling is an issue, granted by the sweep once allowed", async () => {
      const { sub } = await subscribe(3);
      assert.equal(grantOf(sub), undefined);
      assert.match(mirror(sub).issue, /module asked to grant role 40, but it may only grant roles 50 to 99/);
      roles.ceiling = 40;
      const [swept] = await sweepAll(ctx, { only: "shop" });
      assert.equal(swept.subscriptions, 1);
      assert.equal(grantOf(sub).role, 40);
      assert.equal(mirror(sub).issue, null);
    });

    test("the sweep reads live subscriptions again: a renewal never delivered still extends the grant", async () => {
      const { sub } = await subscribe(1, {}, held);
      assert.equal(sub, undefined, "held: nothing reached the module");
      await sweepAll(ctx, { only: "held" });
      const mirrored = store.tables.pay_objects.find((o) => o.kind === "subscription");
      assert.equal(grantOf(mirrored, held).role, 70);
      const first = grantOf(mirrored, held).expires_at;
      await provider.renew(held, mirrored.provider_ref, "succeeded");
      assert.equal(grantOf(mirrored, held).expires_at, first, "held: not delivered");
      await sweepAll(ctx, { only: "held" });
      assert.equal(grantOf(mirrored, held).expires_at, plusDays(mirror(mirrored).current_period_end, 3));
    });

    test("a subscription is a signed-in user's, takes no effects, and reuses the customer the user is", async () => {
      const plan1 = { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR" };
      await assert.rejects(actions.pay_checkout.run(argument({ user: null, configuration: plan1, payload: { plan: 1 } })), /taken out by a signed-in user/);
      await assert.rejects(actions.pay_checkout.run(argument({ configuration: { ...plan1, grant_role: 70 }, payload: { plan: 1 } })), /effects are for one-off payments/);
      await subscribe(1);
      const again = await actions.pay_checkout.run(argument({ configuration: plan1, payload: { plan: 2 } }));
      const customers = store.tables.pay_objects.filter((o) => o.kind === "customer");
      assert.equal(customers.length, 1);
      assert.equal(provider.checkoutCustomer(shop, again.checkout), customers[0].provider_ref);
    });

    // ---- P7: LLM credits ----

    /** Plan 1 (Basic) includes `amount` credits a period, rolling over as `rollover` says. */
    const withCredits = (amount, rollover, cap = null) => Object.assign(store.tables.plans[0], { credits: amount, rollover, rollover_cap: cap });
    const allowances = () => [...credits.buckets.values()].filter((b) => b.kind === "allowance");

    for (const [policy, cap, balances] of [
      ["none", null, [70, 70, 70]],
      ["all", null, [70, 140, 210]],
      ["capped", 50, [70, 120, 120]],
    ]) {
      test(`a plan's credits are granted for each paid period, rolling over ${policy}`, async () => {
        withCredits(100, policy, cap);
        const { sub } = await subscribe(1);
        // Three periods, 30 spent in each.
        for (let period = 0; period < 3; period++) {
          if (period) await provider.renew(shop, sub.provider_ref, "succeeded");
          credits.spend(ann.id, 30);
          assert.equal(credits.of(ann.id), balances[period], `${policy}, period ${period + 1}`);
        }
        // Every event again, newest first, and the sweep: nothing is granted twice.
        for (const e of [...provider.events(shop)].reverse()) await provider.redeliver(shop, e.id);
        await sweepAll(ctx, { only: "shop" });
        assert.equal(credits.of(ann.id), balances[2]);
        assert.equal(allowances().length, 3);
        // Each period's allowance lasts to the period's end; only the newest holds anything.
        assert.equal(allowances().at(-1).expires_at, mirror(sub).current_period_end);
        assert.deepEqual(allowances().map((b) => b.remaining > 0), [false, false, true]);
        assert.equal(mirror(sub).issue || null, null);
      });
    }

    test("a renewal that is not paid grants no credits until it is", async () => {
      withCredits(100, "all");
      const { sub } = await subscribe(1);
      assert.equal(credits.of(ann.id), 100);
      await provider.renew(shop, sub.provider_ref, "failed");
      assert.equal(mirror(sub).status, "past_due");
      assert.equal(credits.of(ann.id), 100, "past due: the period is not paid for");
      await provider.retry(shop, sub.provider_ref, "succeeded");
      assert.equal(credits.of(ann.id), 200, "recovered: granted, the last period's carried over");
      assert.equal(allowances().length, 2);
    });

    test("a trial includes its plan's credits, and a renewal never delivered is granted by the sweep", async () => {
      withCredits(100, "none");
      const { sub } = await subscribe(1, { trial_days: "14" });
      assert.equal(credits.of(ann.id), 100);
      assert.equal(allowances()[0].expires_at, sub.trial_end);
      await provider.renew(shop, sub.provider_ref, "succeeded");
      assert.equal(credits.of(ann.id), 100, "the trial's were not carried over");
      assert.equal(allowances().length, 2);

      await subscribe(1, {}, held, bob);
      await sweepAll(ctx, { only: "held" });
      assert.equal(credits.of(bob.id), 100);
      const bobs = store.tables.pay_objects.find((o) => o.kind === "subscription" && o.account === held.id);
      await provider.renew(held, bobs.provider_ref, "succeeded");
      assert.equal(allowances().length, 3, "held: not delivered");
      await sweepAll(ctx, { only: "held" });
      assert.equal(allowances().length, 4);
      assert.equal(credits.of(bob.id), 100);
    });

    test("a trial is had once per user on an account: a second subscription pays from the first day", async () => {
      withCredits(100, "none");
      const first = await subscribe(1, { trial_days: "14" });
      assert.equal(first.sub.status, "trialing");
      assert.equal(credits.of(ann.id), 100);
      await actions.pay_cancel_subscription.run(argument({ configuration: { account: "shop" }, payload: {} }));
      await provider.flush();
      await provider.renew(shop, first.sub.provider_ref, "succeeded");
      assert.equal(mirror(first.sub).status, "canceled");

      // Subscribing again, to the same or another plan: no trial, charged now.
      const again = await actions.pay_checkout.run(
        argument({ configuration: { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR", trial_days: "14" }, payload: { plan: 2 } }),
      );
      const checkout = store.tables.pay_objects.find((o) => o.kind === "checkout" && o.provider_ref === String(again.checkout));
      assert.equal(checkout.lines[0].trial_days, 0);
      // Bob's first trial is his own.
      const bobs = await actions.pay_checkout.run(
        argument({ user: bob, configuration: { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR", trial_days: "14" }, payload: { plan: 1 } }),
      );
      assert.equal(store.tables.pay_objects.find((o) => o.kind === "checkout" && o.provider_ref === String(bobs.checkout)).lines[0].trial_days, 14);
      // A second trial checkout while the first is open has no trial either.
      const stacked = await actions.pay_checkout.run(
        argument({ user: bob, configuration: { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR", trial_days: "14" }, payload: { plan: 2 } }),
      );
      assert.equal(store.tables.pay_objects.find((o) => o.kind === "checkout" && o.provider_ref === String(stacked.checkout)).lines[0].trial_days, 0);
    });

    test("two trials taken at once grant the plan's credits once", async () => {
      withCredits(100, "none");
      const config = { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR", trial_days: "14" };
      const a = await actions.pay_checkout.run(argument({ configuration: config, payload: { plan: 1 } }));
      // As if both checks ran before either checkout was recorded.
      const rowA = store.tables.pay_objects.find((o) => o.kind === "checkout" && o.provider_ref === String(a.checkout));
      const linesA = rowA.lines;
      rowA.lines = [];
      const b = await actions.pay_checkout.run(argument({ configuration: config, payload: { plan: 1 } }));
      rowA.lines = linesA;
      await provider.pay(shop, a.checkout, "succeeded");
      await provider.pay(shop, b.checkout, "succeeded");
      const subs = store.tables.pay_objects.filter((o) => o.kind === "subscription" && o.user_id === ann.id);
      assert.deepEqual(subs.map((s) => s.status), ["trialing", "trialing"]);
      assert.equal(credits.of(ann.id), 100);
      assert.match(mirror(subs[1]).issue, /second subscription includes no LLM credits/);
    });

    test("a plan whose rollover makes no sense is an issue, and its role is granted all the same", async () => {
      withCredits(100, "capped");
      const { sub } = await subscribe(1);
      assert.equal(grantOf(sub).role, 70);
      assert.match(mirror(sub).issue, /rollover is capped, and its rollover_cap is null, which is no cap/);
      assert.equal(credits.of(ann.id), 0);
      store.tables.plans[0].rollover_cap = 20;
      await sweepAll(ctx, { only: "shop" });
      assert.equal(credits.of(ann.id), 100);
      assert.equal(mirror(sub).issue, null);
    });

    const pack = { account: "shop", table: "credit_packs", amount: "price", description: "name", currency: "EUR", grant_credits: "units" };

    test("a credit pack paid through checkout grants its credits once, and a refund leaves them", async () => {
      const started = await actions.pay_checkout.run(argument({ configuration: { ...pack, key_field: "pack", credits_expire_days: 365 }, payload: { pack: 1 } }));
      assert.equal(started.amount, 200);
      await provider.pay(shop, started.checkout, "succeeded");
      assert.equal(credits.of(ann.id), 100);
      const [bucket] = [...credits.buckets.values()];
      assert.equal(bucket.kind, "top_up");
      assert.ok(Math.abs(Date.parse(bucket.expires_at) - Date.now() - 365 * DAY) < 60000, bucket.expires_at);
      for (const e of [...provider.events(shop)].reverse()) await provider.redeliver(shop, e.id);
      assert.equal(credits.buckets.size, 1);
      await actions.pay_refund.run(argument({ configuration: { source: "row", table: "credit_packs", key_field: "pack" }, payload: { pack: 1 } }));
      await provider.flush();
      assert.ok(kinds().includes("payment.refunded"));
      assert.equal(credits.of(ann.id), 100, "only agent runs spend credits");
    });

    test("pay_charge_saved charges the saved card once per key, and falls back to checkout", async () => {
      await subscribe(2);
      const charge = (payload, user = ann) => actions.pay_charge_saved.run(argument({ user, configuration: { ...pack, key_from: "fixed", key_value: "1" }, payload }));
      const paid = await charge({ key: "low-1" });
      await provider.flush();
      assert.deepEqual([paid.status, paid.amount, paid.currency], ["paid", 200, "EUR"]);
      assert.equal(credits.of(ann.id), 100, "granted from the webhook");
      const succeeded = store.tables.pay_events.filter((e) => e.kind === "payment.succeeded" && e.ref_table === "credit_packs");
      assert.deepEqual(succeeded.map((e) => [e.ref_key, e.user_id, e.amount]), [[1, 7, 200]]);
      // The same key again — a trigger run twice for one notice — charges nothing more.
      const again = await charge({ key: "low-1" });
      await provider.flush();
      assert.deepEqual([again.status, again.payment], ["paid", paid.payment]);
      assert.equal(credits.of(ann.id), 100);
      // Another key is another top-up.
      await charge({ key: "low-2" });
      await provider.flush();
      assert.equal(credits.of(ann.id), 200);

      // The bank asks the buyer to authenticate: a checkout for the same lines, which keeps the card.
      const customer = store.tables.pay_objects.find((o) => o.kind === "customer" && o.user_id === ann.id);
      provider.setCard(shop, customer.provider_ref, "authenticate");
      const asked = await charge({ key: "low-3" });
      await provider.flush();
      assert.equal(asked.status, "requires_checkout");
      assert.match(asked.reason, /authenticat|additional user action/i);
      assert.match(asked.url, /^https:\/\//);
      assert.equal(credits.of(ann.id), 200);
      await provider.pay(shop, asked.checkout, "succeeded");
      assert.equal(credits.of(ann.id), 300, "paid on the page");
      const kept = await charge({ key: "low-4" });
      await provider.flush();
      assert.equal(kept.status, "paid", "the card paid on the page is the saved one now");
      assert.equal(credits.of(ann.id), 400);

      // A declined card: a checkout, and nothing granted.
      provider.setCard(shop, customer.provider_ref, "decline");
      const declined = await charge({ key: "low-5" });
      await provider.flush();
      assert.equal(declined.status, "requires_checkout");
      assert.equal(credits.of(ann.id), 400);

      // Bob has no saved card; a guest has no card at all.
      const bobs = await charge({ key: "b-1" }, bob);
      assert.deepEqual([bobs.status, bobs.reason], ["requires_checkout", 'you have no saved card on "shop" yet']);
      await assert.rejects(charge({ key: "g-1" }, null), /sign in first/);
    });
  });
}
