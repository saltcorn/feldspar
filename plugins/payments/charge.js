// **`pay_charge_saved`** (TODO-payments §6.3, P7.2): charge the card a signed-in user already gave
// the provider — a top-up of LLM credits, bought again with no checkout page.
//
// It takes the checkout configuration (`checkout.js`) in payment mode: the row being paid for (a
// credit pack), its amount and description as formulas, and its effects, *grant credits* above
// all. Where the account's adapter can charge a saved card (`charge_saved_method`: a Stripe
// off-session PaymentIntent, a Polar off-session order, the fake's saved card) and the user is a
// customer of the account already, the card is charged and the answer is `{ status: "paid" }`.
// Otherwise — no saved card, the bank asks the buyer to authenticate (SCA), the card is declined,
// or the adapter cannot — the answer is a hosted checkout for the same lines, `{ status:
// "requires_checkout", url, reason }`, which keeps the card for the next time.
//
// The credits are **granted from the webhook**, never from here: the payment's mirror row carries
// the effects, and the provider's own metadata carries them too, so that a webhook that arrives
// before this action has written the mirror row still grants them. A charge is keyed — by the
// payload's key, the trigger's row, or a new key for each call — so a trigger that runs twice for
// one low-balance notice charges once.

"use strict";

const crypto = require("node:crypto");
const { CHECKOUT_FIELDS, prepareCheckout, startCheckout } = require("./checkout.js");
const { deliverTo } = require("./webhooks.js");
const { actionCtx } = require("./subscriptions.js");

/** The checkout configuration, less what a saved card's charge has no use for: it is a one-off
 * payment, for a buyer who is not sent anywhere unless the bank asks them to be. */
const LEFT_OUT = new Set(["mode", "price_ref", "subscription_quantity", "trial_days", "shipping", "shipping_countries", "save_card"]);

const CHARGE_FIELDS = [
  ...CHECKOUT_FIELDS.filter((f) => !LEFT_OUT.has(f.name) && f.label !== "Subscription"),
  { input_type: "section_header", label: "The charge" },
  {
    name: "charge_key_field",
    label: "Key payload field",
    type: "String",
    default: "key",
    sublabel:
      "The payload field holding a key for this charge: asked again with the same key, nothing more is charged. " +
      "Without one, the key is the id of the row the trigger fires on (a low_balance entry), else each call is a new charge",
  },
];

const blank = (v) => v === undefined || v === null || String(v).trim() === "";

/** What makes a retried charge the same charge. */
function chargeKey(configuration, payload, row) {
  const field = String(configuration.charge_key_field || "key").trim();
  const given = payload && payload[field];
  if (!blank(given) && typeof given !== "object") return `key:${given}`;
  // The trigger's row, unless that row is what is being paid for: a pack bought twice is two charges.
  if (row && configuration.key_from !== "row" && !blank(row.id)) return `row:${row.id}`;
  return `call:${crypto.randomUUID()}`;
}

/** The payment's mirror row, written now with what it was for and what it grants — or brought up
 * to date, when the provider's webhook got there first. */
async function recordPayment(db, prepared, object) {
  const now = new Date().toISOString();
  const values = {
    kind: "payment",
    status: object.status || null,
    user_id: prepared.customer.user_id,
    email: prepared.customer.email,
    ref_table: prepared.table,
    ref_key: prepared.key,
    amount: Number.isSafeInteger(object.amount) ? object.amount : prepared.total,
    currency: object.currency || prepared.currency,
    lines: prepared.lines,
    effects: prepared.effects || null,
    provider_data: object.provider_data || null,
    updated_at: now,
  };
  const key = { account: prepared.account.id, provider_ref: String(object.ref) };
  const existing = await db.pay_objects.where(key).first();
  if (existing) {
    // The webhook's status is the provider's latest word; the charge's answer may be older.
    if (existing.status) delete values.status;
    await db.pay_objects.where({ id: existing.id }).update(values);
    return { ...existing, ...values };
  }
  try {
    return await db.pay_objects.insert({ ...key, ...values, created_at: now });
  } catch (e) {
    const raced = await db.pay_objects.where(key).first();
    if (!raced) throw e;
    return raced;
  }
}

/** `pay_charge_saved`: the saved card charged, or a checkout to pay through instead. */
async function payChargeSaved(argument, moduleConfiguration) {
  const { configuration = {}, payload, row, db } = argument;
  const prepared = await prepareCheckout({ ...argument, configuration: { ...configuration, mode: "payment", save_card: true }, moduleConfiguration });
  const { account, adapter } = prepared;
  if (prepared.customer.user_id === null) {
    throw new Error("sign in first: a saved card is a signed-in user's, and a guest has none");
  }
  const answer = { amount: prepared.total, currency: prepared.currency };
  let reason;
  if (!(adapter.capabilities && adapter.capabilities.charge_saved_method) || typeof adapter.chargeSavedMethod !== "function") {
    reason = `the ${adapter.name} adapter cannot charge a saved card`;
  } else if (!prepared.customer.ref) {
    reason = `you have no saved card on "${account.name}" yet`;
  } else {
    const metadata = { ...prepared.metadata };
    if (prepared.effects) metadata.sc_effects = JSON.stringify(prepared.effects);
    const charged = await adapter.chargeSavedMethod(
      account,
      {
        customer: prepared.customer.ref,
        amount: prepared.total,
        currency: prepared.currency,
        description: prepared.lines.map((l) => (l.quantity > 1 ? `${l.quantity} × ${l.description}` : l.description)).join(", "),
        key: `charge:${account.id}:${chargeKey(configuration, payload, row)}`,
        metadata,
      },
      deliverTo(account, actionCtx(argument, moduleConfiguration)),
    );
    const mirrored = charged.object ? await recordPayment(db, prepared, charged.object) : null;
    if (charged.status !== "requires_checkout") {
      return { status: charged.status, id: mirrored ? mirrored.id : null, payment: charged.object ? String(charged.object.ref) : null, ...answer };
    }
    reason = charged.reason || "the provider asks for the buyer";
  }
  const started = await startCheckout(db, prepared);
  return { status: "requires_checkout", reason, id: started.id, checkout: started.checkout, url: started.url, ...answer };
}

module.exports = { CHARGE_FIELDS, payChargeSaved, recordPayment, chargeKey };
