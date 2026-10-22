// The module's webhooks (TODO-payments §6.5, P2.2), served by the server at
// `/_feldspar/hooks/payments/<name>/<rest>`:
//
// - `events/<account name>`: what each account's provider posts (§6.5). The signature is checked
//   over the raw body by the account's adapter; a delivery that does not verify is a 400 and
//   stores nothing. What verified is processed (`processing.js`): each object's current state is
//   fetched into the mirror and the normalised `pay_events` rows inserted, once each. A failure
//   there throws, which the server answers as a 500 — logged, and retried by the provider.
// - `fake/<account name>/checkout/<id>[/pay|/fail|/cancel]` and
//   `fake/<account name>/events/<event id>/resend`: the fake provider's checkout page and its
//   webhook log, for accounts on the `fake` adapter only.

"use strict";

const { usableAccount } = require("./accounts.js");
const fake = require("./adapters/fake.js");
const { processEvents } = require("./processing.js");

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
  const results = await processEvents(ctx.db, found.account, found.adapter, received);
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

/** `fake/<account>/checkout/<id>[/<action>]` and `fake/<account>/events/<id>/resend`. */
async function fakeProvider(req, ctx) {
  const [name, what, id, action] = segments(req.rest);
  if (!name || (what !== "checkout" && what !== "events") || !id) return json(404, { error: "no such page of the test provider" });
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

module.exports = { events, deliverTo, fakeProvider, page, pagePolicy };
