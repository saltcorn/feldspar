// The **fake** adapter (TODO-payments §3.14, P2.2): a payment provider inside this server.
//
// It is what tests use, and what an admin or a coding agent uses to build an application's
// payment screens before anybody has a provider account. It behaves like a hosted checkout:
//
// - `createCheckout` answers a URL — the fake's own **checkout page**, served by the module's
//   `fake` webhook on the calling application's host, with Pay, Fail and Cancel buttons;
// - paying makes a payment, and the fake **delivers signed webhooks** for what happened to the
//   module's own `events` handler, exactly as a provider would: a raw body, a signature header,
//   and the same `verifyWebhook` on the way in. It delivers in-process rather than over HTTP, so
//   it needs no network permission and no address for this server; what it records of each
//   delivery (the status the handler answered) is shown on its page, as a provider's dashboard
//   shows its webhook attempts;
// - `fetch` answers an object's current state, which is what the webhook handler reads (§3.6).
//
// Its objects live in this worker's memory. A restart of the server, or of the module's worker,
// forgets them — which a test provider may.
//
// It takes no real money, so an account using it cannot be in live mode (`testOnly`).

"use strict";

const crypto = require("node:crypto");
const { currencyCode, requireAmount, format } = require("../money.js");

/** The header the signature arrives in, Stripe's shape: `t=<unix seconds>,v1=<hex HMAC>`. */
const SIGNATURE_HEADER = "fake-signature";

/** How old a signed delivery may be before it is refused as a replay. */
const TOLERANCE_SECONDS = 300;

/** The objects, by id. Every one records the account it belongs to. */
const state = {
  checkouts: new Map(),
  payments: new Map(),
  /** Refunds by id, and by the idempotency key they were asked for with. */
  refunds: new Map(),
  refundKeys: new Map(),
  /** Every event made, oldest first: `{ id, type, ref, account, created }`. */
  events: [],
  /** Every delivery attempt, oldest first: `{ event_id, type, account, status, at, error }`. */
  deliveries: [],
};

/** A random id with a provider-style prefix. */
const newId = (prefix) => `${prefix}_fake_${crypto.randomBytes(12).toString("hex")}`;

const now = () => Math.floor(Date.now() / 1000);

/** The object of `kind` and `id`, if it belongs to `account`. */
function lookup(account, kind, id) {
  const map = { checkout: state.checkouts, payment: state.payments, refund: state.refunds }[kind];
  const found = map && map.get(id);
  return found && found.account === account.name ? found : null;
}

/** A checkout in the module's own words (§4): the fields every adapter fills, and the fake's
 * whole record as `provider_data`. */
function normaliseCheckout(c) {
  return {
    kind: "checkout",
    ref: c.id,
    status: c.status,
    amount: c.amount_total,
    currency: c.currency,
    email: c.customer_email,
    payment_ref: c.payment,
    lines: c.lines,
    metadata: c.metadata,
    updated: c.updated,
    provider_data: { ...c },
  };
}

function normalisePayment(p) {
  return {
    kind: "payment",
    ref: p.id,
    status: p.status,
    amount: p.amount,
    currency: p.currency,
    email: p.customer_email,
    checkout_ref: p.checkout,
    amount_refunded: p.amount_refunded || 0,
    metadata: p.metadata,
    updated: p.updated,
    provider_data: { ...p },
  };
}

function normaliseRefund(r) {
  return {
    kind: "refund",
    ref: r.id,
    status: r.status,
    amount: r.amount,
    currency: r.currency,
    payment_ref: r.payment,
    updated: r.created,
    provider_data: { ...r },
  };
}

const NORMALISE = { checkout: normaliseCheckout, payment: normalisePayment, refund: normaliseRefund };

/** The lines a checkout charges: each `{ description, quantity, unit_amount }`, whole numbers in
 * the currency's smallest unit, and their total. */
function chargedLines(lines) {
  if (!Array.isArray(lines) || lines.length === 0) throw new Error("a checkout charges at least one line");
  let total = 0;
  const charged = lines.map((line, i) => {
    const quantity = requireAmount(line.quantity === undefined ? 1 : line.quantity, `line ${i + 1}'s quantity`);
    const unit = requireAmount(line.unit_amount, `line ${i + 1}'s unit amount`);
    const amount = quantity * unit;
    total += amount;
    if (!Number.isSafeInteger(total)) throw new Error("the checkout's total is too large to count exactly");
    return { description: String(line.description || `Line ${i + 1}`), quantity, unit_amount: unit, amount };
  });
  return { charged, total };
}

/** Where the checkout page of `id` is, under `base` — the calling application's URL, or nothing
 * for a path on whatever host the caller is on. */
