# Saltcorn v2 — Payments: the bundled payments module (milestones P1–P12)

Ordered, checkable task list for **taking payments in applications**: one bundled module,
`@feldspar/payments`, with a provider adapter per payment service, plus the core changes it
needs. It replaces the Saltcorn v1 Stripe module, which put a user on a higher role while their
subscription was active and had a function returning the `stripe` object so that custom code
could use the whole Stripe API.

This builds on modules (`plugins/README.md`, [docs/tutorial-modules.md](./docs/tutorial-modules.md),
TECHNICAL_DESIGN §15.1–15.1d: declared tables, services, `Secret` fields, typed action payloads),
roles (§7.4), exposed triggers (§10.2), agents and their cost accounting (§11.1, `Usage::cost`),
and the React Native framework (`plugins/react-native`).

The design comes first, then the milestones. Each milestone is prefixed **P** and its tasks are
numbered P1.1, P1.2, …. The order follows the use cases in priority order (§1). P1, P3 and P6
are core and do not depend on each other.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Part I — Design

## 1. What has to work

| | Use case | Who pays | Logged in? | What a payment produces |
|---|---|---|---|---|
| **1** | **Shop.** The application sells goods. A basket is paid for at checkout. | a buyer | maybe not | an order, marked paid; stock and fulfilment are the app's |
| **2** | **Subscription SaaS.** Paying gives access, as a role. Tiers give different roles. Free tiers and free trials are optional. | the user | yes | a role grant that lasts while the subscription does |
| **3** | **Credits.** A payment buys units that some activity uses up, for example an LLM agent. A subscription can include an allowance, which may roll over up to a cap. Top-ups are charged to the same card. | the user | yes | credit added to a balance; activity draws it down |
| **4** | **Mobile wallets.** Apple Pay and Google Pay in React Native apps. | as 1–3 | as 1–3 | as 1–3 |
| **5** | **Marketplace.** One user sells, another buys, and the app keeps X% and pays the seller the rest. | a buyer | yes | an order, a fee for the app, a payout to the seller |
| **6** | **Invoices and payment requests** (added use case). A services business (a consultancy, a booking or rental app) bills a customer for an amount the app works out (a timesheet, a booking deposit). It sends a pay link by email, and the customer may pay by card or by bank transfer or direct debit (SEPA, ACH, iDEAL), which settles days later. A deposit can be authorised now and captured later. | a customer, often with no account | no | an invoice row whose status follows the payment, including "processing" for days |

Use case 6 is worth having because it is the commonest payment in the business apps Saltcorn is
used for. It also tests the design on two things the first three use cases do not cover: **slow
payment methods**, where "paid" arrives days later, and **payments with no basket and no user**.

## 2. The providers

Two kinds of provider, and the difference decides the legal setup, not just the API:

- **Payment service provider (PSP).** The app's business is the seller. The provider moves the
  money; the business handles sales tax and VAT, refunds, disputes and invoices. PSPs can sell
  physical goods and run marketplaces.
- **Merchant of record (MoR).** The provider is legally the seller and resells to the customer.
  It collects and pays VAT and sales tax worldwide and handles disputes and buyer support, for a
  higher fee (about 4–6.5%). MoRs sell **digital goods and SaaS only** (no physical goods), and
  **none of them supports marketplaces**.

| Provider | Kind | One-off checkout | Subscriptions and trials | Customer portal | Credits and usage | Apple/Google Pay | Marketplace | Notes |
|---|---|---|---|---|---|---|---|---|
| **Stripe** | PSP, **and** MoR via *Managed Payments* | yes, with ad-hoc prices | yes; trials with or without a card | yes | Meters and credit grants | web, and native in React Native (`@stripe/stripe-react-native`) | Connect | Managed Payments became generally available in 2026: a `managed_payments` flag on Checkout Sessions. It is digital goods only, **Checkout only** (no API-built subscriptions) and **no Connect**. So one adapter covers PSP and MoR. |
| **Paddle** (Billing) | MoR | yes | yes | yes | one-off charges on a subscription | in its web checkout | no | The established MoR. 5% + 50¢. |
| **Polar** | MoR | yes | yes | yes | meters, credits | web checkout | no | Open source and aimed at developers. |
| **Lemon Squeezy** | MoR | yes | yes | yes | usage-based | web checkout | no | Owned by Stripe since 2024. |
| **Creem**, **Dodo Payments** | MoR | yes | yes | yes | usage-based | web checkout | no | Cheaper (about 4% + 40¢) and younger. Later. |
| **Mollie** | PSP (EU) | yes | through mandates (the first payment makes one) | no | no | web | Mollie Connect | The European local methods: iDEAL, Bancontact, SEPA. Good for use case 6. |
| **Adyen** | PSP (enterprise) | yes | yes | no | no | yes | for Platforms | Later, on demand. |
| **PayPal** | PSP or wallet | yes | yes | no | no | no | limited | Later, on demand. |
| **Apple App Store / Google Play** | the store is the seller | in-app purchase | yes | the store's | consumables | n/a | no | Needed for digital goods sold *inside* native apps (§7). |
| **RevenueCat** | aggregates both stores | — | — | — | — | — | — | An option for §7 instead of two store adapters. |

