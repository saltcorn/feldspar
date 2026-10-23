# Tutorial: Payments — part 3, LLM credits

This tutorial builds **Tea Sommelier**, an application whose main feature is an AI agent that
answers questions about tea. Its model calls cost real money, so the agent is paid for in **LLM
credits**:

- the **Assistant** plan costs 5 € a month and includes **100 credits** each month. Up to 50
  unspent credits carry over into the next month;
- a **credit pack** of 100 more costs 2 €, bought with one click on the card the user already
  gave;
- with **auto top-up** on, a pack is bought by itself when the balance runs low.

Credits are kept by Feldspar itself, in a ledger. An agent run is the only thing that spends
them. The **Payments** module only adds them: a plan's allowance when a month is paid, and a
pack when it is paid for. Everything else here is yours, built from tables and triggers.

This continues from [part 2](tutorial-payments-saas.md), which set up subscriptions and the
plan binding. You need the module installed (part 1, Step 1) and an agent with a model
([tutorial-agents.md](tutorial-agents.md)).

## How credits work

A user's balance is made of **buckets**. Each grant is one bucket: an amount, what is left of it,
and when it expires. Three tables hold the ledger, and each user reads only their own rows:

| Table | One row per | |
|---|---|---|
| `credit_buckets` | grant | `amount`, `remaining`, `kind` (`allowance`, `top_up`, `admin`), `expires_at` |
| `credit_entries` | thing that happened | a grant, a debit (with the run, the model and the tokens), a `rollover`, or a `low_balance` notice, each with the `balance` after it |
| `credit_accounts` | user | their `debt`, and their `low_balance_threshold` |

The balance is what is left in the unexpired buckets. An agent's model calls take credits from
the bucket that expires soonest, so this month's allowance is used before a pack that lasts a
year. A model call has already been made by the time it is charged, so the last call of a run
may take the balance below zero. The next grant repays that debt first.

Only Feldspar writes these tables. Your application reads them, and a balance widget is an
ordinary REST read that updates live.

## Step 1 — The plan, with its credits

Add three fields to `plans` from part 2:

| Field | Type | |
|---|---|---|
| `credits` | int | the credits each month of the plan includes |
| `rollover` | text | what happens to unspent credits at the month's end: `none`, `all` or `capped` |
| `rollover_cap` | int | the most a `capped` rollover carries over |

Give Tea Sommelier one plan:

| id | name | price | stripe_price | role | credits | rollover | rollover_cap |
|---|---|---|---|---|---|---|---|
| 1 | Assistant | 5 € a month | `price_assistant` | 70 | 100 | `capped` | 50 |

Use a test account of its own: a `pay_accounts` row named **`tea`**, adapter `fake`, any random
text as its webhook signing secret, and these settings:

```json
{ "prices": { "price_assistant": { "amount": 500 } } }
```

## Step 2 — Tell the module which fields hold the credits

**Modules → Payments → Settings**, the plan binding from part 2, and three more settings:

| Setting | Value |
|---|---|
| Plans table | `plans` |
| Price field | `stripe_price` |
| Role field | `role` |
| LLM credits field | `credits` |
| Rollover field | `rollover` |
| Rollover cap field | `rollover_cap` |
| Grace period (days) | `3` |

From now on, every time a month of the plan is paid for, the module grants its 100 credits as a
bucket that expires at the end of that month. What each rollover means:

| `rollover` | At the end of the month |
|---|---|
| `none` | unspent credits expire with the month |
| `all` | all of them are carried into the next month's bucket |
| `capped` | up to `rollover_cap` of them are carried over; the rest expire |

So someone who spent 85 of their 100 credits starts the next month with 115. Someone who spent
none starts it with 150, not 200. Each carry-over is an entry of kind `rollover`, saying how much
was carried and how much was forfeited.

When the module grants:

| The subscription | Credits |
|---|---|
| starts, paid | the first month's |
| starts with a trial (card taken up front) | the plan's credits, for the trial |
| renews, paid | the new month's, rolling over the last month's |
| past due (the renewal's card was declined) | none, until the payment is recovered; then that month's |
| changes plan mid-month | none until the next month, which is the new plan's |
| is canceled | none; what is left lasts to the month's end |

The allowance is keyed by the subscription and the month, so a webhook delivered twice grants it
once. A renewal whose webhook never arrives is granted by the module's sweep, a few hours later.

## Step 3 — The agent

Make the agent as [tutorial-agents.md](tutorial-agents.md) does, and fill in its **LLM credits**
card:

| Box | Value | |
|---|---|---|
| Credits per dollar | `100` | one credit is a cent of model cost |
| Balance needed to start | `1` | |

Now every model call of a run is charged to the signed-in user who is talking to it, and a run
stops, saying "You are out of LLM credits", once their balance is used up. A guest cannot use
the agent at all, and an admin is never charged, so trying it yourself costs nothing.

## Step 4 — Credit packs

A pack is a row of a table of yours, priced there, out of the buyer's reach (part 1's first
lesson). Make **`credit_packs`**, **Minimum role to read: public** and to write: admin:

