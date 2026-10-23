// What a provider event does here (TODO-payments §3.6, §6.5, P2.3): the part of the webhook
// handler after the signature is checked, shared with the reconciliation sweep (P2.7).
//
// An event says *what changed*; it is not trusted to say what things now are. Providers deliver
// at least once and in no guaranteed order, so for each event the adapter **fetches the object's
// current state**, the mirror (`pay_objects`) is upserted from it, and the normalised
// `pay_events` rows are inserted — which is what the application's triggers react to. Processing
// an event twice, or two events out of order, ends in the same mirror and the same log.
//
// There is no transaction around the steps: the module's `db` has none. Each step is idempotent
// instead — the mirror is an upsert keyed by the provider's id, and an event row is inserted only
// when its `(account, event_id, kind)` is not there yet — so a delivery that fails half way is
// finished by the provider's retry, and no event row (and so no trigger) happens twice.
//
// Between the mirror and the log come the only two effects the module applies itself (§3.7):
// a subscription's role grant, worked out from the subscription's *current* state and its
// plan (`plans.js`, P4.1), and a one-off payment's *grant a role* effect. Both are upserts by a
// reference, so they too may happen any number of times. What cannot be done — a price that is
// no plan's, a role above the module's ceiling — is written on the mirror row as its `issue`,
// and logged.
//
// `ctx` is what the caller has of the module: `db`, and, to grant, `roles` (C4) and
// `configuration` (the module's settings, which hold the plan binding).

"use strict";

const { EVENT_KINDS } = require("./tables.js");
const plans = require("./plans.js");

const nowIso = () => new Date().toISOString();

/** The kinds an adapter maps a provider event to: its `kinds(event)`, else the event's own type
 * when that already is one of ours (the fake's are). Empty is an event the module does not
 * understand, logged as `other`. `null` is one it understands that says nothing an application
 * reacts to — Stripe's invoice for a subscription's first period, say — whose object is still
 * brought up to date. */
function kindsOf(adapter, event) {
  const kinds = typeof adapter.kinds === "function" ? adapter.kinds(event) : EVENT_KINDS.includes(event.type) ? [event.type] : [];
  if (kinds === null) return null;
  return (kinds || []).filter((k) => EVENT_KINDS.includes(k) && k !== "other");
}

/** Where a provider object came from in the application, from what the module put into the
 * provider's metadata when it made the checkout. */
function fromMetadata(metadata) {
  const m = metadata || {};
  const out = {};
  if (m.sc_ref_table) out.ref_table = String(m.sc_ref_table);
  if (m.sc_ref_key !== undefined && m.sc_ref_key !== null && m.sc_ref_key !== "") {
    // Kept as JSON, since provider metadata is text: `12` is the integer key, `"12"` the text.
    try {
      out.ref_key = JSON.parse(m.sc_ref_key);
    } catch (_) {
      out.ref_key = String(m.sc_ref_key);
    }
  }
  if (m.sc_user_id !== undefined && m.sc_user_id !== null && m.sc_user_id !== "") {
    const id = Number(m.sc_user_id);
    out.user_id = Number.isSafeInteger(id) ? id : m.sc_user_id;
  }
  return out;
}

/** What a mirror row learns from a provider object, leaving out what the object does not say:
 * a provider that stops sending an email has not erased it. */
function mirrorValues(object) {
  const values = {
    kind: object.kind,
    status: object.status === undefined ? null : object.status,
    provider_data: object.provider_data === undefined ? null : object.provider_data,
    updated_at: nowIso(),
  };
  for (const key of ["amount", "currency", "email", "price_ref", "current_period_end", "trial_end", "cancel_at_period_end"]) {
    if (object[key] !== undefined && object[key] !== null) values[key] = object[key];
  }
  if (Array.isArray(object.lines) && object.lines.length) values.lines = object.lines;
  return values;
}

/** The mirror row of `object`, inserted or brought up to date. The application row it is for,
 * and its user, come from the row as the checkout action wrote it; failing that from the
 * object it belongs to — a payment inherits its checkout's or its subscription's, a
 * subscription its checkout's — and failing that from the provider's metadata. */
