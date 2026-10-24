# Tutorial: Payments — part 1, a shop

This tutorial builds a small tea shop that takes payments. Buyers can browse without an account,
keep a basket that survives a reload, pay on a provider's checkout page, and download what they
bought. The shop's own triggers mark the order paid and take the stock down.

The shop's data is yours: its products, orders and order lines are ordinary tables you make,
in the shape your business needs. The bundled **Payments** module brings only what it cannot do
without — the provider accounts, a mirror of the provider's objects, and a log of what
happened — and it never writes your tables. Your triggers react to its log instead.

You will start on the module's **test provider** (`fake`), which runs inside your server and
needs no account anywhere, and switch to Stripe at the end.

This continues from [tutorial-react-todo.md](tutorial-react-todo.md) (an application with a React
front end), [tutorial-ownership.md](tutorial-ownership.md) (who may read and write which rows) and
[tutorial-triggers.md](tutorial-triggers.md) (triggers and the row actions). The server needs
**npm**, which installs modules ([tutorial-modules.md](tutorial-modules.md)).

## The first lesson: where the prices live

Everything a buyer pays is worked out **on the server**, by formulas over your rows. The browser
only says *which* order to pay for. That makes the shop exactly as safe as the fields those
formulas read:

- `productⱵprice * quantity` on an order line is safe: the buyer cannot write `products`.
- A `price` column on the order line is not, because the buyer writes their own lines. Whatever
  the page shows, a buyer with the browser's developer tools could set it to 1.

So **an order line has no price column**, and the product table is not writable by buyers. Keep
that in mind for every table below. The amount charged is recorded on every payment event, so a
trigger can compare it with what you expected.

## Step 1 — Install the module and add a test account

**Modules → Modules that ship with Saltcorn → Payments → Install.** The card lists what installing
does: three tables (`pay_accounts`, `pay_objects`, `pay_events`), two webhooks, a background
service, and permission to connect to `api.stripe.com` and nothing else.

Then add an account. Open **Data → pay_accounts** and add a row:

| Field | Value |
|---|---|
| Name | `shop` |
| Adapter | `fake` |
| Mode | `test` |
| Webhook signing secret | any random text, such as `k7e2…` |

The name is part of the account's webhook URL, so it is letters, digits, `-` and `_`. The keys are
**secret fields**: stored encrypted, and never shown back to anybody, the admin included.

To see the account work before there is a shop, make a trigger with event **None** and the action
**`pay_account_test`**, and run it with `{ "account": "shop" }`. It answers a checkout URL; open
it, and you get the test provider's page with **Pay**, **Fail (card declined)** and **Cancel**
buttons.

## Step 2 — The shop's tables

Make five tables. Every field not marked otherwise is nullable.

**`products`** — what the shop sells. **Minimum role to read: public**, to write: admin.

| Field | Type | |
|---|---|---|
| `id` | int, primary key | |
| `name` | text, required | |
| `price` | int, required | in cents: `450` is 4.50 |
| `stock` | int | empty for something that does not run out, such as a download |

Amounts are always **whole numbers of the currency's smallest unit** — cents, pence, yen. Never
a float: 0.1 + 0.2 is not 0.3, and a shop must not be off by a cent.

**`product_files`** — the files a product delivers. Admin only, to read and to write. Its `file`
is a **File** field in a file store of its own (`goods` here). It is a separate table because
`products` is public: a File field on `products` would be downloadable by anybody.

| Field | Type |
|---|---|
| `id` | int, primary key |
| `product` | Key to `products`, required |
| `file` | File, in the store `goods` |

**`orders`** — a basket, until it is paid. Minimum roles admin and admin, so only the ownership
formula lets a buyer in.

| Field | Type | Default value |
|---|---|---|
| `id` | int, primary key | |
| `user_id` | Key to `users` | `user && user.id` |
| `session_id` | text | `session && session.id` |
| `status` | text, required | `"open"` |
| `email` | text | |

A signed-in buyer's order is theirs by `user_id`. A buyer who is not signed in has a **guest
session** (Step 3), and their order is theirs by `session_id`. Each default gives a value for one
kind of buyer and nothing for the other, which leaves the field empty — so both are nullable.

Ownership formula:

```
(user ? user_id === user.id : session_id === session.id) && (_read || status === "open")
```

