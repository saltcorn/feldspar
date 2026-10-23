// What a subscriber does to their own subscription (TODO-payments §6.2, P4.3): open the
// provider's customer portal (`pay_portal`), or, for an application that wants its own buttons,
// change plan (`pay_change_subscription`) and cancel (`pay_cancel_subscription`).
//
// Each acts for the **signed-in caller** on their own subscription only, found through the
// mirror's customer and subscription rows. Nothing here changes the mirror or a grant: the
// provider reports what happened like anything else, and processing applies it (§3.6) — so a
// tier change made here, in the provider's portal or in its dashboard moves the role the same
// way.

"use strict";

const { usableAccount } = require("./accounts.js");
const { requireCapability } = require("./capabilities.js");
const { appUrl } = require("./checkout.js");
const plans = require("./plans.js");
const { deliverTo } = require("./webhooks.js");

/** The statuses of a subscription that can still be changed or canceled. */
const LIVE = [...plans.GRANTING, ...plans.KEEPING];

/** How a plan change is charged, in the module's words; each adapter says it in its provider's
 * (Stripe's `create_prorations`, `always_invoice`, `none`; Polar's `prorate`, `invoice`). */
const PRORATION = ["prorate", "invoice", "none"];

const blank = (v) => v === undefined || v === null || String(v).trim() === "";

const ACCOUNT_FIELD = { name: "account", label: "Payment account", type: "String", required: true, sublabel: "The name of a pay_accounts row" };

const SUBSCRIPTION_FIELD = {
  name: "subscription_field",
  label: "Subscription payload field",
  type: "String",
  default: "subscription",
  sublabel: "The payload field naming which of the caller's subscriptions (a pay_objects id); their live one when it is absent",
};

const PORTAL_FIELDS = [
  ACCOUNT_FIELD,
  { name: "return_path", label: "Back to", type: "String", default: "/", sublabel: "A path on the application's host, where the portal's back link goes" },
];

const CHANGE_FIELDS = [
  ACCOUNT_FIELD,
  { name: "plan_field", label: "Plan payload field", type: "String", default: "plan", sublabel: "The payload field holding the key of the plan to move to: plan for { plan: 3 }" },
  {
    name: "price_ref",
    label: "Price",
    type: "String",
    sublabel: "A formula over the plan row giving the provider's price; empty is the plan binding's price field for the account",
  },
  {
    name: "proration",
    label: "Proration",
    type: "String",
    default: "prorate",
    attributes: { options: PRORATION },
    sublabel: "How the provider charges for the rest of the period: prorated on the next invoice, invoiced now, or not at all (not every provider can)",
  },
  SUBSCRIPTION_FIELD,
];

const CANCEL_FIELDS = [
  ACCOUNT_FIELD,
  { name: "at_period_end", label: "At the end of the period", type: "Bool", default: true, sublabel: "Off cancels now, and access ends now" },
  SUBSCRIPTION_FIELD,
];

/** The caller's user id, or a refusal: these act for a signed-in subscriber. */
function callerId(user) {
  if (!user || user.id === undefined || user.id === null || user.module) {
    throw new Error("sign in first: a subscription is changed by the user it belongs to");
  }
  return user.id;
}

/** The caller's subscription on `account`: the one the payload names, which must be theirs, or
 * their latest live one. */
async function callersSubscription(db, account, userId, configuration, payload) {
  const field = String(configuration.subscription_field || "subscription").trim();
  const named = payload && payload[field];
  if (!blank(named)) {
    const row = await db.pay_objects.get(named);
    if (!row || row.kind !== "subscription" || row.account !== account.id || String(row.user_id) !== String(userId)) {
      throw new Error(`you have no subscription ${JSON.stringify(named)} on "${account.name}"`);
    }
    if (!LIVE.includes(row.status)) throw new Error(`the subscription ${JSON.stringify(named)} is ${row.status}, so it cannot be changed`);
    return row;
  }
  const rows = await db.pay_objects.where({ account: account.id, kind: "subscription", user_id: userId }).orderBy("id", "desc").rows();
  const live = rows.find((r) => LIVE.includes(r.status));
  if (!live) throw new Error(`you have no subscription on "${account.name}" to change`);
  return live;
}