**Which first.** Stripe, because it is the commonest, covers use cases 1–6, and with Managed
Payments it is both a PSP and an MoR. **Paddle** second, as the established MoR and the adapter
that checks the abstraction is not just Stripe's shape (P5). Polar and Mollie after that.

## 3. Decisions

1. **One module with adapters, not a module per provider.** `@feldspar/payments` owns the
   tables, the actions, the webhook, the reconciliation service and the provider-neutral rules
   (prices, trials, roles, credits, guest orders). An **adapter** (`adapters/stripe.js`,
   `adapters/paddle.js`, …) is the one thing that differs per provider. It implements the
   interface in §4 and declares what it can do. What one provider does differently is in one
   file, not copied into six modules. Several adapters can be configured at once, for example
   Stripe on the web plus the app stores for native apps, and they write into the same tables.
   So a subscription bought on iOS gives the same role on the web.

2. **Feldspar holds the billing state the app needs; the provider is the payment rail.** Orders,
   subscriptions, grants and balances are rows in the app's database, kept up to date from the
   provider. This is what makes one module possible: an app reads `pay_subscriptions.status`,
   not a Stripe or Paddle object. Every row keeps the provider's id and its raw object
   (`provider_data`), so nothing the provider knows is lost.

3. **Full functionality goes through escape hatches, not a growing abstraction.** The
   abstraction covers the common path. Everything else uses these:
   - `pay_api(provider, method, path, body)`: a module function that makes an authenticated
     request to the provider's REST API, with the configured keys. It works for every provider
     and from any code body (`modfn("payments").pay_api(…)`).
   - `pay_provider_code`: an action whose configuration is a JavaScript body that runs **in the
     module's worker**, with the provider's SDK client in scope (`stripe`, `paddle`). This is
     what v1's "return the `stripe` object" was for. It cannot be a function returning the
     object, because a module function answers JSON across the worker seam.
   - Every webhook event is stored in `pay_events`. An insert trigger on that table reacts to any
     event type the module does not handle itself.

4. **Webhooks tell the module what changed; it then reads the current state from the provider.**
   Providers deliver events at least once and in no guaranteed order. So an adapter does not
   apply an event's payload. It takes the object reference from the event, **fetches the
   object's current state** from the provider and upserts it. Processing the same event twice,
   or two events out of order, then gives the right final state. Anything that adds value (a
   credit grant, a role grant) is keyed by an **idempotency key** derived from the provider
   object (`invoice:in_123`), with a unique constraint. A replayed event cannot add credit
   twice.

5. **Access is a role grant with an expiry, not a write to `users.role`** (core, P3). v1 set
   the user's role directly. If a cancellation webhook was missed, the user kept the role for
   good. If an admin changed the role, the module overwrote it. A user could not have a role
   from both a subscription and an admin's free upgrade. Here a grant is a row:
   `(user, role, source, source_ref, starts_at, expires_at)`. The **effective role is the most
   privileged of the base role and the grants in force**. A subscription's grant expires at the
   end of the paid period plus a grace period, and each renewal extends it. If every webhook is
   missed, access ends on time. Missing a webhook makes access lapse; it never extends it.

6. **Credits are a core ledger, not module rows** (core, P6). Agents are in core and must check
   the balance before a run and draw it down after each model call. That cannot go through the
   provider (no network call per agent step) or through a module call. Credits also exist
   without payments: an admin gives credits, a promotion, a sign-up bonus. So core has
   **buckets** (an amount with an optional expiry, drawn down soonest-expiring first) and an
   **entry log**. The module adds the policy: how much a plan includes, rollover and its cap,
   top-up packs. The provider's own meters are used only to *bill* overage, never to hold the
   balance.