The first half says whose it is. The second says it can be read always, but written only while
it is open: a buyer cannot set their own order's status to `paid`, because a write is checked
against the row as it would become.

**`order_lines`** — what is in a basket. Admin and admin, and the same rule through the order:

| Field | Type | Default value |
|---|---|---|
| `id` | int, primary key | |
| `order` | Key to `orders`, required | |
| `product` | Key to `products`, required | |
| `quantity` | int, required | `1` |

```
(user ? orderⱵuser_id === user.id : orderⱵsession_id === session.id) && (_read || orderⱵstatus === "open")
```

No price column (see the first lesson).

**`downloads`** — what a buyer may download once they have paid. Admin and admin; only your
triggers write it, and a buyer reads their own:

| Field | Type |
|---|---|
| `id` | int, primary key |
| `order` | Key to `orders`, required |
| `product` | Key to `products`, required |
| `file` | File, in the store `goods` |

```
_read && (user ? orderⱵuser_id === user.id : orderⱵsession_id === session.id)
```

Downloading a file through the API is a read of its row, so this rule is also who may download
it.

## Step 3 — The application

Make the React application of [tutorial-react-todo.md](tutorial-react-todo.md), named `shop`, and
on its page:

- **Tables:** tick `products`, `orders`, `order_lines` and `downloads`.
- **Guest sessions:** switch them on. A visitor who is not signed in can then start a guest
  session, and `session.id` in the formulas above is theirs. The cookie is set on the shop's own
  host and lasts 30 days, so the basket survives a reload and a restart of the browser.
- **Streams:** make a `table_changes` stream on `order_lines` (**Streams → New stream**), named
  `order_lines_live`, with **Minimum role to observe** public, and tick it. A change is delivered
  only to whoever may read the row, so a guest's socket hears about their own basket and nobody
  else's.

Now the checkout. Make a trigger named `checkout`, with event **None**, minimum role **public**,
and the action **`pay_checkout`**:

| Setting | Value | |
|---|---|---|
| Payment account | `shop` | |
| Mode | `payment` | |
| Table | `orders` | the row being paid for |
| Which row | `payload` | the caller sends its key… |
| Payload field | `order` | …as `{ order: 12 }` |
| The caller must be able to read the row | ticked | |
| Lines | `children` | a line per child row |
| Child table | `order_lines` | |
| Its key to the row | `order` | |
| Quantity | `quantity` | |
| Unit amount | `productⱵprice` | read from the product, which the buyer cannot write |
| Line description | `productⱵname` | |
| Item | `product` | |
| Currency | `EUR` | |
| Email | `email` | for a guest; a signed-in buyer's own address is used otherwise |
| Tax | `none` | |
| After paying | `/orders/{key}` | a path on the shop; `{key}` is the order's id |
| After cancelling | `/basket` | |

Tick `checkout` in the application's **Triggers** and build. The generated client now has
`api.startGuestSession()` and `api.runCheckout(payload)`, and `src/feldspar/` has the hooks for
the four tables.

**Nobody can pay for an order they cannot read.** "The caller must be able to read the row" reads
the order as the caller, so another guest's order — or one that does not exist — is refused with
the same sentence: *there is no orders row 12 that you can read*. An amount in the payload is not
read at all.

### The basket

```tsx
// src/pages/Shop.tsx
import { api, invalidate, useProducts, useOrders } from "../feldspar/hooks";
import { useLiveRows } from "../feldspar/live-react";
import { useUser } from "../auth";

export function Shop() {
  const { user } = useUser();
  const { data: products = [] } = useProducts();
  const { data: orders = [] } = useOrders();
  const basket = orders.find((o) => o.status === "open");
  const { rows: lines } = useLiveRows(api.order_lines, {
    where: { order: `eq.${basket?.id ?? 0}` },
  });

  async function add(product: number) {
    let order = basket;
    if (!order) {
      // A visitor gets a guest session when the basket gets its first item — never just for
      // looking, so a crawler makes no rows.
      if (!user) await api.startGuestSession();
      order = await api.orders.create({ email: "" });
      invalidate("orders");
    }
    await api.order_lines.create({ order: order.id, product, quantity: 1 });
  }

  async function checkout() {
    const { url } = await api.runCheckout({ order: basket!.id });
    location.href = url;
  }

  const price = (id: number) => products.find((p) => p.id === id)?.price ?? 0;
  const total = lines.reduce((sum, l) => sum + l.quantity * price(l.product), 0);
  return (
    <>
      <ul>
        {products.map((p) => (
          <li key={p.id}>
            {p.name} — {(p.price / 100).toFixed(2)} € <button onClick={() => add(p.id)}>Add</button>
          </li>
        ))}
      </ul>
      <h2>Basket: {(total / 100).toFixed(2)} €</h2>
      <button disabled={!lines.length} onClick={checkout}>Checkout</button>
    </>
  );
}
```

