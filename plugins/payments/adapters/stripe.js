// The **stripe** adapter (TODO-payments §2, P2.5): Stripe as a payment service provider, and as
// a merchant of record through *Managed Payments* when the account turns it on.
//
// It speaks Stripe's REST API directly — form-encoded requests over `fetch` to the one host the
// module's `net` permission allows — rather than through the `stripe` npm package. The package
// would be downloaded by every installation for the dozen requests this file makes, and the REST
// API is what the package wraps: `client(account)` hands `pay_api` and `pay_provider_code` the
// same authenticated requests, so any Stripe endpoint is reachable (§3.5).
//
// One-off payments: a Checkout Session with ad-hoc prices, Stripe Tax, shipping address
// collection, Managed Payments (never with shipping: a merchant of record sells digital goods
// only), refunds, and webhook verification. Subscriptions (P4): a Checkout Session in
// subscription mode for a Stripe price, with a trial; the subscription's own events and its
// renewals' invoices; the customer portal; plan changes and cancellation. Saved cards (P7): a
// one-off checkout may keep the card (`setup_future_usage`), and `chargeSavedMethod` charges a
// customer's saved card with an off-session PaymentIntent. The native sheet and manual capture
// are later milestones (P8, P10); its capabilities say what Stripe can do, and the actions refuse
// what is not built yet.

"use strict";

const crypto = require("node:crypto");
const { currencyCode, requireAmount } = require("../money.js");

const API = "https://api.stripe.com";
const SIGNATURE_HEADER = "stripe-signature";
const TOLERANCE_SECONDS = 300;

/** The module's proration choices in Stripe's words. */
const PRORATION = { prorate: "create_prorations", invoice: "always_invoice", none: "none" };

/** Currencies Stripe counts differently from ISO 4217 (§3.11): ISK and UGX have no minor unit,
 * but Stripe takes them as two-decimal amounts whose decimals are always 00. */
const STRIPE_TIMES_100 = new Set(["ISK", "UGX"]);
/** Three-decimal currencies: Stripe takes the three decimals, but the last must be 0. */
const STRIPE_THREE_DECIMAL = new Set(["BHD", "JOD", "KWD", "OMR", "TND"]);

/** An amount in the currency's minor unit as Stripe counts it, or a refusal. */
function toStripe(amount, currency) {
  const code = currencyCode(currency);
  const value = requireAmount(amount, "the amount");
  if (STRIPE_TIMES_100.has(code)) return value * 100;
  if (STRIPE_THREE_DECIMAL.has(code) && value % 10 !== 0) {
    throw new Error(`Stripe charges ${code} in steps of 0.010, so ${value} thousandths cannot be charged`);
  }
  return value;
}

/** A Stripe amount back in the currency's minor unit. */
function fromStripe(amount, currency) {
  if (amount === null || amount === undefined) return null;
  return STRIPE_TIMES_100.has(String(currency).toUpperCase()) ? Math.round(amount / 100) : amount;
}

/** Stripe's form encoding of nested parameters: `line_items[0][price_data][currency]=eur`. */
function encodeForm(params, prefix = "", out = new URLSearchParams()) {
  for (const [key, value] of Object.entries(params || {})) {
    if (value === undefined) continue;
    const name = prefix ? `${prefix}[${key}]` : key;
    if (value === null) out.append(name, "");
    else if (Array.isArray(value)) {
      value.forEach((v, i) => {
        if (v !== null && typeof v === "object") encodeForm(v, `${name}[${i}]`, out);
        else out.append(`${name}[${i}]`, String(v));
      });
    } else if (typeof value === "object") encodeForm(value, name, out);
    else out.append(name, String(value));
  }
  return out;
}

