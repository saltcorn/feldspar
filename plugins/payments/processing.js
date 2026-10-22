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

"use strict";

const { EVENT_KINDS } = require("./tables.js");

const nowIso = () => new Date().toISOString();

/** The kinds an adapter maps a provider event to: its `kinds(event)`, else the event's own type
 * when that already is one of ours (the fake's are). Empty is an event the module does not
 * understand, logged as `other`. */
function kindsOf(adapter, event) {
  const kinds = typeof adapter.kinds === "function" ? adapter.kinds(event) : EVENT_KINDS.includes(event.type) ? [event.type] : [];
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
  for (const key of ["amount", "currency", "email", "price_ref", "current_period_end"]) {
    if (object[key] !== undefined && object[key] !== null) values[key] = object[key];
  }
  if (Array.isArray(object.lines) && object.lines.length) values.lines = object.lines;
  return values;
}

/** The mirror row of `object`, inserted or brought up to date. The application row it is for,
 * and its user, come from the row as the checkout action wrote it; failing that from the
 * checkout the object belongs to (a payment inherits its checkout's), and failing that from the
 * provider's metadata. */
async function upsertObject(db, account, object) {
  const key = { account: account.id, provider_ref: String(object.ref) };
  const existing = await db.pay_objects.where(key).first();
  const values = mirrorValues(object);
  const origin = { ref_table: null, ref_key: null, user_id: null, email: null };
  if (object.checkout_ref && object.checkout_ref !== object.ref) {
    const checkout = await db.pay_objects.where({ account: account.id, provider_ref: String(object.checkout_ref) }).first();
    if (checkout) {
      for (const k of Object.keys(origin)) if (checkout[k] !== null && checkout[k] !== undefined) origin[k] = checkout[k];
      if (checkout.effects && object.kind === "payment") values.effects = checkout.effects;
    }
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
async function processEvents(db, account, adapter, events) {
  const results = [];
  for (const event of events) {
    const kinds = kindsOf(adapter, event);
    const base = { account: account.id, event_id: String(event.id), provider_type: event.type || null, raw: event.raw === undefined ? null : event.raw };
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
    let current;
    try {
      current = await adapter.fetch(account, event.ref);
    } catch (e) {
      throw new Error(`the ${event.type} event ${event.id} is about ${event.ref.kind} ${event.ref.id}, which could not be read from the provider: ${e.message}`);
    }
    const object = await upsertObject(db, account, current);
    const logged = [];
    for (const kind of kinds) {
      if (await logEvent(db, { ...base, ...fromObject(object), kind })) logged.push(kind);
    }
    results.push({ id: event.id, logged, object: object.id });
  }
  return results;
}

module.exports = { processEvents, upsertObject, logEvent, kindsOf, fromMetadata };
