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
// Between the mirror and the log come the only effects the module applies itself (§3.7):
// grants of roles and of LLM credits. A subscription's role grant and its plan's credit
// allowance are worked out from the subscription's *current* state and its plan (`plans.js`,
// P4.1, P7.1); a one-off payment's *grant a role* and *grant credits* effects from what its
// checkout (or the charge of a saved card) recorded. Each is keyed — a role grant by its
// reference, a credit grant by its key — so they too may happen any number of times. What cannot
// be done — a price that is no plan's, a role above the module's ceiling — is written on the
// mirror row as its `issue`, and logged.
//
// `ctx` is what the caller has of the module: `db`, and, to grant, `roles` and `credits` (C4)
// and `configuration` (the module's settings, which hold the plan binding).

"use strict";

const { EVENT_KINDS } = require("./tables.js");
const plans = require("./plans.js");

const nowIso = () => new Date().toISOString();

/** The kinds an adapter maps a provider event to: its `kinds(event, { previous })`, else the
 * event's own type when that already is one of ours (the fake's are). Empty is an event the module
 * does not understand, logged as `other`. `null` is one it understands that says nothing an
 * application reacts to — Stripe's invoice for a subscription's first period, say — whose object
 * is still brought up to date. `previous` is the mirror row of the event's object before this
 * event, for a provider whose events do not say what changed (Polar's plan change). */
function kindsOf(adapter, event, previous = null) {
  const kinds = typeof adapter.kinds === "function" ? adapter.kinds(event, { previous }) : EVENT_KINDS.includes(event.type) ? [event.type] : [];
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
  // What a saved card's charge grants once paid (`pay_charge_saved`, P7.2): kept with the payment
  // at the provider, since its webhook may arrive before the action has written the mirror row.
  if (typeof m.sc_effects === "string" && m.sc_effects) {
    try {
      const effects = JSON.parse(m.sc_effects);
      if (effects && typeof effects === "object" && !Array.isArray(effects)) out.effects = effects;
    } catch (_) {
      // Not the module's own: ignored.
    }
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
    if (parent.kind === "checkout" && parent.charged && object.kind === "payment" && !(existing && existing.charged)) values.charged = parent.charged;
  }
  const meta = fromMetadata(object.metadata);
  for (const [k, v] of Object.entries(meta)) if (k in origin && origin[k] === null) origin[k] = v;
  if (meta.effects && object.kind === "payment" && values.effects === undefined && !(existing && existing.effects)) values.effects = meta.effects;
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

/** The user an object's provider customer may be bound to: the object's user — unless the
 * adapter says whose the customer is (`customer_external_id`, Polar's `external_id`) and it is not
 * that user (security review #35). Polar may attach a checkout to an existing customer found by
 * its email; binding that customer to whoever paid would give a sign-up with somebody else's
 * address their portal and their saved card. An adapter that does not say (`undefined`) creates
 * its customers per user, and the object's user is bound. */
function customerUser(current, row) {
  if (row.user_id === null || row.user_id === undefined) return null;
  if (current.customer_external_id === undefined) return row.user_id;
  return current.customer_external_id !== null && String(current.customer_external_id) === String(row.user_id) ? row.user_id : null;
}

/** The reference a module's grant for `object` is made under: one per subscription, or per
 * payment for a one-off pass. */
const grantRef = (account, object) => `${account.name}:${object.ref}`;

/** Run each of `steps`, each answering an issue or null, and answer their issues as one: one
 * that throws is an issue too, and does not stop the others — a role refused above the
 * module's ceiling still leaves the plan's credits to be granted. */
async function issuesOf(steps) {
  const issues = [];
  for (const [what, step] of steps) {
    try {
      const issue = await step();
      if (issue) issues.push(issue);
    } catch (e) {
      issues.push(`${what} could not be granted: ${e.message}`);
    }
  }
  return issues.length ? issues.join("; ") : null;
}

/** What a subscription's current state does to its role grant (§6.2) and its plan's credit
 * allowance (§6.3), and the issue, if any. */
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
  if (row.user_id === null || row.user_id === undefined) return "the subscription is no user's of this instance, so it grants no role";
  return issuesOf([
    [
      "its role",
      async () => {
        const role = plans.planRole(binding, found.plan);
        if (role.problem) return role.problem;
        if (!ctx.roles) return "this delivery could not grant roles";
        await ctx.roles.grant({ user: row.user_id, role: role.role, ref, expires_at: decision.expires_at, note: `subscription ${current.ref}` });
        return null;
      },
    ],
    ["its LLM credits", () => allowanceEffects(ctx, account, current, row, binding, found.plan)],
  ]);
}