/** The account's secret key, or a refusal: none, or one for the other mode. */
function secretKey(account) {
  const key = account.secret_key;
  if (typeof key !== "string" || !key) {
    throw new Error(`the stripe account "${account.name}" has no secret key; set it to the key from Stripe's dashboard (sk_test_… or sk_live_…)`);
  }
  const live = /^(sk|rk)_live_/.test(key);
  const test = /^(sk|rk)_test_/.test(key);
  if (account.mode === "live" && test) throw new Error(`the stripe account "${account.name}" is in live mode, but its secret key is a test key`);
  if (account.mode === "test" && live) throw new Error(`the stripe account "${account.name}" is in test mode, but its secret key is a live key`);
  return key;
}

/** How a request reaches Stripe: `fetch`, unless a test has put a recording in its place. */
const transport = { send: (url, init) => fetch(url, init) };

/** One authenticated request: `params` as the query of a GET or the form body of anything
 * else. Answers the parsed JSON, or throws Stripe's own sentence. */
async function request(account, method, path, params, { idempotencyKey } = {}) {
  if (typeof path !== "string" || !path.startsWith("/v1/")) {
    throw new Error(`a Stripe API path starts with /v1/, not ${JSON.stringify(path)}`);
  }
  const verb = String(method || "GET").toUpperCase();
  const headers = { authorization: `Bearer ${secretKey(account)}` };
  const settings = account.settings || {};
  if (settings.api_version) headers["stripe-version"] = String(settings.api_version);
  if (idempotencyKey) headers["idempotency-key"] = String(idempotencyKey);
  const form = encodeForm(params).toString();
  let url = `${API}${path}`;
  let body;
  if (verb === "GET" || verb === "DELETE") {
    if (form) url += `?${form}`;
  } else {
    headers["content-type"] = "application/x-www-form-urlencoded";
    body = form;
  }
  const response = await transport.send(url, { method: verb, headers, body });
  const text = await response.text();
  let json;
  try {
    json = text ? JSON.parse(text) : {};
  } catch (_) {
    throw new Error(`Stripe answered ${response.status} with something that is not JSON`);
  }
  if (!response.ok) {
    const e = json.error || {};
    const err = new Error(`Stripe refused ${verb} ${path}: ${e.message || `status ${response.status}`}${e.code ? ` (${e.code})` : ""}`);
    err.status = response.status;
    err.stripe = e;
    throw err;
  }
  return json;
}

/** The subscription an invoice is for: `subscription` until API version 2025-03-31, and
 * `parent.subscription_details.subscription` since. */
function invoiceSubscription(invoice) {
  const old = invoice.subscription;
  if (old) return typeof old === "string" ? old : old.id;
  const details = invoice.parent && invoice.parent.subscription_details;
  const sub = details && details.subscription;
  return sub ? (typeof sub === "string" ? sub : sub.id) : null;
}

/** The object an event is about, as a `{ kind, id }` reference the module fetches. */
function refOf(type, object) {
  if (!object || typeof object !== "object") return null;
  if (type.startsWith("customer.subscription.")) return { kind: "subscription", id: object.id };
  if (type.startsWith("invoice.")) {
    const sub = invoiceSubscription(object);
    return sub ? { kind: "subscription", id: sub } : null;
  }
  if (type.startsWith("checkout.session.")) return { kind: "checkout", id: object.id };
  if (type.startsWith("payment_intent.")) return { kind: "payment", id: object.id };
  if (type.startsWith("charge.dispute.")) return { kind: "dispute", id: object.id };
  if (type.startsWith("charge.")) return object.payment_intent ? { kind: "payment", id: object.payment_intent } : null;
  if (type.startsWith("refund.")) return { kind: "refund", id: object.id };
  return null;
}

/** Stripe's event types, in the module's words (§5). Anything else is `other`. A function reads
 * the event to tell which; it answers `null` for an event that only brings its object up to
 * date. */