The total shown is the page's own arithmetic, for the buyer's information. What is charged is
`pay_checkout`'s, from the same products, on the server.

Press **Checkout**: the test provider's page shows the lines and their total. **Fail** shows what
a declined card looks like — the page says so and the order stays open. **Pay** sends the buyer
to `/orders/12`.

### The page they come back to

The guest session's cookie is `SameSite=Strict`, like a signed-in session's. Coming back from a
provider's page is a navigation from another site, so **that first page load arrives without the
cookie**. The page's own API calls are same-site and carry it. So the success page loads the order
through the API, not from anything the server rendered:

```tsx
// src/pages/Order.tsx
import { useParams } from "react-router-dom";
import { useOrders, useDownloads } from "../feldspar/hooks";

export function Order() {
  const { id } = useParams();
  const { data: orders = [] } = useOrders();
  const { data: downloads = [] } = useDownloads();
  const order = orders.find((o) => String(o.id) === id);
  if (!order) return <p>Loading…</p>;
  return (
    <>
      <h1>Order {order.id}: {order.status}</h1>
      {downloads
        .filter((d) => d.order === order.id)
        .map((d) => (
          <a key={d.id} href={`/api/downloads/${d.id}/file`}>Download</a>
        ))}
    </>
  );
}
```

## Step 4 — When the payment succeeds: your triggers

Every time something happens at the provider, the module inserts a row into **`pay_events`**,
in its own words: `kind` is `payment.succeeded`, `payment.failed`, `checkout.completed`,
`payment.refunded` and so on, `ref_table` and `ref_key` name the row it was for, and `amount`
and `currency` say what was charged. It is processed once, however often the provider sends it
and in whatever order. **This is where your triggers go.**

**Mark the order paid** — a trigger named `order_paid`:

| Setting | Value |
|---|---|
| Event | `A row is inserted`, table `pay_events` |
| Only if | `row.kind === "payment.succeeded" && row.ref_table === "orders"` |
| Action | `update_rows` |
| Table | `orders` |
| Where | `id === row.ref_key` |
| Assignments | `{ "status": "\"paid\"" }` |

`ref_key` holds the key as the row has it — a number here, text or a UUID for a table keyed that
way — so `id === row.ref_key` finds the row whatever its key's type.

**What was paid for is what the order holds.** The order stays `open` until it is paid, so the
buyer can still change its lines after the checkout was made — and a provider's checkout page
stays payable for up to a day. So when the payment succeeds, the module works the lines out again
by the checkout's own settings and compares them, and the payment's amount, with what was charged.
Only a match is `payment.succeeded`. A basket changed in between is logged as
**`payment.mismatch`** instead, with the payment's `issue` saying why: none of the triggers here
runs, and the order stays open for you to settle by hand (refund it, or ask the buyer to pay the
difference). Either event carries `lines`, the lines as charged — `item` is the product here — if
you would rather deliver from what was paid for than from the rows.

**Take the stock down** — `take_stock`, the same event and *only if*:

| Setting | Value |
|---|---|
| Action | `update_rows` |
| Table | `products` |
| Where | `stock !== null && order_linesↃproduct.some(l => l.order === row.ref_key)` |
| Assignments | `{ "stock": "stock - order_linesↃproduct.filter(l => l.order === row.ref_key).sum(\"quantity\")" }` |

`order_linesↃproduct` is every order line that names this product; the filter keeps this order's.
A product with no stock count (a download) is left alone.

Nothing here needs code, and the admin sees everything that happens to the shop's data in the
shop's own trigger list.

## Step 5 — Delivering a download

The guide is a product too: add it to `products` with no stock, put its file in the `goods` store,
and add a `product_files` row naming the product and the file.