/**
 * A plan's LLM-credit allowance for the period the subscription is now in (§6.3, P7.1): granted
 * once per period, keyed by the subscription and the period's end, expiring at that end, and
 * rolling over from the last period's grant as the plan says. Nothing while the subscription is
 * past due: the period is granted once it has been paid. The key granted last is kept on the
 * mirror row, which is what the next period rolls over from.
 */
async function allowanceEffects(ctx, account, current, row, binding, plan) {
  const allowance = plans.planAllowance(binding, plan);
  if (!allowance) return null;
  if (allowance.problem) return allowance.problem;
  const period = plans.allowancePeriod(current);
  if (!period) return null;
  const key = `allowance:${account.id}:${current.ref}:${period.end}`;
  if (row.allowance_key === key) return null;
  // A trial's allowance only for the user's first subscription on the account: two trial
  // checkouts opened at once both pass the checkout's one-trial check.
  if (current.status === "trialing") {
    const subs = await ctx.db.pay_objects.where({ account: account.id, kind: "subscription", user_id: row.user_id }).orderBy("id", "asc").rows();
    if (subs.length && subs[0].id !== row.id) return "a trial on a second subscription includes no LLM credits; they are granted from its first paid period";
  }
  if (!ctx.credits) return "this delivery could not grant LLM credits";
  await ctx.credits.grant({
    user: row.user_id,
    amount: allowance.amount,
    key,
    expires_at: period.end,
    kind: "allowance",
    note: `subscription ${current.ref}`,
    rollover: row.allowance_key ? { from: row.allowance_key, cap: allowance.cap } : null,
  });
  await ctx.db.pay_objects.where({ id: row.id }).update({ allowance_key: key });
  row.allowance_key = key;
  return null;
}

/** A one-off payment's effects (§6.1): its *grant a role* and its *grant credits*. */
async function paymentEffects(ctx, account, current, row) {
  const effects = row.effects || {};
  return issuesOf([
    ["its role", () => (effects.role ? paymentRole(ctx, account, current, row, effects.role) : null)],
    ["its LLM credits", () => (effects.credits ? paymentCredits(ctx, account, current, row, effects.credits) : null)],
  ]);
}

const DAY_MS = 24 * 60 * 60 * 1000;

/** Whether a payment's money went back to the buyer, all of it: refunded in full, or charged
 * back by a dispute the merchant lost (security review #34). What it granted is taken back. A
 * partial refund takes nothing back: what it bought is the shop's to settle with the buyer. */
const takenBack = (current) => current.status === "refunded" || current.status === "charged_back";

/** The *grant a role* effect: granted, for its days, once the payment succeeded, and revoked when
 * its money went back ([`takenBack`]). The days are counted from when it was made, so granting it
 * again changes nothing. */
async function paymentRole(ctx, account, current, row, effect) {
  const ref = grantRef(account, current);
  if (takenBack(current)) {
    if (ctx.roles) await ctx.roles.revoke(ref);
    return null;
  }
  if (current.status !== "succeeded" && current.status !== "partially_refunded") return null;
  if (row.user_id === null || row.user_id === undefined) return "the payment is no user's of this instance, so its role was not granted";
  if (!ctx.roles) return "this delivery could not grant roles";
  const made = plans.millis(current.created || current.updated) || Date.now();
  const expires_at = effect.days ? new Date(made + effect.days * DAY_MS).toISOString() : null;
  await ctx.roles.grant({ user: row.user_id, role: effect.role, ref, expires_at, note: `payment ${current.ref}` });
  return null;
}

/** The *grant credits* effect (§6.3, P7.2): a top-up, granted once the payment succeeded, keyed by
 * the payment so that it is granted once however often its events arrive, and expiring its days
 * after the payment was made. When its money goes back — a full refund, or a lost chargeback —
 * what is left of the top-up is taken back (security review #34): a buyer who spent a pack and
 * charged it back keeps only what they spent, and their balance owes nothing more. A partial
 * refund leaves the credits. A payment seen for the first time already refunded grants nothing. */