const KINDS = {
  "checkout.session.completed": ["checkout.completed"],
  "payment_intent.processing": ["payment.processing"],
  "payment_intent.succeeded": ["payment.succeeded"],
  "payment_intent.payment_failed": ["payment.failed"],
  "charge.refunded": ["payment.refunded"],
  "charge.dispute.created": ["dispute.opened"],
  "charge.dispute.closed": ["dispute.closed"],
  "customer.subscription.created": ["subscription.started"],
  "customer.subscription.updated": subscriptionUpdated,
  "customer.subscription.deleted": ["subscription.ended"],
  // A renewal is a paid invoice of a new period (a trial's end included). The first period's
  // invoice, and a plan change's, say nothing the subscription's own events have not.
  "invoice.paid": (event) => {
    const invoice = (event.raw && event.raw.data && event.raw.data.object) || {};
    return invoice.billing_reason === "subscription_cycle" ? ["subscription.renewed"] : null;
  },
};

/** What a `customer.subscription.updated` was, from what it changed: past due, canceled (set to
 * end at the period's end), a change of plan or quantity, or a cancellation undone. Stripe sends
 * it for every renewal and every trial's end too, which move only the period and the status:
 * those are `null`, since the renewal's paid invoice says it. */
function subscriptionUpdated(event) {
  const data = (event.raw && event.raw.data) || {};
  const sub = data.object || {};
  const before = data.previous_attributes || {};
  if ("status" in before && sub.status === "past_due") return ["subscription.past_due"];
  const canceling = ("cancel_at_period_end" in before && sub.cancel_at_period_end) || ("cancel_at" in before && sub.cancel_at);
  if (canceling) return ["subscription.canceled"];
  const uncanceled = ("cancel_at_period_end" in before && !sub.cancel_at_period_end) || ("cancel_at" in before && !sub.cancel_at);
  if (uncanceled || planMoved(before, sub)) return ["subscription.changed"];
  return null;
}

/** The price and quantity a subscription (or its `previous_attributes`) says it is on: its first
 * item's, or the old API's `plan`. */
function planOf(sub) {
  const item = sub.items && sub.items.data && sub.items.data[0];
  if (item) return { price: item.price && item.price.id, quantity: item.quantity };
  if (sub.plan || sub.quantity !== undefined) return { price: sub.plan && sub.plan.id, quantity: sub.quantity };
  return null;
}

/** Whether an update moved the subscription to another price or quantity. Since API version
 * 2025-03-31 the period is on the item, so a renewal changes `items` too: the item's price and
 * quantity are compared, not the key's presence. */
function planMoved(before, sub) {
  if (!("items" in before) && !("plan" in before) && !("quantity" in before)) return false;
  const was = planOf(before);
  const now = planOf(sub);
  if (!was || !now) return true;
  return (was.price !== undefined && was.price !== now.price) || (was.quantity !== undefined && was.quantity !== now.quantity);
}

/** The events to choose when adding the webhook endpoint in Stripe's dashboard. */
const WEBHOOK_EVENTS = Object.keys(KINDS);

const upper = (c) => (c ? String(c).toUpperCase() : null);

function normaliseCheckout(s) {
  const currency = upper(s.currency);
  const items = (s.line_items && s.line_items.data) || [];
  return {
    kind: "checkout",
    ref: s.id,
    status: s.status,
    amount: fromStripe(s.amount_total, currency),
    currency,
    email: (s.customer_details && s.customer_details.email) || s.customer_email || null,
    payment_ref: typeof s.payment_intent === "string" ? s.payment_intent : s.payment_intent && s.payment_intent.id,
    customer_ref: typeof s.customer === "string" ? s.customer : null,
    subscription_ref: typeof s.subscription === "string" ? s.subscription : s.subscription && s.subscription.id,
    lines: items.map((i) => ({
      description: i.description,
      quantity: i.quantity,
      unit_amount: fromStripe(i.price ? i.price.unit_amount : null, currency),
      amount: fromStripe(i.amount_total, currency),
    })),
    metadata: s.metadata || {},
    updated: s.created,
    provider_data: s,
  };
}

