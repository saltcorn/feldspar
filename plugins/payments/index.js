// **Payments** — the bundled module `@feldspar/payments` (TODO-payments.md).
//
// One module with an adapter per provider (§3.1). It owns three tables (§5): the provider
// accounts, a mirror of the provider's objects, and a log of payment events. The application
// owns everything else — its orders, plans and invoices — and reacts to `pay_events` with its
// own triggers (§3.4). The module never writes an application's tables.
//
// The parts:
// - `tables.js`: the three tables;
// - `adapters/`: one file per provider, behind the interface in §4 (`fake` and `stripe`);
// - `capabilities.js`: what an adapter can do, and the refusal naming what it cannot;
// - `money.js`: amounts as whole minor units beside a currency code (§3.11);
// - `accounts.js`: the account a caller names, and its adapter;
// - `webhooks.js`: each account's provider events, and the fake provider's checkout page;
// - `processing.js`: what a provider event does — the mirror, and the `pay_events` rows;
// - `checkout.js`, `refunds.js`: `pay_checkout`'s configuration, and `pay_refund`;
// - `service.js`: the reconcile sweep, which also keeps the accounts for `pay_api`;
// - `actions.js`: the actions an application calls.

"use strict";

const { tables } = require("./tables.js");

/** The actions, each `run` requiring `actions.js` when first used. */
const ACTIONS = require("./actions.js").actions;

module.exports = {
  sc_plugin_api_version: 1,
  tables,
  webhooks: {
    events: {
      description: "Each account's provider events, at events/<account name>",
      handle: (req, ctx) => require("./webhooks.js").events(req, ctx),
    },
    fake: {
      description: "The test provider's checkout page, for accounts on the fake adapter",
      methods: ["GET", "POST"],
      handle: (req, ctx) => require("./webhooks.js").fakeProvider(req, ctx),
    },
  },
  actions: ACTIONS,
  services: {
    reconcile: {
      description: "Every few hours, processes the provider events that were never delivered",
      start: (ctx) => require("./service.js").start(ctx),
    },
  },
  functions: {
    pay_api: {
      description:
        "An authenticated request to a payment account's provider API, with its keys: " +
        'await modfn("payments").pay_api("shop", "GET", "/v1/balance")',
      isAsync: true,
      arguments: [
        { name: "account", type: "String" },
        { name: "method", type: "String" },
        { name: "path", type: "String" },
        { name: "body", type: "Object" },
      ],
      run: async (account, method, path, body) => {
        const { keptAccount } = require("./accounts.js");
        const found = keptAccount(account);
        if (typeof found.adapter.api !== "function") throw new Error(`the ${found.adapter.name} adapter has no API to call`);
        return found.adapter.api(found.account, method, path, body);
      },
    },
  },
  // What an application's pages need allowed to embed Stripe's payment form rather than redirect
  // to its hosted checkout (C6): suggested, and switched on per application by the admin.
  csp: {
    stripe_elements: {
      description: "Stripe's embedded payment form (Payment Element, Express Checkout)",
      sources: {
        "script-src": ["https://js.stripe.com"],
        "frame-src": ["https://js.stripe.com", "https://hooks.stripe.com"],
        "connect-src": ["https://api.stripe.com"],
      },
    },
  },
};