function pageUrl(base, account, id) {
  return `${base || ""}/_feldspar/hooks/payments/fake/${encodeURIComponent(account.name)}/checkout/${id}`;
}

/** The signature over `body` at `t` with `secret`: `t=…,v1=…`. */
function sign(secret, body, t = now()) {
  const mac = crypto.createHmac("sha256", secret).update(`${t}.`).update(body).digest("hex");
  return `t=${t},v1=${mac}`;
}

/** An account's signing secret, or a refusal: an unsigned webhook is worth nothing. */
function signingSecret(account) {
  const secret = account.webhook_secret;
  if (typeof secret !== "string" || !secret) {
    throw new Error(
      `the payment account "${account.name}" has no webhook signing secret, so its webhooks ` +
        `cannot be checked; set one (any random text, for the fake adapter)`,
    );
  }
  return secret;
}

/** Whether the account holds its webhooks back (`settings.webhooks: "hold"`) rather than
 * delivering each as it happens: a test then delivers them by hand, in any order and as often as
 * it likes (`resend`), as a provider's retries and reorderings would. */
const holds = (account) => Boolean(account.settings && account.settings.webhooks === "hold");

/** Hand `event`'s body, signed now, to `deliver`, and record the attempt: what `deliver` answers
 * — the status the module's handler gave — or a throw, as a failed attempt. */
async function attempt(account, event, deliver) {
  const headers = { "content-type": "application/json", [SIGNATURE_HEADER]: sign(signingSecret(account), event.body) };
  const tried = { event_id: event.id, type: event.type, account: account.name, status: null, error: null, at: new Date().toISOString() };
  state.deliveries.push(tried);
  try {
    tried.status = await deliver(event.body, headers);
  } catch (e) {
    tried.status = 500;
    tried.error = e.message;
  }
  return tried;
}

/** Record an event about `ref` and deliver it — or, on an account that holds its webhooks,
 * record it as held. */
async function emit(account, type, ref, deliver) {
  const id = newId("evt");
  const created = now();
  const body = Buffer.from(JSON.stringify({ id, type, created, account: account.name, data: { object: ref } }));
  const event = { id, type, ref, account: account.name, created, body };
  state.events.push(event);
  if (holds(account)) {
    const held = { event_id: id, type, account: account.name, status: null, error: "held", at: new Date().toISOString() };
    state.deliveries.push(held);
    return held;
  }
  return attempt(account, event, deliver);
}

