// A simulated Stripe (TODO-payments P5.2): the part of Stripe's API the adapter calls, in the shapes
// Stripe documents, kept in memory and put in the adapter's `transport`. It plays what happens at
// Stripe — a buyer paying on the hosted page, a test clock moving a subscription on — and sends the
// events Stripe sends, signed as Stripe signs them, to the module's webhook. A test drives it
// through `driver` (see `index.mjs`), as it drives the fake provider and the Polar simulation.
//
// Events are queued as Stripe makes them and delivered by `flush()`, after the request that caused
// them has been answered, as Stripe's arrive. An account the simulation holds records its events
// and delivers none: the sweep reads them from `GET /v1/events`.

import crypto from "node:crypto";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const stripe = require("../../adapters/stripe.js");

const DAY = 86400;
const PRICE = { unit_amount: 1000, days: 30 };
const now = () => Math.floor(Date.now() / 1000);
const id = (prefix) => `${prefix}_${crypto.randomBytes(10).toString("hex")}`;
const copy = (o) => JSON.parse(JSON.stringify(o));

/** Stripe's form encoding back into an object: `a[b][0][c]=x` is `{ a: { b: [{ c: "x" }] } }`. */
function decodeForm(text) {
  const out = {};
  for (const [name, value] of new URLSearchParams(text || "")) {
    const keys = name.replace(/\]/g, "").split("[");
    let at = out;
    keys.forEach((k, i) => {
      const last = i === keys.length - 1;
      const nextIsIndex = !last && /^\d+$/.test(keys[i + 1]);
      if (last) at[k] = value;
      else at = at[k] = at[k] || (nextIsIndex ? [] : {});
    });
  }
  return out;
}