async function paymentCredits(ctx, account, current, row, effect) {
  const key = `payment:${account.id}:${current.ref}`;
  if (takenBack(current)) {
    if (!ctx.credits || typeof ctx.credits.revoke !== "function") return "this delivery could not take back LLM credits";
    await ctx.credits.revoke({ key, note: `payment ${current.ref} ${current.status === "refunded" ? "refunded" : "charged back"}` });
    return null;
  }
  if (current.status !== "succeeded" && current.status !== "partially_refunded") return null;
  if (row.user_id === null || row.user_id === undefined) return "the payment is no user's of this instance, so its LLM credits were not granted";
  if (!ctx.credits) return "this delivery could not grant LLM credits";
  const made = plans.millis(current.created || current.updated) || Date.now();
  const expires_at = effect.expire_days ? new Date(made + effect.expire_days * DAY_MS).toISOString() : null;
  if (expires_at && Date.parse(expires_at) <= Date.now()) return `its LLM credits expired on ${expires_at}, before the payment was heard of`;
  await ctx.credits.grant({ user: row.user_id, amount: effect.amount, key, expires_at, kind: "top_up", note: `payment ${current.ref}` });
  return null;
}

/** The account setting that lets a **test-mode** account grant roles and credits (security review
 * #33). Off by default: a test account takes no real money — the fake adapter's portal changes to
 * any price and renews on a click — so one left enabled in production would hand out plans and
 * unlimited LLM credits. An admin trying the module out turns it on for that account. */
const TEST_GRANTS = "grants_in_test_mode";

/** Whether `account` may grant: a live account, or a test account the admin opted in. */
function grantsFor(account) {
  if (account.mode === "live") return true;
  const settings = account.settings && typeof account.settings === "object" ? account.settings : {};
  return settings[TEST_GRANTS] === true;
}

/** The issue on an object of a test-mode account that may not grant. */
const testModeIssue = (account) =>
  `the payment account "${account.name}" is in test mode, so it grants no roles or LLM credits; ` +
  `set "${TEST_GRANTS}": true in its settings to try grants out`;

/** Whether `current` is an object that grants something: a subscription under a plan binding,
 * or a payment with a *grant a role* or *grant credits* effect. */
function wouldGrant(ctx, current, row) {
  if (current.kind === "subscription") return !!plans.planBinding(ctx.configuration);
  if (current.kind !== "payment") return false;
  const effects = row.effects || {};
  return !!(effects.role || effects.credits);
}

/** Everything the module does for an object besides mirroring it: its customer's row, and its
 * grants. A grant that is refused — a role above the module's ceiling — is an issue rather than
 * a failure: retrying the delivery would be refused the same way. The issue is written on the
 * mirror row, and cleared once the object needs none. */
async function applyEffects(ctx, account, current, row) {
  if (current.customer_ref && current.kind !== "customer") {
    await upsertCustomer(ctx.db, account, current.customer_ref, { user_id: customerUser(current, row), email: row.email });
  }
  let issue = null;
  try {
    if (!grantsFor(account)) issue = wouldGrant(ctx, current, row) ? testModeIssue(account) : null;
    else if (current.kind === "subscription") issue = await subscriptionEffects(ctx, account, current, row);
    else if (current.kind === "payment") issue = await paymentEffects(ctx, account, current, row);
  } catch (e) {
    issue = `its grants could not be made: ${e.message}`;
  }
  const mismatch = row.charged && row.charged.mismatch;
  if (mismatch) issue = issue ? `${issue}; ${mismatch}` : mismatch;
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
  return mirrorAndApply(ctx, account, adapter, await adapter.fetch(account, ref));
}

/** Mirror an object read from the provider and apply its effects. A checkout that names its
 * payment, at a provider whose payment does not name its checkout (Stripe's PaymentIntent does not
 * know its Checkout Session), passes the payment what it was for — its row, its user and its
 * effects — whichever of the two events arrived first. */
async function mirrorAndApply(ctx, account, adapter, current) {
  const row = await upsertObject(ctx.db, account, current);
  const applied = await applyEffects(ctx, account, current, row);
  // A dispute changes what its payment is (a lost one is a chargeback, security review #34), but
  // the provider's dispute events are about the dispute: the payment is read again with it.
  if (current.kind === "dispute" && current.payment_ref) {
    await syncObject(ctx, account, adapter, { kind: "payment", id: current.payment_ref });
  }
  if (current.kind === "checkout" && current.payment_ref) {
    const payment = await adapter.fetch(account, { kind: "payment", id: current.payment_ref });
    if (!payment.checkout_ref) {
      const linked = { ...payment, checkout_ref: current.ref };
      await applyEffects(ctx, account, linked, await upsertObject(ctx.db, account, linked));
    }
  }
  return applied;
}

/**
 * Whether a payment paid for what its row holds now (security review #18). A checkout's lines
 * are priced when it is made, and the buyer pays up to a day later; the rows they came from (a
 * basket's order lines) may have changed in between, and the application's triggers deliver
 * from the rows. So the lines are worked out again, by the settings the checkout kept, and
 * compared with what was charged; the payment's amount must also be that checkout's total.
 *
 * The checkout is the one the payment came from when the mirror knows it, and otherwise any
 * checkout of the same row (a Stripe payment can arrive before its checkout names it). Answers
 * `{ lines }` when it matches, `{ mismatch, lines }` when it does not, and `null` for a payment
 * the module priced no lines for (a subscription's).
 */
