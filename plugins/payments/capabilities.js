// What an adapter can do (TODO-payments §4), and the refusal that names what it cannot.
//
// An adapter declares `capabilities`, a flag each. The trigger forms offer what an account's
// adapter can do, and an action asked for something else refuses by name — "the fake account
// test cannot take subscriptions" — rather than failing somewhere inside the provider.

"use strict";

/** Every capability, with what doing it is called in a refusal. */
const CAPABILITIES = {
  one_off: "take one-off payments",
  adhoc_prices: "charge an amount worked out at checkout",
  subscriptions: "take subscriptions",
  trial_with_card: "start a trial with the card taken up front",
  portal: "open a customer portal",
  charge_saved_method: "charge a saved card without a checkout page",
  native_sheet: "open the native payment sheet",
  manual_capture: "authorise a payment now and capture it later",
  async_methods: "take bank payments that settle later",
  marketplace: "pay sellers on a marketplace",
  physical_goods: "sell physical goods",
};

/** Whether the adapter acts as merchant of record: `no`, `optional` (an account setting) or
 * `always`. Not a flag, so it has its own name. */
const MERCHANT_OF_RECORD = ["no", "optional", "always"];

/** Whether `adapter` can do `capability`. */
function can(adapter, capability) {
  if (!Object.prototype.hasOwnProperty.call(CAPABILITIES, capability)) {
    throw new Error(`"${capability}" is not a payment capability; they are: ${Object.keys(CAPABILITIES).join(", ")}`);
  }
  return Boolean(adapter.capabilities && adapter.capabilities[capability] === true);
}

/** Refuse, naming the account and what it cannot do, unless its adapter can. */
function requireCapability(account, adapter, capability) {
  if (!can(adapter, capability)) {
    throw new Error(
      `the ${adapter.name} account "${account.name}" cannot ${CAPABILITIES[capability]}: ` +
        `its adapter does not support it`,
    );
  }
}

/** An adapter's declaration checked: every flag is a known capability, and merchant of record
 * is one of its three values. What a test of a new adapter asserts first. */
function checkDeclaration(adapter) {
  const problems = [];
  for (const [key, value] of Object.entries(adapter.capabilities || {})) {
    if (key === "merchant_of_record") {
      if (!MERCHANT_OF_RECORD.includes(value)) problems.push(`merchant_of_record is ${JSON.stringify(value)}`);
    } else if (!Object.prototype.hasOwnProperty.call(CAPABILITIES, key)) {
      problems.push(`${key} is not a capability`);
    } else if (typeof value !== "boolean") {
      problems.push(`${key} is not true or false`);
    }
  }
  if (!Array.isArray(adapter.hosts)) problems.push("hosts is not a list");
  return problems;
}

module.exports = { CAPABILITIES, MERCHANT_OF_RECORD, can, requireCapability, checkDeclaration };