7. **The server sets prices; the client sends only what is wanted.** A checkout request carries
   `{item, quantity}` lines. The checkout action reads each line's price and name from the app's
   table, as configured on the trigger. It never takes an amount from the client, except an
   explicitly configured "pay what you want" field with a minimum.

8. **Card data never reaches Feldspar.** Payment uses hosted checkout (a redirect), the
   provider's embedded elements, or the native payment sheet. That keeps the app in the
   lightest PCI scope (SAQ A). There is no card form of our own.

9. **Amounts are integers in the currency's minor unit, plus an ISO currency code.** The adapter
   converts for zero-decimal currencies (JPY) and three-decimal ones (KWD). Floating-point money
   is not allowed anywhere.

10. **There is a `fake` adapter.** It is a provider with a local checkout page ("Pay" / "Fail"
    buttons), which sends webhooks to the module's own endpoint. Tests use it, and so can an
    admin or a coding agent building the app's payment screens before anybody has a Stripe
    account. Each adapter is also tested against recorded provider responses. Live tests against
    test mode run only when keys are present, like the CmdStan tests.

## 4. The adapter interface

```js
// adapters/<name>.js
module.exports = {
  name: "stripe",
  capabilities: {            // what the trigger forms offer, and what actions refuse by name
    one_off: true, adhoc_prices: true, subscriptions: true, trial_with_card: true,
    portal: true, charge_saved_method: true, native_sheet: true, manual_capture: true,
    async_methods: true, marketplace: true, merchant_of_record: "optional", physical_goods: true,
  },
  settings: [/* FormFields: keys (secret), webhook secret, MoR flag, … */],
  hosts: ["api.stripe.com"],                // the module's net allow-list is the union of these

  client(account),                          // the SDK object or an HTTP wrapper: pay_provider_code, pay_api
  createCheckout(account, { lines, mode, customer, success_url, cancel_url, trial, metadata }),
  createPortal(account, { customer, return_url }),
  chargeSavedMethod(account, { customer, amount, currency, description, key }),   // off-session
  createNativeSheet(account, { customer, amount, currency, mode }),               // §7
  changePlan(account, subscription, plan), cancel(account, subscription, { at_period_end }),
  refund(account, payment, { amount, key }),

  verifyWebhook(account, { rawBody, headers }),   // → [{ id, type, ref }] or throws
  fetch(account, ref),                            // → a normalised object (§5) + provider_data
  list(account, kind, { since }),                 // for the reconciliation sweep
};
```

A normalised object is one of `customer`, `checkout`, `order`/`payment`, `subscription`,
`invoice`, `refund`, `dispute` or `payout`, with a fixed set of fields per kind. Where the
adapter cannot fill a field it leaves it null; it never makes a value up.

## 5. Tables (module-declared, all prefixed `pay_`)

| Table | Holds | Notable fields |
|---|---|---|
| `pay_accounts` | one configured provider each (§3.1) | `name`, `adapter`, `mode` (test/live), `secret_key` and `webhook_secret` (`Secret`), `publishable_key`, `settings`, `enabled`. Admin-only. |
| `pay_customers` | the provider's customer for a user (or a guest email) | `user_id → users` (nullable), `account`, `provider_ref`, `email`; unique `(account, provider_ref)` |
| `pay_plans` | what can be subscribed to | `name`, `label`, `role` (the role granted), `amount`, `currency`, `interval`, `trial_days`, `trial_needs_card`, `allowance_meter`, `allowance`, `rollover` (`none`/`capped`/`all`), `rollover_cap`, `provider_refs` (`{stripe: "price_…", paddle: "pri_…", apple: "com.app.pro"}`), `active` |
| `pay_packs` | credit top-ups | `meter`, `units`, `amount`, `currency`, `expires_after_days`, `provider_refs` |
| `pay_subscriptions` | one per provider subscription | `user_id`, `plan`, `account`, `provider_ref`, `status` (`trialing`/`active`/`past_due`/`paused`/`canceled`/`incomplete`), `current_period_end`, `cancel_at_period_end`, `trial_end`, `provider_data`. Readable by its owner. |
| `pay_orders` | a one-off payment: a basket, a top-up or an invoice | `kind`, `user_id` (nullable), `email`, `status` (`open`/`processing`/`paid`/`failed`/`canceled`/`refunded`/`partially_refunded`), `amount_total`, `amount_tax`, `currency`, `shipping`, `account`, `provider_ref`, `access_token` (for guests), `paid_at`, `metadata`. Readable by its owner. |
| `pay_order_lines` | the lines | `order`, `item_table`, `item_key`, `description`, `quantity`, `unit_amount`, `amount` |
| `pay_transactions` | money movements | `order`/`subscription`, `type` (`charge`/`refund`/`dispute`/`fee`/`payout`), `amount`, `status`, `provider_ref` |
| `pay_events` | every webhook delivery | `account`, `event_id` (unique with account), `type`, `payload`, `received_at`, `processed_at`, `error`, `attempts` |

