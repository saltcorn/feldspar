// A simulated Polar (TODO-payments P5.2): the part of Polar's API the adapter calls, in the shapes of
// Polar's OpenAPI description (version 2026-10), kept in memory and put in the adapter's
// `transport`. Like the Stripe simulation it plays what happens at the provider — a buyer paying,
// a subscription's period ending — and sends Polar's events, signed as Standard Webhooks, to the
// module's webhook, after the request that caused them has been answered.
//
// Where Polar behaves its own way, so does this: a checkout always makes a customer; a renewal is
// an order (`billing_reason: subscription_cycle`), left `pending` while its payment fails; a
// checkout's success is reported by more than one `checkout.updated`; and an account it holds has
// its deliveries fail (the endpoint unreachable), which the sweep reads back from
// `GET /v1/webhooks/deliveries`.

import crypto from "node:crypto";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const polar = require("../../adapters/polar.js");

const DAY_MS = 86400000;
const PRICE = { amount: 1000, days: 30 };
const uuid = () => crypto.randomUUID();
const copy = (o) => JSON.parse(JSON.stringify(o));

/** Polar's clock: every object's `modified_at` is later than the last one's. */
let last = 0;
function stamp() {
  last = Math.max(Date.now(), last + 1);
  return new Date(last).toISOString();
}

