// The **polar** adapter (TODO-payments §2, P5): Polar, an open-source merchant of record.
//
// Polar is legally the seller (§2): it charges the buyer, adds and pays the sales tax and VAT,
// and handles disputes. So every checkout is a merchant-of-record sale of digital goods: there
// is no shipping, and the account's tax setting changes nothing. Like the Stripe adapter it
// speaks the REST API over `fetch` — JSON to `api.polar.sh`, or to `sandbox-api.polar.sh` for an
// account in test mode — with the account's organization access token, and pins the API
// version (`Polar-Version`, the account's `api_version` setting or 2026-10).
//
// Where Polar is shaped differently from Stripe, and what the module makes of it (P5.1):
//
// - **A checkout sells one product.** There are no line items: a one-off checkout charges the
//   account's *product for one-off payments* (a setting) at an ad-hoc price, the total worked out
//   from the lines. The lines stay on the module's mirror row as they were charged.
// - **A plan is a product, not a price.** A subscription's `price_ref` is its Polar product id,
//   and the plan binding names the plans table's `polar_product` field for the account.
// - **Amounts.** The module's `amount` is what the buyer paid (`total_amount`, tax included).
//   Polar refunds a pre-tax amount and the tax with it in proportion, so a refund of an amount is
//   converted (exactly, when there was no tax).
// - **Events carry no previous state** (Stripe's `previous_attributes`). Most changes have an
//   event of their own (`subscription.canceled`, `.past_due`, `.revoked`); a plan change has
//   only the catch-all `subscription.updated`, so `kinds` compares the event's product with the
//   mirror row as it was (`previous`). A checkout's success may be reported by more than one
//   `checkout.updated`, so it is logged under one id per checkout.
// - **A renewal is an order**, paid with `billing_reason: subscription_cycle`: one delivery about
//   two objects. `verifyWebhook` answers two events for it — the payment, and the subscription's
//   renewal — under the delivery's one id.
// - **Webhooks follow Standard Webhooks** (`webhook-id`, `webhook-timestamp`, `webhook-signature`).
//   Secrets made before 8 September 2026 are used as their UTF-8 bytes, later ones the standard's
//   way (the base64 after `whsec_`); both are tried, as Polar's SDK does.
// - **No idempotency keys.** A refund and a saved-card charge carry the module's key in their
//   metadata, and the adapter looks for one made with it before making another.
// - **The sweep** reads the account's webhook endpoint's deliveries (its id is a setting) and
//   processes the events that were never delivered.
// - **Saved cards** (P5.3): Polar's off-session charges (in preview, for organisations on a paid
//   plan) make a draft order for the customer and finalize it against their saved card;
//   `chargeSavedMethod` answers whether it was paid or needs a checkout instead.

"use strict";

const crypto = require("node:crypto");
const { currencyCode, requireAmount } = require("../money.js");

const HOSTS = { live: "api.polar.sh", test: "sandbox-api.polar.sh" };
const DEFAULT_API_VERSION = "2026-10";
const TOLERANCE_SECONDS = 300;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The module's proration choices in Polar's words. Polar always prorates or invoices a change
 * of product; `next_period` and `reset` are other things, not "no proration". */
const PRORATION = { prorate: "prorate", invoice: "invoice" };

/** The module's refund reasons (Stripe's three, which `pay_refund` passes through) in Polar's. */
const REFUND_REASONS = { requested_by_customer: "customer_request", duplicate: "duplicate", fraudulent: "fraudulent" };

/** The account's organization access token, or a refusal. */
function accessToken(account) {
  const token = account.secret_key;
  if (typeof token !== "string" || !token) {
    throw new Error(
      `the polar account "${account.name}" has no secret key; set it to an organization access token from Polar's settings ` +
        `(from ${account.mode === "live" ? "polar.sh" : "sandbox.polar.sh, since the account is in test mode"})`,
    );
  }
  return token;
}

/** How a request reaches Polar: `fetch`, unless a test has put a simulation in its place. */
const transport = { send: (url, init) => fetch(url, init) };

/** A query string: arrays as repeated keys, objects as `key[sub]` (Polar's metadata filter). */
function query(params) {
  const out = new URLSearchParams();
  for (const [key, value] of Object.entries(params || {})) {
    if (value === undefined || value === null) continue;
    if (Array.isArray(value)) value.forEach((v) => out.append(key, String(v)));
    else if (typeof value === "object") for (const [k, v] of Object.entries(value)) out.append(`${key}[${k}]`, String(v));
    else out.append(key, String(value));
  }
  return out.toString();
}