Ownership follows the mail module's patterns (§15.1b): owners read their own rows, and only the
module writes. App-specific data is the app's own table with a key onto these, for example
`shipments.order → pay_orders`.

## 6. The flows

### 6.1 Checkout (use case 1, guest or signed in)

The basket lives **in the client** (React state and `localStorage`). Feldspar has no anonymous
session, and none is needed. An app that wants a basket that follows a signed-in user across
devices keeps an ordinary `basket_items` table with an ownership formula.

1. The app calls `api.runCheckout({ lines: [{ item: 12, quantity: 2 }], email? })`. This is an
   exposed trigger on `pay_checkout`, with `min_role` 100 when guests may buy, and its payload
   and answer are typed.
2. The action's **trigger configuration** says where the items are: the table, the price field
   or formula, the name, an optional stock field, the currency, whether to collect a shipping
   address, and the shipping options. Configuring this per trigger, not as a module-wide
   mapping, is the normal Feldspar shape: two shops in one instance are two triggers.
3. It prices the lines, writes a `pay_orders` row (`open`) and its lines, and asks the adapter
   for a checkout. The success and cancel URLs are on the **calling application's** host (core
   change C5), with the order's `access_token`. It answers `{ order, url }` for a redirect, or
   `{ order, client_secret }` for embedded elements.
4. The webhook (§6.5) marks the order `paid` (or `processing` for a bank method). Triggers on
   `pay_orders` do the app's work: decrement stock, send the confirmation, create the shipment.
5. A guest's order page calls `api.runOrderStatus({ token })`, which reads one order by its
   unguessable token. When the order's email signs up or signs in, it can be attached to the
   account (an option, off by default).

The tax mode is a checkout setting: none, the provider's tax calculation (Stripe Tax), or MoR.
MoR is refused when the configuration collects a shipping address, because MoRs do not sell
physical goods.

### 6.2 Subscriptions (use case 2)

Access policy is the module's configuration. It is a few independent switches, so every
combination in the request can be set:

- **Base role**: what a signed-up user with no subscription has, usually 80. This is the
  ordinary `users.role`. Paid plans grant more privileged roles (70, 60, …).
- **Is a subscription required?** If it is, the app shows a paywall to users at the base role.
  This needs no core support: the app checks `user.role`, and every table already does. If it
  is not, the base role *is* the free tier.
- **Free trial**: none, or N days, and **card up front or not**:
  - *No card*: on sign-up (a `login` or user-insert trigger), or when the user presses "Start
    trial", the module grants the plan's role directly with `expires_at = now + N days`. The
    provider is not involved, so this works with every provider, MoRs and app stores included.
    `pay_customers.trial_used_at` allows one trial per user.
  - *Card up front*: the trial is the provider's (`trial_period_days` on the checkout). The
    subscription is `trialing`, the role grant lasts until `trial_end`, and the provider charges
    and converts when the trial ends.
- **Grace for failed renewals**: N days. A `past_due` subscription keeps its grant until
  `current_period_end + grace`, while the provider retries the card.

What each status means for the grant:

| status | grant |
|---|---|
| `trialing`, `active` | role until `current_period_end` (or `trial_end`) + grace; each renewal extends it |
| `past_due` | unchanged: it runs out on its own unless the provider recovers the payment |
| `canceled` with `cancel_at_period_end` | kept to the period's end |
| `canceled` now, `paused`, `incomplete` | revoked |

