// The module's actions: what a trigger runs, and what an application calls through an exposed
// trigger (`api.runCheckout({ order })`).
//
// They are a function of the module's settings (`actionsFor(configuration)`, v1's `withCfg`):
// the settings hold the plan binding, which a subscription checkout, a plan change and the
// processing a fake provider's in-process delivery does all read.

"use strict";

const { usableAccount } = require("./accounts.js");
const { requireCapability } = require("./capabilities.js");
const { currencyCode, requireAmount } = require("./money.js");
const { CHECKOUT_FIELDS, payCheckout, recordCheckout } = require("./checkout.js");
const { REFUND_FIELDS, payRefund } = require("./refunds.js");
const { CHARGE_FIELDS, payChargeSaved } = require("./charge.js");
const subscriptions = require("./subscriptions.js");

const opt = (t) => ({ optional: t });

/** `pay_account_test`: a small one-off checkout on an account, to see it work end to end. */
const pay_account_test = {
  description:
    "Try a payment account: start a test checkout of a small amount and answer its URL. " +
    "Payload: { account, amount, currency, description }.",
  payload: {
    object: { account: "string", amount: opt("int"), currency: opt("string"), description: opt("string") },
  },
  returns: { object: { id: "string", url: "string", amount: "int", currency: "string" } },
  run: async ({ payload, app, db }) => {
    const body = payload || {};
    const { account, adapter } = await usableAccount(db, body.account);
    requireCapability(account, adapter, "one_off");
    const amount = requireAmount(body.amount === undefined ? 1000 : body.amount, "the amount");
    if (amount === 0) throw new Error("a test checkout charges something; the amount is 0");
    const currency = currencyCode(body.currency || "USD");
    const lines = [{ description: body.description || "Test payment", quantity: 1, unit_amount: amount }];
    const created = await adapter.createCheckout(account, {
      mode: "payment",
      currency,
      lines,
      // The page is served on the application the action was called through; from the admin
      // there is none, and the page is a path on the admin's own host.
      base_url: (app && app.url) || "",
      metadata: { test: true },
    });
    await recordCheckout(db, { account, total: amount, currency, lines }, created);
    return { id: created.id, url: created.url, amount, currency };
  },
};

/** `pay_checkout` (§6.1): a hosted checkout for a row of the application's. */
const pay_checkout = (moduleConfiguration) => ({
  description:
    "Take a payment for a row of one of your tables — an order, an invoice, a booking, a plan to " +
    "subscribe to — through a payment account's hosted checkout. The amounts are formulas over " +
    "the row (and its child rows), worked out on the server; the caller says only which row. " +
    "Answers the URL to send the buyer to. When it is paid, pay_events gets a payment.succeeded " +
    "row naming the row; a subscription also gives the subscriber its plan's role.",
  configFields: CHECKOUT_FIELDS,
  // The row's key is in the payload field the configuration names: `id` unless it says
  // otherwise (`{ order: 12 }`), which the open object still takes.
  payload: { object: { id: { optional: "json" } } },
  returns: { object: { id: "int", checkout: "string", url: "string", amount: opt("int"), currency: "string" } },
  run: (argument) => payCheckout({ ...argument, moduleConfiguration }),
});

/** `pay_charge_saved` (§6.3, P7.2): the signed-in user's saved card charged for a row — a
 * top-up — or, where it cannot be, a checkout for it. */
const pay_charge_saved = (moduleConfiguration) => ({
  description:
    "Charge the signed-in user's saved card for a row of one of your tables — a credit pack, " +
    "say — with no checkout page: the same settings as pay_checkout. Answers { status: \"paid\" } " +
    "when it was charged; when there is no saved card, the bank asks the buyer to authenticate, " +
    "or the card is declined, answers { status: \"requires_checkout\", url } for the same lines. " +
    "Its effects (LLM credits) are granted when the provider reports the payment.",
  configFields: CHARGE_FIELDS,
  payload: { object: { id: { optional: "json" }, key: { optional: "json" } } },
  returns: {
    object: {
      status: "string",
      id: opt("int"),
      payment: opt("string"),
      checkout: opt("string"),
      url: opt("string"),
      reason: opt("string"),
      amount: "int",
      currency: "string",
    },
  },
  run: (argument) => payChargeSaved(argument, moduleConfiguration),
});

/** `pay_refund` (P2.6): money back for a payment, given the row it was for or its mirror row. */
const pay_refund = (moduleConfiguration) => ({
  description:
    "Refund a payment, all of it or an amount worked out from the row: given the row of your table " +
    "it was for (its latest payment), or a pay_objects row. The provider then reports it, and " +
    "pay_events gets a payment.refunded row.",
  configFields: REFUND_FIELDS,
  payload: { object: { id: { optional: "json" } } },
  returns: { object: { refund: "string", payment: "int", amount: "int", currency: "string", status: "string" } },
  run: (argument) => payRefund(argument, moduleConfiguration),
});