export function polarSimulation() {
  const s = {
    /** Accounts by access token: `{ account, deliver, hold }`. */
    accounts: new Map(),
    checkouts: new Map(),
    orders: new Map(),
    subscriptions: new Map(),
    customers: new Map(),
    refunds: new Map(),
    /** Webhook events, oldest first: `{ id, type, created_at, payload, succeeded, _key }`. */
    events: [],
    deliveries: [],
    queue: [],
  };
  /** Which products are products: anything is, at 10.00 for 30 days. */
  const products = { oneOff: uuid(), basic: uuid(), pro: uuid(), staff: uuid(), mystery: uuid(), gone: uuid() };

  const accountOf = (headers) => s.accounts.get(String(headers.authorization || "").replace(/^Bearer /, ""));

  function emit(key, type, data) {
    const event = { id: uuid(), type, created_at: stamp(), payload: JSON.stringify({ type, timestamp: new Date().toISOString(), api_version: "2026-10", data: copy(data) }), succeeded: null, _key: key };
    s.events.push(event);
    s.queue.push(event);
  }

  // A checkout with no customer: the customer of that external id, else — as Polar is suspected
  // to (security review #35) — the customer of that email, else a new one.
  function customerFor(key, { id, email, external_id }) {
    if (id) return s.customers.get(id);
    const all = [...s.customers.values()].filter((c) => c._key === key);
    const found = (external_id && all.find((c) => c.external_id === external_id)) || (email && all.find((c) => c.email === email));
    if (found) return found;
    const c = { id: uuid(), created_at: stamp(), modified_at: null, email, external_id: external_id || null, metadata: {}, deleted_at: null, _key: key };
    s.customers.set(c.id, c);
    return c;
  }

  const customerView = (c) => ({ id: c.id, email: c.email, external_id: c.external_id, metadata: c.metadata });

  function newOrder(key, { customer, amount, currency, reason, checkout, subscription, metadata, status, product, description }) {
    const o = {
      id: uuid(),
      created_at: stamp(),
      modified_at: null,
      status,
      paid: status === "paid",
      subtotal_amount: amount,
      discount_amount: 0,
      net_amount: amount,
      tax_amount: 0,
      total_amount: amount,
      refunded_amount: 0,
      refunded_tax_amount: 0,
      refundable_amount: status === "paid" ? amount : 0,
      refundable_tax_amount: 0,
      currency,
      billing_reason: reason,
      customer_id: customer.id,
      product_id: product,
      subscription_id: subscription || null,
      checkout_id: checkout || null,
      metadata: metadata || {},
      customer: customerView(customer),
      items: [{ id: uuid(), label: description || "Order", amount, tax_amount: 0, proration: false }],
      description: description || "Order",
      _key: key,
    };
    s.orders.set(o.id, o);
    return o;
  }

  function payOrder(o) {
    o.status = "paid";
    o.paid = true;
    o.refundable_amount = o.net_amount;
    o.modified_at = stamp();
  }

  /** An object as Polar answers it: without the simulation's own `_` fields. */
  const view = (o) => copy(Object.fromEntries(Object.entries(o).filter(([k]) => !k.startsWith("_"))));
  const subView = (sub) => ({ ...view(sub), customer: customerView(s.customers.get(sub.customer_id)) });

  function handle(method, url, headers, body) {
    const entry = accountOf(headers);
    if (!entry) return [401, { error: "Unauthorized", detail: "Invalid access token" }];
    const key = entry.account.secret_key;
    const u = new URL(url);
    if (u.host !== "sandbox-api.polar.sh") return [404, { detail: `the test account must use the sandbox, not ${u.host}` }];
    if (headers["polar-version"] !== "2026-10") return [404, { detail: "unknown API version" }];
    const path = u.pathname;
    const q = u.searchParams;
    const json = body ? JSON.parse(body) : {};
    const mine = (map, oid) => {
      const o = map.get(oid);
      return o && o._key === key ? o : null;
    };
    const notFound = (what) => [404, { error: "ResourceNotFound", detail: `${what} not found` }];
    let m;
    if (method === "POST" && path === "/v1/checkouts/") {
      const product = json.products[0];
      const adhoc = json.prices && json.prices[product] && json.prices[product][0];
      const subscription = product !== entry.account.settings.product;
      const trialDays = json.trial_interval === "day" ? json.trial_interval_count : 0;
      const amount = adhoc ? adhoc.price_amount : PRICE.amount;
      const c = {
        id: uuid(),
        created_at: stamp(),
        modified_at: null,
        status: "open",
        client_secret: `polar_c_${crypto.randomBytes(8).toString("hex")}`,
        success_url: json.success_url,
        return_url: json.return_url || null,
        amount,
        net_amount: amount,
        tax_amount: null,
        total_amount: trialDays ? 0 : amount,
        currency: (adhoc && adhoc.price_currency) || json.currency || "eur",
        trial_end: trialDays ? new Date(Date.now() + trialDays * DAY_MS).toISOString() : null,
        product_id: product,
        customer_id: json.customer_id || null,
        customer_email: json.customer_email || (json.customer_id ? s.customers.get(json.customer_id).email : null),
        external_customer_id: json.external_customer_id || null,
        metadata: json.metadata || {},
        subscription_id: null,
        _key: key,
        _sub: subscription ? { trialDays } : null,
      };
      c.url = `https://sandbox.polar.sh/checkout/${c.client_secret}`;
      s.checkouts.set(c.id, c);
      return [201, view(c)];
    }
    if (method === "GET" && (m = /^\/v1\/checkouts\/([^/]+)$/.exec(path))) {
      const c = mine(s.checkouts, m[1]);
      return c ? [200, view(c)] : notFound("Checkout");
    }
    if (method === "GET" && (m = /^\/v1\/orders\/([^/]+)$/.exec(path))) {
      const o = mine(s.orders, m[1]);
      return o ? [200, view(o)] : notFound("Order");
    }
    if (method === "GET" && path === "/v1/orders/") {
      const sc = q.get("metadata[sc_key]");
      const items = [...s.orders.values()].filter((o) => o._key === key && (!q.get("customer_id") || o.customer_id === q.get("customer_id")) && (!sc || o.metadata.sc_key === sc));
      return [200, { items: items.map(view), pagination: { total_count: items.length, max_page: 1 } }];
    }
    if (method === "POST" && path === "/v1/orders/") {
      const customer = mine(s.customers, json.customer_id);
      if (!customer) return [422, { detail: [{ loc: ["body", "customer_id"], msg: "Customer does not exist." }] }];
      const o = newOrder(key, { customer, amount: json.amount, currency: json.currency, reason: "purchase", metadata: json.metadata, status: "draft", product: json.product_id, description: json.description });
      emit(key, "order.created", view(o));
      return [201, view(o)];
    }
    if (method === "POST" && (m = /^\/v1\/orders\/([^/]+)\/finalize$/.exec(path))) {
      const o = mine(s.orders, m[1]);
      if (!o) return notFound("Order");
      if (o.status !== "draft") return [412, { error: "OrderNotDraft", detail: "The order is not a draft." }];
      if (!entry.account._offSession) return [403, { error: "OffSessionChargesNotEnabled", detail: "Off-session charges are not enabled for this organization." }];
      const card = s.customers.get(o.customer_id)._card;
      if (card !== "ok") {
        return [402, card === "3ds" ? { error: "PaymentActionRequired", detail: "The payment requires the customer to authenticate." } : { error: "PaymentFailed", detail: "The customer has no payment method." }];
      }
      payOrder(o);
      emit(key, "order.paid", view(o));
      return [200, view(o)];
    }
    if (method === "GET" && (m = /^\/v1\/customers\/([^/]+)$/.exec(path))) {
      const c = mine(s.customers, m[1]);
      return c ? [200, view(c)] : notFound("Customer");
    }
    if ((m = /^\/v1\/subscriptions\/([^/]+)$/.exec(path))) {
      const sub = mine(s.subscriptions, m[1]);
      if (!sub) return notFound("Subscription");
      if (method === "GET") return [200, subView(sub)];
      if (sub.status === "canceled") return [403, { error: "AlreadyCanceledSubscription", detail: "This subscription is already canceled." }];
      if (method === "DELETE") {
        revoke(key, sub);
        return [200, subView(sub)];
      }
      if (method === "PATCH") {
        if (json.product_id) {
          if (!["prorate", "invoice", "next_period", "reset"].includes(json.proration_behavior)) return [422, { detail: [{ loc: ["body", "proration_behavior"], msg: "Input should be 'invoice', 'prorate', 'next_period' or 'reset'" }] }];
          sub.product_id = json.product_id;
          sub.modified_at = stamp();
          emit(key, "subscription.updated", subView(sub));
        }
        if (json.cancel_at_period_end === true) {
          sub.cancel_at_period_end = true;
          sub.canceled_at = sub.modified_at = stamp();
          sub.ends_at = sub.current_period_end;
          emit(key, "subscription.canceled", subView(sub));
          emit(key, "subscription.updated", subView(sub));
        }
        return [200, subView(sub)];
      }
    }
    if (method === "POST" && path === "/v1/customer-sessions/") {
      const c = mine(s.customers, json.customer_id);
      if (!c) return [422, { detail: [{ loc: ["body", "customer_id"], msg: "Customer does not exist." }] }];
      const token = `polar_cst_${crypto.randomBytes(8).toString("hex")}`;
      return [201, { id: uuid(), token, expires_at: new Date(Date.now() + 3600000).toISOString(), return_url: json.return_url || null, customer_portal_url: `https://sandbox.polar.sh/portal?customer_session_token=${token}`, customer_id: c.id }];
    }
    if (method === "GET" && path === "/v1/refunds/") {
      const items = [...s.refunds.values()].filter((r) => r._key === key && (!q.get("order_id") || r.order_id === q.get("order_id")) && (!q.get("id") || r.id === q.get("id")));
      return [200, { items: items.map(view), pagination: { total_count: items.length, max_page: 1 } }];
    }
    if (method === "POST" && path === "/v1/refunds/") {
      const o = mine(s.orders, json.order_id);
      if (!o) return notFound("Order");
      if (!(json.amount > 0) || json.amount > o.refundable_amount) return [400, { error: "RefundAmountTooHigh", detail: "Refund amount exceeds remaining order balance." }];
      if (!["duplicate", "fraudulent", "customer_request", "service_disruption", "satisfaction_guarantee", "other"].includes(json.reason)) return [422, { detail: [{ loc: ["body", "reason"], msg: "not a reason" }] }];
      const r = { id: uuid(), created_at: stamp(), modified_at: null, metadata: json.metadata || {}, status: "succeeded", reason: json.reason, amount: json.amount, tax_amount: 0, currency: o.currency, order_id: o.id, subscription_id: o.subscription_id, customer_id: o.customer_id, revoke_benefits: false, dispute: null, _key: key };
      s.refunds.set(r.id, r);
      o.refunded_amount += json.amount;
      o.refundable_amount -= json.amount;
      o.status = o.refundable_amount === 0 ? "refunded" : "partially_refunded";
      o.modified_at = stamp();
      emit(key, "refund.created", view(r));
      emit(key, "order.refunded", view(o));
      emit(key, "order.updated", view(o));
      return [201, view(r)];
    }
    if (method === "GET" && path === "/v1/webhooks/deliveries") {
      if (q.get("endpoint_id") !== entry.account.settings.webhook_endpoint_id) return [200, { items: [], pagination: { total_count: 0, max_page: 1 } }];
      const since = q.get("start_timestamp") ? Date.parse(q.get("start_timestamp")) : 0;
      const items = s.deliveries.filter((d) => d._key === key && Date.parse(d.created_at) >= since);
      const page = Number(q.get("page") || 1);
      const limit = Number(q.get("limit") || 10);
      const slice = items.slice((page - 1) * limit, page * limit).map((d) => {
        const { _key, _event, ...out } = d;
        const e = s.events.find((x) => x.id === _event);
        return { ...out, webhook_event: { id: e.id, created_at: e.created_at, succeeded: e.succeeded, payload: e.payload, type: e.type, api_version: "2026-10", skipped: false, is_archived: false } };
      });
      return [200, { items: slice, pagination: { total_count: items.length, max_page: Math.max(1, Math.ceil(items.length / limit)) } }];
    }
    return [404, { detail: "Not Found" }];
  }

  const send = async (url, init) => {
    const [status, json] = handle(String(init.method || "GET").toUpperCase(), url, init.headers || {}, init.body);
    return { ok: status < 300, status, text: async () => JSON.stringify(json) };
  };

  /** One delivery attempt of `event`, signed now, recorded as Polar records it. A held account's
   * endpoint is unreachable. */
  async function attempt(entry, event) {
    const delivery = { id: uuid(), created_at: stamp(), succeeded: false, http_code: null, response: null, _key: event._key, _event: event.id };
    s.deliveries.push(delivery);
    if (entry.hold) {
      delivery.response = "endpoint unreachable";
      event.succeeded = event.succeeded || false;
      return null;
    }
    const t = Math.floor(Date.now() / 1000);
    const signature = polar.sign(Buffer.from(entry.account.webhook_secret.slice("whsec_".length), "base64"), event.id, t, event.payload);
    let status;
    try {
      status = await entry.deliver(Buffer.from(event.payload), { "content-type": "application/json", "webhook-id": event.id, "webhook-timestamp": String(t), "webhook-signature": signature });
    } catch (_) {
      status = 500;
    }
    delivery.http_code = status;
    delivery.succeeded = status >= 200 && status < 300;
    event.succeeded = event.succeeded === true || delivery.succeeded;
    return status;
  }

  async function flush() {
    while (s.queue.length) {
      const event = s.queue.shift();
      await attempt(s.accounts.get(event._key), event);
    }
  }

  function revoke(key, sub) {
    sub.status = "canceled";
    sub.ended_at = sub.canceled_at = sub.modified_at = stamp();
    sub.ends_at = sub.ended_at;
    emit(key, "subscription.canceled", subView(sub));
    emit(key, "subscription.revoked", subView(sub));
    emit(key, "subscription.updated", subView(sub));
  }

  function pay(key, checkoutId, outcome) {
    const c = s.checkouts.get(checkoutId);
    if (!c || c._key !== key) throw new Error(`Polar has no checkout ${checkoutId}`);
    if (c.status === "succeeded") return;
    c.modified_at = stamp();
    if (outcome !== "succeeded") {
      c.status = "failed";
      emit(key, "checkout.updated", view(c));
      return;
    }
    const customer = customerFor(key, { id: c.customer_id, email: c.customer_email, external_id: c.external_customer_id });
    customer._card = "ok";
    c.customer_id = customer.id;
    c.status = "succeeded";
    if (!c._sub) {
      const o = newOrder(key, { customer, amount: c.total_amount, currency: c.currency, reason: "purchase", checkout: c.id, metadata: c.metadata, status: "pending", product: c.product_id });
      emit(key, "checkout.updated", view(c));
      emit(key, "order.created", view(o));
      payOrder(o);
      emit(key, "order.paid", view(o));
      // Polar tells of a checkout's success more than once.
      c.modified_at = stamp();
      emit(key, "checkout.updated", view(c));
      return;
    }
    const start = Date.now();
    const trialEnd = c._sub.trialDays ? start + c._sub.trialDays * DAY_MS : null;
    const end = trialEnd || start + PRICE.days * DAY_MS;
    const sub = {
      id: uuid(),
      created_at: stamp(),
      modified_at: null,
      amount: PRICE.amount,
      currency: c.currency,
      recurring_interval: "month",
      status: trialEnd ? "trialing" : "active",
      current_period_start: new Date(start).toISOString(),
      current_period_end: new Date(end).toISOString(),
      trial_start: trialEnd ? new Date(start).toISOString() : null,
      trial_end: trialEnd ? new Date(trialEnd).toISOString() : null,
      cancel_at_period_end: false,
      canceled_at: null,
      started_at: new Date(start).toISOString(),
      ends_at: null,
      ended_at: null,
      customer_id: customer.id,
      product_id: c.product_id,
      checkout_id: c.id,
      seats: null,
      metadata: c.metadata,
      _key: key,
      _days: PRICE.days,
    };
    s.subscriptions.set(sub.id, sub);
    emit(key, "checkout.updated", view(c));
    emit(key, "subscription.created", subView(sub));
    if (!trialEnd) {
      emit(key, "subscription.active", subView(sub));
      const o = newOrder(key, { customer, amount: PRICE.amount, currency: c.currency, reason: "subscription_create", checkout: c.id, subscription: sub.id, metadata: c.metadata, status: "paid", product: c.product_id });
      emit(key, "order.created", view(o));
      emit(key, "order.paid", view(o));
    }
  }

  function renew(key, subId, outcome) {
    const sub = s.subscriptions.get(subId);
    if (!sub || sub._key !== key) throw new Error(`Polar has no subscription ${subId}`);
    if (sub.cancel_at_period_end) {
      sub.status = "canceled";
      sub.ended_at = sub.current_period_end;
      sub.modified_at = stamp();
      emit(key, "subscription.revoked", subView(sub));
      emit(key, "subscription.updated", subView(sub));
      return;
    }
    const wasTrial = sub.status === "trialing";
    sub.current_period_start = sub.current_period_end;
    sub.current_period_end = new Date(Date.parse(sub.current_period_end) + sub._days * DAY_MS).toISOString();
    sub.modified_at = stamp();
    const customer = s.customers.get(sub.customer_id);
    const o = newOrder(key, { customer, amount: sub.amount, currency: sub.currency, reason: "subscription_cycle", subscription: sub.id, metadata: sub.metadata, status: "pending", product: sub.product_id });
    emit(key, "subscription.cycled", subView(sub));
    emit(key, "order.created", view(o));
    if (outcome === "succeeded") {
      sub.status = "active";
      payOrder(o);
      if (wasTrial) emit(key, "subscription.active", subView(sub));
      emit(key, "subscription.updated", subView(sub));
      emit(key, "order.paid", view(o));
    } else {
      sub.status = "past_due";
      emit(key, "subscription.past_due", subView(sub));
      emit(key, "subscription.updated", subView(sub));
    }
  }

  function retry(key, subId, outcome) {
    const sub = s.subscriptions.get(subId);
    const o = [...s.orders.values()].reverse().find((x) => x.subscription_id === sub.id && x.status === "pending");
    o.modified_at = stamp();
    if (outcome !== "succeeded") {
      emit(key, "order.updated", view(o));
      return;
    }
    payOrder(o);
    sub.status = "active";
    sub.modified_at = stamp();
    emit(key, "subscription.active", subView(sub));
    emit(key, "subscription.updated", subView(sub));
    emit(key, "order.paid", view(o));
  }

  function portalChange(key, subId, product) {
    const sub = s.subscriptions.get(subId);
    sub.product_id = product;
    sub.modified_at = stamp();
    emit(key, "subscription.updated", subView(sub));
  }

  const signed = (account, payload, eventId = uuid()) => {
    const t = Math.floor(Date.now() / 1000);
    const body = Buffer.from(JSON.stringify(payload));
    return { rawBody: body, headers: { "webhook-id": eventId, "webhook-timestamp": String(t), "webhook-signature": polar.sign(Buffer.from(account.webhook_secret.slice(6), "base64"), eventId, t, body) } };
  };

  const driver = {
    name: "polar",
    priceField: "polar_product",
    prices: { basic: products.basic, pro: products.pro, staff: products.staff, mystery: products.mystery, gone: products.gone },
    renewalPaymentsKnowTheirRow: true,
    products,
    simulation: s,
    account(n, name, { hold = false } = {}) {
      return {
        id: n,
        name,
        adapter: "polar",
        mode: "test",
        enabled: true,
        secret_key: `polar_oat_sim_${name}`,
        webhook_secret: `whsec_${Buffer.from(`secret of ${name}`).toString("base64")}`,
        settings: { product: products.oneOff, webhook_endpoint_id: uuid(), grants_in_test_mode: true },
        _hold: hold,
        // The organisation is on a plan with off-session charges (P5.3).
        _offSession: true,
      };
    },
    install(accounts, deliverFor) {
      for (const a of accounts) s.accounts.set(a.secret_key, { account: a, deliver: deliverFor(a), hold: a._hold });
      driver.saved = polar.transport.send;
      polar.transport.send = send;
    },
    uninstall() {
      polar.transport.send = driver.saved;
    },
    flush,
    async pay(account, checkout, outcome) {
      pay(account.secret_key, checkout, outcome);
      await flush();
    },
    async renew(account, sub, outcome) {
      renew(account.secret_key, sub, outcome);
      await flush();
    },
    async retry(account, sub, outcome) {
      retry(account.secret_key, sub, outcome);
      await flush();
    },
    async portalChange(account, sub, product) {
      portalChange(account.secret_key, sub, product);
      await flush();
    },
    events: (account) => s.events.filter((e) => e._key === account.secret_key).map((e) => ({ id: e.id, type: e.type })),
    async redeliver(account, eventId) {
      const event = s.events.find((e) => e.id === eventId && e._key === account.secret_key);
      return attempt(s.accounts.get(account.secret_key), event);
    },
    forged(account) {
      const { rawBody, headers } = signed(account, { type: "order.paid", timestamp: new Date().toISOString(), data: { id: uuid() } });
      return { rawBody, headers: { ...headers, "webhook-signature": `v1,${Buffer.alloc(32).toString("base64")}` } };
    },
    unknown: (account) => signed(account, { type: "benefit.created", timestamp: new Date().toISOString(), api_version: "2026-10", data: { id: uuid(), type: "custom" } }),
    signed,
    checkoutCustomer: (account, checkout) => s.checkouts.get(checkout).customer_id,
    /** The customer's saved card: `ok`, `authenticate` (3-D Secure), `decline`, or null for none. */
    setCard(account, customer, card) {
      s.customers.get(customer)._card = card === "authenticate" ? "3ds" : card;
    },
  };
  return driver;
}