Upgrading, downgrading, proration, changing the card and cancelling all go through the
provider's **customer portal** (`api.runBillingPortal()` → URL). `pay_change_plan` and
`pay_cancel` exist for apps that want their own buttons. Plan changes upsert the grant (the
`source_ref` is the subscription), so a tier change is one row that changes role.

Daily trigger: emails the user before a card-less trial ends, and before an annual renewal. It
uses `send_email` and the app's own templates.

### 6.3 Credits (use case 3)

**Core (P6).** Meters are named (`ai_units`). A user's balance on a meter is the sum of their
unexpired buckets. Operations:

- `grant(user, meter, amount, { expires_at, kind, key })`: a new bucket. The `key` is unique,
  so replaying it does nothing. A grant first repays any debt.
- `debit(user, meter, amount, { key, allow_debt })`: takes from the soonest-expiring bucket
  first, in one transaction with the buckets locked. Without `allow_debt`, a debit larger than
  the balance is refused and nothing is taken.
- `balance(user, meter)`.

Surfaces: code bodies (`credits.debit("ai_units", 5, { key })`), a `credit_debit` workflow
action, and module contexts (C4). Buckets and entries are ordinary tables that owners can read,
so a balance widget is a REST read and updates live. **Agents** get a `meter` setting with a
rate (units per dollar of model cost from `Usage::cost`, or per 1,000 tokens when prices are
unknown) and a minimum balance to start. A run checks the balance first. It debits after each
model call with `allow_debt`, because the call has already happened, and it stops when the
balance reaches zero. An anonymous caller cannot run a metered agent.

**Module (P7).** It is all policy:

- **Allowance**: on each paid renewal (`invoice.paid`, keyed by the invoice) the plan's
  allowance is granted as a bucket that expires at the period's end. **Rollover**: `none`, `all`,
  or `capped`, which carries `min(remaining, cap)` into the new bucket. The old bucket is
  expired by the same key.
- **Top-up packs**: `api.runBuyCredits({ pack })`. If the adapter can charge a saved card
  (`charge_saved_method`, for example a Stripe off-session PaymentIntent or a Paddle one-time
  charge on the subscription), it charges **the same card** with no checkout page and answers
  `{ status: "paid" }`. If the bank asks for authentication (SCA), or there is no saved card, it
  falls back to a checkout URL. The credit is granted from the webhook, never from the request.
- **Auto top-up** (optional): when a debit takes the balance below a threshold, buy a pack
  (rate-limited, at most once per hour).
- **Postpaid overage** (later): instead of blocking at zero, report usage to the provider's
  meter and bill it on the next invoice.

### 6.4 Invoices and payment requests (use case 6)

`pay_request({ amount or lines, email, due, methods, capture: "now" | "later" })` creates a
`pay_orders` row of `kind: "invoice"` and answers a pay link. The app emails it with its own
template. Bank methods leave the order `processing` until they settle or fail, and triggers on
the status change do the rest. `capture: "later"` authorises a deposit, and
`pay_capture(order, amount?)` or `pay_cancel(order)` settles it. Stripe and Mollie only.

### 6.5 Webhooks and reconciliation

`POST /_feldspar/hooks/payments/<account name>` (core change C1) is the URL the admin registers
with the provider. The account's settings page shows it with a copy button. The handler:

1. verifies the signature over the **raw body**. If it fails, it answers 400 and stores nothing;
2. inserts the events into `pay_events`. If the event id is already there, it answers 200 and
   stops;
3. processes them: `fetch` the current state of each object it refers to, upsert, and apply
   grants and credits by key;
4. answers 200. If processing fails, it stores the error and answers 500, so the provider
   retries.

The **`reconcile` service** re-processes events with an error (with backoff), and every few
hours lists the subscriptions changed since the last sweep and syncs them, which catches a
webhook that never arrived. Admin action `pay_sync` does the same on demand.

## 7. Mobile (use case 4)

Two different things are both called "mobile payments", and the store rules separate them:

**(a) Apple Pay and Google Pay as a way to pay**, for physical goods and services used outside
the app: the shop (1), invoices (6) and the marketplace (5). This is the provider's native
SDK. For Stripe that is `@stripe/stripe-react-native`'s PaymentSheet, or `PlatformPayButton`
for a wallet-only button. It needs a server action, `pay_native_sheet`, that answers
`{ paymentIntent, ephemeralKey, customer, publishableKey }`, not a checkout URL.
`plugins/react-native` adds the SDK and its Expo config plugin (the Apple merchant id and
Google Pay enabled) when the payments module has a Stripe account, generates
`src/feldspar/payments.ts` (a `usePayment()` hook around the sheet), and sets the app's URL
scheme as the 3-D Secure return URL. On the **web** target (react-native-web) the same hook
redirects to hosted checkout, which itself offers Apple Pay and Google Pay. The admin still has
to create an Apple merchant id and payment-processing certificate and upload it to Stripe; the
tutorial covers this.

