// The module's webhooks (TODO-payments §6.5, P2.2), served by the server at
// `/_feldspar/hooks/payments/<name>/<rest>`:
//
// - `events/<account name>`: what each account's provider posts (§6.5). The signature is checked
//   over the raw body by the account's adapter; a delivery that does not verify is a 400 and
//   stores nothing. What verified is processed (`processing.js`): each object's current state is
//   fetched into the mirror and the normalised `pay_events` rows inserted, once each. A failure
//   there throws, which the server answers as a 500 — logged, and retried by the provider.
// - `fake/<account name>/checkout/<id>[/pay|/fail|/cancel]`,
//   `fake/<account name>/portal/<session>[/<subscription>/<action>]` and
//   `fake/<account name>/events/<event id>/resend`: the fake provider's checkout page, its
//   customer portal (with its test clock), and its webhook log, for accounts on the `fake`
//   adapter only.

"use strict";

const { usableAccount } = require("./accounts.js");
const fake = require("./adapters/fake.js");
const { processEvents } = require("./processing.js");
const plans = require("./plans.js");

/** `rest` cut into its path segments. */
const segments = (rest) => String(rest || "").split("/").filter(Boolean);

const json = (status, body) => ({ status, body });

/** `POST events/<account name>`. */
async function events(req, ctx) {
  const [name] = segments(req.rest);
  if (!name) return json(404, { error: "the URL names no payment account: …/events/<account name>" });
  let found;
  try {
    found = await usableAccount(ctx.db, name);
  } catch (e) {
    return json(404, { error: e.message });
  }
  let received;
  try {
    received = await found.adapter.verifyWebhook(found.account, { rawBody: req.rawBody, headers: req.headers });
  } catch (e) {
    ctx.log.warn(`payments: a webhook for "${name}" was refused: ${e.message}`);
    return json(400, { error: e.message });
  }
  const results = await processEvents(ctx, found.account, found.adapter, received);
  return json(200, { received: results });
}

/** A `deliver(rawBody, headers)` that hands a delivery to the `events` handler in this worker,
 * as the fake provider does: what a provider's HTTP request would reach, minus the HTTP. */
function deliverTo(account, ctx) {
  return async (rawBody, headers) => {
    const answer = await events({ method: "POST", headers, rawBody, rest: account.name, query: {} }, ctx);
    return answer.status;
  };
}

/** HTML-escaped text. */
const esc = (value) =>
  String(value === undefined || value === null ? "" : value).replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
  );

/** The origin of an absolute http(s) URL, or nothing for a path. */
function originOf(url) {
  try {
    const u = new URL(url);
    return u.protocol === "http:" || u.protocol === "https:" ? u.origin : null;
  } catch (_) {
    return null;
  }
}

/** The page's own policy: no script at all, its one stylesheet inline, and forms that post to
 * this server — whose answer then sends the buyer to the shop's success or cancel page. */
function pagePolicy(checkout) {
  const targets = [checkout.success_url, checkout.cancel_url].map(originOf).filter(Boolean);
  return [
    "default-src 'none'",
    "style-src 'unsafe-inline'",
    `form-action 'self' ${[...new Set(targets)].join(" ")}`.trim(),
    "base-uri 'none'",
    "frame-ancestors 'none'",
  ].join("; ");
}

/** The checkout page: what is being paid for, the three buttons while it is open, and what
 * became of the webhooks. */
