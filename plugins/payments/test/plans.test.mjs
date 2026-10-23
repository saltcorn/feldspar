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
  assert.deepEqual(one, { table: "plans", price_fields: { all: "stripe_price", accounts: {} }, role_field: "role", grace_days: 3 });
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