/** A PaymentIntent's status in the module's words: Stripe's, except that a failed attempt
 * (`requires_payment_method` with an error) is `failed`, a hold is `authorized`, and a refunded
 * charge is `refunded` or `partially_refunded`. */
function paymentStatus(pi) {
  const charge = pi.latest_charge && typeof pi.latest_charge === "object" ? pi.latest_charge : null;
  switch (pi.status) {
    case "succeeded":
      if (charge && charge.amount_refunded > 0) return charge.refunded ? "refunded" : "partially_refunded";
      return "succeeded";
    case "requires_payment_method":
      return pi.last_payment_error ? "failed" : "open";
    case "requires_capture":
      return "authorized";
    case "requires_confirmation":
    case "requires_action":
      return "open";
    default:
      return pi.status;
  }
}

function normalisePayment(pi) {
  const currency = upper(pi.currency);
  const charge = pi.latest_charge && typeof pi.latest_charge === "object" ? pi.latest_charge : null;
  return {
    kind: "payment",
    ref: pi.id,
    status: paymentStatus(pi),
    amount: fromStripe(pi.amount, currency),
    currency,
    email: pi.receipt_email || (charge && charge.billing_details && charge.billing_details.email) || null,
    customer_ref: typeof pi.customer === "string" ? pi.customer : null,
    amount_refunded: fromStripe(charge ? charge.amount_refunded : 0, currency),
    metadata: pi.metadata || {},
    created: pi.created,
    updated: pi.created,
    provider_data: pi,
  };
}

/** Unix seconds as the mirror keeps an instant, or null. */
const isoOf = (seconds) => (seconds ? new Date(seconds * 1000).toISOString() : null);

/** A subscription in the module's words. Its period is on the subscription until API version
 * 2025-03-31, and on each item since. The module subscribes to one price, so the first item is
 * the plan. */
function normaliseSubscription(s) {
  const item = (s.items && s.items.data && s.items.data[0]) || {};
  const price = item.price || s.plan || {};
  const currency = upper(price.currency || s.currency);
  const quantity = item.quantity === undefined ? s.quantity : item.quantity;
  const unit = price.unit_amount === undefined ? price.amount : price.unit_amount;
  return {
    kind: "subscription",
    ref: s.id,
    status: s.status,
    amount: unit === undefined || unit === null || quantity === undefined || quantity === null ? null : fromStripe(unit * quantity, currency),
    currency,
    price_ref: price.id || null,
    quantity: quantity === undefined ? null : quantity,
    current_period_end: isoOf(s.current_period_end || item.current_period_end),
    trial_end: isoOf(s.trial_end),
    cancel_at_period_end: Boolean(s.cancel_at_period_end),
    cancel_at: isoOf(s.cancel_at),
    customer_ref: typeof s.customer === "string" ? s.customer : s.customer && s.customer.id,
    item_ref: item.id || null,
    metadata: s.metadata || {},
    created: s.created,
    updated: s.created,
    provider_data: s,
  };
}

function normaliseCustomer(c) {
  return { kind: "customer", ref: c.id, status: c.deleted ? "deleted" : null, email: c.email || null, metadata: c.metadata || {}, updated: c.created, provider_data: c };
}

function normaliseRefund(r) {
  const currency = upper(r.currency);
  return {
    kind: "refund",
    ref: r.id,
    status: r.status,
    amount: fromStripe(r.amount, currency),
    currency,
    payment_ref: r.payment_intent,
    metadata: r.metadata || {},
    updated: r.created,
    provider_data: r,
  };
}

function normaliseDispute(d) {
  const currency = upper(d.currency);
  return {
    kind: "dispute",
    ref: d.id,
    status: d.status,
    amount: fromStripe(d.amount, currency),
    currency,
    payment_ref: d.payment_intent,
    metadata: d.metadata || {},
    updated: d.created,
    provider_data: d,
  };
}