When an order is paid, each of its products that has a file becomes a `downloads` row — a
trigger named `deliver_downloads`, on the same event and *only if* as Step 4, with the action
**`run_js_code`**:

```js
const lines = await db.order_lines.where({ order: row.ref_key }).rows();
for (const line of lines) {
  const download = await db.product_files.where({ product: line.product }).first();
  if (download) {
    await db.downloads.insert({ order: line.order, product: line.product, file: download.file });
  }
}
```

The buyer — and only the buyer — now reads the row, and downloads the file from
`/api/downloads/<id>/file`. Another guest's download of the same URL is refused.

Then tell them — `email_download`, on **A row is inserted** into `downloads`, with the action
**`send_email`**:

| Setting | Value |
|---|---|
| To | `{{ orderⱵemail }}` |
| Subject | `Your download of {{ productⱵname }}` |
| Text | `Thank you for your order. Your download is on its page: https://shop.example.com/orders/{{ order }}` |

The link opens the order's page in the browser the buyer ordered from. A guest who opens it on
another device has no guest session there; if your shop needs that, put a random token on the
order, send it in the email, and write a code query that reads one order by its token.

## Step 6 — Signing in moves the guest's orders to the account

A guest who signs up, or signs in, **in the same browser** keeps what they bought. The `login`
event's payload carries the guest session's id as `guest_session`, and the guest session ends.
A trigger moves the orders — `adopt_orders`:

| Setting | Value |
|---|---|
| Event | `A user logs in` |
| Action | `update_rows` |
| Table | `orders` |
| Where | `session_id !== null && session_id === payload.guest_session` |
| Assignments | `{ "user_id": "user.id" }` |

**Do not leave out `session_id !== null`.** A sign-in from a browser with no guest session has
no `guest_session` in its payload, and a field that is not there reads as null — so without the
first half, `session_id === payload.guest_session` would match every order with no session, which
is every signed-in buyer's, and give them all to whoever signed in.

Matching the guest session is safe where matching by email is not: holding the guest session's
cookie proves it is the same buyer, while a password sign-up does not prove the address is theirs.

## Step 7 — Real money: Stripe

Add a second `pay_accounts` row:

| Field | Value |
|---|---|
| Name | `stripe` |
| Adapter | `stripe` |
| Mode | `test` (then `live`) |
| Secret key | `sk_test_…` from Stripe's dashboard |
| Webhook signing secret | `whsec_…`, from the endpoint below |

In Stripe's dashboard, add a webhook endpoint at

```
https://<your admin host>/_feldspar/hooks/payments/events/stripe
```

— the last part is the account's name, and the module's card shows the URL with a copy button —
for the events `checkout.session.completed`, `payment_intent.succeeded`,
`payment_intent.payment_failed`, `payment_intent.processing`, `charge.refunded`,
`charge.dispute.created` and `charge.dispute.closed` (and, if you sell subscriptions as in
[part 2](tutorial-payments-saas.md), `customer.subscription.created`,
`customer.subscription.updated`, `customer.subscription.deleted` and `invoice.paid`). Then set
the `checkout` trigger's
**Payment account** to `stripe`. Nothing else changes: the same `pay_events` rows arrive, and the
same triggers react to them.

A test key in live mode, or a live key in test mode, is refused by name, and so is a webhook from
the other mode. Stripe's return addresses must be full URLs, so a checkout has to be started
through the application (which it is, from the shop).

The checkout's other settings, on Stripe:

- **Tax** `provider` turns on Stripe Tax.
- **Collect a shipping address**, with the countries you ship to, for physical goods.
- **Tax** `merchant_of_record` makes Stripe the seller through *Managed Payments*: it collects and
  pays sales tax and VAT everywhere, for a higher fee. Turn it on in the account's **Settings**
  (`{ "managed_payments": true }`). Merchants of record sell digital goods only, so it is refused
  with shipping.

The module can also **suggest** what an application's pages need allowed to embed Stripe's
payment form instead of redirecting: switch on **@feldspar/payments: Stripe's embedded payment
form** under the application's *Sources suggested by modules*. Hosted checkout, as above, needs
nothing.

### Or Polar, a merchant of record