/** Polar's own sentence for a refusal: `{ error, detail }`, or a validation error's list. */
function refusalOf(json, status) {
  if (typeof json.detail === "string") return json.detail;
  if (Array.isArray(json.detail)) {
    return json.detail.map((d) => `${(d.loc || []).filter((l) => l !== "body").join(".")}: ${d.msg}`).join("; ");
  }
  return `status ${status}`;
}

/** One authenticated request: `params` as the query of a GET or DELETE, the JSON body of anything
 * else. Answers the parsed JSON, or throws Polar's own sentence, with its status and error name. */
async function request(account, method, path, params) {
  if (typeof path !== "string" || !path.startsWith("/v1/")) {
    throw new Error(`a Polar API path starts with /v1/, not ${JSON.stringify(path)}`);
  }
  const verb = String(method || "GET").toUpperCase();
  const settings = account.settings || {};
  const headers = {
    authorization: `Bearer ${accessToken(account)}`,
    accept: "application/json",
    "polar-version": String(settings.api_version || DEFAULT_API_VERSION),
  };
  let url = `https://${HOSTS[account.mode] || HOSTS.test}${path}`;
  let body;
  if (verb === "GET" || verb === "DELETE") {
    const q = query(params);
    if (q) url += `?${q}`;
  } else {
    headers["content-type"] = "application/json";
    body = JSON.stringify(params || {});
  }
  const response = await transport.send(url, { method: verb, headers, body });
  const text = await response.text();
  let json;
  try {
    json = text ? JSON.parse(text) : {};
  } catch (_) {
    throw new Error(`Polar answered ${response.status} with something that is not JSON`);
  }
  if (!response.ok) {
    const err = new Error(`Polar refused ${verb} ${path}: ${refusalOf(json, response.status)}${typeof json.error === "string" ? ` (${json.error})` : ""}`);
    err.status = response.status;
    err.polar = json;
    throw err;
  }
  return json;
}

/** A Polar id, or a refusal naming what it was meant to be. */
function polarId(value, what) {
  if (!UUID.test(String(value || ""))) throw new Error(`${JSON.stringify(value)} is not a Polar ${what} id`);
  return String(value);
}

const upper = (c) => (c ? String(c).toUpperCase() : null);

/** Polar's checkout statuses in the module's: `confirmed` (the buyer pressed Pay) is still open. */
const CHECKOUT_STATUS = { open: "open", confirmed: "open", succeeded: "complete", expired: "expired", failed: "failed" };

function normaliseCheckout(c) {
  return {
    kind: "checkout",
    ref: c.id,
    status: CHECKOUT_STATUS[c.status] || c.status,
    amount: c.total_amount === undefined ? null : c.total_amount,
    currency: upper(c.currency),
    email: c.customer_email || null,
    // Polar's checkout does not name the order it made; the order names its checkout.
    payment_ref: null,
    customer_ref: c.customer_id || null,
    subscription_ref: null,
    trial_end: c.trial_end || null,
    metadata: c.metadata || {},
    created: c.created_at,
    updated: c.modified_at || c.created_at,
    provider_data: c,
  };
}

/** An order's status as a payment's: `pending` is being collected (a renewal being retried),
 * `void` will never be. */
const ORDER_STATUS = { paid: "succeeded", refunded: "refunded", partially_refunded: "partially_refunded", pending: "open", draft: "open", void: "canceled" };

function normaliseOrder(o) {
  const currency = upper(o.currency);
  return {
    kind: "payment",
    ref: o.id,
    status: ORDER_STATUS[o.status] || o.status,
    amount: o.total_amount,
    currency,
    email: (o.customer && o.customer.email) || null,
    checkout_ref: o.checkout_id || null,
    subscription_ref: o.subscription_id || null,
    customer_ref: o.customer_id || null,
    amount_refunded: (o.refunded_amount || 0) + (o.refunded_tax_amount || 0),
    billing_reason: o.billing_reason || null,
    lines: (o.items || []).map((i) => ({ description: i.label, quantity: 1, unit_amount: i.amount, amount: i.amount })),
    metadata: o.metadata || {},
    created: o.created_at,
    updated: o.modified_at || o.created_at,
    provider_data: o,
  };
}