async function checkCharged(ctx, account, row) {
  const { chargedLines, sameLines } = require("./checkout.js");
  let candidates = row.charged ? [row] : [];
  if (!candidates.length && row.ref_table) {
    const checkouts = await ctx.db.pay_objects.where({ account: account.id, kind: "checkout", ref_table: row.ref_table, ref_key: row.ref_key }).rows();
    candidates = checkouts.filter((c) => c.charged);
  }
  if (!candidates.length) return null;
  let now;
  try {
    now = await chargedLines(ctx.db, candidates[0].charged.source || {}, row.ref_table, row.ref_key);
  } catch (e) {
    return { mismatch: `its lines could not be worked out again: ${e.message}`, lines: candidates[0].charged.lines };
  }
  const total = (lines) => lines.reduce((sum, l) => sum + l.quantity * l.unit_amount, 0);
  const paid = Number.isSafeInteger(row.amount) ? row.amount : null;
  const match = candidates.find((c) => sameLines(c.charged.lines, now) && (paid === null || total(c.charged.lines) === paid));
  if (match) return { lines: match.charged.lines };
  const charged = candidates.find((c) => paid === null || total(c.charged.lines) === paid) || candidates[0];
  return {
    mismatch: `what was paid for is not what ${row.ref_table} ${JSON.stringify(row.ref_key)} holds now: its lines changed after the checkout was made`,
    lines: charged.charged.lines,
  };
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
 * null. One delivery may be about several objects (Polar's paid renewal order is a payment and
 * the renewal of its subscription): it is then several events under the one id, each with its
 * own object and kinds. Answers, per event, `{ id, logged: [kinds inserted], object }`. Throws, naming the event,
 * when an object it is about cannot be fetched: the delivery is then a 500 and the provider
 * tries again.
 */
async function processEvents(ctx, account, adapter, events) {
  const db = ctx.db;
  const results = [];
  for (const event of events) {
    const previous = event.ref && event.ref.id ? await db.pay_objects.where({ account: account.id, provider_ref: String(event.ref.id) }).first() : null;
    const kinds = kindsOf(adapter, event, previous);
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
    // A delivery seen before, all of it: nothing to fetch. A payment found not to match its lines
    // was logged as `payment.mismatch` in place of `payment.succeeded`, and stays so.
    const seen = await db.pay_events.where({ account: account.id, event_id: String(event.id) }).rows();
    const logs = (k) => seen.some((s) => s.kind === k || (k === "payment.succeeded" && s.kind === "payment.mismatch"));
    if (kinds.every(logs)) {
      results.push({ id: event.id, logged: [], object: seen[0] && seen[0].object });
      continue;
    }
    if (!event.ref) throw new Error(`the ${event.type} event ${event.id} names no object`);
    const object = await syncOrThrow(ctx, account, adapter, event);
    let final = kinds;
    const extra = {};
    if (kinds.includes("payment.succeeded")) {
      // Decided once per event: a delivery finished by a retry finds the decision in the log.
      const before = seen.find((s) => s.kind === "payment.succeeded" || s.kind === "payment.mismatch");
      const checked = before ? { lines: before.lines, mismatch: before.kind === "payment.mismatch" } : await checkCharged(ctx, account, object);
      if (checked && checked.lines) extra.lines = checked.lines;
      if (checked && checked.mismatch) {
        final = kinds.map((k) => (k === "payment.succeeded" ? "payment.mismatch" : k));
        if (!before) {
          // Kept on the row's `charged`, so that applying its effects again keeps the issue.
          const charged = { ...(object.charged || { lines: checked.lines }), mismatch: checked.mismatch };
          await db.pay_objects.where({ id: object.id }).update({ charged, issue: checked.mismatch });
          if (ctx.log && typeof ctx.log.warn === "function") ctx.log.warn(`payments: payment ${object.provider_ref} on "${account.name}": ${checked.mismatch}`);
        }
      }
    }
    const logged = [];
    for (const kind of final) {
      if (await logEvent(db, { ...base, ...fromObject(object), ...extra, kind })) logged.push(kind);
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
  return mirrorAndApply(ctx, account, adapter, current);
}

module.exports = { TEST_GRANTS, grantsFor, processEvents, upsertObject, upsertCustomer, applyEffects, syncObject, logEvent, kindsOf, fromMetadata, grantRef };
