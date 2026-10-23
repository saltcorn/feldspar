// The **plan binding** and what a subscription grants (TODO-payments §6.2, P4.1).
//
// Plans are rows of a table the application's admin made, with whatever fields the business
// needs. The module's settings say only which of them mean what: "plans are the rows of
// `plans`; `stripe_price` holds the Stripe price; `role` is the role it gives". Whenever a
// subscription changes, the plan is looked up from its **current** price, so a tier changed in
// the provider's portal moves the role with nothing for the application to do.
//
// A subscription's access is a role grant (§3.7) with the module's package name as its source
// and `<account>:<subscription id>` as its reference: one grant per subscription, upserted. It
// runs to the end of the period paid for, plus a grace period for a renewal whose payment
// failed, and each renewal extends it. Missing every webhook makes access lapse on time; it
// never extends it.

"use strict";

/** What the module's settings form asks (v1's `configuration_workflow` fields). */
const PLAN_FIELDS = [
  {
    name: "plans_table",
    label: "Plans table",
    type: "String",
    sublabel: "The table of your plans, one row per plan you sell: plans. Leave empty if you sell no subscriptions",
  },
  {
    name: "plan_price_field",
    label: "Price field",
    type: "String",
    sublabel:
      "The plans table's field holding the provider's price: stripe_price. Accounts on different " +
      "providers each name theirs: shop=stripe_price, polar=polar_product",
  },
  { name: "plan_role_field", label: "Role field", type: "String", default: "role", sublabel: "The plans table's field holding the role a plan gives: role" },
  {
    name: "grace_days",
    label: "Grace period (days)",
    type: "Integer",
    default: 3,
    sublabel: "How long access lasts past the end of a period whose renewal has not been paid yet",
  },
];

/** The days of grace when the settings give none. */
const DEFAULT_GRACE_DAYS = 3;

/** A subscription status, and what it does to the grant (§6.2's table). */
const GRANTING = ["trialing", "active"];
const KEEPING = ["past_due"];

const blank = (v) => v === undefined || v === null || String(v).trim() === "";

/** The price fields, from `shop=stripe_price, polar=polar_product` or one field for every
 * account (`stripe_price`). */
function parsePriceFields(text) {
  const out = { all: null, accounts: {} };
  for (const part of String(text || "").split(",")) {
    const item = part.trim();
    if (!item) continue;
    const eq = item.indexOf("=");
    if (eq < 0) out.all = item;
    else {
      const account = item.slice(0, eq).trim();
      const field = item.slice(eq + 1).trim();
      if (account && field) out.accounts[account] = field;
    }
  }
  return out;
}

/** The binding the module's settings describe, or null when no plans table is named. */
function planBinding(configuration) {
  const c = configuration || {};
  if (blank(c.plans_table)) return null;
  const grace = blank(c.grace_days) ? DEFAULT_GRACE_DAYS : Number(c.grace_days);
  return {
    table: String(c.plans_table).trim(),
    price_fields: parsePriceFields(c.plan_price_field),
    role_field: blank(c.plan_role_field) ? "role" : String(c.plan_role_field).trim(),
    grace_days: Number.isFinite(grace) && grace >= 0 ? grace : DEFAULT_GRACE_DAYS,
  };
}

/** The plans table's field holding `accountName`'s prices, or null. */
function priceField(binding, accountName) {
  if (!binding) return null;
  return binding.price_fields.accounts[accountName] || binding.price_fields.all || null;
}

/** The plan row whose price for `account` is `priceRef`, or a sentence saying why there is none. */
async function findPlan(db, binding, account, priceRef) {
  if (!binding) return { problem: "no plan binding is set in the payments module's settings, so a subscription grants no role" };
  const field = priceField(binding, account.name);
  if (!field) {
    return { problem: `the plan binding names no price field for the account "${account.name}", so its subscriptions grant no role` };
  }
  if (blank(priceRef)) return { problem: "the subscription has no price, so it is no plan's" };
  const plan = await db[binding.table].where({ [field]: String(priceRef) }).first();
  if (!plan) {
    return { problem: `its price ${priceRef} is no plan's: no row of ${binding.table} has it as ${field}, so it grants no role` };
  }
  return { plan, field };
}

/** The role a plan row gives, or a sentence saying why it gives none. */
function planRole(binding, plan) {
  const value = plan[binding.role_field];
  const role = typeof value === "string" && /^\d+$/.test(value.trim()) ? Number(value) : value;
  if (!Number.isInteger(role) || role <= 1 || role >= 100) {
    return { problem: `the plan's ${binding.role_field} is ${JSON.stringify(value)}, which is not a role to grant (2 to 99)` };
  }
  return { role };
}

/** An instant as milliseconds, from an ISO string, a Date or unix seconds; null for none. */
function millis(value) {
  if (value === undefined || value === null || value === "") return null;
  if (value instanceof Date) return value.getTime();
  if (typeof value === "number") return value < 1e12 ? value * 1000 : value;
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : parsed;
}

const DAY_MS = 24 * 60 * 60 * 1000;

/**
 * What a subscription's current state does to its grant (§6.2):
 * - `trialing` and `active`: granted until the trial's or the period's end, plus grace — or
 *   exactly to that end when it is canceled at the period's end, since no renewal is coming;
 * - `past_due`: left as it is, to run out on its own unless the payment is recovered;
 * - anything else (canceled now, ended, paused, incomplete, unpaid): revoked.
 * Answers `{ op: "grant", expires_at }`, `{ op: "keep" }` or `{ op: "revoke" }`, or
 * `{ op: "keep", problem }` when it cannot tell.
 */
function subscriptionGrant(subscription, graceDays) {
  const status = subscription.status;
  if (KEEPING.includes(status)) return { op: "keep" };
  if (!GRANTING.includes(status)) return { op: "revoke" };
  const trialEnd = status === "trialing" ? millis(subscription.trial_end) : null;
  let end = trialEnd || millis(subscription.current_period_end);
  if (end === null) return { op: "keep", problem: `the ${status} subscription says nothing of when its period ends` };
  const cancelAt = millis(subscription.cancel_at);
  if (subscription.cancel_at_period_end || cancelAt !== null) {
    if (cancelAt !== null && cancelAt < end) end = cancelAt;
    return { op: "grant", expires_at: new Date(end).toISOString() };
  }
  return { op: "grant", expires_at: new Date(end + graceDays * DAY_MS).toISOString() };
}

module.exports = {
  PLAN_FIELDS,
  DEFAULT_GRACE_DAYS,
  GRANTING,
  KEEPING,
  parsePriceFields,
  planBinding,
  priceField,
  findPlan,
  planRole,
  subscriptionGrant,
  millis,
};