/** A subscription: Polar's statuses are Stripe's (with `paused`), and its plan is its product. */
function normaliseSubscription(s) {
  return {
    kind: "subscription",
    ref: s.id,
    status: s.status,
    amount: s.amount === undefined ? null : s.amount,
    currency: upper(s.currency),
    price_ref: s.product_id || null,
    quantity: s.seats || 1,
    current_period_end: s.current_period_end || null,
    trial_end: s.trial_end || null,
    cancel_at_period_end: Boolean(s.cancel_at_period_end),
    cancel_at: s.cancel_at_period_end ? s.ends_at || null : null,
    email: (s.customer && s.customer.email) || null,
    customer_ref: s.customer_id || null,
    checkout_ref: s.checkout_id || null,
    metadata: s.metadata || {},
    created: s.created_at,
    updated: s.modified_at || s.created_at,
    provider_data: s,
  };
}

function normaliseCustomer(c) {
  return { kind: "customer", ref: c.id, status: c.deleted_at ? "deleted" : null, email: c.email || null, metadata: c.metadata || {}, updated: c.created_at, provider_data: c };
}

/** A refund, in what the buyer gets back: Polar's pre-tax amount and the tax refunded with it. */
function normaliseRefund(r) {
  return {
    kind: "refund",
    ref: r.id,
    status: r.status,
    amount: (r.amount || 0) + (r.tax_amount || 0),
    currency: upper(r.currency),
    payment_ref: r.order_id,
    metadata: r.metadata || {},
    updated: r.created_at,
    provider_data: r,
  };
}

/** Where each kind of object is read, and what it is read as. */
const OBJECTS = {
  checkout: { path: "/v1/checkouts", normalise: normaliseCheckout },
  payment: { path: "/v1/orders", normalise: normaliseOrder },
  subscription: { path: "/v1/subscriptions", normalise: normaliseSubscription },
  customer: { path: "/v1/customers", normalise: normaliseCustomer },
  refund: { path: "/v1/refunds", normalise: normaliseRefund, byQuery: true },
};

/** What each Polar event is about. */
function refOf(type, data) {
  if (!data || typeof data !== "object") return null;
  if (type.startsWith("checkout.")) return { kind: "checkout", id: data.id };
  if (type.startsWith("order.")) return { kind: "payment", id: data.id };
  if (type.startsWith("subscription.")) return { kind: "subscription", id: data.id };
  // A refund is told by its order, whose state includes what was refunded.
  if (type.startsWith("refund.")) return data.order_id ? { kind: "payment", id: data.order_id } : null;
  if (type.startsWith("customer.")) return data.id ? { kind: "customer", id: data.id } : null;
  return null;
}

/** The events one Polar delivery carries, in the shape `verifyWebhook` answers. A paid renewal
 * order is about the payment and the subscription it renews; a checkout's success is logged
 * once per checkout, however many `checkout.updated` say so. */
function eventsOf(id, payload) {
  const type = String(payload.type || "");
  const data = payload.data || {};
  if (type === "checkout.updated" && data.status === "succeeded") {
    return [{ id: `${data.id}:succeeded`, type, ref: refOf(type, data), raw: payload }];
  }
  const events = [{ id, type, ref: refOf(type, data), raw: payload }];
  if (type === "order.paid" && data.billing_reason === "subscription_cycle" && data.subscription_id) {
    events.push({ id, type, ref: { kind: "subscription", id: data.subscription_id }, raw: payload });
  }
  return events;
}

/** A plan change: the subscription's product (or seats) is not what the mirror last had. An
 * event older than what the mirror has is no change. */
function planChanged(previous, data) {
  if (!previous || !data) return false;
  const seen = previous.provider_data && previous.provider_data.modified_at;
  if (seen && data.modified_at && Date.parse(data.modified_at) < Date.parse(seen)) return false;
  const seats = previous.provider_data && previous.provider_data.seats;
  return (previous.price_ref && data.product_id && previous.price_ref !== data.product_id) || (seats !== undefined && data.seats !== undefined && seats !== data.seats);
}

/** Polar's event types in the module's words; `null` brings the object up to date and logs
 * nothing. Anything not here is `other`. */