function page(path, view, base) {
  const { checkout, payment, deliveries, total } = view;
  const rows = checkout.lines
    .map((l) => `<tr><td>${esc(l.description)}</td><td>${l.quantity}</td><td>${esc(l.amount)}</td></tr>`)
    .join("");
  const button = (action, label) =>
    `<form method="post" action="${esc(`${path}/${action}`)}"><button type="submit">${label}</button></form>`;
  const open = checkout.status === "open";
  const declined = checkout.declined
    ? `<p class="declined">The card was declined${checkout.declined > 1 ? ` ${checkout.declined} times` : ""}. Try again.</p>`
    : "";
  const outcome = (d) => (d.status === 200 ? "delivered" : d.status === null ? "held" : `answered ${esc(d.status)}`);
  const resend = (d) =>
    `<form method="post" action="${esc(`${base}/events/${d.event_id}/resend`)}"><button type="submit">Resend</button></form>`;
  const sent = deliveries.length
    ? `<h2>Webhooks</h2><ul>${deliveries
        .map((d) => `<li>${esc(d.type)}: ${outcome(d)}${d.error && d.status !== null ? ` (${esc(d.error)})` : ""} ${resend(d)}</li>`)
        .join("")}</ul>`
    : "";
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Test checkout</title>
<style>body{font-family:system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 1rem}
.notice{background:#fff3cd;padding:.5rem 1rem;border-radius:.25rem}.declined{color:#b00020}
table{width:100%;border-collapse:collapse}td{padding:.25rem 0}form{display:inline-block;margin-right:.5rem}
button{font-size:1rem;padding:.5rem 1rem}</style></head><body>
<p class="notice">A <strong>test</strong> checkout: no money moves. It is the fake payment provider of this server.</p>
<h1>${esc(total)}</h1>
<table>${rows}</table>
<p>Status: <strong id="status">${esc(checkout.status)}</strong>${payment ? ` · payment ${esc(payment.status)}` : ""}</p>
${declined}
${open ? `${button("pay", "Pay")}${button("fail", "Fail (card declined)")}${button("cancel", "Cancel")}` : ""}
${sent}
</body></html>`;
}

/** What each button of the portal does: the adapter's call, and its arguments. */
const PORTAL_ACTIONS = {
  change: (account, sub, form, deliver) => fake.changePlan(account, sub, form.get("price"), {}, deliver),
  cancel: (account, sub, form, deliver) => fake.cancel(account, sub, { at_period_end: true }, deliver),
  cancel_now: (account, sub, form, deliver) => fake.cancel(account, sub, { at_period_end: false }, deliver),
  renew: (account, sub, form, deliver) => fake.advance(account, sub, "succeeded", deliver),
  renew_fail: (account, sub, form, deliver) => fake.advance(account, sub, "failed", deliver),
  retry: (account, sub, form, deliver) => fake.retry(account, sub, "succeeded", deliver),
  retry_fail: (account, sub, form, deliver) => fake.retry(account, sub, "failed", deliver),
};

/** A posted form's fields: form-encoded, or JSON. */
function formOf(req) {
  const text = Buffer.from(req.rawBody || "").toString("utf8");
  if (String((req.headers || {})["content-type"] || "").includes("json")) {
    try {
      return new Map(Object.entries(JSON.parse(text || "{}")).map(([k, v]) => [k, String(v)]));
    } catch (_) {
      return new Map();
    }
  }
  return new Map(new URLSearchParams(text));
}

/** The prices the portal offers for a plan change: each plan's price for this account, from the
 * plan binding, when the module's settings have one. */
async function planChoices(ctx, account) {
  const binding = plans.planBinding(ctx.configuration);
  const field = plans.priceField(binding, account.name);
  if (!field) return [];
  try {
    const rows = await ctx.db[binding.table].rows();
    return rows.map((r) => r[field]).filter((v) => typeof v === "string" && v);
  } catch (_) {
    return [];
  }
}

/** What each of the portal's card buttons makes the customer's saved card. */
const CARD_LABELS = { ok: "Pays", authenticate: "Asks to authenticate", decline: "Is declined", none: "Remove it" };

/** The portal page: the customer's subscriptions, with what the customer can do to each, and
 * the test clock's buttons beside them; and the saved card, with buttons that change what it
 * does when it is charged without a page. */
function portalPage(path, view, choices) {
  const { session, customer, subscriptions } = view;
  const button = (sub, action, label) =>
    `<form method="post" action="${esc(`${path}/${sub.id}/${action}`)}"><button type="submit">${label}</button></form>`;
  const change = (sub) => {
    const input = choices.length
      ? `<select name="price">${choices.map((c) => `<option${c === sub.price ? " selected" : ""}>${esc(c)}</option>`).join("")}</select>`
      : `<input name="price" value="${esc(sub.price)}" aria-label="Price">`;
    return `<form method="post" action="${esc(`${path}/${sub.id}/change`)}">${input} <button type="submit">Change plan</button></form>`;
  };
  const date = (seconds) => (seconds ? new Date(seconds * 1000).toISOString().slice(0, 10) : "");
  const items = subscriptions
    .map((sub) => {
      const live = sub.status !== "canceled";
      const ends = sub.cancel_at_period_end ? `ends ${date(sub.current_period_end)}` : `renews ${date(sub.current_period_end)}`;
      const customerButtons = live
        ? `${change(sub)}${sub.cancel_at_period_end ? "" : button(sub, "cancel", "Cancel at period end")}${button(sub, "cancel_now", "Cancel now")}`
        : "";
      const clock = live
        ? `<p class="clock">Test clock: ${button(sub, "renew", sub.status === "trialing" ? "End the trial (pay)" : "Renew (pay)")}${button(
            sub,
            "renew_fail",
            sub.status === "trialing" ? "End the trial (card declined)" : "Renew (card declined)",
          )}${sub.status === "past_due" ? `${button(sub, "retry", "Retry (pay)")}${button(sub, "retry_fail", "Retry (declined)")}` : ""}</p>`
        : "";
      return `<li><strong>${esc(sub.price)}</strong> × ${esc(sub.quantity)} · ${esc(sub.amount_text)} · <span class="status">${esc(sub.status)}</span>${
        live ? ` · ${esc(ends)}` : ""
      }${customerButtons ? `<div>${customerButtons}</div>` : ""}${clock}</li>`;
    })
    .join("");
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Test customer portal</title>
<style>body{font-family:system-ui,sans-serif;max-width:40rem;margin:3rem auto;padding:0 1rem}
.notice{background:#fff3cd;padding:.5rem 1rem;border-radius:.25rem}.clock{color:#555}
li{margin-bottom:1rem}form{display:inline-block;margin:.25rem .5rem .25rem 0}button{font-size:1rem;padding:.4rem .8rem}</style></head><body>
<p class="notice">A <strong>test</strong> customer portal: no money moves. The test clock moves a subscription to the end of its period.</p>
<h1>${esc(customer.email || customer.id)}</h1>
<ul>${items || "<li>No subscriptions.</li>"}</ul>
<h2>Saved card</h2>
<p>When charged without a page: <strong class="card">${esc(customer.card ? CARD_LABELS[customer.card] : "no saved card")}</strong></p>
<p class="clock">${Object.entries(CARD_LABELS)
    .map(([card, label]) => `<form method="post" action="${esc(`${path}/card/${card}`)}"><button type="submit">${esc(label)}</button></form>`)
    .join("")}</p>
${session.return_url ? `<p><a href="${esc(session.return_url)}">Back</a></p>` : ""}
</body></html>`;
}

/** `fake/<account>/portal/<session>[/<subscription>/<action>]`. */
async function fakePortal(req, ctx, account, id, subId, action, base) {
  const path = `${base}/portal/${id}`;
  const view = fake.portal(account, id);
  if (!view) return json(404, { error: `there is no test portal session ${id}` });
  if (req.method === "GET") {
    if (subId) return json(405, { error: "press the button: the portal's actions are posted" });
    if (req.query.format === "json") return json(200, view);
    return {
      status: 200,
      headers: {
        "content-type": "text/html; charset=utf-8",
        "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        "cache-control": "no-store",
      },
      body: portalPage(path, view, await planChoices(ctx, account)),
    };
  }
  if (subId === "card") {
    if (!(action in CARD_LABELS)) return json(404, { error: `a saved card is made ${Object.keys(CARD_LABELS).join(", ")}` });
    const changed = fake.setCard(account, view.session.customer, action === "none" ? null : action);
    if (req.query.format === "json") return json(200, changed);
    return { status: 303, headers: { location: path, "cache-control": "no-store" } };
  }
  const run = PORTAL_ACTIONS[action];
  if (!subId || !run) return json(404, { error: `"${action}" is not something the portal does; it does ${Object.keys(PORTAL_ACTIONS).join(", ")}` });
  if (!view.subscriptions.some((s) => s.id === subId)) return json(404, { error: `the subscription ${subId} is not this customer's` });
  let subscription;
  try {
    subscription = await run(account, subId, formOf(req), deliverTo(account, ctx));
  } catch (e) {
    return json(409, { error: e.message });
  }
  if (req.query.format === "json") return json(200, subscription);
  return { status: 303, headers: { location: path, "cache-control": "no-store" } };
}

/** `fake/<account>/checkout/<id>[/<action>]`, `fake/<account>/portal/…` and
 * `fake/<account>/events/<id>/resend`. */
async function fakeProvider(req, ctx) {
  const [name, what, id, action, more] = segments(req.rest);
  if (!name || !["checkout", "events", "portal"].includes(what) || !id) return json(404, { error: "no such page of the test provider" });
  let found;
  try {
    found = await usableAccount(ctx.db, name);
  } catch (e) {
    return json(404, { error: e.message });
  }
  const { account, adapter } = found;
  if (adapter !== fake) return json(404, { error: `"${account.name}" is not an account of the test provider` });
  const base = `/_feldspar/hooks/payments/fake/${encodeURIComponent(account.name)}`;
  const path = `${base}/checkout/${id}`;
  const back = (location) => ({ status: 303, headers: { location, "cache-control": "no-store" } });
  if (what === "portal") return fakePortal(req, ctx, account, id, action, more, base);

  if (what === "events") {
    if (req.method !== "POST" || action !== "resend") return json(405, { error: "an event is resent with POST …/events/<id>/resend" });
    const tried = await fake.resend(account, id, deliverTo(account, ctx));
    if (!tried) return json(404, { error: `the test provider has no event ${id} for "${account.name}"` });
    if (req.query.format === "json") return json(200, tried);
    const event = fake.events(account).find((e) => e.id === id);
    const checkout = event && event.ref && (event.ref.kind === "checkout" ? event.ref.id : fake.checkoutOf(account, event.ref));
    return checkout ? back(`${base}/checkout/${checkout}`) : json(200, tried);
  }

  if (req.method === "GET") {
    if (action) return json(405, { error: "press the button: the page's actions are posted" });
    const view = fake.view(account, id);
    if (!view) return json(404, { error: `there is no test checkout ${id}` });
    if (req.query.format === "json") return json(200, view);
    return {
      status: 200,
      headers: {
        "content-type": "text/html; charset=utf-8",
        "content-security-policy": pagePolicy(view.checkout),
        "cache-control": "no-store",
      },
      body: page(path, view, base),
    };
  }

  if (!fake.view(account, id)) return json(404, { error: `there is no test checkout ${id}` });
  if (action === "cancel") return back(fake.cancelUrl(account, id));
  if (action !== "pay" && action !== "fail") return json(404, { error: `"${action}" is not something a checkout does` });
  // Deliver each event as the provider would: the raw body and its signature, into the
  // `events` handler, which verifies it like any other.
  const checkout = await fake.settle(account, id, action === "pay" ? "succeeded" : "failed", deliverTo(account, ctx));
  return back(checkout.status === "complete" ? checkout.success_url : path);
}

module.exports = { events, deliverTo, fakeProvider, page, pagePolicy, portalPage };
