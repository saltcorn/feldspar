# Tutorial: Payments — part 2, a subscription SaaS

This tutorial builds **Brew Notes**, a small tasting-notes application that people pay for by
the month. Anybody can sign up and keep notes for free. **Basic** adds blends, and **Pro** adds
tasting flights. A new account gets a fortnight of Basic without entering a card, and an email
the day before that trial ends.

Access is a **role**. Your tables already decide what each role may read and write, so a plan
is just "which role this subscription gives, for as long as it lasts". The **Payments** module
looks after that: when somebody subscribes, renews, changes plan or cancels, it grants or ends the
role. Everything else is yours, built from ordinary tables and triggers: the plans, the free
tier, the trial and the reminder.

This continues from [part 1](tutorial-payments.md), which installed the module and built a shop
on the test provider. You need the module installed (part 1, Step 1). The shop itself is not
needed.

## How roles decide what a plan gives

A role is a number, and **a lower number is more privileged**: 1 is the admin, 100 is the public.
This tutorial uses three:

| Role | Name | Who |
|---|---|---|
| 60 | Pro | subscribers to Pro |
| 70 | Basic | subscribers to Basic, and anybody on a trial |
| 80 | User | everybody else who has an account: the free tier |

Add 60 and 70 under **Users → Roles** (80 is usually there already). A user's `role` stays what an
admin set, or what sign-up gave them: 80. A subscription adds a **role grant** beside it, and the
user is served at the most privileged of the two while the grant lasts. When it ends, nothing is
written: they are simply served at 80 again.

**The module may grant only roles 50 to 99.** That is what installing it allowed (its card says
*may grant users roles 50 to 99*), so it can never make anybody staff or an admin. If your paid
tiers are more privileged than 50, change it on the Modules tab.

## Step 1 — The plans

Plans are rows of a table you make, with whatever fields your pricing page needs. Make
**`plans`**, with **Minimum role to read: public** and to write: admin:

| Field | Type | |
|---|---|---|
| `id` | int, primary key | |
| `name` | text, required | |
| `price` | text | what the pricing page shows: `5 € a month` |
| `stripe_price` | text | the provider's price id |
| `role` | int, required | the role it gives |

Add two rows:

| id | name | price | stripe_price | role |
|---|---|---|---|---|
| 1 | Basic | 5 € a month | `price_basic` | 70 |
| 2 | Pro | 15 € a month | `price_pro` | 60 |

The free tier is not a row: it is what an account is with no plan.

On the test provider any price id works. It costs 10.00 for 30 days unless the account's
**Settings** say otherwise. Give the account those settings so the test checkout shows your
prices:

```json
{ "prices": { "price_basic": { "amount": 500 }, "price_pro": { "amount": 1500 } } }
```

Use a test account of its own: add a `pay_accounts` row named **`saas`**, adapter `fake`, mode
`test`, any random text as its webhook signing secret, and these settings.

## Step 2 — Tell the module which fields mean what

**Modules → Payments → Settings**, the **plan binding**:

| Setting | Value |
|---|---|
| Plans table | `plans` |
| Price field | `stripe_price` |
| Role field | `role` |
| Grace period (days) | `3` |

Save. The card now lists **Reads: `plans`**. When the provider reports a subscription, the module
looks up the plan whose `stripe_price` is the subscription's price, and grants its `role`. It
looks the plan up again on every change, so a plan changed in the provider's customer portal
moves the role with nothing for you to do. The module may read `plans` because these settings
name it, and only read it: it never writes your tables.

The **grace period** is for a renewal whose payment failed. While the provider retries the card,
the user keeps their plan for that many days past the end of the period they paid for.

## Step 3 — The application's tables

Three tables of notes, one per tier, each with **Minimum roles** admin and admin, so that only
their ownership formula lets anybody in. All three have the same fields:

| Field | Type | Default value |
|---|---|---|
| `id` | int, primary key | |
| `owner` | Key to `users`, required | `user.id` |
| `text` | text | |