/** `pay_portal` (§6.2, P4.3): the provider's customer portal, for the signed-in caller. */
const pay_portal = {
  description:
    "Open the payment provider's customer portal for the signed-in user, where they change plan, " +
    "change their card or cancel. Answers its URL. What they do there reaches pay_events, and a " +
    "plan change moves their role.",
  // No payload: whose portal it is, is the caller.
  configFields: subscriptions.PORTAL_FIELDS,
  returns: { object: { url: "string" } },
  run: (argument) => subscriptions.payPortal(argument),
};

/** `pay_change_subscription` (P4.3): the caller's subscription moved to another plan. */
const pay_change_subscription = (moduleConfiguration) => ({
  description:
    "Move the signed-in user's subscription to another plan, a row of the plans table the plan " +
    "binding names: { plan: <its key> }. The provider reports the change, and their role moves " +
    "with it.",
  configFields: subscriptions.CHANGE_FIELDS,
  // `plan` unless the trigger names another field for it, which the open object still takes.
  payload: { object: { plan: opt("json"), subscription: opt("int") } },
  returns: { object: { subscription: "int", price_ref: "string", status: "string" } },
  run: (argument) => subscriptions.payChangeSubscription(argument, moduleConfiguration),
});

/** `pay_cancel_subscription` (P4.3): the caller's subscription canceled. */
const pay_cancel_subscription = (moduleConfiguration) => ({
  description:
    "Cancel the signed-in user's subscription: at the end of the period paid for (access lasts " +
    "until then), or now. The provider reports it, as subscription.canceled and, when it ends, " +
    "subscription.ended.",
  configFields: subscriptions.CANCEL_FIELDS,
  payload: { object: { subscription: opt("int") } },
  returns: { object: { subscription: "int", status: "string", cancel_at_period_end: "bool" } },
  run: (argument) => subscriptions.payCancelSubscription(argument, moduleConfiguration),
});

/** `pay_provider_code` (§3.5): a body of the admin's, run in the module's worker with the
 * account's provider client in scope — Stripe's REST client as `stripe` — for anything the
 * actions do not cover. It reaches only the hosts the module may (its `net` permission). */
const pay_provider_code = {
  description:
    "Run JavaScript with a payment account's provider client in scope, for what the other " +
    "actions do not cover: `stripe` (and `client`) on a Stripe account, with get, post, del and " +
    "request over Stripe's API; also `account` (its name and mode), `payload`, `row`, `user` and " +
    "`db`. Answers what the code returns.",
  configFields: [
    { name: "account", label: "Payment account", type: "String", required: true, sublabel: "The name of a pay_accounts row" },
    {
      name: "code",
      label: "Code",
      type: "String",
      required: true,
      fieldview: "textarea",
      sublabel: 'An async function body: return await stripe.get("/v1/balance")',
    },
  ],
  run: async ({ configuration = {}, payload, row, user, db }) => {
    const { account, adapter } = await usableAccount(db, configuration.account);
    const AsyncFunction = Object.getPrototypeOf(async () => {}).constructor;
    let body;
    try {
      body = new AsyncFunction("client", adapter.name, "account", "payload", "row", "user", "db", String(configuration.code || ""));
    } catch (e) {
      throw new Error(`the code of pay_provider_code does not parse: ${e.message}`);
    }
    const client = adapter.client(account);
    // The account as the code sees it: which one, never its keys.
    const visible = { name: account.name, adapter: account.adapter, mode: account.mode };
    const result = await body(client, client, visible, payload, row, user, db);
    return result === undefined ? null : result;
  },
};

/** `pay_sync` (§6.5): the reconciliation sweep, now, for one account or all of them. */
const pay_sync = (moduleConfiguration) => ({
  description:
    "Ask each payment account's provider for the events since the last sweep and process any that " +
    "were never delivered, and read each live subscription again, as the reconcile service does " +
    "every few hours. Payload: { account } for one account; all of them otherwise.",
  payload: { object: { account: opt("string") } },
  returns: { object: { swept: { array: "json" } } },
  run: async ({ payload, db, roles, credits }) => {
    const { sweepAll } = require("./service.js");
    const only = payload && payload.account ? String(payload.account) : undefined;
    return { swept: await sweepAll(subscriptions.actionCtx({ db, roles, credits }, moduleConfiguration), { only }) };
  },
});

/** The actions, for the module's settings. */
function actionsFor(moduleConfiguration = {}) {
  return {
    pay_account_test,
    pay_checkout: pay_checkout(moduleConfiguration),
    pay_charge_saved: pay_charge_saved(moduleConfiguration),
    pay_refund: pay_refund(moduleConfiguration),
    pay_portal,
    pay_change_subscription: pay_change_subscription(moduleConfiguration),
    pay_cancel_subscription: pay_cancel_subscription(moduleConfiguration),
    pay_provider_code,
    pay_sync: pay_sync(moduleConfiguration),
  };
}

module.exports = { actionsFor, actions: actionsFor({}) };