const KINDS = {
  "checkout.created": null,
  "checkout.expired": null,
  "checkout.updated": (event) => {
    const status = event.raw && event.raw.data && event.raw.data.status;
    if (status === "succeeded") return ["checkout.completed"];
    if (status === "failed") return ["payment.failed"];
    return null;
  },
  "order.created": null,
  "order.updated": null,
  "order.paid": (event) => (event.ref && event.ref.kind === "subscription" ? ["subscription.renewed"] : ["payment.succeeded"]),
  "order.refunded": ["payment.refunded"],
  "refund.created": null,
  "refund.updated": null,
  "subscription.created": ["subscription.started"],
  "subscription.migrated": ["subscription.started"],
  "subscription.active": null,
  "subscription.cycled": null,
  "subscription.updated": (event, { previous } = {}) => (planChanged(previous, event.raw && event.raw.data) ? ["subscription.changed"] : null),
  "subscription.uncanceled": ["subscription.changed"],
  "subscription.paused": ["subscription.changed"],
  "subscription.resumed": ["subscription.changed"],
  "subscription.past_due": ["subscription.past_due"],
  "subscription.canceled": ["subscription.canceled"],
  "subscription.revoked": ["subscription.ended"],
};

/** The events to choose when adding the webhook endpoint in Polar's settings. */
const WEBHOOK_EVENTS = Object.keys(KINDS);

/** The HMAC keys a signing secret may stand for: its UTF-8 bytes (secrets made before 8 September
 * 2026), and, for a `whsec_` secret, the standard's base64 key. */
function signingKeys(secret) {
  const keys = [Buffer.from(secret, "utf8")];
  if (secret.startsWith("whsec_")) {
    const decoded = Buffer.from(secret.slice("whsec_".length), "base64");
    if (decoded.length) keys.push(decoded);
  }
  return keys;
}

/** A Standard Webhooks signature header for `body`: what Polar sends, and what a test signs with. */
function sign(key, id, timestamp, body) {
  const mac = crypto.createHmac("sha256", key).update(`${id}.${timestamp}.`).update(Buffer.from(body)).digest("base64");
  return `v1,${mac}`;
}

/** A return address Polar can send the buyer to: absolute, made from the calling application. */
function absolute(url, what) {
  if (!/^https?:\/\//.test(String(url || ""))) {
    throw new Error(`Polar sends the buyer back to a full address, and the ${what} is ${JSON.stringify(url)}: start the checkout through an application, whose address it is built on`);
  }
  return url;
}

/** The account's product for one-off payments, or a refusal saying how to make one. */
function oneOffProduct(account) {
  const product = account.settings && account.settings.product;
  if (!product) {
    throw new Error(
      `the polar account "${account.name}" has no product for one-off payments: Polar charges for a product, so make a ` +
        `one-time product in Polar (its name is what buyers see, say "Order") and set its id as the account's product setting`,
    );
  }
  return polarId(product, "product");
}

/** All pages of a list endpoint, newest first, until `stop(item)` says an item is too old. */
async function allPages(account, path, params, stop = () => false) {
  const out = [];
  for (let page = 1; ; page += 1) {
    const answer = await request(account, "GET", path, { ...params, page, limit: 100 });
    const items = answer.items || [];
    for (const item of items) {
      if (stop(item)) return out;
      out.push(item);
    }
    const max = (answer.pagination && answer.pagination.max_page) || 1;
    if (page >= max || !items.length) return out;
  }
}