/** A log for a delivery the fake makes from inside an action, which has no service log. */
const actionLog = Object.assign((...a) => console.log(...a), { warn: (...a) => console.warn(...a), error: (...a) => console.error(...a) });

/** The `ctx` processing gets when the fake delivers from inside an action: the action's own
 * `db` and `roles`, and the module's settings. */
const actionCtx = ({ db, roles }, moduleConfiguration) => ({ db, roles, configuration: moduleConfiguration || {}, log: actionLog });

/** `pay_portal`: the provider's customer portal for the caller, answered as its URL. */
async function payPortal(argument) {
  const { configuration = {}, user, app, db } = argument;
  const { account, adapter } = await usableAccount(db, configuration.account);
  requireCapability(account, adapter, "portal");
  const userId = callerId(user);
  const customer = await db.pay_objects.where({ account: account.id, kind: "customer", user_id: userId }).first();
  if (!customer) throw new Error(`you are not a customer of "${account.name}" yet: the portal opens once you have subscribed or paid`);
  const portal = await adapter.createPortal(account, {
    customer: customer.provider_ref,
    return_url: appUrl(app, configuration.return_path, "", "path to go back to"),
    base_url: (app && app.url) || "",
  });
  return { url: portal.url };
}

/** `pay_change_subscription`: move the caller's subscription to another plan. */
async function payChangeSubscription(argument, moduleConfiguration) {
  const { configuration = {}, payload, user, db } = argument;
  const { account, adapter } = await usableAccount(db, configuration.account);
  requireCapability(account, adapter, "subscriptions");
  const userId = callerId(user);
  const binding = plans.planBinding(moduleConfiguration);
  if (!binding) throw new Error("set the plan binding in the payments module's settings first: a plan change moves to a row of the plans table");
  const field = String(configuration.plan_field || "plan").trim();
  const planKey = payload && payload[field];
  if (blank(planKey) || typeof planKey === "object") throw new Error(`say which plan to move to: { ${field}: <its key> }`);
  // The plan as the caller sees it: a plan they could not read is not one to move to.
  const visible = await db.asUser()[binding.table].get(planKey);
  if (!visible) throw new Error(`there is no ${binding.table} row ${JSON.stringify(planKey)} that you can read`);
  let price;
  if (!blank(configuration.price_ref)) {
    const values = await db[binding.table].select({ price_ref: configuration.price_ref }).get(planKey);
    price = values && values.price_ref;
  } else {
    const priceField = plans.priceField(binding, account.name);
    if (!priceField) throw new Error(`the plan binding names no price field for the account "${account.name}"`);
    price = visible[priceField];
  }
  if (blank(price) || typeof price !== "string") throw new Error(`the plan ${JSON.stringify(planKey)} has no price on "${account.name}"`);
  const proration = configuration.proration || "prorate";
  if (!PRORATION.includes(proration)) throw new Error(`proration is ${PRORATION.join(", ")}, not "${proration}"`);
  const subscription = await callersSubscription(db, account, userId, configuration, payload);
  if (subscription.price_ref === price) throw new Error("the subscription is on that plan already");
  const changed = await adapter.changePlan(account, subscription.provider_ref, price, { proration }, deliverTo(account, actionCtx(argument, moduleConfiguration)));
  return { subscription: subscription.id, price_ref: changed.price_ref, status: changed.status };
}

/** `pay_cancel_subscription`: cancel the caller's subscription, at the period's end or now. */
async function payCancelSubscription(argument, moduleConfiguration) {
  const { configuration = {}, payload, user, db } = argument;
  const { account, adapter } = await usableAccount(db, configuration.account);
  requireCapability(account, adapter, "subscriptions");
  const userId = callerId(user);
  const subscription = await callersSubscription(db, account, userId, configuration, payload);
  const atPeriodEnd = configuration.at_period_end !== false;
  const canceled = await adapter.cancel(account, subscription.provider_ref, { at_period_end: atPeriodEnd }, deliverTo(account, actionCtx(argument, moduleConfiguration)));
  return { subscription: subscription.id, status: canceled.status, cancel_at_period_end: Boolean(canceled.cancel_at_period_end) };
}

module.exports = {
  PORTAL_FIELDS,
  CHANGE_FIELDS,
  CANCEL_FIELDS,
  LIVE,
  payPortal,
  payChangeSubscription,
  payCancelSubscription,
  callersSubscription,
  actionCtx,
};