| Field | Type | |
|---|---|---|
| `id` | int, primary key | |
| `name` | text, required | |
| `price` | int, required | in cents: `200` is 2.00 |
| `units` | int, required | the credits it gives |

| id | name | price | units |
|---|---|---|---|
| 1 | 100 credits | 200 | 100 |

Make a trigger named **`buy_credits`**, event **None**, minimum role **80** (signed in), action
**`pay_checkout`**:

| Setting | Value | |
|---|---|---|
| Payment account | `tea` | |
| Mode | `payment` | |
| Table | `credit_packs` | |
| Which row | `payload` | |
| Payload field | `pack` | `{ pack: 1 }` |
| Amount | `price` | |
| Description | `name` | |
| Currency | `EUR` | |
| Keep the card for later charges | ticked | so the next pack needs no checkout page |
| Grant LLM credits | `units` | |
| Credits expire after (days) | `365` | |
| After paying | `/credits` | |

When the provider reports the payment, the module grants `units` credits to the buyer, as a
`top_up` bucket that lasts a year from the payment. The grant is keyed by the payment, so a
webhook delivered twice grants the pack once.

**A refund does not take credits back.** Only an agent run spends credits, so a refunded pack's
credits stay with the user. Refund only packs that have not been used, or grant nothing for a
refund you make as a courtesy.

## Step 5 — One-click top-ups

Someone who has subscribed, or bought a pack with **Keep the card** ticked, has a card saved with
the provider. **`pay_charge_saved`** charges that card with no checkout page. It takes the same
settings as `pay_checkout`.

Make a trigger named **`top_up`**, event **None**, minimum role **80**, action
**`pay_charge_saved`**, with the settings of `buy_credits` above (it has no **Keep the card**: it
always keeps it). It answers one of two things:

- `{ status: "paid" }`: the card was charged. The credits arrive with the provider's webhook,
  a moment later, and the balance widget updates by itself.
- `{ status: "requires_checkout", url, reason }`: the card could not be charged without the
  buyer. Perhaps there is no saved card yet, the bank wants the buyer to confirm the payment
  (3-D Secure, common in Europe), or the card was declined. The `url` is a checkout for the same
  pack. Send the buyer there, and the card they pay with is the saved one from then on.

```tsx
// src/pages/Credits.tsx
import { api, useCreditBuckets, useCreditEntries } from "../feldspar/hooks";

export function Credits() {
  const { data: buckets = [] } = useCreditBuckets();
  const { data: entries = [] } = useCreditEntries();
  const now = new Date();
  const balance = buckets
    .filter((b) => !b.expires_at || new Date(b.expires_at) > now)
    .reduce((sum, b) => sum + Number(b.remaining), 0);

  async function topUp() {
    const answer = await api.runTopUp({ pack: 1 });
    if (answer.status === "requires_checkout") location.href = answer.url;
  }

  return (
    <>
      <h1>{balance.toFixed(0)} credits</h1>
      <button onClick={topUp}>Add 100 credits for 2 €</button>
      <ul>
        {entries.map((e) => (
          <li key={e.id}>
            {e.kind} {Number(e.amount).toFixed(2)} → {Number(e.balance).toFixed(2)}
          </li>
        ))}
      </ul>
    </>
  );
}
```

Tick `buy_credits` and `top_up` in the application's **Triggers**, and `credit_buckets` and
`credit_entries` in its **Tables**. (The balance here leaves out debt, which is never more than one
model call. `credit_accounts` has it, if you want it exact.)

**Each press is a new top-up.** A press is not repeated by a retry, but a double click charges
twice. To stop that, send a key that is the same for one intent, such as
`api.runTopUp({ pack: 1, key: crypto.randomUUID() })` made once when the page opens. The same key
charges once, however often it is sent.

## Step 6 — Auto top-up