[Polar](https://polar.sh) is the seller in law: it adds and pays sales tax and VAT wherever the
buyer is, and handles disputes, for a higher fee than Stripe's. It sells **digital goods only**,
so it suits the shop's downloads but not its tea: a checkout that collects a shipping address is
refused on a Polar account, and the **Tax** setting changes nothing there, because Polar always
does the tax.

Polar charges for a *product*, not for lines. Make one **one-time product** in Polar for the
shop's checkouts (its name, say "Order", is what the buyer sees), then add the account:

| Field | Value |
|---|---|
| Name | `polar` |
| Adapter | `polar` |
| Mode | `test` (Polar's sandbox, sandbox.polar.sh), then `live` |
| Secret key | an organization access token, from Polar's settings |
| Webhook signing secret | the secret shown on the endpoint below |
| Settings | `{ "product": "<the product's id>", "webhook_endpoint_id": "<the endpoint's id>" }` |

Each checkout charges that product at the basket's total, worked out by the same formulas. The
lines stay on the module's `pay_objects` row as they were charged; Polar's page shows the total.

In Polar's settings, add a webhook endpoint at `…/_feldspar/hooks/payments/events/polar` for
`checkout.updated`, `order.paid` and `order.refunded` (and, for subscriptions, the `subscription.*`
events). The endpoint's id in the account's settings lets the sweep (Step 9) read back what Polar
failed to deliver.

The amount on a Polar `pay_events` row is what the buyer paid, **tax included**, so with tax it is
more than the order's total. A refund of an amount is of what the buyer paid, too; Polar gives the
tax back with it.

## Step 8 — Refunds, and anything else the provider does

**`pay_refund`** gives money back. Configure it with the table the payment was for (`orders`,
payload field `order`), and it refunds that order's latest payment — all of it, or an **Amount**
formula's worth. A repeated click before the provider has answered is the same refund, not a
second one. The provider then reports it like everything else, as a `payment.refunded` event,
which a trigger can react to (`status: "refunded"`). Give the trigger an admin's minimum role,
unless your buyers may refund themselves.

For anything the actions do not cover, there are two ways to the provider's whole API, both with
the account's keys and neither showing them:

- **`pay_api`**, a module function, from any code body:

  ```js
  const balance = await modfn.pay_api("stripe", "GET", "/v1/balance");
  ```

- **`pay_provider_code`**, an action whose setting is a JavaScript body run beside the module,
  with the account's client in scope as `stripe`:

  ```js
  const coupon = await stripe.post("/v1/coupons", { percent_off: 10, duration: "once" });
  return coupon.id;
  ```

Both reach only `api.stripe.com`: that is all the module may connect to.

## Step 9 — When a webhook never arrives

Providers retry a webhook that fails, for days. One can still be lost — a server down for longer,
a misconfigured endpoint. The module's **reconcile** service asks each account's provider, every
three hours, for the events since it last asked, and processes the ones it has not seen. A
duplicate does nothing, so this is safe to run any time: **`pay_sync`** runs it now, for one
account (`{ "account": "stripe" }`) or for all.

The test provider can show it. Set the `shop` account's **Settings** to
`{ "webhooks": "hold" }`, and paying delivers nothing — the page lists each event as *held*, with
a **Resend** button. Run `pay_sync`, and the order is paid. Resend an event twice, or in the
wrong order, and nothing happens twice.

## Things that trip people up

- **A price the buyer can write is a price the buyer chooses.** Read prices through a Key into a
  table only the admin writes (the first lesson).
- **The success page arrives without the guest cookie.** Load the order through the API from the
  page (Step 3).
- **`session_id !== null`** in the sign-in trigger (Step 6).
- **The test provider forgets** on a restart: its checkouts live in the server's memory. A
  checkout started before a restart cannot be paid after it.
- **The account's name is in the webhook URL**, so renaming an account means changing the URL
  registered at the provider.
- **Amounts are whole numbers.** `19.99` is refused by name, wherever it comes from. Currencies
  without minor units (JPY) are whole units; three-decimal ones (KWD) are thousandths.
- **A payment that is not your row's** — made in the provider's dashboard, say — is still logged
  in `pay_events`, with an empty `ref_table`. Events the module does not know are logged as
  `other`, with the provider's raw event, for a trigger that wants them.

## Next

[Part 2](tutorial-payments-saas.md) sells subscriptions: plans in a table of your own, a role for
each, a free tier, and a trial without a card.
