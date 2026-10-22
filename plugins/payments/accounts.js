// Finding an account (TODO-payments §5): the `pay_accounts` row a caller names, read through
// the `db` chain a module action, a service and a webhook all have, with its secrets revealed
// because the table is the module's own.

"use strict";

const { adapterFor } = require("./adapters/index.js");

/** What an account's name may be: it is a segment of its webhook URL. */
const ACCOUNT_NAME = /^[A-Za-z0-9_-]+$/;

/** The row named `name`, or a refusal naming the accounts there are. */
async function loadAccount(db, name) {
  if (typeof name !== "string" || !name.trim()) {
    throw new Error("name a payment account (the name of a pay_accounts row)");
  }
  const account = await db.pay_accounts.where({ name: name.trim() }).first();
  if (!account) {
    const all = await db.pay_accounts.select("name").rows();
    const names = all.map((a) => `"${a.name}"`).join(", ");
    throw new Error(`there is no payment account "${name}"; ${names ? `there are ${names}` : "there are none yet"}`);
  }
  if (!ACCOUNT_NAME.test(account.name)) {
    throw new Error(
      `the payment account "${account.name}" cannot be used: its name is part of its webhook URL, ` +
        `so it is letters, digits, - and _`,
    );
  }
  return account;
}

/** The account named `name` and its adapter, ready to use — or a refusal saying why not. */
async function usableAccount(db, name) {
  const account = await loadAccount(db, name);
  return { account, adapter: adapterFor(account) };
}

/** Every account, kept in this worker by the `reconcile` service (`service.js`), which loads
 * them when it starts and again whenever `pay_accounts` changes. It is what a module
 * **function** reads: a function is called with nobody's authority and has no `db` (`pay_api`). */
const cache = { accounts: null };

/** Replace the kept accounts with `rows`. */
function keepAccounts(rows) {
  cache.accounts = new Map(rows.map((row) => [row.name, row]));
}

/** The kept account named `name` and its adapter, or a refusal saying why not. */
function keptAccount(name) {
  if (!cache.accounts) {
    throw new Error("the payment accounts are not loaded yet: the payments module's reconcile service loads them when it starts");
  }
  const account = cache.accounts.get(typeof name === "string" ? name.trim() : name);
  if (!account) {
    const names = [...cache.accounts.keys()].map((n) => `"${n}"`).join(", ");
    throw new Error(`there is no payment account "${name}"; ${names ? `there are ${names}` : "there are none yet"}`);
  }
  return { account, adapter: adapterFor(account) };
}

module.exports = { ACCOUNT_NAME, loadAccount, usableAccount, keepAccounts, keptAccount };