export function stripeSimulation() {
  const s = {
    /** Accounts by secret key: `{ account, deliver, hold }`. */
    accounts: new Map(),
    customers: new Map(),
    sessions: new Map(),
    intents: new Map(),
    charges: new Map(),
    subscriptions: new Map(),
    refunds: new Map(),
    refundKeys: new Map(),
    /** Off-session PaymentIntents by idempotency key: the answer Stripe gave. */
    intentKeys: new Map(),
    events: [],
    queue: [],
  };

  const accountOf = (headers) => s.accounts.get(String(headers.authorization || "").replace(/^Bearer /, ""));

  /** Record an event about `object`, and queue it for delivery. */
  function emit(key, type, object, previous) {
    const event = { id: id("evt"), object: "event", type, created: now(), livemode: false, data: { object: copy(object) }, _key: key };
    if (previous) event.data.previous_attributes = previous;
    s.events.push(event);
    s.queue.push(event);
  }

  const lineItems = (session) => ({
    object: "list",
    data: session._lines.map((l) => ({ description: l.description, quantity: l.quantity, price: { unit_amount: l.unit_amount }, amount_total: l.unit_amount * l.quantity })),
  });

  const intentView = (pi, expand) => {
    const out = copy(pi);
    if (expand.includes("latest_charge") && pi.latest_charge) out.latest_charge = copy(s.charges.get(pi.latest_charge));
    return out;
  };

  function subscriptionView(sub) {
    return copy(sub);
  }

  function newIntent(key, { amount, currency, customer, metadata, email, outcome }) {
    const charge = { id: id("ch"), object: "charge", amount_refunded: 0, refunded: false, billing_details: { email: email || null }, payment_intent: null };
    const pi = {
      id: id("pi"),
      object: "payment_intent",
      amount,
      currency,
      customer: customer || null,
      metadata: metadata || {},
      receipt_email: email || null,
      created: now(),
      _key: key,
      status: outcome === "succeeded" ? "succeeded" : "requires_payment_method",
      last_payment_error: outcome === "succeeded" ? null : { code: "card_declined", message: "Your card was declined." },
      latest_charge: charge.id,
    };
    charge.payment_intent = pi.id;
    s.charges.set(charge.id, charge);
    s.intents.set(pi.id, pi);
    return pi;
  }

  /** The answer to one request, as Stripe would give it. */
  function handle(method, url, headers, body) {
    const entry = accountOf(headers);
    if (!entry) return [401, { error: { message: "Invalid API Key provided" } }];
    const key = entry.account.secret_key;
    const u = new URL(url);
    if (u.host !== "api.stripe.com") return [404, { error: { message: `no such host ${u.host}` } }];
    const path = u.pathname;
    const form = method === "GET" || method === "DELETE" ? decodeForm(u.search.slice(1)) : decodeForm(body);
    const expand = Object.values(form.expand || {});
    const mine = (map, oid) => {
      const o = map.get(oid);
      return o && o._key === key ? o : null;
    };
    let m;
    if (method === "POST" && path === "/v1/checkout/sessions") {
      const session = {
        id: id("cs_test"),
        object: "checkout.session",
        mode: form.mode,
        status: "open",
        currency: null,
        customer: form.customer || null,
        customer_email: form.customer_email || null,
        customer_details: null,
        payment_intent: null,
        subscription: null,
        metadata: form.metadata || {},
        customer_creation: form.customer_creation || null,
        success_url: form.success_url,
        cancel_url: form.cancel_url,
        created: now(),
        _key: key,
      };
      session.url = `https://checkout.stripe.com/c/pay/${session.id}`;
      if (form.mode === "subscription") {
        const item = form.line_items[0];
        const trial = form.subscription_data && form.subscription_data.trial_period_days ? Number(form.subscription_data.trial_period_days) : 0;
        session.currency = "eur";
        session._lines = [{ description: item.price, quantity: Number(item.quantity), unit_amount: PRICE.unit_amount }];
        session._sub = { price: item.price, quantity: Number(item.quantity), trial, metadata: (form.subscription_data && form.subscription_data.metadata) || {} };
        session.amount_total = trial ? 0 : PRICE.unit_amount * Number(item.quantity);
      } else {
        session.currency = form.line_items[0].price_data.currency;
        session._lines = form.line_items.map((l) => ({ description: l.price_data.product_data.name, quantity: Number(l.quantity), unit_amount: Number(l.price_data.unit_amount) }));
        session._pi_metadata = (form.payment_intent_data && form.payment_intent_data.metadata) || {};
        session._save = Boolean(form.payment_intent_data && form.payment_intent_data.setup_future_usage === "off_session");
        session.amount_total = session._lines.reduce((t, l) => t + l.quantity * l.unit_amount, 0);
      }
      s.sessions.set(session.id, session);
      return [200, { ...copy(session), line_items: lineItems(session) }];
    }
    if (method === "GET" && (m = /^\/v1\/checkout\/sessions\/([^/]+)$/.exec(path))) {
      const session = mine(s.sessions, m[1]);
      if (!session) return [404, { error: { message: `No such checkout.session: '${m[1]}'` } }];
      return [200, expand.includes("line_items") ? { ...copy(session), line_items: lineItems(session) } : copy(session)];
    }
    if (method === "GET" && (m = /^\/v1\/payment_intents\/([^/]+)$/.exec(path))) {
      const pi = mine(s.intents, m[1]);
      return pi ? [200, intentView(pi, expand)] : [404, { error: { message: `No such payment_intent: '${m[1]}'` } }];
    }
    if (method === "GET" && (m = /^\/v1\/customers\/([^/]+)\/payment_methods$/.exec(path))) {
      const c = mine(s.customers, m[1]);
      if (!c) return [404, { error: { message: `No such customer: '${m[1]}'` } }];
      const data = c._card ? [{ id: c._pm, object: "payment_method", type: "card", customer: c.id }] : [];
      return [200, { object: "list", data, has_more: false }];
    }
    if (method === "POST" && path === "/v1/payment_intents") {
      const idem = headers["idempotency-key"];
      if (idem && s.intentKeys.has(`${key}:${idem}`)) return copy(s.intentKeys.get(`${key}:${idem}`));
      const c = mine(s.customers, form.customer);
      if (!c) return [400, { error: { message: `No such customer: '${form.customer}'` } }];
      if (!c._card || form.payment_method !== c._pm) return [400, { error: { message: `No such PaymentMethod: '${form.payment_method}'` } }];
      if (form.off_session !== "true" || form.confirm !== "true") return [400, { error: { message: "the simulation takes off-session, confirmed PaymentIntents only" } }];
      const pi = newIntent(key, { amount: Number(form.amount), currency: form.currency, customer: c.id, metadata: form.metadata, email: c.email, outcome: c._card === "ok" ? "succeeded" : "failed" });
      pi.description = form.description || null;
      let answer;
      if (c._card === "ok") {
        emit(key, "payment_intent.succeeded", pi);
        answer = [200, intentView(pi, [])];
      } else {
        const error =
          c._card === "3ds"
            ? { code: "authentication_required", message: "This payment requires additional user action before it can be completed successfully." }
            : { code: "card_declined", message: "Your card was declined." };
        pi.last_payment_error = error;
        emit(key, "payment_intent.payment_failed", pi);
        answer = [402, { error: { ...error, type: "card_error", payment_intent: intentView(pi, []) } }];
      }
      if (idem) s.intentKeys.set(`${key}:${idem}`, copy(answer));
      return answer;
    }
    if (method === "GET" && (m = /^\/v1\/customers\/([^/]+)$/.exec(path))) {
      const c = mine(s.customers, m[1]);
      return c ? [200, copy(c)] : [404, { error: { message: `No such customer: '${m[1]}'` } }];
    }
    if ((m = /^\/v1\/subscriptions\/([^/]+)$/.exec(path))) {
      const sub = mine(s.subscriptions, m[1]);
      if (!sub) return [404, { error: { message: `No such subscription: '${m[1]}'` } }];
      if (method === "GET") return [200, subscriptionView(sub)];
      if (sub.status === "canceled") return [400, { error: { message: "A canceled subscription can only update its cancellation_details and metadata." } }];
      if (method === "DELETE") {
        sub.status = "canceled";
        sub.ended_at = now();
        emit(key, "customer.subscription.deleted", sub);
        return [200, subscriptionView(sub)];
      }
      if (method === "POST") {
        const before = {};
        if (form.items) {
          const item = sub.items.data[0];
          if (form.items[0].id !== item.id) return [400, { error: { message: `Invalid subscription item: '${form.items[0].id}'` } }];
          before.items = copy(sub.items);
          item.price = { ...item.price, id: form.items[0].price };
        }
        if (form.cancel_at_period_end !== undefined) {
          before.cancel_at_period_end = sub.cancel_at_period_end;
          sub.cancel_at_period_end = form.cancel_at_period_end === "true";
        }
        emit(key, "customer.subscription.updated", sub, before);
        return [200, subscriptionView(sub)];
      }
    }
    if (method === "POST" && path === "/v1/billing_portal/sessions") {
      const c = mine(s.customers, form.customer);
      if (!c) return [400, { error: { message: `No such customer: '${form.customer}'` } }];
      const sid = id("bps");
      return [200, { id: sid, object: "billing_portal.session", customer: c.id, return_url: form.return_url, url: `https://billing.stripe.com/p/session/test_${sid}` }];
    }
    if (method === "POST" && path === "/v1/refunds") {
      const idem = headers["idempotency-key"];
      if (idem && s.refundKeys.has(`${key}:${idem}`)) return [200, copy(s.refunds.get(s.refundKeys.get(`${key}:${idem}`)))];
      const pi = mine(s.intents, form.payment_intent);
      if (!pi || pi.status !== "succeeded") return [400, { error: { message: "This PaymentIntent does not have a successful charge to refund." } }];
      const charge = s.charges.get(pi.latest_charge);
      const left = pi.amount - charge.amount_refunded;
      const amount = form.amount === undefined ? left : Number(form.amount);
      if (amount > left) return [400, { error: { message: `Refund amount (${amount}) is greater than unrefunded amount on charge (${left})` } }];
      const refund = { id: id("re"), object: "refund", amount, currency: pi.currency, payment_intent: pi.id, status: "succeeded", metadata: form.metadata || {}, created: now(), _key: key };
      s.refunds.set(refund.id, refund);
      if (idem) s.refundKeys.set(`${key}:${idem}`, refund.id);
      charge.amount_refunded += amount;
      charge.refunded = charge.amount_refunded === pi.amount;
      emit(key, "charge.refunded", charge);
      return [200, copy(refund)];
    }
    if (method === "GET" && path === "/v1/events") {
      const since = form.created && form.created.gte ? Number(form.created.gte) : 0;
      const types = Object.values(form.types || {});
      const data = s.events.filter((e) => e._key === key && e.created >= since && (!types.length || types.includes(e.type))).reverse();
      return [200, { object: "list", data: data.map(copy), has_more: false }];
    }
    return [404, { error: { message: `Unrecognized request URL (${method}: ${path})` } }];
  }

  const send = async (url, init) => {
    const [status, json] = handle(String(init.method || "GET").toUpperCase(), url, init.headers || {}, init.body);
    return { ok: status < 300, status, text: async () => JSON.stringify(json) };
  };

  /** Deliver the queued events, oldest first, unless their account is held. */
  async function flush() {
    while (s.queue.length) {
      const event = s.queue.shift();
      const entry = s.accounts.get(event._key);
      if (!entry || entry.hold) continue;
      await deliverEvent(entry, event);
    }
  }

  /** One delivery of `event`, signed now: the status the module's handler answered. */
  async function deliverEvent(entry, event) {
    const { _key, ...wire } = event;
    const body = Buffer.from(JSON.stringify(wire));
    const t = now();
    const mac = crypto.createHmac("sha256", entry.account.webhook_secret).update(`${t}.`).update(body).digest("hex");
    try {
      return await entry.deliver(body, { "content-type": "application/json", "stripe-signature": `t=${t},v1=${mac}` });
    } catch (e) {
      return 500;
    }
  }

  /** The buyer pays (or is declined) on the hosted page of `sessionId`. */
  function pay(key, sessionId, outcome) {
    const session = s.sessions.get(sessionId);
    if (!session || session._key !== key) throw new Error(`Stripe has no session ${sessionId}`);
    if (session.status !== "open") return;
    const email = session.customer_email || (session.customer && s.customers.get(session.customer).email);
    if (session.mode === "payment" && outcome === "succeeded" && session._save) {
      // The card is saved to the buyer's customer, made now when Checkout was told to.
      let buyer = session.customer && s.customers.get(session.customer);
      if (!buyer && session.customer_creation === "always") buyer = newCustomer(key, email);
      if (buyer) {
        saveCard(buyer);
        session.customer = buyer.id;
      }
    }
    if (session.mode === "payment") {
      // A session keeps one PaymentIntent: a decline leaves it to be tried again.
      let pi = session.payment_intent && s.intents.get(session.payment_intent);
      if (!pi) {
        pi = newIntent(key, { amount: session.amount_total, currency: session.currency, customer: session.customer, metadata: session._pi_metadata, email, outcome });
        session.payment_intent = pi.id;
      } else if (outcome === "succeeded") {
        pi.status = "succeeded";
        pi.last_payment_error = null;
      }
      if (outcome === "succeeded") {
        session.status = "complete";
        session.customer_details = { email };
        emit(key, "checkout.session.completed", session);
        emit(key, "payment_intent.succeeded", pi);
      } else {
        emit(key, "payment_intent.payment_failed", pi);
      }
      return;
    }
    if (outcome !== "succeeded") return;
    const customer = (session.customer && s.customers.get(session.customer)) || newCustomer(key, email);
    // A subscription's card is kept for its renewals, and for charges with no page.
    saveCard(customer);
    const plan = session._sub;
    const start = now();
    const trialEnd = plan.trial ? start + plan.trial * DAY : null;
    const sub = {
      id: id("sub"),
      object: "subscription",
      status: trialEnd ? "trialing" : "active",
      customer: customer.id,
      items: { object: "list", data: [{ id: id("si"), price: { id: plan.price, unit_amount: PRICE.unit_amount, currency: "eur" }, quantity: plan.quantity, current_period_end: trialEnd || start + PRICE.days * DAY }] },
      trial_end: trialEnd,
      cancel_at_period_end: false,
      cancel_at: null,
      metadata: plan.metadata,
      created: start,
      _key: key,
      _days: PRICE.days,
    };
    s.subscriptions.set(sub.id, sub);
    session.status = "complete";
    session.customer = customer.id;
    session.subscription = sub.id;
    session.customer_details = { email };
    emit(key, "checkout.session.completed", session);
    emit(key, "customer.subscription.created", sub);
    emit(key, "invoice.paid", invoice(sub, "subscription_create"));
    if (!trialEnd) emit(key, "payment_intent.succeeded", newIntent(key, { amount: session.amount_total, currency: "eur", customer: customer.id, email, outcome: "succeeded" }));
  }

  function newCustomer(key, email) {
    const customer = { id: id("cus"), object: "customer", email, metadata: {}, invoice_settings: { default_payment_method: null }, created: now(), _key: key };
    s.customers.set(customer.id, customer);
    return customer;
  }

  /** The customer's card, saved: it pays (`ok`) until a test says otherwise. */
  function saveCard(customer) {
    customer._pm = customer._pm || id("pm");
    customer._card = "ok";
  }

  const invoice = (sub, reason) => ({ id: id("in"), object: "invoice", billing_reason: reason, parent: { subscription_details: { subscription: sub.id } }, customer: sub.customer, status: "paid" });

  /** The test clock: the subscription reaches the end of its period (or trial). */
  function renew(key, subId, outcome) {
    const sub = s.subscriptions.get(subId);
    if (!sub || sub._key !== key) throw new Error(`Stripe has no subscription ${subId}`);
    const item = sub.items.data[0];
    if (sub.cancel_at_period_end) {
      sub.status = "canceled";
      sub.ended_at = item.current_period_end;
      emit(key, "customer.subscription.deleted", sub);
      return;
    }
    const before = { status: sub.status, items: copy(sub.items) };
    item.current_period_end += sub._days * DAY;
    const email = s.customers.get(sub.customer).email;
    const pi = newIntent(key, { amount: item.price.unit_amount * item.quantity, currency: "eur", customer: sub.customer, email, outcome });
    if (outcome === "succeeded") {
      sub.status = "active";
      emit(key, "customer.subscription.updated", sub, before);
      emit(key, "invoice.paid", invoice(sub, "subscription_cycle"));
      emit(key, "payment_intent.succeeded", pi);
    } else {
      sub.status = "past_due";
      emit(key, "customer.subscription.updated", sub, before);
      emit(key, "payment_intent.payment_failed", pi);
    }
  }

  /** Stripe's retry of a past-due subscription's invoice. */
  function retry(key, subId, outcome) {
    const sub = s.subscriptions.get(subId);
    const item = sub.items.data[0];
    const pi = newIntent(key, { amount: item.price.unit_amount * item.quantity, currency: "eur", customer: sub.customer, email: s.customers.get(sub.customer).email, outcome });
    if (outcome === "succeeded") {
      sub.status = "active";
      emit(key, "customer.subscription.updated", sub, { status: "past_due" });
      emit(key, "invoice.paid", invoice(sub, "subscription_cycle"));
      emit(key, "payment_intent.succeeded", pi);
    } else {
      emit(key, "payment_intent.payment_failed", pi);
    }
  }

  /** The customer moves to another price in Stripe's customer portal. */
  function portalChange(key, subId, price) {
    const sub = s.subscriptions.get(subId);
    const before = { items: copy(sub.items) };
    sub.items.data[0].price = { ...sub.items.data[0].price, id: price };
    emit(key, "customer.subscription.updated", sub, before);
  }

  const driver = {
    name: "stripe",
    priceField: "stripe_price",
    prices: { basic: "price_basic", pro: "price_pro", staff: "price_staff", mystery: "price_mystery", gone: "price_gone" },
    /** What differs at Stripe: a renewal's PaymentIntent does not name its subscription (since
     * API version 2025-03-31), so its payment.succeeded names no application row. */
    renewalPaymentsKnowTheirRow: false,
    account(n, name, { hold = false } = {}) {
      return { id: n, name, adapter: "stripe", mode: "test", enabled: true, secret_key: `sk_test_sim_${name}`, webhook_secret: `whsec_sim_${name}`, settings: {}, _hold: hold };
    },
    install(accounts, deliverFor) {
      for (const a of accounts) s.accounts.set(a.secret_key, { account: a, deliver: deliverFor(a), hold: a._hold });
      driver.saved = stripe.transport.send;
      stripe.transport.send = send;
    },
    uninstall() {
      stripe.transport.send = driver.saved;
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
    async portalChange(account, sub, price) {
      portalChange(account.secret_key, sub, price);
      await flush();
    },
    events: (account) => s.events.filter((e) => e._key === account.secret_key).map((e) => ({ id: e.id, type: e.type })),
    async redeliver(account, eventId) {
      const event = s.events.find((e) => e.id === eventId && e._key === account.secret_key);
      return deliverEvent(s.accounts.get(account.secret_key), event);
    },
    forged(account) {
      const body = Buffer.from(JSON.stringify({ id: "evt_forged", object: "event", type: "payment_intent.succeeded", created: now(), livemode: false, data: { object: { id: "pi_x" } } }));
      return { rawBody: body, headers: { "stripe-signature": `t=${now()},v1=${"0".repeat(64)}` } };
    },
    unknown(account) {
      const body = Buffer.from(JSON.stringify({ id: "evt_odd", object: "event", type: "payout.paid", created: now(), livemode: false, data: { object: { id: "po_1", object: "payout" } } }));
      const t = now();
      const mac = crypto.createHmac("sha256", account.webhook_secret).update(`${t}.`).update(body).digest("hex");
      return { rawBody: body, headers: { "stripe-signature": `t=${t},v1=${mac}` } };
    },
    checkoutCustomer: (account, checkout) => s.sessions.get(checkout).customer,
    /** The customer's saved card: `ok`, `authenticate` (3-D Secure), `decline`, or null for none. */
    setCard(account, customer, card) {
      const c = s.customers.get(customer);
      c._card = card === "authenticate" ? "3ds" : card;
    },
  };
  return driver;
}