Auto top-up is a trigger of yours, not a module feature. It reacts to a notice Feldspar writes
when an agent run takes a user's balance below their **low-balance threshold**: a
`credit_entries` row of kind `low_balance`.

**Turning it on.** A user has no threshold until it is set, and only an admin, or a trigger with
an admin's authority, may set one. Make a trigger named **`auto_top_up_on`**, event **None**,
minimum role **80**, action **`update_rows`**:

| Setting | Value |
|---|---|
| Table | `credit_accounts` |
| Where | `user_id === user.id` |
| Assignments | `low_balance_threshold`: `"20"` |

The threshold is written as text, `"20"`, because credits are exact decimals. `null` turns auto
top-up off again, from a second trigger the same way. A user's `credit_accounts` row is made by
their first grant, so a user who has never had credits has none to update yet: offer the switch
once they have subscribed or bought a pack.

**Topping up.** Make a trigger named **`auto_top_up`**, event **A row is inserted** into
`credit_entries`, **only if** `row.kind === "low_balance"`, action **`pay_charge_saved`**:

| Setting | Value | |
|---|---|---|
| Payment account | `tea` | |
| Table | `credit_packs` | |
| Which row | `fixed` | |
| Key | `1` | the pack to buy |
| The caller must be able to read the row | ticked | |
| Amount | `price` | |
| Description | `name` | |
| Currency | `EUR` | |
| Grant LLM credits | `units` | |
| Credits expire after (days) | `365` | |

The notice's user is the person whose run it was, so the card charged is theirs. The charge is
keyed by the notice's own id, so a trigger that runs twice for one notice charges once. A notice
is written when the balance *falls below* the threshold, not every time it is below it. So one
low balance buys one pack, and the next notice comes only after the balance has been above 20
again.

When the bank asks to confirm, or the card is declined, nothing is charged. The action answers a
checkout URL, but nobody is there to open it. The run carries on until the balance runs out, and
the credits page's **Add 100 credits** button then opens the checkout. To tell the user sooner,
add a trigger on `pay_events` for `payment.failed` with `ref_table === "credit_packs"` that sends
an email.

## Step 7 — Watching it work on the test provider

1. Sign up, open the pricing page, subscribe to **Assistant** and press **Pay**. The credits page
   shows 100, in a bucket of kind `allowance` that ends with the month.
2. Turn on auto top-up, and talk to the agent until the balance falls below 20. A pack is
   bought by itself: a `low_balance` entry, then a `top_up` bucket of 100.
3. Open the customer portal (part 2, Step 8) and press **Renew (pay)**. The month's allowance
   arrives with up to 50 of last month's carried over.
4. In the portal, set the **Saved card** to **Asks to authenticate**, then press **Add 100
   credits**: the test checkout opens instead. Pay there, and the card pays without a page
   again.

## Step 8 — Real money

**Stripe.** Nothing more to set up than part 2's. A subscriber's card is saved by Stripe with the
subscription, and a pack bought with **Keep the card** saves the card to a Stripe customer, with
the buyer's consent shown on Stripe's page. `pay_charge_saved` charges it with an off-session
PaymentIntent. When the bank wants 3-D Secure, Stripe refuses the charge with
`authentication_required`, and the action answers the checkout instead. The webhook events are
part 1's `payment_intent.*`.

**Polar** charges a saved card through its off-session orders, which are in preview and offered
to organisations on a paid plan. Without them, Polar refuses the charge by name, and the
action's refusal says so: use `buy_credits` (the checkout) for packs instead. Polar keeps a paying
customer's card by itself, so **Keep the card** asks nothing more of it.

**The fake provider** keeps a card for every subscriber and every buyer of a pack with **Keep
the card**. The portal's **Saved card** buttons make it ask to authenticate, be declined, or go
away, so you can try each answer of `pay_charge_saved`.

## Things that trip people up

- **Allowances are per paid month.** A past-due subscription gets no credits until its renewal
  is paid. A plan change waits for the next month: the month already granted is not granted
  again.
- **A refund keeps the credits.** See Step 4.
- **Thresholds are text.** `low_balance_threshold` is an exact decimal: assign `"20"`, not `20`.
- **No account, no threshold.** `credit_accounts` has a row for a user only after their first
  grant.
- **A plan whose rollover makes no sense** (`capped` with no cap, or a word that is not one of the
  three) grants no credits. The subscription's `pay_objects` row's **issue** says why, and its
  role is granted all the same. Fix the plan, and the next sweep, or `pay_sync`, grants them.
- **An admin is never charged.** Try the agent as a user to see credits move.
