// The **reconcile** service (TODO-payments §6.5, P2.7): every few hours, each account's provider
// is asked for the events since the last sweep, and they are processed exactly as if they had
// been delivered (`processing.js`). One that was delivered is a duplicate and does nothing; one
// that never arrived is caught here. `pay_sync` runs the same sweep on demand.
//
// The service also keeps the accounts in this worker for the module's functions (`pay_api`),
// which have no `db` of their own (`accounts.js`).

"use strict";

const { adapterFor } = require("./adapters/index.js");
const { keepAccounts } = require("./accounts.js");
const { processEvents } = require("./processing.js");

/** The first sweep after the service starts, and the sweeps after it. */
const FIRST_SWEEP_MS = 5 * 60 * 1000;
const EVERY_MS = 3 * 60 * 60 * 1000;
/** How far back a sweep reaches past the last one: events are listed by when they were made,
 * and one made just before the last sweep may have been listed only after it. */
const OVERLAP_SECONDS = 60 * 60;
/** How far back an account's first sweep reaches: Stripe keeps 30 days of events. */
const FIRST_REACH_SECONDS = 30 * 24 * 60 * 60;

/** Sweep one account: its provider's events since the last sweep, processed. Answers what it
 * did. An adapter with no event log cannot be swept, and says so. */
async function sweep(db, account, adapter, now = new Date()) {
  if (typeof adapter.listEvents !== "function") {
    throw new Error(`the ${adapter.name} adapter keeps no event log, so the account "${account.name}" cannot be swept`);
  }
  const nowSeconds = Math.floor(now.getTime() / 1000);
  const last = account.synced_at ? Math.floor(Date.parse(account.synced_at) / 1000) : null;
  const since = last ? last - OVERLAP_SECONDS : nowSeconds - FIRST_REACH_SECONDS;
  const events = await adapter.listEvents(account, { since });
  const results = await processEvents(db, account, adapter, events);
  await db.pay_accounts.where({ id: account.id }).update({ synced_at: now.toISOString() });
  return { account: account.name, events: events.length, logged: results.reduce((n, r) => n + r.logged.length, 0) };
}

/** Sweep every enabled account that can be (or the one named), each on its own: one account's
 * failure is reported and the others are still swept. */
async function sweepAll(db, { only, log } = {}) {
  const accounts = await db.pay_accounts.rows();
  const out = [];
  for (const account of accounts) {
    if (only && account.name !== only) continue;
    if (account.enabled === false && !only) continue;
    try {
      const adapter = adapterFor(account);
      if (typeof adapter.listEvents !== "function" && !only) continue;
      out.push(await sweep(db, account, adapter));
    } catch (e) {
      if (log) log.warn(`payments: the sweep of "${account.name}" failed: ${e.message}`);
      out.push({ account: account.name, error: e.message });
    }
  }
  if (only && !out.length) {
    const names = accounts.map((a) => `"${a.name}"`).join(", ");
    throw new Error(`there is no payment account "${only}"; ${names ? `there are ${names}` : "there are none yet"}`);
  }
  return out;
}

/** The service: keep the accounts, and sweep on a timer. */
async function start(ctx) {
  const refresh = async () => keepAccounts(await ctx.db.pay_accounts.rows());
  await refresh();
  await ctx.onTableChange("pay_accounts", (change) => {
    // The sweep's own `synced_at` writes change nothing a function reads.
    if (change.own) return;
    refresh().catch((e) => ctx.log.error(`payments: the accounts could not be reloaded: ${e.message}`));
  });
  let running = false;
  const tick = async () => {
    if (running) return;
    running = true;
    try {
      await sweepAll(ctx.db, { log: ctx.log });
    } catch (e) {
      ctx.log.error(`payments: the sweep failed: ${e.message}`);
    } finally {
      running = false;
    }
  };
  let every = null;
  const first = setTimeout(() => {
    tick();
    every = setInterval(tick, EVERY_MS);
  }, FIRST_SWEEP_MS);
  return async () => {
    clearTimeout(first);
    if (every) clearInterval(every);
  };
}

module.exports = { start, sweep, sweepAll, FIRST_SWEEP_MS, EVERY_MS, OVERLAP_SECONDS };