| Table | Ownership formula | |
|---|---|---|
| `notes` | `owner === user.id` | everybody's own notes |
| `blends` | `owner === user.id && user.role <= 70` | Basic and Pro |
| `flights` | `owner === user.id && user.role <= 60` | Pro only |

That is the paywall. `user.role` is the role the user is served at, grants included, so a
subscriber reads their blends and a lapsed one is refused. The application can hide the menu
items by the same `user.role`, but the table rules are what protect the data.

## Step 4 — The application

Make a React application named `notes`, and on its page:

- **API → Allow sign-up**, with role **80** for a new account.
- **Tables:** tick `notes`, `blends`, `flights`, `plans`, `pay_objects` and `role_grants`. The last
  two are the module's mirror and core's grants. Both are readable by their owner only, so each
  user sees their own subscription and their own trial.

## Step 5 — A trial without a card

A trial with no card involves no payment at all, so it is not the payments module's job. It is
a role grant, made by core's **`grant_role`** action. Make a trigger named `trial`:

| Setting | Value |
|---|---|
| Event | `A user logs in` |
| Action | `grant_role` |
| User | `user.id` |
| Role | `70` |
| Duration (days) | `14` |
| Key | `"trial"` |
| Once | ticked |
| Note | `14-day trial` |

Signing up signs the new account in, so its first `login` event is the moment it was made.
**Once** is what makes this a trial and not a renewable pass: the key is per user, and once a
grant with it has existed for a user, even an expired one, `grant_role` never makes another. Every
later sign-in does nothing. (An account an admin invites gets its trial at its first sign-in,
not when it was invited, which is what you want.)

The trial works the same whichever way the user later pays. If they subscribe to Basic during
the trial, the two grants overlap harmlessly. If they subscribe to Pro, they are served at 60.

## Step 6 — Subscribing

Make a trigger named `subscribe`, event **None**, minimum role **80** (signed in), action
**`pay_checkout`**:

| Setting | Value | |
|---|---|---|
| Payment account | `saas` | |
| Mode | `subscription` | |
| Table | `plans` | the row is the plan subscribed to |
| Which row | `payload` | |
| Payload field | `plan` | `{ plan: 2 }` |
| The caller must be able to read the row | ticked | |
| Price | *(empty)* | the plan binding's `stripe_price` |
| Quantity | `1` | |
| Currency | `EUR` | |
| After paying | `/welcome` | |
| After cancelling | `/pricing` | |

**Price** can be any formula over the row, for a checkout of something that is not a plan of the
binding. Empty, on a plan row, it is the binding's price field.

Tick `subscribe` in the application's **Triggers**, and the pricing page:

```tsx
// src/pages/Pricing.tsx
import { api, usePlans } from "../feldspar/hooks";
import { useUser } from "../auth";

export function Pricing() {
  const { user } = useUser();
  const { data: plans = [] } = usePlans();
  async function subscribe(plan: number) {
    const { url } = await api.runSubscribe({ plan });
    location.href = url;
  }
  return (
    <ul>
      {plans.map((p) => (
        <li key={p.id}>
          <strong>{p.name}</strong> — {p.price}{" "}
          {user && user.role > p.role && <button onClick={() => subscribe(p.id)}>Subscribe</button>}
        </li>
      ))}
    </ul>
  );
}
```

`user.role > p.role` offers only the plans that would give more than the user has now. Pressing
**Subscribe** opens the test provider's checkout. **Pay** sends the user to `/welcome`, and by then
the webhook has arrived and they are served at the plan's role: the blends page works. A guest
cannot subscribe: a subscription's role has to be granted to an account, and the action says so.

What the module does from then on, with nothing more from you:

| The provider says | The role grant |
|---|---|
| active (or trialing) | the plan's role, until the end of the period paid for, plus the grace period |
| renewed | extended to the new period's end, plus grace |
| past due (a renewal's card was declined) | left alone: it runs out unless the payment is recovered |
| canceled at the end of the period | kept exactly to that end |
| canceled now, or ended | ended |
| a plan change | the new plan's role |

**A missed webhook makes access lapse; it never extends it.** The grant has its own end date, so
a cancellation the server never heard about still ends access on time. The module's sweep, every
three hours, reads each live subscription again anyway, and catches up.

Each of those is also a `pay_events` row (`subscription.started`, `subscription.renewed`,
`subscription.past_due`, `subscription.canceled`, `subscription.ended`, `subscription.changed`),
with `ref_table` `plans` and `ref_key` the plan. That is where you would put triggers of your
own: a welcome email, a note to the team when somebody leaves.

**A trial with the card taken up front** is the provider's job: set **Trial days** to `14` on
the subscribe trigger. Nothing is charged at checkout, the subscription is `trialing`, and the
grant lasts to the trial's end plus grace. When the trial ends the card is charged and the
subscription carries on. If the user cancels during the trial, it ends with the trial.

## Step 7 — The account page

The module's mirror, `pay_objects`, has the user's subscription: its `status`, its plan's price
(`price_ref`), `current_period_end` (the renewal date) and `cancel_at_period_end`. Core's
`role_grants` has their trial. Both are readable by their owner, so the account page reads them
through the API:

```tsx
// src/pages/Account.tsx
import { api, usePayObjects, useRoleGrants } from "../feldspar/hooks";

const day = (iso: string) => new Date(iso).toLocaleDateString();

export function Account() {
  const { data: objects = [] } = usePayObjects();
  const { data: grants = [] } = useRoleGrants();
  const sub = objects.find((o) => o.kind === "subscription" && o.status !== "canceled");
  const trial = grants.find((g) => g.source === "grant_role" && !g.revoked_at);
  async function manage() {
    const { url } = await api.runManagePlan();
    location.href = url;
  }
  return (
    <>
      {sub ? (
        <p>
          Your plan {sub.cancel_at_period_end ? "ends" : "renews"} on {day(sub.current_period_end)}.{" "}
          <button onClick={manage}>Change or cancel</button>
        </p>
      ) : trial && new Date(trial.expires_at) > new Date() ? (
        <p>Your trial ends on {day(trial.expires_at)}. <a href="/pricing">Choose a plan</a></p>
      ) : (
        <p>You are on the free plan. <a href="/pricing">Upgrade</a></p>
      )}
    </>
  );
}
```

## Step 8 — Changing plan and cancelling

The provider's **customer portal** is where a subscriber changes plan, changes their card or
cancels. Make a trigger named `manage_plan`, event **None**, minimum role **80**, action
**`pay_portal`**, with **Payment account** `saas` and **Back to** `/account`. Tick it in the
application. It answers the portal's URL for the signed-in user, as above.

The test provider's portal lists the user's subscriptions with **Change plan** (a list of your
plans' prices, from the plan binding), **Cancel at period end** and **Cancel now**. Move Basic to
`price_pro` there, and the user is served at 60 by the time the page reloads.

It also has a **test clock**, which a real provider's portal does not: **Renew (pay)** moves the
subscription to the end of its period and renews it, and **Renew (card declined)** does the same
with a card that fails, so the subscription is past due. **Retry** then recovers it. **End the
trial** does the same for a trial. That is how you see a month go by in a second: the account page
moves to the next renewal date, and a declined renewal keeps the user's plan until the grace
period runs out.

If your application wants its own buttons rather than the portal, there are two actions:

- **`pay_change_subscription`**, payload `{ plan: 2 }`: moves the caller's subscription to that
  row of the plans table. **Proration** says how the provider charges for the rest of the period.
- **`pay_cancel_subscription`**: cancels the caller's subscription, at the end of the period
  (**At the end of the period** ticked, the usual choice) or now.

Both act only on the signed-in caller's own subscription. As with the portal, the role moves
when the provider reports the change, not when the button is pressed.

## Step 9 — "Your trial ends tomorrow"

Reminders are an email per trial, the day before it ends, to users who have not subscribed. Two
triggers and a table do it.

**`reminders`**, admin-only, to read and to write:

| Field | Type |
|---|---|
| `id` | int, primary key |
| `user` | Key to `users`, required |
| `ends` | timestamp |

**`trial_reminders`**, event **Once a day** at 09:00, action **`run_js_code`**:

```js
const tomorrow = Date.now() + 24 * 3600 * 1000;
const ending = await db.role_grants
  .where({ source: "grant_role", revoked_at: null })
  .where({ expires_at: { gte: new Date(tomorrow).toISOString() } })
  .where({ expires_at: { lt: new Date(tomorrow + 24 * 3600 * 1000).toISOString() } })
  .rows();
for (const grant of ending) {
  // Somebody who has subscribed needs no reminder.
  const subscribed = await db.pay_objects
    .where({ kind: "subscription", user_id: grant.user_id, status: { in: ["trialing", "active", "past_due"] } })
    .count();
  if (!subscribed) await db.reminders.insert({ user: grant.user_id, ends: grant.expires_at });
}
```

It runs once a day, so the window from 24 to 48 hours ahead sees each trial once. A condition
takes one operator, so the window's two ends are two `.where()` calls, which AND together.

**`remind_by_email`**, event **A row is inserted** into `reminders`, action **`send_email`**:

| Setting | Value |
|---|---|
| To | `{{ userⱵemail }}` |
| Subject | `Your Brew Notes trial ends tomorrow` |
| Text | `Your blends stay yours. Choose a plan to keep using them: https://notes.example.com/pricing` |

To try it without waiting for 09:00, make the first trigger with event **None**, press **Run**,
and change its event to **Once a day** when it does what you meant
([tutorial-triggers.md](tutorial-triggers.md), Step 4).

## Step 10 — Real money: Stripe

1. In Stripe's dashboard, make a product per plan, each with a **recurring** price, and put the
   price ids (`price_1Q…`) in `plans.stripe_price`.
2. Add a `pay_accounts` row for Stripe, as in part 1, Step 7. Its webhook endpoint also needs the
   subscription events: `customer.subscription.created`, `customer.subscription.updated`,
   `customer.subscription.deleted` and `invoice.paid`, beside part 1's.
3. Set up Stripe's **customer portal** (Settings → Billing → Customer portal): let customers switch
   between your plans' products, and cancel at the end of the period.
4. Set the `subscribe` and `manage_plan` triggers' **Payment account** to the Stripe account.

Nothing else changes. To keep the test provider for development beside Stripe, give each its
own price field, `test_price` and `stripe_price`, and name both in the binding's **Price field**:
`saas=test_price, stripe=stripe_price`.

**On Polar** a plan is a *product*: make a recurring product per plan, put its id in a
`polar_product` field of `plans`, and name it for the Polar account in the binding
(`saas=test_price, polar=polar_product`). Polar's customer portal needs no setting up. A plan
change through `manage_plan` is prorated on the next invoice or invoiced now (**Proration**
`prorate` or `invoice`); Polar has no change without proration, so `none` is refused there.

## Things that trip people up

- **A lower role number is more privileged.** `user.role <= 70` is "Basic or better".
- **A price that is no plan's grants nothing.** The subscription is still mirrored and logged, and
  its `pay_objects` row's **issue** says why: *its price price_x is no plan's*. Fix the plan row
  (or the binding), and the next sweep — or `pay_sync`, now — grants the role.
- **A role above the module's ceiling** is refused the same way, with the issue *may only grant
  roles 50 to 99*. Raise the ceiling on the Modules tab, then run `pay_sync`.
- **The trial is on `login`, not on a new `users` row.** Accounts are not made by a row insert a
  trigger can see; signing up signs in, and `once` keeps it to one trial.
- **Past due is not canceled.** The provider is still trying the card; the user keeps their plan
  for the grace period after the end of what they paid for, and longer if the payment is
  recovered.
- **The test provider forgets** its subscriptions on a restart, as it forgets checkouts. The
  mirror and the grants are kept, and run out on their own dates.