**(b) Digital goods and subscriptions sold inside a native app** (use cases 2 and 3). By default
the **stores require their own in-app purchase**, and Apple Pay does not count. The exceptions
change often and differ by storefront. As of October 2026:

- In the **US**, iOS apps may link to web checkout with no Apple fee for now. Apple is appealing
  and has asked for a 15% fee.
- In the **EU**, there is an entitlement and a 15–20% fee.
- **Google Play** allows alternative billing and external links in the US, with fees of 10–25%.

So the module supports both:

- **Link-out** (P8, cheap): the plan button opens web checkout in the system browser
  (`Linking.openURL`). It is a per-storefront switch, because whether it is allowed depends on
  the storefront, not on our code. The subscription is the web provider's and the role follows
  the user to the app.
- **In-app purchase** (P9): `apple` and `google_play` adapters (or `revenuecat`, which covers
  both). The app buys with `expo-iap`, passing the user's id as `appAccountToken` /
  `obfuscatedAccountId`. It sends the transaction to `pay_verify_store_purchase`, which checks
  it against the App Store Server API or the Play Developer API. Store server notifications (App
  Store Server Notifications v2, Play RTDN) arrive at the same webhook route. Plans carry store
  product ids in `provider_refs`, and grants work exactly as in §6.2. A user who subscribed on
  iOS is subscribed on the web.

## 8. Marketplace (use case 5, later)

Stripe Connect (Express accounts) first. Sellers get a `pay_sellers` row (`user_id`, connected
account id, onboarding status). `pay_seller_onboarding` answers Stripe's hosted KYC link, so the
seller's identity checks and payouts are Stripe's job, not the app's. A purchase is a
**destination charge** with `application_fee_amount` = the commission. Baskets with lines from
several sellers use **separate charges and transfers**. Refunds and disputes reverse the
transfer and the fee by a policy setting. MoRs cannot do this, and Stripe Managed Payments
excludes Connect, so a marketplace is always PSP mode. Mollie Connect and Adyen for Platforms are
the alternatives. This is the most legally involved use case (who is the seller of record, and
1099/DAC7 reporting), which is another reason to leave it until last.

## 9. Core changes

