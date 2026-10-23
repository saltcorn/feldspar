# Saltcorn v2 — Payments: the bundled payments module (milestones P1–P12)

Ordered, checkable task list for **taking payments in applications**: one bundled module,
`@feldspar/payments`, with a provider adapter per payment service, plus the core changes it
needs. It replaces the Saltcorn v1 Stripe module, which put a user on a higher role while their
subscription was active and had a function returning the `stripe` object so that custom code
could use the whole Stripe API.

This builds on modules (`plugins/README.md`, [docs/tutorial-modules.md](./docs/tutorial-modules.md),
TECHNICAL_DESIGN §15.1–15.1d: declared tables, services, `Secret` fields, typed action payloads),
roles (§7.4), triggers and the row actions `insert_row` and `update_rows` (§10.2), workflows
(§10.3), agents and their cost accounting (§11.1, `Usage::cost`), and the React Native framework
(`plugins/react-native`).

The design comes first, then the milestones. Each milestone is prefixed **P** and its tasks are
numbered P1.1, P1.2, …. The order follows the use cases in priority order (§1). P1, P3 and P6
are core and do not depend on each other.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Part I — Design

## 1. What has to work

| | Use case | Who pays | Logged in? | What a payment produces |
|---|---|---|---|---|
| **1** | **Shop.** The application sells goods. A basket is paid for at checkout. | a buyer | maybe not | a payment event; the app's own order is marked paid by the app's trigger |
| **2** | **Subscription SaaS.** Paying gives access, as a role. Tiers give different roles. Free tiers and free trials are optional. | the user | yes | a role grant that lasts while the subscription does |
| **3** | **LLM credits.** A payment buys units that LLM calls made by agent runs use up. A subscription can include an allowance, which may roll over up to a cap. Top-ups are charged to the same card. | the user | yes | credit added to a balance; agent runs' LLM calls draw it down |
| **4** | **Mobile wallets.** Apple Pay and Google Pay in React Native apps. | as 1–3 | as 1–3 | as 1–3 |
| **5** | **Marketplace.** One user sells, another buys, and the app keeps X% and pays the seller the rest. | a buyer | yes | a payment, a fee for the app, a transfer to the seller |
| **6** | **Invoices and payment requests** (added use case). A services business (a consultancy, a booking or rental app) bills a customer for an amount the app works out (a timesheet, a booking deposit). It sends a pay link by email, and the customer may pay by card or by bank transfer or direct debit (SEPA, ACH, iDEAL), which settles days later. A deposit can be authorised now and captured later. | a customer, often with no account | no | payment events that the app's invoice row follows, including "processing" for days |

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
| **RevenueCat** | aggregates both stores | — | — | — | — | — | — | **Not used** (§3.12): we write our own `apple` and `google_play` adapters. |

**Which first.** Stripe, because it is the commonest, covers use cases 1–6, and with Managed
Payments it is both a PSP and an MoR. **Polar** second (decided), as the open-source MoR aimed at
developers, with meters and credits of its own, and the adapter that checks the abstraction is
not just Stripe's shape (P5). Paddle and Mollie after that.

## 3. Decisions

1. **One module with adapters, not a module per provider.** `@feldspar/payments` owns the
   actions, the webhook, the reconciliation service and three infrastructure tables (§5). An
   **adapter** (`adapters/stripe.js`, `adapters/polar.js`, …) is the one thing that differs per
   provider. It implements the interface in §4 and declares what it can do. Several adapters can
   be configured at once, for example Stripe on the web plus the app stores for native apps. A
   subscription bought on iOS then gives the same role on the web.

2. **The app owns its business data; the module owns only the connection to the provider.**
   The module has **no orders, order lines, customers, plans or products tables**. Those are the
   app's own tables, in whatever shape the business needs: an order with a delivery slot, a plan
   with a feature list, a booking with a deposit. Building those tables into the module would
   force one shape on every app. The module keeps only what it cannot do without: the provider
   accounts, a mirror of the provider's objects, and a log of payment events (§5).

3. **Bindings: the app tells the module which of its rows and fields mean what.** A payment is
   always *for* a row of the app's: an order, an invoice, a credit pack, a booking. The checkout
   action's **trigger configuration** names that row's table and gives **formulas** over it for
   the amounts, descriptions and quantities (§6.1). Plans live in a table the app's admin
   creates (decided), and are bound the same way, once, in the module's configuration: "plans
   are the rows of `plans`; `stripe_price` holds the Stripe price; `role` is the role it gives"
   (§6.2). Configuring a binding per trigger, not as a module-wide
   field mapping, is the normal Feldspar shape: two shops in one instance are two triggers.