/** Where each kind of object is read, and what it is read as. */
const OBJECTS = {
  checkout: { path: "/v1/checkout/sessions", expand: ["line_items"], normalise: normaliseCheckout },
  payment: { path: "/v1/payment_intents", expand: ["latest_charge"], normalise: normalisePayment },
  refund: { path: "/v1/refunds", expand: [], normalise: normaliseRefund },
  dispute: { path: "/v1/disputes", expand: [], normalise: normaliseDispute },
  subscription: { path: "/v1/subscriptions", expand: [], normalise: normaliseSubscription, list: { status: "all" } },
  customer: { path: "/v1/customers", expand: [], normalise: normaliseCustomer },
};

/** A return address Stripe can send the buyer to: absolute, so made from the application the
 * checkout was started through. */
function absolute(url, what) {
  if (!/^https?:\/\//.test(String(url || ""))) {
    throw new Error(
      `Stripe sends the buyer back to a full address, and the ${what} is ${JSON.stringify(url)}: start the checkout through an application, whose address it is built on`,
    );
  }
  return url;
}

module.exports = {
  name: "stripe",
  capabilities: {
    one_off: true,
    adhoc_prices: true,
    subscriptions: true,
    trial_with_card: true,
    portal: true,
    charge_saved_method: true,
    native_sheet: true,
    manual_capture: true,
    async_methods: true,
    marketplace: true,
    merchant_of_record: "optional",
    physical_goods: true,
  },
  settings: [
    { name: "managed_payments", type: "Bool", label: "Managed Payments", sublabel: "Stripe acts as merchant of record (digital goods only)" },
    { name: "api_version", type: "String", label: "API version", sublabel: "Pin a Stripe API version; the account's default otherwise" },
  ],
  hosts: ["api.stripe.com"],
  SIGNATURE_HEADER,
  WEBHOOK_EVENTS,
  transport,
  encodeForm,
  toStripe,
  fromStripe,

  /** What `pay_api` and `pay_provider_code` get: authenticated requests to any endpoint. */
  client(account) {
    return {
      request: (method, path, params, options) => request(account, method, path, params, options),
      get: (path, params) => request(account, "GET", path, params),
      post: (path, params, options) => request(account, "POST", path, params, options),
      del: (path, params) => request(account, "DELETE", path, params),
    };
  },

  /** `pay_api`: one authenticated request to any Stripe endpoint. */
  api(account, method, path, body, options) {
    return request(account, method, path, body, options);
  },

  /** A Checkout Session: in payment mode with a price made for each line, or in subscription
   * mode for one Stripe price, its quantity and its trial. The module's metadata goes on the
   * payment or the subscription too, since Stripe links neither back to the session. */
  async createCheckout(account, input) {
    const mode = input.mode || "payment";
    if (mode !== "payment" && mode !== "subscription") throw new Error(`a checkout is in mode payment or subscription, not "${mode}"`);
    const currency = currencyCode(input.currency);
    const tax = input.tax || "none";
    const settings = account.settings || {};
    const params = {
      mode,
      success_url: absolute(input.success_url, "address after paying"),
      cancel_url: absolute(input.cancel_url, "address after cancelling"),
      metadata: input.metadata || {},
    };
    if (mode === "subscription") {
      const [line, ...more] = input.lines || [];
      if (!line || more.length) throw new Error("a subscription checkout is for one price");
      if (!/^[A-Za-z0-9_]+$/.test(String(line.price_ref || ""))) throw new Error(`${JSON.stringify(line.price_ref)} is not a Stripe price id (price_…)`);
      params.line_items = [{ price: line.price_ref, quantity: requireAmount(line.quantity === undefined ? 1 : line.quantity, "the quantity") }];
      params.subscription_data = { metadata: input.metadata || {} };
      if (line.trial_days) params.subscription_data.trial_period_days = requireAmount(line.trial_days, "the trial's days");
    } else {
      params.line_items = (input.lines || []).map((l, i) => ({
        price_data: {
          currency: currency.toLowerCase(),
          unit_amount: toStripe(l.unit_amount, currency),
          product_data: { name: String(l.description || `Line ${i + 1}`) },
        },
        quantity: requireAmount(l.quantity === undefined ? 1 : l.quantity, `line ${i + 1}'s quantity`),
      }));
      params.payment_intent_data = { metadata: input.metadata || {} };
      // Kept for later charges with no page (a top-up): Stripe asks the buyer's consent on the
      // page, and the card is saved to a customer — a new one, when the buyer is none yet.
      if (input.save_card) {
        params.payment_intent_data.setup_future_usage = "off_session";
        if (!(input.customer && input.customer.ref)) params.customer_creation = "always";
      }
    }
    if (!params.line_items.length) throw new Error("a checkout charges at least one line");
    // The customer this user already is, so the same card and the same portal; a new one
    // otherwise, made by Stripe from the email.
    if (input.customer && input.customer.ref) params.customer = input.customer.ref;
    else if (input.customer && input.customer.email) params.customer_email = input.customer.email;
    if (input.client_reference) params.client_reference_id = String(input.client_reference).slice(0, 200);
    if (tax === "provider") params.automatic_tax = { enabled: true };
    if (tax === "merchant_of_record") {
      if (settings.managed_payments !== true) {
        throw new Error(`the stripe account "${account.name}" does not use Managed Payments, so it cannot act as merchant of record; turn it on in the account's settings`);
      }
      if (input.shipping) throw new Error("Managed Payments sells digital goods only, so it cannot collect a shipping address");
      params.managed_payments = { enabled: true };
    }
    if (input.shipping) params.shipping_address_collection = { allowed_countries: input.shipping.countries };
    const session = await request(account, "POST", "/v1/checkout/sessions", params);
    return { id: session.id, url: session.url, object: normaliseCheckout(session) };
  },

  /** The event a delivery carries, or a refusal: no signature, none of its `v1` signatures made
   * with the account's signing secret over these exact bytes, too old, or from the other mode. */
  verifyWebhook(account, { rawBody, headers }) {
    const secret = account.webhook_secret;
    if (typeof secret !== "string" || !secret) {
      throw new Error(`the stripe account "${account.name}" has no webhook signing secret; it is the whsec_… shown on the endpoint in Stripe's dashboard`);
    }
    const header = headers && headers[SIGNATURE_HEADER];
    if (!header) throw new Error(`the webhook carries no ${SIGNATURE_HEADER} header`);
    let t = null;
    const given = [];
    for (const part of String(header).split(",")) {
      const [k, v] = part.split("=", 2);
      if (k === "t") t = Number(v);
      if (k === "v1" && v) given.push(v);
    }
    if (!Number.isInteger(t) || !given.length) throw new Error(`the ${SIGNATURE_HEADER} header is not t=…,v1=…`);
    const expected = crypto.createHmac("sha256", secret).update(`${t}.`).update(Buffer.from(rawBody)).digest();
    const matches = given.some((v) => {
      const b = Buffer.from(v, "hex");
      return b.length === expected.length && crypto.timingSafeEqual(b, expected);
    });
    if (!matches) throw new Error("the webhook's signature does not match this account's signing secret");
    if (Math.abs(Math.floor(Date.now() / 1000) - t) > TOLERANCE_SECONDS) {
      throw new Error("the webhook's signature is too old to accept (a replay?)");
    }
    const event = JSON.parse(Buffer.from(rawBody).toString("utf8"));
    if (typeof event.livemode === "boolean" && event.livemode !== (account.mode === "live")) {
      throw new Error(`the webhook is from Stripe's ${event.livemode ? "live" : "test"} mode, and "${account.name}" is in ${account.mode} mode`);
    }
    const object = event.data && event.data.object;
    return [{ id: event.id, type: event.type, ref: refOf(String(event.type || ""), object), raw: event }];
  },

  /** The module's kinds for a Stripe event: `[]` for one it does not know, and `null` for one
   * that only brings its object up to date. */
  kinds(event) {
    const kinds = KINDS[event.type];
    if (typeof kinds === "function") return kinds(event);
    return kinds || [];
  },

  /** The current state of an object, normalised. */
  async fetch(account, ref) {
    const spec = ref && OBJECTS[ref.kind];
    if (!spec) throw new Error(`the stripe adapter does not read ${ref && ref.kind} objects`);
    if (!/^[A-Za-z0-9_]+$/.test(String(ref.id || ""))) throw new Error(`${JSON.stringify(ref.id)} is not a Stripe id`);
    const object = await request(account, "GET", `${spec.path}/${ref.id}`, spec.expand.length ? { expand: spec.expand } : undefined);
    return spec.normalise(object);
  },

  /** The objects of `kind` created since `since` (unix seconds), every page of them. */
  async list(account, kind, { since } = {}) {
    const spec = OBJECTS[kind];
    if (!spec) throw new Error(`the stripe adapter does not list ${kind} objects`);
    const out = [];
    let after;
    for (;;) {
      const params = { limit: 100, ...(spec.list || {}) };
      if (since) params.created = { gte: since };
      if (after) params.starting_after = after;
      const page = await request(account, "GET", spec.path, params);
      out.push(...page.data.map(spec.normalise));
      if (!page.has_more || !page.data.length) return out;
      after = page.data[page.data.length - 1].id;
    }
  },

  /** The events since `since` (unix seconds) of the types the module knows, oldest first, in
   * the shape `verifyWebhook` answers: the reconciliation sweep (P2.7) processes them as if
   * they had been delivered, so one that was already is a duplicate. Stripe keeps 30 days. */
  async listEvents(account, { since } = {}) {
    const out = [];
    let after;
    for (;;) {
      const params = { limit: 100, types: WEBHOOK_EVENTS };
      if (since) params.created = { gte: since };
      if (after) params.starting_after = after;
      const page = await request(account, "GET", "/v1/events", params);
      for (const event of page.data) {
        out.push({ id: event.id, type: event.type, ref: refOf(event.type, event.data && event.data.object), raw: event });
      }
      if (!page.has_more || !page.data.length) break;
      after = page.data[page.data.length - 1].id;
    }
    // Stripe lists newest first.
    return out.reverse();
  },

  /** A customer portal session, answered as `{ id, url }`: Stripe's hosted page where the
   * customer changes plan, changes card or cancels, configured in Stripe's dashboard. */
  async createPortal(account, { customer, return_url } = {}) {
    if (!/^cus_[A-Za-z0-9]+$/.test(String(customer || ""))) throw new Error(`${JSON.stringify(customer)} is not a Stripe customer id`);
    const session = await request(account, "POST", "/v1/billing_portal/sessions", {
      customer,
      return_url: absolute(return_url, "address to go back to"),
    });
    return { id: session.id, url: session.url };
  },

  /** Move a subscription's one item to another price, prorated as the module says (`prorate`,
   * `invoice` or `none`). Stripe then sends `customer.subscription.updated`. */
  async changePlan(account, ref, priceRef, { proration } = {}) {
    if (!/^[A-Za-z0-9_]+$/.test(String(priceRef || ""))) throw new Error(`${JSON.stringify(priceRef)} is not a Stripe price id (price_…)`);
    const current = await module.exports.fetch(account, { kind: "subscription", id: ref });
    if (!current.item_ref) throw new Error(`the subscription ${ref} has no item to change`);
    const params = { items: [{ id: current.item_ref, price: priceRef }] };
    if (proration) {
      if (!PRORATION[proration]) throw new Error(`a plan change is prorated ${Object.keys(PRORATION).join(", ")}, not "${proration}"`);
      params.proration_behavior = PRORATION[proration];
    }
    return normaliseSubscription(await request(account, "POST", `/v1/subscriptions/${ref}`, params));
  },

  /** Cancel a subscription at the end of its period (it runs on until then), or now. */
  async cancel(account, ref, { at_period_end = true } = {}) {
    if (!/^sub_[A-Za-z0-9]+$/.test(String(ref || ""))) throw new Error(`${JSON.stringify(ref)} is not a Stripe subscription id`);
    const sub = at_period_end
      ? await request(account, "POST", `/v1/subscriptions/${ref}`, { cancel_at_period_end: true })
      : await request(account, "DELETE", `/v1/subscriptions/${ref}`);
    return normaliseSubscription(sub);
  },

  /**
   * Charge a customer's saved card with no page (P7.2): an off-session PaymentIntent, confirmed at
   * once, for the customer's default payment method — or, when it has none, the card it last
   * saved. Once per `key` (Stripe's idempotency key, kept 24 hours). Answers `{ status: "paid" |
   * "processing", object }`, or `{ status: "requires_checkout", reason, object }` when there is no
   * saved card, the bank asks the buyer to authenticate (`authentication_required`), or the card
   * is declined. Stripe sends the PaymentIntent's events itself.
   */
  async chargeSavedMethod(account, { customer, amount, currency, description, key, metadata } = {}) {
    if (!/^cus_[A-Za-z0-9]+$/.test(String(customer || ""))) throw new Error(`${JSON.stringify(customer)} is not a Stripe customer id`);
    const code = currencyCode(currency);
    const value = toStripe(amount, code);
    if (value === 0) throw new Error("a charge of 0 charges nothing");
    const found = await request(account, "GET", `/v1/customers/${customer}`);
    const preferred = found.invoice_settings && found.invoice_settings.default_payment_method;
    let method = preferred && typeof preferred === "object" ? preferred.id : preferred;
    if (!method) {
      const saved = await request(account, "GET", `/v1/customers/${customer}/payment_methods`, { limit: 1 });
      method = saved.data && saved.data[0] && saved.data[0].id;
    }
    if (!method) return { status: "requires_checkout", reason: "the customer has no saved card", object: null };
    const params = { amount: value, currency: code.toLowerCase(), customer, payment_method: method, off_session: true, confirm: true, metadata: metadata || {} };
    if (description) params.description = String(description).slice(0, 1000);
    try {
      const pi = await request(account, "POST", "/v1/payment_intents", params, { idempotencyKey: key });
      const object = normalisePayment(pi);
      if (pi.status === "succeeded") return { status: "paid", object };
      if (pi.status === "processing") return { status: "processing", object };
      return { status: "requires_checkout", reason: `the payment is ${pi.status}`, object };
    } catch (e) {
      // A card the bank wants confirmed, or one declined: a 402, with the PaymentIntent it made.
      if (e.status !== 402) throw e;
      const pi = e.stripe && e.stripe.payment_intent;
      return { status: "requires_checkout", reason: (e.stripe && e.stripe.message) || e.message, object: pi ? normalisePayment(pi) : null };
    }
  },

  /** Refund `amount` of a PaymentIntent (all of it when none is given), once per `key`. Stripe
   * then sends `charge.refunded` itself. */
  async refund(account, ref, { amount, key, reason } = {}) {
    const params = { payment_intent: ref };
    if (amount !== undefined && amount !== null) {
      const pi = await request(account, "GET", `/v1/payment_intents/${ref}`);
      params.amount = toStripe(amount, pi.currency);
    }
    // Stripe takes three reasons of its own; anything else is kept beside the refund.
    if (["duplicate", "fraudulent", "requested_by_customer"].includes(reason)) params.reason = reason;
    else if (reason) params.metadata = { reason: String(reason).slice(0, 500) };
    const refund = await request(account, "POST", "/v1/refunds", params, { idempotencyKey: key });
    return normaliseRefund(refund);
  },
};