async function upsertObject(db, account, object) {
  const key = { account: account.id, provider_ref: String(object.ref) };
  const existing = await db.pay_objects.where(key).first();
  const values = mirrorValues(object);
  const origin = { ref_table: null, ref_key: null, user_id: null, email: null };
  for (const parentRef of [object.checkout_ref, object.subscription_ref]) {
    if (!parentRef || parentRef === object.ref) continue;
    const parent = await db.pay_objects.where({ account: account.id, provider_ref: String(parentRef) }).first();
    if (!parent) continue;
    for (const k of Object.keys(origin)) if (origin[k] === null && parent[k] !== null && parent[k] !== undefined) origin[k] = parent[k];
    if (parent.kind === "checkout" && parent.effects && object.kind === "payment") values.effects = parent.effects;
  }
  for (const [k, v] of Object.entries(fromMetadata(object.metadata))) if (origin[k] === null) origin[k] = v;
  for (const [k, v] of Object.entries(origin)) {
    if (v !== null && (!existing || existing[k] === null || existing[k] === undefined) && values[k] === undefined) values[k] = v;
  }
  if (existing) {
    await db.pay_objects.where({ id: existing.id }).update(values);
    return { ...existing, ...values };
  }
  try {
    return await db.pay_objects.insert({ ...key, ...values, created_at: nowIso() });
  } catch (e) {
    // Two deliveries about one object, at once: the other inserted it first.
    const raced = await db.pay_objects.where(key).first();
    if (!raced) throw e;
    await db.pay_objects.where({ id: raced.id }).update(values);
    return { ...raced, ...values };
  }
}

/** The customer mirror row of a provider customer an object names, with the user it is: which
 * provider customer a user is, so that the portal opens on their account (§5). Only the user,
 * the email and the reference are known here — a customer is not fetched for itself. */
async function upsertCustomer(db, account, ref, { user_id, email }) {
  const key = { account: account.id, provider_ref: String(ref) };
  const existing = await db.pay_objects.where(key).first();
  const values = { updated_at: nowIso() };
  if (user_id !== null && user_id !== undefined && (!existing || existing.user_id === null || existing.user_id === undefined)) values.user_id = user_id;
  if (email && (!existing || !existing.email)) values.email = email;
  if (existing) {
    if (Object.keys(values).length > 1) await db.pay_objects.where({ id: existing.id }).update(values);
    return { ...existing, ...values };
  }
  try {
    return await db.pay_objects.insert({ ...key, kind: "customer", ...values, created_at: nowIso() });
  } catch (e) {
    const raced = await db.pay_objects.where(key).first();
    if (!raced) throw e;
    return raced;
  }
}

/** The reference a module's grant for `object` is made under: one per subscription, or per
 * payment for a one-off pass. */
const grantRef = (account, object) => `${account.name}:${object.ref}`;

/** What a subscription's current state does to its role grant (§6.2), and the issue, if any. */
async function subscriptionEffects(ctx, account, current, row) {
  const ref = grantRef(account, current);
  const binding = plans.planBinding(ctx.configuration);
  const decision = plans.subscriptionGrant(current, binding ? binding.grace_days : plans.DEFAULT_GRACE_DAYS);
  if (decision.op === "keep") return decision.problem || null;
  if (decision.op === "revoke") {
    if (ctx.roles) await ctx.roles.revoke(ref);
    return null;
  }
  const found = await plans.findPlan(ctx.db, binding, account, current.price_ref);
  if (found.problem) return found.problem;
  const role = plans.planRole(binding, found.plan);
  if (role.problem) return role.problem;
  if (row.user_id === null || row.user_id === undefined) return "the subscription is no user's of this instance, so it grants no role";
  if (!ctx.roles) return "this delivery could not grant roles";
  await ctx.roles.grant({ user: row.user_id, role: role.role, ref, expires_at: decision.expires_at, note: `subscription ${current.ref}` });
  return null;
}

/** A one-off payment's *grant a role* effect (§6.1): granted, for its days, once it succeeded,
 * and revoked when all of it is refunded. The days are counted from when it was made, so
 * granting it again changes nothing. */
async function paymentEffects(ctx, account, current, row) {
  const effect = row.effects && row.effects.role;
  if (!effect) return null;
  const ref = grantRef(account, current);
  if (current.status === "refunded") {
    if (ctx.roles) await ctx.roles.revoke(ref);
    return null;
  }
  if (current.status !== "succeeded" && current.status !== "partially_refunded") return null;
  if (row.user_id === null || row.user_id === undefined) return "the payment is no user's of this instance, so its role was not granted";
  if (!ctx.roles) return "this delivery could not grant roles";
  const made = plans.millis(current.created || current.updated) || Date.now();
  const expires_at = effect.days ? new Date(made + effect.days * 24 * 60 * 60 * 1000).toISOString() : null;
  await ctx.roles.grant({ user: row.user_id, role: effect.role, ref, expires_at, note: `payment ${current.ref}` });
  return null;
}

/** Everything the module does for an object besides mirroring it: its customer's row, and its
 * grants. A grant that is refused — a role above the module's ceiling — is an issue rather than
 * a failure: retrying the delivery would be refused the same way. The issue is written on the
 * mirror row, and cleared once the object needs none. */