| | Change | Needed by |
|---|---|---|
| **C1** | **Module webhooks.** A module declares `webhooks: { name: async (req, ctx) => ({ status, body }) }`, served at `POST /_feldspar/hooks/<module>/<name>/<rest>` on the admin host, as the OAuth callback is (one stable URL to register). `req` has `rawBody` (bytes), `headers` and `rest`. There is no session and no CSRF check. There is a body cap (1 MiB) and a per-IP rate limit. `ctx` is a service's (`db` over the module's own tables, `files`, `log`) plus C4. The bundled card says "Receives webhooks at …". | all |
| **C2** | **Role grants.** `_fd_role_grants` and one `effective_role(user)` function, used wherever a user is loaded: sessions, API tokens, MCP, the `login` event. `user.role` in formulas, the RLS GUC and `Event.role` are the effective role. `users.role` stays the base role an admin sets. The admin's user page lists grants and can add a time-limited one by hand. | 2, 3 |
| **C3** | **Credits ledger.** Meters (`_fd_credit_meters`), plus the ordinary tables `credit_buckets` and `credit_entries`, owner-readable and written only by core. `grant`/`debit`/`balance` as in §6.3, a code-body `credits` surface, a `credit_debit` action, and agent metering. | 3 |
| **C4** | **Module access to C2/C3.** `ctx.roles.grant/revoke` and `ctx.credits.grant/debit` for module actions, services and webhooks. Granting roles is a **permission the admin gives at install**, with a ceiling: "may grant roles 70 to 100". Admin (1) can never be granted by a module. | 2, 3 |
| **C5** | **The calling application in `Event`**: its id, public base URL and (for native apps) URL scheme. Checkout success, cancel and portal return URLs need it. | 1, 2, 4 |
| **C6** | **CSP additions per application.** Embedded elements need `js.stripe.com` in `script-src` and `frame-src`, and `api.stripe.com` in `connect-src`; Paddle.js needs its CDN. A module manifest may *suggest* sources per adapter. The admin turns them on per application (shown on the application's page); they are not added on install. Hosted-checkout redirects need nothing. | 1, 2 (embedded) |
| **C7** | **React Native native payments** in `plugins/react-native` (§7a): the dependency, config plugin, generated hook and return URL. Later, `expo-iap` (§7b). | 4 |

The module's `net` permission is **an exact allow-list**: the union of the configured adapters'
`hosts` (`api.stripe.com`, `api.paddle.com`, `sandbox-api.paddle.com`, …), not `*`.

## 10. Open questions

- **Credits in core** (§3.6, C3): agreed? The alternative, a module-owned ledger, means agents
  call a module action on every model step.
- **Second provider**: Paddle (established, conservative) or Polar (open source, credits and
  meters built in)?
- **In-app purchase**: our own `apple`/`google_play` adapters (using Apple's official server
  library), or RevenueCat first (one integration, but a paid third party above $2.5k a month)?
- **Guest orders**: attach them to an account by email on sign-up, or keep them guest-only?
- **Delivering digital goods** (licence keys, file downloads after purchase): built in to the
  module, or left to triggers on `pay_orders`?

---

# Part II — Milestones

## P1 — Core seams: webhooks, the calling application, CSP (core)

- [ ] **P1.1** C1 module webhooks: manifest key, route, raw body and headers, cap, rate limit,
  `ctx` with the module's own tables. Test: a fixture module's webhook receives a raw body and
  headers byte-for-byte, writes a row to its table, and its status code reaches the caller.
  It cannot write a table it did not declare.
- [ ] **P1.2** Bundled card and installed card list the webhook URLs.
- [ ] **P1.3** C5: `Event.app` (id, base URL, scheme) on events raised through an app's API.
  Test: an exposed trigger sees its app's URL on two different hosts.
- [ ] **P1.4** C6: per-application CSP additions, with module-suggested sources shown and
  switched on by the admin. Test: the app's CSP header carries them only once enabled.

## P2 — The module and use case 1 (shop), on Stripe and `fake`

- [ ] **P2.1** `plugins/payments` skeleton: manifest, tables (§5), `pay_accounts` with `Secret`
  keys, adapter loading, capability checks with named refusals, net allow-list from hosts.
- [ ] **P2.2** `fake` adapter: a local checkout page and self-delivered signed webhooks.
- [ ] **P2.3** Webhook handler and event store (§6.5): verify, dedupe, fetch-and-upsert, 500 on
  failure. Tests: a duplicate event is processed once, out-of-order events give the right final
  state, and a bad signature stores nothing.
- [ ] **P2.4** `pay_checkout` (§6.1) with its trigger configuration (item table, price, name,
  stock, currency, shipping), server-side pricing, guest orders and `access_token`; typed payload
  and answer. `pay_order_status`. Tests against `fake`: a client-sent price is ignored, a guest
  order reaches `paid`, and an order triggers a stock decrement.
- [ ] **P2.5** Stripe adapter, one-off: Checkout Session with ad-hoc prices, Stripe Tax option,
  shipping, the `managed_payments` flag (refused with shipping), refunds, webhook verification.
  Recorded-response tests; live test-mode test when keys are present.
- [ ] **P2.6** Escape hatches: `pay_api` function and the `pay_provider_code` action (`stripe`
  in scope).
- [ ] **P2.7** `reconcile` service: retry errored events, periodic sync.
- [ ] **P2.8** Tutorial `docs/tutorial-payments.md` part 1: a React shop with a guest checkout.

## P3 — Role grants (core)

- [ ] **P3.1** `_fd_role_grants` and `effective_role`, used in sessions, tokens, MCP, the
  `login` event, `user.role` in formulas and the RLS GUC. Tests: a grant changes what a user
  can read within the session freshness TTL, and an expired grant ends access with no write.
- [ ] **P3.2** Admin user page: base role, grants in force, add a time-limited grant by hand.
- [ ] **P3.3** C4 for roles: `ctx.roles.grant/revoke` (upsert by source and ref), with the
  install-time ceiling. Test: a module granting a role above its ceiling, or role 1, is
  refused.

## P4 — Use case 2: subscriptions on Stripe and `fake`

- [ ] **P4.1** `pay_plans`; `pay_subscribe(plan)` → checkout URL; subscription sync; grants per
  §6.2's table, with grace.
- [ ] **P4.2** Trials: card-less (granted by the module, one per user) and card-up-front (the
  provider's). Tests with `fake` and Stripe test clocks: a trial ends with and without
  conversion, and a renewal fails then recovers within grace.
- [ ] **P4.3** `pay_billing_portal`, `pay_change_plan`, `pay_cancel`; a tier change moves the
  grant's role.
- [ ] **P4.4** Access-policy settings (base role, required or free tier, trial mode, grace) and
  the daily reminder trigger.
- [ ] **P4.5** Tutorial part 2: a SaaS app with a free tier, two paid tiers and a card-less trial.

## P5 — Second adapter: a merchant of record (Paddle, or Polar per §10)

- [ ] **P5.1** Adapter for one-off and subscriptions, with webhook verification and portal.
  Anything in the adapter interface that turns out to be Stripe-shaped is fixed **here**,
  before credits and mobile build on it.
- [ ] **P5.2** The P2 and P4 test suites run against both adapters (one parametrised suite,
  not a copy).

## P6 — Credits ledger (core)

- [ ] **P6.1** Meters, buckets, entries; `grant`/`debit`/`balance` with bucket locking,
  soonest-expiring first, debt and repayment, idempotency keys. Tests: concurrent debits
  never overdraw, and a replayed grant does nothing.
- [ ] **P6.2** Code-body `credits` surface and the `credit_debit` action.
- [ ] **P6.3** Agent metering: `meter`, rate, minimum balance; a debit per model call; the run
  stops at zero; anonymous callers are refused.
- [ ] **P6.4** C4 for credits (`ctx.credits`).

## P7 — Use case 3: allowances, rollover, top-ups

- [ ] **P7.1** Plan allowances granted on paid renewals; rollover `none`/`all`/`capped`. Tests
  over three periods for each policy.
- [ ] **P7.2** `pay_packs` and `pay_buy_credits`: charge the saved card where the adapter can,
  falling back to checkout when the bank asks for authentication or there is no saved card.
  The credit is granted from the webhook.
- [ ] **P7.3** Auto top-up below a threshold, rate-limited.
- [ ] **P7.4** Tutorial part 3: an agent app with a $5 plan including 100 units and $2 top-ups.

## P8 — Use case 4a: Apple Pay and Google Pay in React Native

- [ ] **P8.1** `pay_native_sheet` (Stripe): customer, ephemeral key, PaymentIntent; the order
  is created as in P2.4.
- [ ] **P8.2** C7 in `plugins/react-native`: the dependency and Expo config plugin when a Stripe
  account exists, generated `usePayment()`, URL-scheme return URL, hosted-checkout fallback on
  web.
- [ ] **P8.3** Link-out subscribing from native apps, as a per-storefront switch (§7b).
- [ ] **P8.4** Tutorial: the merchant id and certificate steps; an Android emulator test with
  Google Pay's test environment.

## P9 — Use case 4b: in-app purchase

- [ ] **P9.1** `apple` adapter: App Store Server API verification, Server Notifications v2,
  `appAccountToken`.
- [ ] **P9.2** `google_play` adapter: Play Developer API (`subscriptionsv2`), RTDN through
  Pub/Sub push to the webhook route.
- [ ] **P9.3** `expo-iap` in the React Native scaffold, and `pay_verify_store_purchase`.
  Test: a store subscription gives the same role on the web.

## P10 — Use case 6: invoices and payment requests

- [ ] **P10.1** `pay_request`, pay links, the `processing` status, bank methods (Stripe; Mollie
  adapter).
- [ ] **P10.2** Manual capture: `pay_capture` and cancelling an authorisation.

## P11 — Use case 5: marketplace (Stripe Connect)

- [ ] **P11.1** `pay_sellers`, hosted onboarding, account status sync.
- [ ] **P11.2** Destination charges with a commission; multi-seller baskets with separate
  charges and transfers; refund and dispute policy.

## P12 — More adapters, on demand

- [ ] **P12.1** Polar or Paddle (whichever P5 did not do), Mollie (if P10 did not), Lemon
  Squeezy, Creem, Dodo, PayPal: each one adapter file and a run of the shared suite.
