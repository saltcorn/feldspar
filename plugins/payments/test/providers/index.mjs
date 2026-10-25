// The providers the shared suite (`shared.test.mjs`, TODO-payments P5.2) runs against, each behind
// one **driver**: what a test does at the provider rather than through the module — the buyer on
// the hosted page, the test clock, the customer in the provider's portal, a delivery forged or
// sent again. The fake provider is driven through its own test hooks; Stripe and Polar through
// simulations of their APIs (`stripe-sim.mjs`, `polar-sim.mjs`).
//
// A driver:
// - `account(id, name, { hold })`: a `pay_accounts` row for it (held: its events are not delivered);
// - `install(accounts, deliverFor)` and `uninstall()`: put it in place, `deliverFor(account)`
//   being how its webhooks reach the module;
// - `pay(account, checkout, outcome)`, `renew` and `retry(account, subscription, outcome)`,
//   `portalChange(account, subscription, price)`: what happens at the provider, its events then
//   delivered;
// - `flush()`: deliver what the provider has made since (after a module action);
// - `events(account)`, `redeliver(account, id)`, `forged(account)`, `unknown(account)`;
// - `priceField` and `prices` (`basic`, `pro`, `staff`, `mystery`, `gone`): the plan binding's
//   field for the provider, and a price or product id for each plan;
// - `checkoutCustomer(account, checkout)`: the provider customer a checkout is for;
// - `setCard(account, customer, card)`: what the customer's saved card does when it is charged
//   with no page — `ok`, `authenticate` (the bank asks for 3-D Secure), `decline` — or null for none;
// - what differs at the provider, as flags: `renewalPaymentsKnowTheirRow`.

import { createRequire } from "node:module";
import { stripeSimulation } from "./stripe-sim.mjs";
import { polarSimulation } from "./polar-sim.mjs";

const require = createRequire(import.meta.url);
const fake = require("../../adapters/fake.js");

function fakeDriver() {
  const deliverers = new Map();
  const deliver = (account) => deliverers.get(account.name);
  const driver = {
    name: "fake",
    priceField: "fake_price",
    prices: { basic: "price_basic", pro: "price_pro", staff: "price_staff", mystery: "price_mystery", gone: "price_gone" },
    renewalPaymentsKnowTheirRow: true,
    account(n, name, { hold = false } = {}) {
      return { id: n, name, adapter: "fake", mode: "test", enabled: true, webhook_secret: `whsec_${name}`, settings: hold ? { webhooks: "hold", grants_in_test_mode: true } : { grants_in_test_mode: true } };
    },
    install(accounts, deliverFor) {
      fake.reset();
      for (const a of accounts) deliverers.set(a.name, deliverFor(a));
    },
    uninstall() {},
    // The fake delivers as it goes, through the module's own `deliver`.
    flush: async () => {},
    pay: (account, checkout, outcome) => fake.settle(account, checkout, outcome, deliver(account)),
    renew: (account, sub, outcome) => fake.advance(account, sub, outcome, deliver(account)),
    retry: (account, sub, outcome) => fake.retry(account, sub, outcome, deliver(account)),
    portalChange: (account, sub, price) => fake.changePlan(account, sub, price, {}, deliver(account)),
    events: (account) => fake.events(account),
    redeliver: async (account, id) => (await fake.resend(account, id, deliver(account))).status,
    forged(account) {
      const body = Buffer.from(JSON.stringify({ id: "evt_forged", type: "payment.succeeded", account: account.name }));
      return { rawBody: body, headers: { [fake.SIGNATURE_HEADER]: "t=1,v1=00" } };
    },
    unknown(account) {
      const body = Buffer.from(JSON.stringify({ id: "evt_odd", type: "payout.paid", account: account.name, data: { object: null } }));
      return { rawBody: body, headers: { [fake.SIGNATURE_HEADER]: fake.sign(account.webhook_secret, body) } };
    },
    checkoutCustomer: (account, checkout) => fake.view(account, checkout).checkout.customer,
    setCard: (account, customer, card) => fake.setCard(account, customer, card),
  };
  return driver;
}

/** A fresh driver for each provider. */
export const PROVIDERS = [fakeDriver, stripeSimulation, polarSimulation];