module.exports = {
  name: "polar",
  capabilities: {
    one_off: true,
    adhoc_prices: true,
    subscriptions: true,
    trial_with_card: true,
    portal: true,
    // Off-session charges, in preview for organisations on a paid plan (P5.3).
    charge_saved_method: true,
    native_sheet: false,
    manual_capture: false,
    async_methods: false,
    marketplace: false,
    merchant_of_record: "always",
    physical_goods: false,
  },
  settings: [
    { name: "product", type: "String", label: "Product for one-off payments", sublabel: "The id of a one-time product in Polar; each one-off checkout charges it at the amount worked out" },
    { name: "webhook_endpoint_id", type: "String", label: "Webhook endpoint id", sublabel: "The id of this account's webhook endpoint in Polar, so the sweep can find events that were never delivered" },
    { name: "api_version", type: "String", label: "API version", sublabel: `The Polar API version to pin; ${DEFAULT_API_VERSION} otherwise` },
  ],
  hosts: [HOSTS.live, HOSTS.test],
  WEBHOOK_EVENTS,
  transport,
  sign,

  /** What `pay_api` and `pay_provider_code` (as `polar`) get: authenticated JSON requests. */
  client(account) {
    return {
      request: (method, path, params) => request(account, method, path, params),
      get: (path, params) => request(account, "GET", path, params),
      post: (path, body) => request(account, "POST", path, body),
      patch: (path, body) => request(account, "PATCH", path, body),
      del: (path, params) => request(account, "DELETE", path, params),
    };
  },

  /** `pay_api`: one authenticated request to any Polar endpoint. */
  api(account, method, path, body) {
    return request(account, method, path, body);
  },

  /** A checkout session: a subscription to one product, with its trial, or a one-off charge of
   * the lines' total as an ad-hoc price of the account's one-off product. The module's metadata
   * is copied by Polar to the order and the subscription the checkout makes. */
  async createCheckout(account, input) {
    const mode = input.mode || "payment";
    if (mode !== "payment" && mode !== "subscription") throw new Error(`a checkout is in mode payment or subscription, not "${mode}"`);
    if (input.shipping) throw new Error("Polar is a merchant of record and sells digital goods only, so a checkout through it cannot collect a shipping address");
    const currency = currencyCode(input.currency);
    const body = {
      success_url: absolute(input.success_url, "address after paying"),
      metadata: input.metadata || {},
    };
    if (input.cancel_url) body.return_url = absolute(input.cancel_url, "address after cancelling");
    if (mode === "subscription") {
      const [line, ...more] = input.lines || [];
      if (!line || more.length) throw new Error("a subscription checkout is for one product");
      body.products = [polarId(line.price_ref, "product")];
      const quantity = line.quantity === undefined ? 1 : requireAmount(line.quantity, "the quantity");
      if (quantity !== 1) throw new Error(`Polar subscribes to one of a product, not ${quantity}: seats are a product's own pricing in Polar`);
      if (line.trial_days) {
        body.trial_interval = "day";
        body.trial_interval_count = requireAmount(line.trial_days, "the trial's days");
      }
    } else {
      const product = oneOffProduct(account);
      let total = 0;
      for (const [i, l] of (input.lines || []).entries()) {
        total += requireAmount(l.quantity === undefined ? 1 : l.quantity, `line ${i + 1}'s quantity`) * requireAmount(l.unit_amount, `line ${i + 1}'s unit amount`);
        if (!Number.isSafeInteger(total)) throw new Error("the checkout's total is too large to count exactly");
      }
      if (!(input.lines || []).length) throw new Error("a checkout charges at least one line");
      body.products = [product];
      body.currency = currency.toLowerCase();
      body.prices = { [product]: [{ amount_type: "fixed", price_amount: total, price_currency: currency.toLowerCase() }] };
    }
    if (input.customer && input.customer.ref) body.customer_id = polarId(input.customer.ref, "customer");
    else if (input.customer && input.customer.email) body.customer_email = input.customer.email;
    const checkout = await request(account, "POST", "/v1/checkouts/", body);
    return { id: checkout.id, url: checkout.url, object: normaliseCheckout(checkout) };
  },

  /** The events a delivery carries, or a refusal: a header missing, a signature made with neither
   * key the account's secret stands for, or a timestamp too far from now. */
  verifyWebhook(account, { rawBody, headers }) {
    const secret = account.webhook_secret;
    if (typeof secret !== "string" || !secret) {
      throw new Error(`the polar account "${account.name}" has no webhook signing secret; it is the secret shown on the endpoint in Polar's webhook settings`);
    }
    const h = headers || {};
    const id = h["webhook-id"];
    const timestamp = h["webhook-timestamp"];
    const header = h["webhook-signature"];
    if (!id || !timestamp || !header) throw new Error("the webhook lacks its webhook-id, webhook-timestamp or webhook-signature header");
    const t = Number(timestamp);
    if (!Number.isInteger(t)) throw new Error("the webhook-timestamp header is not a time in seconds");
    const given = String(header)
      .split(" ")
      .map((s) => s.split(","))
      .filter(([v, sig]) => v === "v1" && sig)
      .map(([, sig]) => Buffer.from(sig, "base64"));
    if (!given.length) throw new Error("the webhook-signature header has no v1 signature");
    const matches = signingKeys(secret).some((key) => {
      const expected = Buffer.from(sign(key, id, timestamp, rawBody).slice(3), "base64");
      return given.some((g) => g.length === expected.length && crypto.timingSafeEqual(g, expected));
    });
    if (!matches) throw new Error("the webhook's signature does not match this account's signing secret");
    if (Math.abs(Math.floor(Date.now() / 1000) - t) > TOLERANCE_SECONDS) throw new Error("the webhook's signature is too old to accept (a replay?)");
    return eventsOf(String(id), JSON.parse(Buffer.from(rawBody).toString("utf8")));
  },

  /** The module's kinds for a Polar event: `[]` for one it does not know, `null` for one that only
   * brings its object up to date. `previous` is the mirror row of the event's object before it. */
  kinds(event, context = {}) {
    if (!Object.prototype.hasOwnProperty.call(KINDS, event.type)) return [];
    const kinds = KINDS[event.type];
    return typeof kinds === "function" ? kinds(event, context) : kinds;
  },

  /** The current state of an object, normalised. */
  async fetch(account, ref) {
    const spec = ref && OBJECTS[ref.kind];
    if (!spec) throw new Error(`the polar adapter does not read ${ref && ref.kind} objects`);
    const id = polarId(ref.id, ref.kind);
    if (spec.byQuery) {
      const answer = await request(account, "GET", `${spec.path}/`, { id });
      const found = (answer.items || [])[0];
      if (!found) throw new Error(`Polar has no ${ref.kind} ${id}`);
      return spec.normalise(found);
    }
    return spec.normalise(await request(account, "GET", `${spec.path}/${id}`));
  },

  /** The objects of `kind` created since `since` (unix seconds), every page of them. */
  async list(account, kind, { since } = {}) {
    const spec = OBJECTS[kind];
    if (!spec || kind === "customer") throw new Error(`the polar adapter does not list ${kind} objects`);
    const old = (item) => since && Date.parse(item.created_at) / 1000 < since;
    return (await allPages(account, `${spec.path}/`, { sorting: "-created_at" }, old)).map(spec.normalise);
  },

  /** The events since `since` (unix seconds) that were never delivered, oldest first, read from
   * the account's webhook endpoint's deliveries. With no endpoint id set there is nothing to
   * read, and the sweep only reads the live subscriptions again. */
  async listEvents(account, { since } = {}) {
    const endpoint = account.settings && account.settings.webhook_endpoint_id;
    if (!endpoint) return [];
    const params = { endpoint_id: polarId(endpoint, "webhook endpoint") };
    if (since) params.start_timestamp = new Date(since * 1000).toISOString();
    const deliveries = await allPages(account, "/v1/webhooks/deliveries", params);
    const events = new Map();
    for (const d of deliveries) {
      const e = d.webhook_event;
      // Delivered is processed: a 200 is answered only once it was.
      if (!e || e.succeeded === true || !e.payload || events.has(e.id)) continue;
      events.set(e.id, e);
    }
    return [...events.values()]
      .sort((a, b) => Date.parse(a.created_at) - Date.parse(b.created_at))
      .flatMap((e) => eventsOf(e.id, JSON.parse(e.payload)));
  },

  /** A customer portal session, answered as `{ id, url }`: Polar's customer portal, where the
   * customer changes plan, changes card or cancels. */
  async createPortal(account, { customer, return_url } = {}) {
    const body = { customer_id: polarId(customer, "customer") };
    if (return_url) body.return_url = absolute(return_url, "address to go back to");
    const session = await request(account, "POST", "/v1/customer-sessions/", body);
    return { id: session.id, url: session.customer_portal_url };
  },

  /** Move a subscription to another product: prorated on the next invoice, or invoiced now. */
  async changePlan(account, ref, productId, { proration = "prorate" } = {}) {
    const behaviour = PRORATION[proration];
    if (!behaviour) {
      throw new Error(`Polar cannot change a plan with proration "${proration}": it prorates the change on the next invoice (prorate) or invoices it now (invoice)`);
    }
    const sub = await request(account, "PATCH", `/v1/subscriptions/${polarId(ref, "subscription")}`, {
      product_id: polarId(productId, "product"),
      proration_behavior: behaviour,
    });
    return normaliseSubscription(sub);
  },

  /** Cancel a subscription at the end of its period (it runs on until then), or now (revoke). */
  async cancel(account, ref, { at_period_end = true } = {}) {
    const path = `/v1/subscriptions/${polarId(ref, "subscription")}`;
    const sub = at_period_end ? await request(account, "PATCH", path, { cancel_at_period_end: true }) : await request(account, "DELETE", path);
    return normaliseSubscription(sub);
  },

  /** Refund `amount` (what the buyer paid, tax included) of an order, or all that is left, once per
   * `key`. Polar refunds a pre-tax amount, with the tax in proportion. */
  async refund(account, ref, { amount, key, reason } = {}) {
    const orderId = polarId(ref, "order");
    if (key) {
      const made = await request(account, "GET", "/v1/refunds/", { order_id: orderId, limit: 100 });
      const same = (made.items || []).find((r) => r.metadata && r.metadata.sc_key === key);
      if (same) return normaliseRefund(same);
    }
    const order = await request(account, "GET", `/v1/orders/${orderId}`);
    const net = order.refundable_amount || 0;
    const left = net + (order.refundable_tax_amount || 0);
    let value = net;
    if (amount !== undefined && amount !== null) {
      const asked = requireAmount(amount, "the refund's amount");
      if (asked > left) throw new Error(`the refund of ${asked} is more than the ${left} left of the order`);
      value = asked === left ? net : Math.round((asked * net) / left);
    }
    if (value <= 0) throw new Error("there is nothing left of the order to refund");
    const body = { order_id: orderId, amount: value, reason: REFUND_REASONS[reason] || (reason ? "other" : "customer_request"), metadata: {} };
    if (reason && !REFUND_REASONS[reason]) body.comment = String(reason).slice(0, 500);
    if (key) body.metadata.sc_key = String(key).slice(0, 500);
    return normaliseRefund(await request(account, "POST", "/v1/refunds/", body));
  },

  /**
   * Charge a customer's saved card with nobody at a checkout (P5.3): a draft order of the
   * account's one-off product at `amount`, finalized against the customer's default payment
   * method. Answers `{ status: "paid", object }`, or `{ status: "requires_checkout", reason }`
   * when the card was declined, there is none, or the bank asks the buyer to authenticate — for
   * the caller to send them to a checkout instead. Once per `key`: a paid order made with it is
   * answered again, and a draft one is finalized rather than made anew.
   */
  async chargeSavedMethod(account, { customer, amount, currency, description, key, metadata } = {}) {
    const customerId = polarId(customer, "customer");
    const value = requireAmount(amount, "the amount");
    if (value === 0) throw new Error("a charge of 0 charges nothing");
    let order = null;
    if (key) {
      const found = await request(account, "GET", "/v1/orders/", { customer_id: customerId, metadata: { sc_key: key }, limit: 10 });
      order = (found.items || []).find((o) => o.metadata && o.metadata.sc_key === key) || null;
    }
    if (order && order.status !== "draft") {
      return order.status === "paid" || order.status === "refunded" || order.status === "partially_refunded"
        ? { status: "paid", object: normaliseOrder(order) }
        : { status: "requires_checkout", reason: `the order made for ${key} is ${order.status}`, object: normaliseOrder(order) };
    }
    if (!order) {
      const body = { customer_id: customerId, product_id: oneOffProduct(account), amount: value, currency: currencyCode(currency).toLowerCase(), metadata: { ...(metadata || {}) } };
      if (description) body.description = String(description);
      if (key) body.metadata.sc_key = String(key).slice(0, 500);
      order = await request(account, "POST", "/v1/orders/", body);
    }
    try {
      const paid = await request(account, "POST", `/v1/orders/${order.id}/finalize`, {});
      return { status: "paid", object: normaliseOrder(paid) };
    } catch (e) {
      if (e.status === 402) return { status: "requires_checkout", reason: (e.polar && e.polar.detail) || e.message, object: normaliseOrder(order) };
      if (e.status === 403) {
        throw new Error(`the polar account "${account.name}" cannot charge a saved card: ${(e.polar && e.polar.detail) || e.message} (off-session charges are in preview, for organisations on a paid plan)`);
      }
      throw e;
    }
  },
};
