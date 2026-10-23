// The plan binding and what a subscription's state does to its grant (TODO-payments §6.2, P4.1):
// the table in §6.2, row by row.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const plans = require("../plans.js");

test("the binding is read from the module's settings, with a price field per account or one for all", () => {
  assert.equal(plans.planBinding({}), null);
  assert.equal(plans.planBinding({ plans_table: "  " }), null);
  const one = plans.planBinding({ plans_table: "plans", plan_price_field: "stripe_price" });
  assert.deepEqual(one, {
    table: "plans",
    price_fields: { all: "stripe_price", accounts: {} },
    role_field: "role",
    allowance_field: null,
    rollover_field: null,
    rollover_cap_field: null,
    grace_days: 3,
  });
  assert.equal(plans.priceField(one, "anything"), "stripe_price");
  const many = plans.planBinding({
    plans_table: "tiers",
    plan_price_field: "shop=stripe_price, ios = apple_product",
    plan_role_field: "gives",
    grace_days: "7",
  });
  assert.equal(plans.priceField(many, "shop"), "stripe_price");
  assert.equal(plans.priceField(many, "ios"), "apple_product");
  assert.equal(plans.priceField(many, "polar"), null);
  assert.deepEqual([many.role_field, many.grace_days], ["gives", 7]);
  assert.equal(plans.planBinding({ plans_table: "p", grace_days: 0 }).grace_days, 0);
});

test("a plan's role is a role a module may be asked to grant, or a sentence", () => {
  const binding = plans.planBinding({ plans_table: "plans" });
  assert.deepEqual(plans.planRole(binding, { role: 70 }), { role: 70 });
  assert.deepEqual(plans.planRole(binding, { role: "60" }), { role: 60 });
  for (const bad of [1, 100, null, "admin", 70.5]) {
    assert.match(plans.planRole(binding, { role: bad }).problem, /not a role to grant/);
  }
});

const PERIOD_END = "2026-11-23T10:00:00.000Z";
const TRIAL_END = "2026-11-06T10:00:00.000Z";
const plus = (iso, days) => new Date(Date.parse(iso) + days * 86400000).toISOString();

test("each status does to the grant what §6.2's table says", () => {
  const grant = (fields) => plans.subscriptionGrant({ current_period_end: PERIOD_END, ...fields }, 3);
  // Trialing and active: to the period's end (or the trial's), plus grace.
  assert.deepEqual(grant({ status: "active" }), { op: "grant", expires_at: plus(PERIOD_END, 3) });
  assert.deepEqual(grant({ status: "trialing", trial_end: TRIAL_END, current_period_end: TRIAL_END }), { op: "grant", expires_at: plus(TRIAL_END, 3) });
  // Past due: left alone, to run out by itself.
  assert.deepEqual(grant({ status: "past_due" }), { op: "keep" });
  // Canceled at the period's end: kept exactly to that end — no renewal is coming.
  assert.deepEqual(grant({ status: "active", cancel_at_period_end: true }), { op: "grant", expires_at: PERIOD_END });
  assert.deepEqual(grant({ status: "trialing", trial_end: TRIAL_END, cancel_at_period_end: true }), { op: "grant", expires_at: TRIAL_END });
  // A cancellation set for a day before the period's end ends it then.
  const early = plus(PERIOD_END, -5);
  assert.deepEqual(grant({ status: "active", cancel_at: early }), { op: "grant", expires_at: early });
  // Canceled now, paused, incomplete, unpaid: revoked.
  for (const status of ["canceled", "paused", "incomplete", "incomplete_expired", "unpaid"]) {
    assert.deepEqual(grant({ status }), { op: "revoke" }, status);
  }
  // An active subscription with no period end cannot be granted for, and says so.
  assert.match(plans.subscriptionGrant({ status: "active" }, 3).problem, /says nothing of when its period ends/);
});

test("a plan's LLM-credit allowance and its rollover, or a sentence", () => {
  const binding = plans.planBinding({ plans_table: "plans", plan_allowance_field: "credits", plan_rollover_field: "rollover", plan_rollover_cap_field: "cap" });
  // Nothing included: no allowance, and no issue.
  assert.equal(plans.planAllowance(plans.planBinding({ plans_table: "plans" }), { credits: 100 }), null);
  assert.equal(plans.planAllowance(binding, { credits: null }), null);
  assert.equal(plans.planAllowance(binding, { credits: 0 }), null);
  // The policy as a cap: none carries nothing, all everything, capped up to the cap.
  assert.deepEqual(plans.planAllowance(binding, { credits: 100 }), { amount: 100, cap: 0 });
  assert.deepEqual(plans.planAllowance(binding, { credits: "100", rollover: "none" }), { amount: 100, cap: 0 });
  assert.deepEqual(plans.planAllowance(binding, { credits: 100, rollover: "All" }), { amount: 100, cap: null });
  assert.deepEqual(plans.planAllowance(binding, { credits: 100, rollover: "capped", cap: 50 }), { amount: 100, cap: 50 });
  assert.match(plans.planAllowance(binding, { credits: 100, rollover: "capped" }).problem, /capped, and its cap is null, which is no cap/);
  assert.match(plans.planAllowance(binding, { credits: 100, rollover: "some" }).problem, /a rollover is none, all, capped/);
  assert.match(plans.planAllowance(binding, { credits: "lots" }).problem, /not an amount of LLM credits/);
});

test("an allowance is for the period a trialing or active subscription is in, and none other", () => {
  const now = Date.parse("2026-10-23T10:00:00.000Z");
  assert.deepEqual(plans.allowancePeriod({ status: "active", current_period_end: PERIOD_END }, now), { end: PERIOD_END });
  assert.deepEqual(plans.allowancePeriod({ status: "trialing", trial_end: TRIAL_END, current_period_end: PERIOD_END }, now), { end: TRIAL_END });
  // Unix seconds, as some providers say it, are the same instant.
  assert.deepEqual(plans.allowancePeriod({ status: "active", current_period_end: Date.parse(PERIOD_END) / 1000 }, now), { end: PERIOD_END });
  // Past due: not paid for yet. Canceled: no period. Ended already: nothing to grant for.
  for (const status of ["past_due", "canceled", "incomplete"]) assert.equal(plans.allowancePeriod({ status, current_period_end: PERIOD_END }, now), null, status);
  assert.equal(plans.allowancePeriod({ status: "active", current_period_end: "2026-10-01T00:00:00.000Z" }, now), null);
});
