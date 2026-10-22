// The adapters (TODO-payments §3.1, §4): the one part of the module that differs per provider.
//
// Each is a file here implementing the interface in §4 and declaring what it can do. Several may
// be configured at once — a `pay_accounts` row names one each — and the module's `net`
// permission is the union of their `hosts` (`hosts()`), which `bundled_catalog` checks against
// `feldspar-module.json`.

"use strict";

/** Every adapter, by the name an account's `adapter` field gives. Required here, at load: an
 * adapter needing an SDK requires it when first used, so this file loads with no package
 * installed. */
const ADAPTERS = {
  fake: require("./fake.js"),
  stripe: require("./stripe.js"),
};

/** The modes an account may be in. */
const MODES = ["test", "live"];

/** The adapter `account` names, or a refusal naming why it cannot be used: an adapter this
 * module does not have, a mode that is neither, a test-only adapter in live mode, or a disabled
 * account. */
function adapterFor(account) {
  const adapter = ADAPTERS[account.adapter];
  if (!adapter) {
    throw new Error(
      `the payment account "${account.name}" names the adapter "${account.adapter}", which this ` +
        `module does not have; it has: ${Object.keys(ADAPTERS).join(", ")}`,
    );
  }
  if (!MODES.includes(account.mode)) {
    throw new Error(`the payment account "${account.name}" is in mode "${account.mode}"; it is test or live`);
  }
  if (adapter.testOnly && account.mode === "live") {
    throw new Error(
      `the payment account "${account.name}" uses the ${adapter.name} adapter, which takes no real ` +
        `money, so it cannot be in live mode`,
    );
  }
  if (account.enabled === false) {
    throw new Error(`the payment account "${account.name}" is disabled`);
  }
  return adapter;
}

/** The hosts every adapter reaches: what the module's `net` permission must allow, and no more. */
function hosts() {
  return [...new Set(Object.values(ADAPTERS).flatMap((a) => a.hosts))].sort();
}

module.exports = { ADAPTERS, MODES, adapterFor, hosts };