module.exports = {
  name: "fake",
  testOnly: true,
  capabilities: {
    one_off: true,
    adhoc_prices: true,
    subscriptions: false,
    trial_with_card: false,
    portal: false,
    charge_saved_method: false,
    native_sheet: false,
    manual_capture: false,
    async_methods: false,
    marketplace: false,
    merchant_of_record: "no",
    physical_goods: true,
  },
  settings: [],
  hosts: [],
  SIGNATURE_HEADER,
  sign,

  /** What `pay_provider_code` and `pay_api` get: the fake has no SDK, so this is its store. */
  client(account) {
    return {
      checkouts: { retrieve: (id) => module.exports.fetch(account, { kind: "checkout", id }) },
      payments: { retrieve: (id) => module.exports.fetch(account, { kind: "payment", id }) },
      refunds: { retrieve: async (id) => (lookup(account, "refund", id) ? { ...lookup(account, "refund", id) } : null) },
    };
  },

  /** A hosted checkout for `lines`, answered as `{ id, url, object }`. `base_url` is the calling
   * application's, where the checkout page is served; `success_url` and `cancel_url` are where
   * the page sends the buyer, and default to the page itself. */
  async createCheckout(account, input) {
    if ((input.mode || "payment") !== "payment") {
      throw new Error(`the fake adapter takes one-off payments only, not mode "${input.mode}"`);
    }
    // Refused now rather than at the first delivery: a checkout whose webhooks cannot be
    // signed would be paid and never heard of.
    signingSecret(account);
    if (input.tax && input.tax !== "none") {
      throw new Error(`the fake adapter calculates no tax, so a checkout on "${account.name}" cannot ask for tax "${input.tax}"`);
    }
    const currency = currencyCode(input.currency);
    const { charged, total } = chargedLines(input.lines);
    const id = newId("cs");
    const url = pageUrl(input.base_url, account, id);
    const checkout = {
      id,
      account: account.name,
      status: "open",
      amount_total: total,
      currency,
      lines: charged,
      customer_email: (input.customer && input.customer.email) || null,
      metadata: input.metadata || {},
      shipping_countries: (input.shipping && input.shipping.countries) || null,
      client_reference: input.client_reference || null,
      success_url: input.success_url || url,
      cancel_url: input.cancel_url || url,
      url,
      payment: null,
      declined: 0,
      created: now(),
      updated: now(),
    };
    state.checkouts.set(id, checkout);
    return { id, url, object: normaliseCheckout(checkout) };
  },

  /** The events a signed delivery carries, `[{ id, type, ref }]`, or a refusal: no signature, one
   * that does not match the account's secret over these exact bytes, or one too old. */
  verifyWebhook(account, { rawBody, headers }) {
    const header = headers && headers[SIGNATURE_HEADER];
    if (!header) throw new Error(`the webhook carries no ${SIGNATURE_HEADER} header`);
    const parts = Object.fromEntries(
      String(header)
        .split(",")
        .map((p) => p.split("=", 2))
        .filter((p) => p.length === 2),
    );
    const t = Number(parts.t);
    if (!Number.isInteger(t) || !parts.v1) throw new Error(`the ${SIGNATURE_HEADER} header is not t=…,v1=…`);
    const expected = Buffer.from(sign(signingSecret(account), rawBody, t).split("v1=")[1], "hex");
    const given = Buffer.from(parts.v1, "hex");
    if (given.length !== expected.length || !crypto.timingSafeEqual(given, expected)) {
      throw new Error("the webhook's signature does not match this account's signing secret");
    }
    if (Math.abs(now() - t) > TOLERANCE_SECONDS) {
      throw new Error("the webhook's signature is too old to accept (a replay?)");
    }
    const event = JSON.parse(Buffer.from(rawBody).toString("utf8"));
    if (event.account !== account.name) throw new Error("the webhook is for another account");
    return [{ id: event.id, type: event.type, ref: (event.data && event.data.object) || null, raw: event }];
  },

  /** The current state of the object `ref` names, normalised, or a refusal naming it. */
  async fetch(account, ref) {
    const found = ref && lookup(account, ref.kind, ref.id);
    if (!found) throw new Error(`the fake provider has no ${ref && ref.kind} ${ref && ref.id} for "${account.name}"`);
    return NORMALISE[ref.kind](found);
  },

  /** The objects of `kind` changed since `since` (unix seconds), for the reconciliation sweep. */
  async list(account, kind, { since } = {}) {
    const map = { checkout: state.checkouts, payment: state.payments, refund: state.refunds }[kind] || new Map();
    return [...map.values()]
      .filter((o) => o.account === account.name && (!since || (o.updated || o.created) >= since))
      .map((o) => NORMALISE[kind](o));
  },

  /** What the checkout page shows: the checkout, its payment, and the deliveries of its events. */
  view(account, id) {
    const checkout = lookup(account, "checkout", id);
    if (!checkout) return null;
    const payment = checkout.payment ? state.payments.get(checkout.payment) : null;
    // Every payment the checkout has had, declined ones included: their events are its too.
    const attempts = [...state.payments.values()].filter((p) => p.checkout === checkout.id).map((p) => p.id);
    const refs = new Set([checkout.id, ...attempts]);
    const mine = state.events.filter((e) => e.ref && refs.has(e.ref.id));
    const events = new Set(mine.map((e) => e.id));
    return {
      checkout: { ...checkout },
      payment: payment ? { ...payment } : null,
      events: mine.map(({ id, type, ref, created }) => ({ id, type, ref, created })),
      deliveries: state.deliveries.filter((d) => events.has(d.event_id)).map((d) => ({ ...d })),
      total: format(checkout.amount_total, checkout.currency),
    };
  },

  /** The buyer pressed Pay (`succeeded`) or Fail (`failed`, a declined card): make the payment,
   * update the checkout, and deliver an event for each with `deliver(rawBody, headers)`.
   * Answers the checkout as it now is. A checkout that is not open is answered as it is. */
  async settle(account, id, outcome, deliver) {
    const checkout = lookup(account, "checkout", id);
    if (!checkout) throw new Error(`the fake provider has no checkout ${id} for "${account.name}"`);
    if (checkout.status !== "open") return checkout;
    const payment = {
      id: newId("pay"),
      account: account.name,
      status: outcome === "succeeded" ? "succeeded" : "failed",
      amount: checkout.amount_total,
      currency: checkout.currency,
      customer_email: checkout.customer_email,
      checkout: checkout.id,
      metadata: checkout.metadata,
      created: now(),
      updated: now(),
    };
    state.payments.set(payment.id, payment);
    checkout.updated = now();
    if (payment.status === "succeeded") {
      checkout.status = "complete";
      checkout.payment = payment.id;
      await emit(account, "checkout.completed", { kind: "checkout", id: checkout.id }, deliver);
      await emit(account, "payment.succeeded", { kind: "payment", id: payment.id }, deliver);
    } else {
      // A declined card leaves the checkout open, as a real one does: the buyer may try again.
      checkout.declined += 1;
      await emit(account, "payment.failed", { kind: "payment", id: payment.id }, deliver);
    }
    return checkout;
  },

  /** Refund `amount` of the payment `ref` (all that is left when no amount is given), once per
   * `key`: asked again with the same key, it answers the refund it already made. The payment's
   * status becomes `refunded` or `partially_refunded`, and a `payment.refunded` event is
   * delivered. */
  async refund(account, ref, { amount, key, reason } = {}, deliver) {
    const payment = lookup(account, "payment", ref);
    if (!payment) throw new Error(`the fake provider has no payment ${ref} for "${account.name}"`);
    const scoped = key ? `${account.name}:${key}` : null;
    if (scoped && state.refundKeys.has(scoped)) return normaliseRefund(state.refunds.get(state.refundKeys.get(scoped)));
    if (payment.status !== "succeeded" && payment.status !== "partially_refunded") {
      throw new Error(`the payment ${ref} is ${payment.status}, so there is nothing to refund`);
    }
    const left = payment.amount - (payment.amount_refunded || 0);
    const value = amount === undefined || amount === null ? left : requireAmount(amount, "the refund's amount");
    if (value === 0) throw new Error("a refund of 0 refunds nothing");
    if (value > left) {
      throw new Error(`the refund of ${format(value, payment.currency)} is more than the ${format(left, payment.currency)} left of the payment`);
    }
    const refund = { id: newId("re"), account: account.name, payment: payment.id, amount: value, currency: payment.currency, status: "succeeded", reason: reason || null, created: now() };
    state.refunds.set(refund.id, refund);
    if (scoped) state.refundKeys.set(scoped, refund.id);
    payment.amount_refunded = (payment.amount_refunded || 0) + value;
    payment.status = payment.amount_refunded === payment.amount ? "refunded" : "partially_refunded";
    payment.updated = now();
    await emit(account, "payment.refunded", { kind: "payment", id: payment.id }, deliver);
    return normaliseRefund(refund);
  },

  /** The checkout a payment (or a refund of one) belongs to. */
  checkoutOf(account, ref) {
    if (!ref) return null;
    if (ref.kind === "checkout") return ref.id;
    const payment = lookup(account, "payment", ref.id);
    return payment ? payment.checkout : null;
  },

  /** Deliver the event `id` again — signed afresh, as a provider's retry is — and answer the
   * attempt, or null when the account has no such event. */
  async resend(account, id, deliver) {
    const event = state.events.find((e) => e.id === id && e.account === account.name);
    return event ? attempt(account, event, deliver) : null;
  },

  /** The events since `since` (unix seconds), oldest first, in the shape `verifyWebhook`
   * answers: what the reconciliation sweep processes, held ones included. */
  async listEvents(account, { since } = {}) {
    return state.events
      .filter((e) => e.account === account.name && (!since || e.created >= since))
      .map((e) => ({ id: e.id, type: e.type, ref: e.ref, raw: JSON.parse(e.body.toString("utf8")) }));
  },

  /** `pay_api` on the fake: its objects as it keeps them. */
  async api(account, method, path) {
    const verb = String(method || "GET").toUpperCase();
    const m = /^\/(checkouts|payments|refunds)\/([^/]+)$/.exec(String(path || ""));
    if (verb === "GET" && m) {
      const kind = m[1].slice(0, -1);
      const found = lookup(account, kind, m[2]);
      if (!found) throw new Error(`the fake provider has no ${kind} ${m[2]} for "${account.name}"`);
      return { ...found };
    }
    if (verb === "GET" && path === "/events") return module.exports.events(account);
    throw new Error(`the fake provider answers GET /checkouts/<id>, /payments/<id>, /refunds/<id> and /events, not ${verb} ${path}`);
  },

  /** The account's events, oldest first, as `{ id, type, ref, created }`. */
  events(account) {
    return state.events.filter((e) => e.account === account.name).map(({ id, type, ref, created }) => ({ id, type, ref, created }));
  },

  /** The buyer pressed Cancel: nothing happens at a provider, so no event. */
  cancelUrl(account, id) {
    const checkout = lookup(account, "checkout", id);
    return checkout ? checkout.cancel_url : null;
  },

  /** Forget everything — for a test that wants a provider with nothing in it. */
  reset() {
    state.checkouts.clear();
    state.payments.clear();
    state.refunds.clear();
    state.refundKeys.clear();
    state.events.length = 0;
    state.deliveries.length = 0;
  },
};
