// `pay_refund` (TODO-payments P2.6): give money back for a payment, found either from the
// application row it was for (an order) or as a mirror row (`pay_objects`). The provider then
// reports the refund like anything else, as a `payment.refunded` event the application's triggers
// react to; the mirror is brought up to date from that event, never from this request.

"use strict";

const { usableAccount } = require("./accounts.js");
const { requireAmount } = require("./money.js");
const { deliverTo } = require("./webhooks.js");

const REFUNDABLE = ["succeeded", "partially_refunded"];

const REFUND_FIELDS = [
  {
    name: "source",
    label: "The payment",
    type: "String",
    required: true,
    default: "row",
    attributes: { options: ["row", "payment"] },
    sublabel: "row: a row of your table that was paid for (its latest payment is refunded); payment: a pay_objects row",
  },
  { name: "table", label: "Table", type: "String", showIf: { source: "row" }, sublabel: "The table of the row that was paid for: orders" },
  {
    name: "key_from",
    label: "Which row",
    type: "String",
    default: "payload",
    attributes: { options: ["payload", "row"] },
    sublabel: "payload: the caller sends the key; row: the row this trigger fires on",
  },
  { name: "key_field", label: "Payload field", type: "String", default: "id", sublabel: "The payload field holding the key (payment: the pay_objects id)" },
  { name: "read_check", label: "The caller must be able to read the row", type: "Bool", default: true },
  { name: "amount", label: "Amount", type: "String", showIf: { source: "row" }, sublabel: "A formula over the row, in the smallest unit; empty refunds all that is left" },
  { name: "reason", label: "Reason", type: "String", sublabel: "Kept with the refund at the provider" },
];

const blank = (v) => v === undefined || v === null || String(v).trim() === "";

/** The key the caller named, or the trigger's own row's. */
function keyOf(configuration, payload, row, pk) {
  if ((configuration.key_from || "payload") === "row") {
    if (!row || blank(row[pk])) throw new Error("the refund is for the row its trigger fires on, and this event has none");
    return row[pk];
  }
  const field = String(configuration.key_field || "id").trim();
  const value = payload && payload[field];
  if (blank(value) || typeof value === "object") throw new Error(`say which payment to refund: { ${field}: <its key> }`);
  return value;
}

/** A log for a delivery the fake makes from inside an action, which has no service log. */
const actionLog = Object.assign((...a) => console.log(...a), { warn: (...a) => console.warn(...a), error: (...a) => console.error(...a) });

async function payRefund({ configuration = {}, payload, row, db, roles }, moduleConfiguration = {}) {
  const source = configuration.source || "row";
  const checked = configuration.read_check !== false;
  let payment;
  let amount = null;
  if (source === "payment") {
    const id = keyOf(configuration, payload, row, "id");
    payment = checked ? await db.asUser().pay_objects.get(id) : await db.pay_objects.get(id);
    if (!payment || payment.kind !== "payment") throw new Error(`there is no payment ${JSON.stringify(id)} that you can read`);
  } else {
    const table = String(configuration.table || "").trim();
    if (!table) throw new Error("the refund needs the table setting: the table of the row that was paid for");
    let pk = "id";
    try {
      const Table = require("@saltcorn/data/models/table");
      const found = Table.findOne({ name: table });
      if (!found) throw new Error(`the refund names the table "${table}", which does not exist`);
      pk = found.pk_name || "id";
    } catch (e) {
      if (e.code !== "MODULE_NOT_FOUND") throw e;
    }
    const key = keyOf(configuration, payload, row, pk);
    const target = checked ? await db.asUser()[table].get(key) : await db[table].get(key);
    if (!target) throw new Error(`there is no ${table} row ${JSON.stringify(key)} that you can read`);
    const refKey = target[pk] === undefined || target[pk] === null ? key : target[pk];
    const payments = await db.pay_objects.where({ ref_table: table, ref_key: refKey, kind: "payment" }).orderBy("id", "desc").rows();
    payment = payments.find((p) => REFUNDABLE.includes(p.status));
    if (!payment) throw new Error(`there is no payment for ${table} ${JSON.stringify(refKey)} to refund`);
    if (!blank(configuration.amount)) {
      const computed = await db[table].select({ amount: configuration.amount }).get(key);
      amount = requireAmount(computed && computed.amount, "the refund's amount");
    }
  }
  if (!REFUNDABLE.includes(payment.status)) {
    throw new Error(`the payment ${payment.provider_ref} is ${payment.status}, so there is nothing to refund`);
  }
  // The account the payment is on, whatever account a trigger names.
  const accountRow = await db.pay_accounts.get(payment.account);
  if (!accountRow) throw new Error(`the payment ${payment.provider_ref} is on an account that no longer exists`);
  const { account, adapter } = await usableAccount(db, accountRow.name);
  if (typeof adapter.refund !== "function") throw new Error(`the ${adapter.name} adapter cannot refund`);
  if (amount === 0) throw new Error("a refund of 0 refunds nothing");
  // A click repeated before the provider's webhook has arrived is the same refund: the key is
  // the payment, what is asked, and how many refunds of it the log has seen so far.
  const before = await db.pay_events.where({ object: payment.id, kind: "payment.refunded" }).count();
  const key = `refund:${payment.provider_ref}:${amount === null ? "rest" : amount}:${before}`;
  const refund = await adapter.refund(account, payment.provider_ref, { amount, key, reason: configuration.reason || null }, deliverTo(account, { db, roles, configuration: moduleConfiguration, log: actionLog }));
  return { refund: refund.ref || refund.id, payment: payment.id, amount: refund.amount, currency: refund.currency || payment.currency, status: refund.status };
}

module.exports = { REFUND_FIELDS, payRefund };