4. **Payment events flow back as rows; the app reacts with its own triggers.** When a payment
   succeeds, fails, is refunded or a subscription changes, the module inserts a normalised row
   into `pay_events`. The row names the app row the payment was for (`ref_table`, `ref_key`).
   The app reacts with ordinary triggers on that table, usually with no code. For example, an
   `update_rows` action sets `orders.status = "paid"` where `id === row.ref_key`. **The module
   never writes the app's tables.** This keeps the module's authority to what it declared
   (§15.1c's `ScopedTables`, unchanged). An admin can also see everything that happens to the
   app's data in its own trigger list.

5. **Full functionality goes through escape hatches, not a growing abstraction.** The
   abstraction covers the common path. Everything else uses these:
   - `pay_api(account, method, path, body)`: a module function that makes an authenticated
     request to the provider's REST API, with the configured keys. It works for every provider
     and from any code body (`modfn("payments").pay_api(…)`).
   - `pay_provider_code`: an action whose configuration is a JavaScript body that runs **in the
     module's worker**, with the provider's SDK client in scope (`stripe`, `polar`). This is
     what v1's "return the `stripe` object" was for. It cannot be a function returning the
     object, because a module function answers JSON across the worker seam.
   - Provider events the module does not understand are still inserted into `pay_events`, as
     `kind: "other"` with the raw event, so an app trigger can react to any of them.

6. **Webhooks tell the module what changed; it then reads the current state from the provider.**
   Providers deliver events at least once and in no guaranteed order. So an adapter does not
   apply an event's payload. It takes the object reference from the event, **fetches the
   object's current state** from the provider and upserts the mirror. Processing the same event
   twice, or two events out of order, then gives the right final state. Anything that adds value
   (a credit grant, a role grant) is keyed by an **idempotency key** derived from the provider
   object (`invoice:in_123`), with a unique constraint. A replayed event cannot add credit
   twice.

7. **Access is a role grant with an expiry, not a write to `users.role`** (core, P3). v1 set
   the user's role directly. If a cancellation webhook was missed, the user kept the role for
   good. If an admin changed the role, the module overwrote it. A user could not have a role
   from both a subscription and an admin's free upgrade. Here a grant is a row:
   `(user, role, source, source_ref, starts_at, expires_at)`. The **effective role is the most
   privileged of the base role and the grants in force**. A subscription's grant expires at the
   end of the paid period plus a grace period, and each renewal extends it. If every webhook is
   missed, access ends on time. Missing a webhook makes access lapse; it never extends it. Role
   grants and credit grants are the **only** effects the module applies itself, because they
   must be idempotent and must follow the subscription's lifecycle. Leaving them to every app's
   triggers would mean every app re-implementing grace periods and renewals, and getting it
   wrong.

8. **LLM usage credits are a core ledger, and only for LLM usage** (core, P6). Agent runs are
   the only place LLM calls are made, and agents are in core. A run must check the balance
   before it starts and draw it down after each model call. That cannot go through the provider
   (a network call per agent step) or through a module call. So core has **buckets** (an amount
   with an optional expiry, drawn down soonest-expiring first) and an **entry log**, and **the
   only thing that spends credits is an agent run's model calls**. The ledger is deliberately
   not a general metering facility. One would need more flexibility (what is charged, when, and
   whether to refuse beforehand or charge afterwards), and an app that wants to charge for other
   activities can build that with its own tables, ownership formulas and actions. Credits can
   still be **granted** from outside: the payments module (allowances, top-ups) and an admin by
   hand. The provider's own meters are used only to *bill* overage, never to hold the balance.

9. **Amounts come from formulas evaluated on the server, over the app's rows.** The client
   sends only *which* row to pay for (an order id, a pack id) or `{item, quantity}` lines. It
   never sends an amount, except through an explicitly configured "pay what you want" field with
   a minimum. Because the formulas read the app's tables, **the app must keep the fields they
   read out of the buyer's reach**. `productⱵprice * quantity` on an order line is safe, because
   the buyer cannot write `products`. A `price` column the buyer may write on their own order line
   is not. The tutorial makes this its first lesson. The amount actually charged is recorded on
   the mirror and on every event, so a trigger can compare it with the order's total.

10. **Card data never reaches Feldspar.** Payment uses hosted checkout (a redirect), the
    provider's embedded elements, or the native payment sheet. That keeps the app in the
    lightest PCI scope (SAQ A). There is no card form of our own.

11. **Amounts are integers in the currency's minor unit, plus an ISO currency code.** The adapter
    converts for zero-decimal currencies (JPY) and three-decimal ones (KWD). Floating-point money
    is not allowed anywhere.

12. **Our own app-store adapters.** In-app purchases are verified by `apple` and `google_play`
    adapters written here, using Apple's official App Store Server Library and Google's Play
    Developer API, not through RevenueCat. This avoids a paid third party between an app and
    its stores, and the store subscriptions land in the same mirror as every other provider's.

13. **Building blocks, not full solutions.** The module takes payments and reports what
    happened. What a payment *means* for the business is the app's, built with triggers and
    workflows. Examples: delivering digital goods, decrementing stock, sending a confirmation,
    creating a shipment, letting a guest see their order, reminding a user that their trial
    ends. A feature belongs in the module only if an app could not reasonably build it from
    tables, triggers and actions (grants, §3.7, are the example of one that could not). The
    tutorials show how to build the common ones.

14. **There is a `fake` adapter.** It is a provider with a local checkout page ("Pay" / "Fail"
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
  changePlan(account, subscription, price_ref), cancel(account, subscription, { at_period_end }),
  refund(account, payment, { amount, key }), capture(account, payment, { amount }),

  verifyWebhook(account, { rawBody, headers }),   // → [{ id, type, ref }] or throws
  fetch(account, ref),                            // → a normalised object (§5) + provider_data
  list(account, kind, { since }),                 // for the reconciliation sweep
};
```

A normalised object is one of `customer`, `checkout`, `payment`, `subscription`, `refund`,
`dispute` or `connected_account`, with a fixed set of fields per kind. Where the adapter cannot
fill a field it leaves it null; it never makes a value up.

## 5. The module's tables, and what the app brings

The module declares **three tables**, all infrastructure:

| Table | Holds | Notable fields |
|---|---|---|
| `pay_accounts` | one configured provider each | `name`, `adapter`, `mode` (test/live), `secret_key` and `webhook_secret` (`Secret`), `publishable_key`, `settings`, `enabled`. Admin-only. |
| `pay_objects` | **a mirror** of the provider objects this instance created or was told about: customers, checkouts, payments, subscriptions, refunds, disputes | `account`, `kind`, `provider_ref` (unique with account), `user_id → users` (nullable), `email`, `ref_table` and `ref_key` (the app row it was for; the key is stored so that `id === row.ref_key` holds for an integer, text or UUID key, which P2.4 tests), `status`, `amount`, `currency`, `price_ref` and `current_period_end` (subscriptions), `lines` (the lines as charged, JSON), `effects` (what to grant when it is paid, §6.1), `provider_data`. Readable by its owner (`user_id === user.id`). |
| `pay_events` | **the log the app reacts to**: one row per thing that happened, normalised | `account`, `event_id` (unique with account and kind), `kind` (below), `object → pay_objects`, `user_id`, `email`, `ref_table`, `ref_key`, `amount`, `currency`, `status`, `provider_type`, `raw`, `created_at`. Admin-only by default; the app's triggers fire on its inserts. |

Event kinds: `checkout.completed`, `payment.processing`, `payment.succeeded`, `payment.failed`,
`payment.refunded`, `dispute.opened`, `dispute.closed`, `subscription.started`,
`subscription.renewed`, `subscription.changed`, `subscription.past_due`,
`subscription.canceled`, `subscription.ended`, `connected_account.updated`, and `other`.

**Why a mirror, when orders and customers are the app's.** `pay_objects` is not a business
table: its shape is the provider's, not the business's, and the app never writes it. The module
needs it for four things the app's tables cannot hold for it:
- which provider customer a user is, so that a top-up can charge the same card and the portal
  opens on the right account;
- which app row a checkout was for. Provider metadata cannot hold this for every provider: an
  App Store purchase has only `appAccountToken`;
- a subscription's state, for grants, for renewals and for the reconciliation sweep;
- the amount and lines as charged.

An app can show "your plan renews on 3 November" by reading its own user's subscription row
from it, with no table of its own.

**What the app brings**, in whatever shape it likes:
- what it sells: products, plans, credit packs, invoices, bookings;
- what it records: orders and lines, shipments, licence keys, sellers;
- the triggers that connect `pay_events` to those tables.

The tutorials (P2.8, P4.5, P7.4) show one shape for each use case, not the only one.

## 6. The flows

### 6.1 Checkout (use case 1, guest or signed in)

**`pay_checkout`** is one action whose trigger configuration has these parts:

- **Account and mode**: which `pay_accounts` row; `payment` or `subscription`.
- **Source**: always **a row of the app's**: a table, and where its key comes from: a payload
  field (`{ order: 12 }`), or the trigger's own row when the trigger fires on an insert. "The
  caller must be able to read the row" is on by default. It is turned off only for a checkout
  that runs as a later step of a workflow that just created the row. A guest passes this check
  through their guest session (C9). There is no "basket" source with no row behind it (decided):
  one way to do it is enough, and guest sessions make it unnecessary.
- **Lines**, as formulas evaluated on the server:
  - *one line* from the source row: `amount` (`total`), `description` (`"Invoice " + number`);
  - *child rows* of the source row: a child table and its key field, then `quantity`,
    `unit_amount` (`productⱵprice`), `description` (`productⱵname`) and `item` (`product`), in
    the child row's scope;
  - for a subscription, `price_ref` (`planⱵstripe_price`, or a literal), `quantity` and
    `trial_days`.
- **Currency** (a literal or a formula), **customer** (the caller, or an `email` formula for
  guests), **tax** (none, the provider's calculation, or MoR, which is refused when a shipping
  address is collected, because MoRs do not sell physical goods), **shipping**, **payment
  methods**, **capture** (now, or later for deposits), **success and cancel paths** on the
  calling application's host (core change C5).
- **Effects** (optional), applied by the module once the payment succeeds (§3.7): *grant
  credits* (an amount formula, say `units` on a credit pack row, and an optional expiry), and
  *grant a role* (a role and a duration, for a one-off "pass").

It answers `{ url }` for a redirect, or `{ client_secret }` for embedded elements, plus the
mirror row's id. The same configuration shape is shared by `pay_charge_saved` (§6.3),
`pay_native_sheet` (§7) and `pay_request` (§6.4).

**A shop, end to end** (the tutorial's version; any other shape works the same way):

1. The app creates **`orders`** and **`order_lines`** tables of its own. `orders` has a
   `user_id` (default `user && user.id`) and a **`session_id`** (default `session.id`, C9), a
   `status` (`open`, then `paid`) and an `email`. `order_lines` has `order`, `product` and
   `quantity`, and **no price column**.
2. Ownership lets the buyer, signed in or not, keep their own basket as an `open` order through
   the ordinary REST API, live updates included:
   - `orders`: `user ? user_id === user.id : session_id === session.id`, with writes only while
     `status === "open"`;
   - `order_lines`: the same rule through `orderⱵ…`.

   The basket survives a reload and needs no client-side storage.
3. Checkout is `api.runCheckout({ order })`, an exposed trigger with `min_role` 100 on
   `pay_checkout`. The order is the source, and the lines are its `order_lines` priced as
   `productⱵprice * quantity`, which the buyer cannot change.
4. On `payment.succeeded` with `ref_table === "orders"`, the app's triggers mark the order paid
   (`update_rows`), decrement stock, send the confirmation, and deliver a digital good.
5. A guest sees their paid order through the same ownership rule, for as long as the guest
   session lasts. A guest on another device uses whatever the app built for that, for example a
   token in the confirmation email and a code query that reads one order by token.
6. When the guest **signs up or signs in in the same browser**, the `login` event carries the
   guest session's id (C9). An app trigger moves the guest's orders to the account:
   `update_rows` on `orders` where `session_id !== null && session_id === payload.guest_session`,
   set `user_id = user.id`. (The first half matters: a sign-in with no guest session has no
   `guest_session`, which reads as null and would otherwise match every signed-in order.) This is safe where matching by email is not: holding the guest session's
   cookie proves it is the same buyer, while password sign-up does not verify email
   addresses.

### 6.2 Subscriptions (use case 2)

**The plan binding**, in the module's configuration, is a list, usually of one entry:

- the app's plans table;
- for each account, the field holding that provider's price or product id (`stripe_price`,
  `polar_product`, `apple_product`, `google_product`);
- the field holding the role it grants, and optionally the fields for the LLM-credit allowance,
  the rollover policy and the rollover cap;
- the grace period for failed renewals.

The plan is looked up from the subscription's **current** price on every change, so a tier
change made in the provider's portal moves the role with no app involvement. A subscription
whose price matches no plan row is mirrored and logged, but grants nothing, and the module
reports it as an issue.

What each status means for the grant (`source_ref` is the subscription, so there is one grant
per subscription, upserted):

| status | grant |
|---|---|
| `trialing`, `active` | the plan's role until `current_period_end` (or `trial_end`) + grace; each renewal extends it |
| `past_due` | unchanged: it runs out on its own unless the provider recovers the payment |
| `canceled` with `cancel_at_period_end` | kept to the period's end |
| `canceled` now, `paused`, `incomplete` | revoked |

Everything else is the app's, built from smaller pieces:

- **Base role and the free tier.** A signed-up user's ordinary `users.role` (say 80) is the free
  tier, or the "must subscribe" state. The app shows a paywall at that role; tables already
  check `user.role`.
- **Card-less trials** need no payments at all. A trigger on the `login` event (sign-up signs
  the new account in; a `users` insert trigger never sees a sign-up, P4 notes) runs core's
  **`grant_role`** action (C8): role 70 for 14 days, key `trial`, `once`, so a second trial is
  never granted. It works whichever provider the user later pays through, app stores included.
- **Trials with the card up front** are the provider's: `trial_days` on the subscription
  checkout. The subscription is `trialing`, and the grant lasts until `trial_end` + grace.
- **Reminders** ("your trial ends tomorrow") are a daily trigger over core's `role_grants` table
  with `send_email`.
- **Changing plan, changing the card, cancelling**: `pay_portal` answers the provider's customer
  portal URL. `pay_change_subscription` (a `price_ref` formula) and `pay_cancel_subscription` are
  for apps that want their own buttons.

### 6.3 LLM credits (use case 3)

**Core (P6).** The ledger exists for one purpose: paying for the LLM calls agent runs make. It
is not a general metering facility (§3.8). A user's balance is the sum of their unexpired
buckets. Operations:

- `grant(user, amount, { expires_at, kind, key })`: a new bucket. The `key` is unique, so
  replaying it does nothing. A grant first repays any debt. Callers: the payments module
  (C4) and an admin by hand on the user page.
- `balance(user)`.
- The debit is **internal to the agent loop**. It takes from the soonest-expiring bucket first,
  in one transaction with the buckets locked. It is not exposed to code bodies, workflows or
  modules.

**Metering an agent.** An agent has an optional **LLM credits** setting, with a rate: units per
dollar of model cost (from `Usage::cost`), or per 1,000 input and output tokens when the
model's prices are unknown. A metered run:

1. refuses to start if the caller's balance is below a minimum (an agent setting), and refuses
   an anonymous caller;
2. debits after each model call, keyed by the run and the step. The call has already
   happened, so the debit is allowed to take the balance below zero (debt), and the next grant
   repays it;
3. stops at the next step boundary once the balance is zero or below, ending with a
   reason ("out of credits") the chat UI shows.

Delegated sub-agents debit the same caller. The admin's copilot is never metered.

**One balance, one unit.** There is a single credit unit per instance, not named meters:
named meters only matter for charging different activities, which is out of scope. Buckets and
entries are ordinary tables that owners can read (an entry records the run, model and tokens),
so a balance widget is a REST read and updates live.

**Module (P7).**

- **Allowance**, from the plan binding: on each paid renewal (keyed by the invoice) the plan's
  allowance is granted as a bucket that expires at the period's end. **Rollover**: `none`, `all`,
  or `capped`, which carries `min(remaining, cap)` into the new bucket. The old bucket is
  expired by the same key.
- **Top-ups** are the app's credit-pack rows paid through `pay_checkout` with a *grant credits*
  effect (§6.1). **`pay_charge_saved`** takes the same configuration but charges **the same
  card** with no checkout page, when the adapter can (`charge_saved_method`: a Stripe
  off-session PaymentIntent, a Paddle one-time charge on the subscription), and answers
  `{ status: "paid" }`. If the bank asks for authentication (SCA), or there is no saved card, it
  answers a checkout URL instead. The credit is granted from the webhook, never from the
  request.
- **Auto top-up** is an app trigger, not a module feature: core inserts a `credit_entries` row
  with `kind: "low_balance"` when an agent run takes the balance below the user's threshold, and
  the app's trigger on it runs `pay_charge_saved` for the pack it chooses.
- **Postpaid overage** (later): instead of stopping at zero, report usage to the provider's
  meter and bill it on the next invoice.

### 6.4 Invoices and payment requests (use case 6)

The invoice is the app's row. `pay_request` takes the checkout configuration with that row as
the source and an `email` formula, with **payment methods** (card, SEPA debit, ACH, bank
transfer, iDEAL) and **capture** (now, or later for a deposit). It answers a payment URL.
Provider checkout links expire (Stripe's after 24 hours), so the link the app emails points at
**the app's own page**, which calls `pay_request` when the customer opens it. Bank methods log
`payment.processing`, then `payment.succeeded` or `payment.failed` days later, and the app's
triggers move the invoice along. A deposit is settled with `pay_capture` (an optional amount)
or released with `pay_cancel_payment`, each given the source row. Stripe and Mollie only.

### 6.5 Webhooks and reconciliation

`POST /_feldspar/hooks/payments/<account name>` (core change C1) is the URL the admin registers
with the provider. The account's settings page shows it with a copy button. The handler:

1. verifies the signature over the **raw body**. If it fails, it answers 400 and stores nothing;
2. skips events already in `pay_events` (answering 200);
3. in one transaction: `fetch`es the current state of each object an event refers to, upserts
   the mirror, applies grants and credits by key, and inserts the normalised `pay_events` rows.
   The app's triggers fire on those inserts;
4. answers 200. If processing fails, it reports the error to the error log and answers 500, so
   the provider retries.

The **`reconcile` service** lists, every few hours, the subscriptions and payments the provider
changed since the last sweep and processes them as above, which catches a webhook that never
arrived. The admin action `pay_sync` does the same on demand.

## 7. Mobile (use case 4)

Two different things are both called "mobile payments", and the store rules separate them:

**(a) Apple Pay and Google Pay as a way to pay**, for physical goods and services used outside
the app: the shop (1), invoices (6) and the marketplace (5). This is the provider's native
SDK. For Stripe that is `@stripe/stripe-react-native`'s PaymentSheet, or `PlatformPayButton`
for a wallet-only button. It needs a server action, `pay_native_sheet` (the checkout
configuration of §6.1), that answers `{ paymentIntent, ephemeralKey, customer, publishableKey }`,
not a checkout URL. `plugins/react-native` adds the SDK and its Expo config plugin (the Apple
merchant id and Google Pay enabled) when the payments module has a Stripe account, generates
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
- **In-app purchase** (P9): our own `apple` and `google_play` adapters (§3.12). The app buys with
  `expo-iap`, passing the user's id as `appAccountToken` / `obfuscatedAccountId`. It sends the
  transaction to `pay_verify_store_purchase`, which checks it against the App Store Server API
  or the Play Developer API. Store server notifications (App Store Server Notifications v2, Play
  RTDN) arrive at the same webhook route. The plan binding names the plans table's
  `apple_product` and `google_product` fields, and grants work exactly as in §6.2. A user who
  subscribed on iOS is subscribed on the web.

## 8. Marketplace (use case 5, later)

Stripe Connect (Express accounts) first. The sellers are the app's own rows. `pay_connect_onboarding`
creates a connected account for the caller and answers Stripe's hosted KYC link, so the seller's
identity checks and payouts are Stripe's job. The module mirrors the connected account and logs
`connected_account.updated`. The app's trigger stores the account id and its status on its own
sellers table. The checkout configuration gains two formulas: the **destination** (for example
`sellerⱵstripe_account`) and the **application fee** (`round(total * 0.1)`), giving a
destination charge. Baskets with lines from several sellers use **separate charges and
transfers**, with a destination formula per line. Refunds and disputes reverse the transfer and
the fee by a setting. MoRs cannot do this, and Stripe Managed Payments excludes Connect, so a
marketplace is always PSP mode. Mollie Connect and Adyen for Platforms are the alternatives.
This is the most legally involved use case (who is the seller of record, and 1099/DAC7
reporting), which is another reason to leave it until last.

## 9. Core changes

| | Change | Needed by |
|---|---|---|
| **C1** | **Module webhooks.** A module declares `webhooks: { name: async (req, ctx) => ({ status, body }) }`, served at `POST /_feldspar/hooks/<module>/<name>/<rest>` on the admin host, as the OAuth callback is (one stable URL to register). `req` has `rawBody` (bytes), `headers` and `rest`. There is no session and no CSRF check. There is a body cap (1 MiB) and a per-IP rate limit. `ctx` is a service's (`db` over the module's own tables, `files`, `log`) plus C4. The bundled card says "Receives webhooks at …". | all |
| **C2** | **Role grants.** The ordinary table `role_grants` (owner-readable, written only by core, so an app can show "your trial ends on …" and a daily trigger can send reminders), and one `effective_role(user)` function, used wherever a user is loaded: sessions, API tokens, MCP, the `login` event. `user.role` in formulas, the RLS GUC and `Event.role` are the effective role. `users.role` stays the base role an admin sets. The admin's user page lists grants and can add a time-limited one by hand. | 2, 3 |
| **C3** | **LLM credits ledger.** The ordinary tables `credit_buckets` and `credit_entries`, owner-readable and written only by core. `grant` and `balance` as in §6.3; the debit is internal to the agent loop, and agent runs are the only spender. No code-body surface and no action. | 3 |
| **C4** | **Module access to C2/C3.** `ctx.roles.grant/revoke` and `ctx.credits.grant/balance` (no debit) for module actions, services and webhooks. Granting roles is a **permission the admin gives at install**, with a ceiling: "may grant roles 70 to 100". Admin (1) can never be granted by a module. | 2, 3 |
| **C5** | **The calling application in `Event`**: its id, public base URL and (for native apps) URL scheme. Checkout success, cancel and portal return URLs need it. | 1, 2, 4 |
| **C6** | **CSP additions per application.** Embedded elements need `js.stripe.com` in `script-src` and `frame-src`, and `api.stripe.com` in `connect-src`. A module manifest may *suggest* sources per adapter. The admin turns them on per application (shown on the application's page); they are not added on install. Hosted-checkout redirects need nothing. | 1, 2 (embedded) |
| **C7** | **React Native native payments** in `plugins/react-native` (§7a): the dependency, config plugin, generated hook and return URL. Later, `expo-iap` (§7b). | 4 |
| **C8** | **The `grant_role` action**: a user formula, a role, a duration or `expires_at` formula, a key formula, and `once` (no grant if one with that key ever existed, expired included). For card-less trials, comps and promotions, with no payments involved. Like `update_rows`, it runs with the trigger's authority, and only an admin configures it. | 2 |
| **C9** | **Guest sessions.** An application can switch on sessions for visitors who are not signed in (off by default). The generated client's `startGuestSession()` starts one explicitly, when the basket gets its first item, so a crawler or a bot that only looks at pages never creates a row. A guest session has the same hashed cookie token as a signed-in session, plus a separate random **public id**. That public id is what a table stores and what formulas see as `session.id`. The cookie token is never stored in a table, so a database dump or a REST read of `orders.session_id` is not a usable cookie. `session` is in scope wherever `user` is: ownership formulas, default expressions, `only_if` and code bodies, plus a third RLS GUC (`sc.session`). `user` stays `null` and the role stays public (100), so existing formulas keep their meaning, and `user && …` still means "signed in". A request carrying a guest session must pass the CSRF check to use it, otherwise it is served as a session-less anonymous caller (§7.3's rule). Guest sessions are kept in a **logged** table, not the unlogged session table: a guest session is the only record of who owns a guest's rows, so losing it in a crash would lose the guest's basket. **The cookie is persistent**, with `Max-Age` equal to the guest session's lifetime (a setting, default 30 days), so the basket survives a reload, closing the tab and restarting the browser. It is lost only when the visitor clears cookies, uses a private window that has since closed, or switches device. Guest sessions are **per application** (decided): the cookie is set on the application's own host, never on the shared cookie domain that signed-in sessions can use. The cookie is `SameSite=Strict` like a signed-in session's. So the first page load after returning from the provider's checkout (a cross-site redirect) arrives without it, as an OAuth return does (§7.2). A React app's later API calls are same-site and do carry it. The success page therefore loads the order through the API rather than from the initial page request. At sign-in or sign-up in the same browser, the `login` event's payload carries `guest_session` (the public id) and the guest session ends. | 1 |

The module's `net` permission is **an exact allow-list**: the union of the configured adapters'
`hosts` (`api.stripe.com`, `api.polar.sh`, `sandbox-api.polar.sh`, …), not `*`.

## 10. Decided, and open questions

Decided:
- **Plans live in a table the app's admin creates**, with whatever fields the business needs.
  The module's plan binding (§6.2) only says which fields hold the provider price ids, the role
  and the credit allowance. No plan data is kept on the provider's products as metadata.
- **A checkout is always for a row the app created** (§6.1). Guests get one through guest
  sessions (C9), not through a separate basket mode.

- **Guest sessions are per application**, even where signed-in sessions share a cookie
  between applications.

Open: none at present.

---

# Part II — Milestones

## P1 — Core seams: webhooks, the calling application, CSP, guest sessions (core)

- [x] **P1.1** C1 module webhooks: manifest key, route, raw body and headers, cap, rate limit,
  `ctx` with the module's own tables. Test: a fixture module's webhook receives a raw body and
  headers byte-for-byte, writes a row to its table, and its status code reaches the caller.
  It cannot write a table it did not declare.
- [x] **P1.2** Bundled card and installed card list the webhook URLs.
- [x] **P1.3** C5: `Event.app` (id, base URL, scheme) on events raised through an app's API.
  Test: an exposed trigger sees its app's URL on two different hosts.
- [x] **P1.4** C6: per-application CSP additions, with module-suggested sources shown and
  switched on by the admin. Test: the app's CSP header carries them only once enabled.
- [x] **P1.5** C9 guest sessions: the per-application switch, `startGuestSession()` in the
  generated client, the logged session rows, `session.id` in ownership formulas, default
  expressions, `only_if`, code bodies and the RLS GUC, and the CSRF rule. Tests: two guests
  each see only their own `open` order through REST, under runtime checks and under RLS; the
  same basket is read again by a new request carrying the cookie (a reload); a guest write
  without the CSRF token is served as session-less; an expired guest session reads nothing;
  the cookie is set on the application's host only; after a cross-site return from the `fake`
  checkout, the success page's API call sees the guest's order.
- [x] **P1.6** The `login` event's `guest_session`, and the guest session ending at sign-in.
  Test: a trigger moves a guest's orders to the account that signed in from the same browser.

Notes from P1, for the milestones that build on it:
- C1's URL has a webhook name between the module and the rest:
  `/_feldspar/hooks/payments/<webhook>/<rest>`. §6.5's `/_feldspar/hooks/payments/<account name>`
  becomes, for example, `/_feldspar/hooks/payments/events/<account name>` (P2.3).
- The P1.5 test for a return from the `fake` checkout is a navigation that arrives without the
  cookie, then an API call that carries it. Repeat it against the real `fake` page in P2.2/P2.4.
- A guest's live socket does not carry the guest session yet. The basket's live updates (§6.1
  step 2) need that before P2.8.
- A default expression is still allowed only on a required field. `orders` with both `user_id`
  (`user && user.id`) and `session_id` (`session.id`) defaults needs nullable fields to take
  defaults (P2.8).

## P2 — The module and use case 1 (shop), on Stripe and `fake`

- [x] **P2.1** `plugins/payments` skeleton: manifest, the three tables (§5), `pay_accounts` with
  `Secret` keys, adapter loading, capability checks with named refusals, net allow-list from
  hosts.
- [x] **P2.2** `fake` adapter: a local checkout page and self-delivered signed webhooks.

  Notes from P2.1–P2.2:
  - The provider events webhook is `POST /_feldspar/hooks/payments/events/<account name>`. It
    verifies the signature with the account's adapter and answers `{ received: [ids] }`; the
    processing (dedupe, fetch-and-upsert, `pay_events`) is P2.3's to add there.
  - The fake provider's checkout page is the module's `fake` webhook, which declares `methods:
    ["GET", "POST"]` (C1 now allows a webhook to answer `GET`). Webhook routes now answer on
    every host, like the OAuth routes, so the page is served on the calling application's host
    (`app.url`, P1.3). The card still shows the admin-host URL to register.
  - The fake delivers its signed webhooks **in-process**, into the same `events` handler a
    provider's HTTP request reaches, so it needs no `net` permission and no address for this
    server. Its objects live in the worker's memory and are lost on a restart.
  - `pay_account_test` is the one action so far: an admin (or a test) starts a small checkout on
    an account. `pay_checkout` (P2.4) builds on the same `usableAccount` and capability checks.
  - Module actions now get `db`, `app` and `session` in their argument (§15.1e).
- [x] **P2.3** Webhook handler (§6.5): verify, dedupe, fetch-and-upsert the mirror, insert
  normalised events, 500 on failure. Tests: a duplicate event is processed once, out-of-order
  events give the right final state, a bad signature stores nothing, and an unknown provider
  event is logged as `other`.
- [x] **P2.4** `pay_checkout` (§6.1): the shared checkout configuration (the source row, line
  formulas, currency, customer, tax, shipping, paths, effects), the read check on the source
  row, and a typed payload and answer. Tests against `fake`: an app table's row is paid for,
  and an app trigger on `payment.succeeded` marks it paid with `update_rows`; a payload amount
  is ignored; line formulas read through Ⱶ; a guest checks out their own order through a guest
  session; nobody can check out a row they cannot read.
- [x] **P2.5** Stripe adapter, one-off: Checkout Session with ad-hoc prices, Stripe Tax option,
  shipping, the `managed_payments` flag (refused with shipping), refunds, webhook verification.
  Recorded-response tests; live test-mode test when keys are present.
- [x] **P2.6** `pay_refund` (given the source row or a mirror row), and the escape hatches:
  `pay_api` and `pay_provider_code` (`stripe` in scope).
- [x] **P2.7** `reconcile` service: the periodic sweep.
- [x] **P2.8** Tutorial `docs/tutorial-payments.md` part 1: a React shop with its own
  `products`, `orders` and `order_lines` tables, the basket as an `open` order owned by the
  user or the guest session, prices read through Ⱶ so the buyer cannot set them, triggers that
  mark orders paid and decrement stock, moving a guest's orders to the account at sign-in, and
  **delivering a digital good** (a paid order
  of a downloadable product writes a download row the buyer can read, and emails the link).

  Notes from P2.3–P2.8, for the milestones that build on them:
  - Processing (`processing.js`) has no transaction: each step is an upsert or an
    insert-if-absent, so a retry finishes a delivery that failed half way, and a trigger on
    `pay_events` fires once per row. The sweep processes the provider's **event log**
    (`adapter.listEvents`) as if delivered; `list(kind, since)` is there for P4's subscription
    sync but nothing calls it yet.
  - `ref_key` is a JSON field (the key with its own type), so `id === row.ref_key` holds for
    integer, text and UUID keys; a row action reads a JSON column's scalar as itself.
  - Effects are evaluated and recorded on the checkout's mirror row (and inherited by its
    payment), but **not applied**: role grants are P3.3's `ctx.roles`, credit grants P6.4/P7.2.
  - `pay_checkout` takes `mode: "payment"` only; `subscription` is refused by capability (fake)
    or as not built (Stripe) until P4.1. Stripe's capabilities say what Stripe can do; the
    actions refuse what is not built.
  - The Stripe adapter speaks REST over `fetch`, not the `stripe` package: `pay_provider_code`'s
    `stripe` is an authenticated `get/post/del/request` client (§15.1f). Its fixtures are
    written to Stripe's documented response shapes; `test/stripe-live.test.mjs` checks the same
    calls against Stripe's test mode when `STRIPE_TEST_SECRET_KEY` is set (not run here).
  - `pay_api` is a module function, and a function has no `db`: it reads the accounts the
    `reconcile` service keeps in the worker's memory, so it fails, by name, until the service
    has started.
  - Core changes made for P2: string `+` translates to SQL concatenation; a default expression
    is allowed on a nullable field; a guest's live socket carries the guest session (verdicts on
    `table_changes` included), and closes when the guest session ends.
  - A payload field that is not there reads as null in a translated formula, as `old.x` does on an
    insert. The tutorial's sign-in trigger guards with `session_id !== null`; a formula comparing
    a nullable column with an optional payload field needs the same guard.

## P3 — Role grants (core)

- [x] **P3.1** `role_grants` and `effective_role`, used in sessions, tokens, MCP, the
  `login` event, `user.role` in formulas and the RLS GUC. Tests: a grant changes what a user
  can read within the session freshness TTL, and an expired grant ends access with no write.
- [x] **P3.2** Admin user page: base role, grants in force, add a time-limited grant by hand.
- [x] **P3.3** C4 for roles: `ctx.roles.grant/revoke` (upsert by source and ref), with the
  install-time ceiling. Test: a module granting a role above its ceiling, or role 1, is
  refused.
- [x] **P3.4** C8, the `grant_role` action, with `once`. Test: a trial granted on sign-up is not
  granted again after it expires.

  Notes from P3, for the milestones that build on it:
  - A module's grant has the module's **package name** as its source (`@feldspar/payments`)
    and the module's own `ref`, so P4's subscription grant is `ctx.roles.grant({ user, role,
    ref: "<account>:<subscription id>", expires_at })`, extended by the same call on each
    renewal and ended by `ctx.roles.revoke(ref)`. `ctx.roles` exists in actions, services and
    webhooks, so the processing code can grant from both the webhook and the sweep.
  - The ceiling is the module's `roles` permission (one number: the most privileged role it may
    grant). The payments manifest does not request one yet. P4.1 should either request it in
    `feldspar-module.json` (`"permissions": { "roles": [<n>] }`, granted by the install) or tell
    the admin to set it on the Modules tab. A grant refused for want of a ceiling should then
    be reported as an issue, like an unbound price.
  - `pay_checkout`'s *grant a role* effect is still only recorded on the mirror (P2 notes). It
    can now be applied from processing with `ctx.roles.grant`, keyed by the payment, once the
    module has a ceiling. That belongs with P4.1, which adds the ceiling.
  - The `grant_role` action's key is **per user** (stored as `<user id>:<key>`), so the
    tutorial's trial is `key: "trial"`, `once: true` (P4.5). A daily reminder trigger reads
    `role_grants` where `source === "grant_role"` and `expires_at` is tomorrow.
  - `role_grants` is owner-readable, so an application's generated client can show "your trial
    ends on …" once the app exposes the table.
  - Only the database session store (`SessionStore::database`, what a server runs) re-reads a
    user. The in-memory store used by some tests never does, so grant tests use the database
    store.
  - Core change made for P3: an ordered comparison of two literals in a translated formula is
    folded by the translator (`user.role <= 60` under runtime checks used to fail to bind).

## P4 — Use case 2: subscriptions on Stripe and `fake`

- [x] **P4.1** The plan binding (§6.2) in the module's configuration; subscription mode in
  `pay_checkout`; subscription sync; grants per §6.2's table, with grace; the plan looked up
  from the current price on every change. Test: an unbound price grants nothing and is
  reported.
- [x] **P4.2** Trials with the card up front. Tests with `fake` and Stripe test clocks: a trial
  ends with and without conversion, and a renewal fails then recovers within grace.
- [x] **P4.3** `pay_portal`, `pay_change_subscription`, `pay_cancel_subscription`. Test: a tier
  change in the portal moves the grant's role.
- [x] **P4.4** Owner-readable subscription rows in the mirror, so an app can show the renewal
  date.
- [x] **P4.5** Tutorial part 2: a SaaS app with its own `plans` table, a free tier, two paid
  tiers, a card-less trial with `grant_role`, and a daily trial-ending reminder.

  Notes from P4, for the milestones that build on it:
  - **A module now reads an application table its settings name** (core, `reads`): the plans
    table, from the webhook and the sweep, read-only, and only because the stored settings hold
    its name (TECHNICAL_DESIGN §15.1, "Tables a module reads"). P7's allowance and rollover
    fields are more fields of the same plan row, so they need no new seam.
  - The binding is **one** entry, from the settings form; its price field may be per account
    (`shop=stripe_price, ios=apple_product`), which is what P5 (Polar) and P9 (app stores) fill
    in. `plans.planBinding` returns one binding; a list, if a second plans table is ever wanted,
    is a change there and in `findPlan`.
  - The module asks for the role ceiling 50 in its manifest, granted by the install. A grant it
    may not make, a price that is no plan's, and a subscription with no user are written on the
    mirror row's new **`issue`** column (and logged), not thrown: a retried delivery would be
    refused the same way. The sweep re-reads every **live** subscription in the mirror and
    re-applies its grant, so fixing the binding or the ceiling takes effect at the next sweep or
    `pay_sync`. The issue is not on the module's card (a module has no runtime issues there).
  - `adapter.list(kind, since)` is still unused: Stripe lists subscriptions by creation, not by
    change, so the sweep re-reads the mirror's live subscriptions instead of listing.
  - Processing's `ctx` is `{ db, roles, configuration, log }` everywhere (webhook, service,
    action). An action that makes the fake deliver in-process passes its own `roles` and the
    module's settings (`subscriptions.actionCtx`).
  - `adapter.kinds(event)` may answer `null`: understood, object synced, nothing logged (Stripe's
    first-period `invoice.paid`). P7's allowance grants hang off `subscription.renewed`, which
    Stripe sends as `invoice.paid` with `billing_reason: subscription_cycle` — keyed by the
    invoice (the event's object is the invoice; processing currently fetches the subscription, so
    P7 needs the invoice id from `event.raw` for its key).
  - Customers are mirror rows (`kind: customer`, with the user) made from the `customer_ref` of a
    checkout, payment or subscription. `pay_checkout` passes the user's known customer to the
    provider, so P7's `pay_charge_saved` has the customer to charge.
  - The `pay_checkout` *grant a role* effect is now applied (per payment, from the payment's own
    time, revoked on a full refund). The *grant credits* effect is still only recorded (P6/P7).
  - **§6.2 corrected:** sign-up inserts the `users` row below the row layer, so no table trigger
    sees it. The tutorial's trial is a `grant_role` trigger on `login` with `once`.
  - The code-body `db` chain's `where` takes **one operator per field condition**
    (`{ expires_at: { gte: … } }`); a range is two `.where()` calls. The in-memory test `db`
    (`test/fake-db.mjs`) now refuses more, as the server does.
  - Core change made for P4: a bare numeric or boolean literal in a select list is rendered with
    a cast (`CAST($1 AS BIGINT)`), because Postgres typed its placeholder as text and refused the
    integer bound to it (`select({ quantity: "1" })`, which `pay_checkout`'s default quantity is).
  - The Stripe live tests drive subscriptions on a Stripe test clock; they run only with
    `STRIPE_TEST_SECRET_KEY` and were not run here.

## P5 — Second adapter: Polar, a merchant of record

- [ ] **P5.1** Adapter for one-off and subscriptions, with webhook verification and portal.
  Anything in the adapter interface that turns out to be Stripe-shaped is fixed **here**,
  before credits and mobile build on it.
- [ ] **P5.2** The P2 and P4 test suites run against both adapters (one parametrised suite,
  not a copy).
- [ ] **P5.3** Find out whether Polar can charge a customer's saved payment method for a
  one-off purchase. If it can, set `charge_saved_method` so P7's top-ups charge the same card.
  If it cannot, top-ups on Polar always go through checkout, and the capability says so.

## P6 — LLM credits ledger (core)

- [ ] **P6.1** Buckets and entries; `grant`/`balance` and the internal debit, with bucket locking,
  soonest-expiring first, debt and repayment, idempotency keys. Tests: concurrent debits
  never overdraw, and a replayed grant does nothing.
- [ ] **P6.2** Admin user page: the balance, the entries, and a manual grant.
- [ ] **P6.3** Agent metering (§6.3): the LLM credits setting, rate, minimum balance; a debit
  per model call; the run stops at zero; anonymous callers are refused.
- [ ] **P6.4** C4 for credits (`ctx.credits.grant/balance`), and the `low_balance` entry with a
  per-user threshold.

## P7 — Use case 3: allowances, rollover, top-ups

- [ ] **P7.1** Allowance and rollover fields in the plan binding; allowances granted on paid
  renewals; rollover `none`/`all`/`capped`. Tests over three periods for each policy.
- [ ] **P7.2** The *grant credits* effect on `pay_checkout`, and `pay_charge_saved`: the saved
  card where the adapter can, falling back to checkout when the bank asks for authentication or
  there is no saved card. The credit is granted from the webhook.
- [ ] **P7.3** Tutorial part 3: an agent app with a $5 plan including 100 units, a
  `credit_packs` table with a $2 top-up, and auto top-up as a trigger on `low_balance`.

## P8 — Use case 4a: Apple Pay and Google Pay in React Native

- [ ] **P8.1** `pay_native_sheet` (Stripe), on the shared checkout configuration: customer,
  ephemeral key, PaymentIntent.
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

- [ ] **P10.1** `pay_request`, payment methods, the `payment.processing` event, bank methods
  (Stripe; Mollie adapter).
- [ ] **P10.2** Manual capture: `pay_capture` and `pay_cancel_payment`.
- [ ] **P10.3** Tutorial part 4: an invoices table, a pay page that calls `pay_request`, and a
  booking deposit captured on completion.

## P11 — Use case 5: marketplace (Stripe Connect)

- [ ] **P11.1** `pay_connect_onboarding`, the connected-account mirror and its event.
- [ ] **P11.2** Destination and application-fee formulas; multi-seller baskets with separate
  charges and transfers; refund and dispute policy.

## P12 — More adapters, on demand

- [ ] **P12.1** Paddle, Mollie (if P10 did not), Lemon Squeezy, Creem, Dodo, PayPal: each one
  adapter file and a run of the shared suite.
