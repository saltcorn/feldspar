// The fake provider's own customer portal page (TODO-payments P4.3): what it offers and what posting
// it does. What subscriptions do — the grant through a trial, renewals, a failed renewal, plan
// changes, cancelling, an unbound price, a refused grant and the sweep — is the shared suite's
// (`shared.test.mjs`), run against every provider.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb } from "./fake-db.mjs";

const require = createRequire(import.meta.url);
const fake = require("../adapters/fake.js");
const { actionsFor } = require("../actions.js");
const { deliverTo, fakeProvider } = require("../webhooks.js");

const account = { id: 1, name: "shop", adapter: "fake", mode: "test", enabled: true, webhook_secret: "whsec_test", settings: { grants_in_test_mode: true } };
const configuration = { plans_table: "plans", plan_price_field: "stripe_price", plan_role_field: "role", grace_days: 3 };
const actions = actionsFor(configuration);

/** A `roles` that keeps what it was asked, refusing (as the server does) a role above its
 * ceiling. */
function recordingRoles(ceiling = 50) {
  const grants = new Map();
  return {
    grants,
    ceiling,
    async grant({ user, role, ref, expires_at }) {
      if (role === 1 || role < this.ceiling) throw new Error(`module asked to grant role ${role}, but it may only grant roles ${this.ceiling} to 99`);
      grants.set(ref, { user, role, expires_at: expires_at || null, revoked: false });
      return grants.get(ref);
    },
    async revoke(ref) {
      const g = grants.get(ref);
      if (g) g.revoked = true;
      return { revoked: Boolean(g) };
    },
  };
}

let store;
let roles;
let ctx;
beforeEach(() => {
  fake.reset();
  store = fakeDb({
    pay_accounts: [{ ...account }],
    plans: [
      { id: 1, name: "Basic", stripe_price: "price_basic", role: 70 },
      { id: 2, name: "Pro", stripe_price: "price_pro", role: 60 },
    ],
  });
  roles = recordingRoles();
  ctx = { db: store.db, roles, configuration, log: Object.assign(() => {}, { warn: () => {}, error: () => {} }) };
});

const ann = { id: 7, email: "ann@example.com" };
const argument = (extra) => ({ row: null, user: ann, app: { id: "a", url: "https://saas.example.com" }, db: store.db, roles, ...extra });

/** Ann subscribes to `plan` through `pay_checkout`, and pays on the fake's page. */
async function subscribe(plan) {
  const started = await actions.pay_checkout.run(
    argument({ configuration: { account: "shop", mode: "subscription", table: "plans", key_field: "plan", currency: "EUR" }, payload: { plan } }),
  );
  await fake.settle(account, started.checkout, "succeeded", deliverTo(account, ctx));
  return { started, sub: store.tables.pay_objects.find((o) => o.kind === "subscription" && o.ref_key === plan) };
}

const grantOf = (sub) => roles.grants.get(`${account.name}:${sub.provider_ref}`);

test("the fake's portal page offers the plans' prices, goes back to the application, and a change posted there moves the role", async () => {
  const { sub } = await subscribe(1);
  await actions.pay_change_subscription.run(argument({ configuration: { account: "shop" }, payload: { plan: 2 } }));
  const { url } = await actions.pay_portal.run(argument({ configuration: { account: "shop", return_path: "/account" } }));
  assert.match(url, /^https:\/\/saas\.example\.com\/_feldspar\/hooks\/payments\/fake\/shop\/portal\/bps_fake_/);
  const path = new URL(url).pathname;
  const rest = path.replace("/_feldspar/hooks/payments/fake/", "");
  const page = await fakeProvider({ method: "GET", rest, query: {}, headers: {}, rawBody: Buffer.alloc(0) }, ctx);
  assert.equal(page.status, 200);
  assert.match(page.body, /<option selected>price_pro<\/option>/);
  assert.match(page.body, /href="https:\/\/saas\.example\.com\/account"/);
  const posted = await fakeProvider(
    {
      method: "POST",
      rest: `${rest}/${sub.provider_ref}/change`,
      query: {},
      headers: { "content-type": "application/x-www-form-urlencoded" },
      rawBody: Buffer.from("price=price_basic"),
    },
    ctx,
  );
  assert.equal(posted.status, 303);
  assert.equal(grantOf(sub).role, 70);
});