async function applyEffects(ctx, account, current, row) {
  if (current.customer_ref && current.kind !== "customer") {
    await upsertCustomer(ctx.db, account, current.customer_ref, { user_id: row.user_id, email: row.email });
  }
  let issue = null;
  try {
    if (current.kind === "subscription") issue = await subscriptionEffects(ctx, account, current, row);
    else if (current.kind === "payment") issue = await paymentEffects(ctx, account, current, row);
  } catch (e) {
    issue = `its role could not be granted: ${e.message}`;
  }
  if ((row.issue || null) !== issue) {
    await ctx.db.pay_objects.where({ id: row.id }).update({ issue });
    row.issue = issue;
  }
  if (issue && ctx.log && typeof ctx.log.warn === "function") {
    ctx.log.warn(`payments: ${current.kind} ${current.ref} on "${account.name}": ${issue}`);
  }
  return row;
}

/** Fetch an object's current state, mirror it and apply its effects: what an event about it,
 * and the sweep of an account's live subscriptions, both do. */
async function syncObject(ctx, account, adapter, ref) {
  const current = await adapter.fetch(account, ref);
  const row = await upsertObject(ctx.db, account, current);
  return applyEffects(ctx, account, current, row);
}

/** Insert one `pay_events` row unless its `(account, event_id, kind)` is there. Answers whether
 * it was inserted. */
async function logEvent(db, row) {
  const key = { account: row.account, event_id: row.event_id, kind: row.kind };
  if (await db.pay_events.where(key).first()) return false;
  try {
    await db.pay_events.insert({ ...row, created_at: nowIso() });
    return true;
  } catch (e) {
    // The same event delivered twice at once: the other delivery logged it.
    if (await db.pay_events.where(key).first()) return false;
    throw e;
  }
}

/** The event row's fields that come from the mirror row. */
function fromObject(object) {
  if (!object) return {};
  const out = { object: object.id };
  for (const k of ["user_id", "email", "ref_table", "ref_key", "amount", "currency", "status"]) {
    if (object[k] !== undefined) out[k] = object[k];
  }
  return out;
}

/**
 * Process the verified events of one delivery, in order. Each is `{ id, type, ref, raw }` as an
 * adapter's `verifyWebhook` answers: `ref` is `{ kind, id }`, the object the event is about, or
 * null. Answers, per event, `{ id, logged: [kinds inserted], object }`. Throws, naming the event,
 * when an object it is about cannot be fetched: the delivery is then a 500 and the provider
 * tries again.
 */
async function processEvents(ctx, account, adapter, events) {
  const db = ctx.db;
  const results = [];
  for (const event of events) {
    const kinds = kindsOf(adapter, event);
    const base = { account: account.id, event_id: String(event.id), provider_type: event.type || null, raw: event.raw === undefined ? null : event.raw };
    if (kinds === null) {
      // Understood, and nothing to log: its object is brought up to date all the same.
      const object = event.ref ? await syncOrThrow(ctx, account, adapter, event) : null;
      results.push({ id: event.id, logged: [], object: object && object.id });
      continue;
    }
    if (!kinds.length) {
      // Not understood: logged with the raw event, so an application trigger can still react.
      // Its object is mirrored when the adapter can fetch it, and left out when it cannot.
      let object = null;
      if (event.ref) {
        try {
          object = await upsertObject(db, account, await adapter.fetch(account, event.ref));
        } catch (_) {
          object = null;
        }
      }
      const logged = (await logEvent(db, { ...base, ...fromObject(object), kind: "other" })) ? ["other"] : [];
      results.push({ id: event.id, logged, object: object && object.id });
      continue;
    }
    // A delivery seen before, all of it: nothing to fetch.
    const seen = await db.pay_events.where({ account: account.id, event_id: String(event.id) }).rows();
    if (kinds.every((k) => seen.some((s) => s.kind === k))) {
      results.push({ id: event.id, logged: [], object: seen[0] && seen[0].object });
      continue;
    }
    if (!event.ref) throw new Error(`the ${event.type} event ${event.id} names no object`);
    const object = await syncOrThrow(ctx, account, adapter, event);
    const logged = [];
    for (const kind of kinds) {
      if (await logEvent(db, { ...base, ...fromObject(object), kind })) logged.push(kind);
    }
    results.push({ id: event.id, logged, object: object.id });
  }
  return results;
}

/** `syncObject` for the object an event is about, or a failure naming the event. */
async function syncOrThrow(ctx, account, adapter, event) {
  let current;
  try {
    current = await adapter.fetch(account, event.ref);
  } catch (e) {
    throw new Error(`the ${event.type} event ${event.id} is about ${event.ref.kind} ${event.ref.id}, which could not be read from the provider: ${e.message}`);
  }
  const row = await upsertObject(ctx.db, account, current);
  return applyEffects(ctx, account, current, row);
}

module.exports = { processEvents, upsertObject, upsertCustomer, applyEffects, syncObject, logEvent, kindsOf, fromMetadata, grantRef };
