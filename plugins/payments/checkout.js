// The **checkout configuration** (TODO-payments §6.1, P2.4): what `pay_checkout` is configured
// with, and what P7's `pay_charge_saved`, P8's `pay_native_sheet` and P10's `pay_request` will
// share. A payment is always *for* a row of the application's — an order, an invoice, a booking —
// and everything charged is worked out **on the server**, by formulas over that row and its
// child rows, read through the `db` chain (`select({ alias: formula })`), so Ⱶ-paths reach the
// product a line names. The caller says only *which* row; an amount it sends is never read.

"use strict";

const { usableAccount } = require("./accounts.js");
const { requireCapability } = require("./capabilities.js");
const { currencyCode, requireAmount } = require("./money.js");
const plans = require("./plans.js");

const TAX = ["none", "provider", "merchant_of_record"];

/** The settings, in v1's `configFields` vocabulary, which the trigger form renders. */
const CHECKOUT_FIELDS = [
  { name: "account", label: "Payment account", type: "String", required: true, sublabel: "The name of a pay_accounts row" },
  { name: "mode", label: "Mode", type: "String", required: true, default: "payment", attributes: { options: ["payment", "subscription"] } },
  { input_type: "section_header", label: "What is paid for" },
  {
    name: "table",
    label: "Table",
    type: "String",
    required: true,
    sublabel: "The application table whose row is paid for: an order, an invoice, a booking",
  },
  {
    name: "key_from",
    label: "Which row",
    type: "String",
    required: true,
    default: "payload",
    attributes: { options: ["payload", "row"] },
    sublabel: "payload: the caller sends the row's key; row: the row this trigger fires on",
  },
  { name: "key_field", label: "Payload field", type: "String", default: "id", showIf: { key_from: "payload" }, sublabel: "The payload field holding the key: order for { order: 12 }" },
  {
    name: "read_check",
    label: "The caller must be able to read the row",
    type: "Bool",
    default: true,
    sublabel: "Turn off only for a step of a workflow that has just created the row",
  },
  { input_type: "section_header", label: "Lines" },
  { name: "lines", label: "Lines", type: "String", required: true, default: "one", attributes: { options: ["one", "children"] }, sublabel: "one line from the row, or a line per child row" },
  { name: "amount", label: "Amount", type: "String", showIf: { lines: "one" }, sublabel: "A formula over the row, in the currency's smallest unit (1999 for 19.99): total" },
  { name: "description", label: "Description", type: "String", showIf: { lines: "one" }, sublabel: 'A formula over the row: "Invoice " + number' },
  { name: "child_table", label: "Child table", type: "String", showIf: { lines: "children" }, sublabel: "The table of the lines: order_lines" },
  { name: "child_key", label: "Its key to the row", type: "String", showIf: { lines: "children" }, sublabel: "The child table's field holding the row's key: order" },
  { name: "quantity", label: "Quantity", type: "String", default: "1", showIf: { lines: "children" }, sublabel: "A formula over the child row: quantity" },
  { name: "unit_amount", label: "Unit amount", type: "String", showIf: { lines: "children" }, sublabel: "A formula over the child row, in the smallest unit: productⱵprice" },
  { name: "line_description", label: "Line description", type: "String", showIf: { lines: "children" }, sublabel: "A formula over the child row: productⱵname" },
  { name: "item", label: "Item", type: "String", showIf: { lines: "children" }, sublabel: "A formula over the child row naming what is sold: product" },
  { input_type: "section_header", label: "Subscription" },
  {
    name: "price_ref",
    label: "Price",
    type: "String",
    showIf: { mode: "subscription" },
    sublabel:
      'A formula over the row giving the provider\'s price: stripe_price on a plan, or planⱵstripe_price, or a literal "price_123". ' +
      "Empty, when the row is a plan, is the plan binding's price field",
  },
  { name: "subscription_quantity", label: "Quantity", type: "String", default: "1", showIf: { mode: "subscription" }, sublabel: "A formula over the row: seats" },
  {
    name: "trial_days",
    label: "Trial days",
    type: "String",
    showIf: { mode: "subscription" },
    sublabel: "A number, or a formula over the row: days of free trial, with the card taken now and charged when it ends",
  },
  { name: "currency", label: "Currency", type: "String", required: true, sublabel: "A three-letter code (EUR), or a formula over the row: currency" },
  { input_type: "section_header", label: "Customer, tax and shipping" },
  { name: "email", label: "Email", type: "String", sublabel: "A formula over the row, for a buyer who is not signed in: email. A signed-in buyer's own address is used otherwise" },
  { name: "tax", label: "Tax", type: "String", required: true, default: "none", attributes: { options: TAX }, sublabel: "none, the provider's calculation, or a merchant of record (digital goods only)" },
  { name: "shipping", label: "Collect a shipping address", type: "Bool", default: false },
  { name: "shipping_countries", label: "Ship to", type: "String", showIf: { shipping: true }, sublabel: "Two-letter country codes, separated by commas: GB, IE, FR" },
  { input_type: "section_header", label: "Where the buyer goes" },
  { name: "success_path", label: "After paying", type: "String", default: "/", sublabel: "A path on the application's host; {key} is the row's key: /orders/{key}" },
  { name: "cancel_path", label: "After cancelling", type: "String", default: "/", sublabel: "A path on the application's host" },
  { input_type: "section_header", label: "Effects, once it is paid" },
  { name: "grant_credits", label: "Grant LLM credits", type: "String", sublabel: "A formula over the row: units" },
  { name: "credits_expire_days", label: "Credits expire after (days)", type: "Integer" },
  { name: "grant_role", label: "Grant a role", type: "Integer", sublabel: "A role id, for a one-off pass" },
  { name: "role_days", label: "For (days)", type: "Integer" },
];

const blank = (v) => v === undefined || v === null || String(v).trim() === "";

/** The table's primary key field: v1's `Table`, answered by the server from its schema. Outside
 * a server (the module's own tests) there is no such package, and the key is `id`. */
function primaryKey(table) {
  let Table;
  try {
    Table = require("@saltcorn/data/models/table");
  } catch (_) {
    return "id";
  }
  const found = Table.findOne({ name: table });
  if (!found) throw new Error(`the checkout names the table "${table}", which does not exist`);
  return found.pk_name || "id";
}

/** The key of the row being paid for, from the payload or from the trigger's own row. */
function sourceKey(configuration, payload, row, table, pk) {
  if ((configuration.key_from || "payload") === "row") {
    if (!row) throw new Error("the checkout pays for the row its trigger fires on, and this event has none");
    if (blank(row[pk])) throw new Error(`the row this trigger fires on has no ${pk}`);
    return row[pk];
  }
  const field = String(configuration.key_field || "id").trim();
  const value = payload && payload[field];
  if (blank(value) || typeof value === "object") {
    throw new Error(`say which ${table} row to pay for: { ${field}: <its key> }`);
  }
  return value;
}

/** A currency setting: a three-letter code as it is, else a formula over the row. */
const currencyIsLiteral = (setting) => /^[A-Z]{3}$/.test(String(setting).trim());

/** A path on the application's host, `{key}` filled in: refused when it is not a path, since a
 * buyer is never sent off the application by a checkout's configuration. */
function appUrl(app, setting, key, what) {
  const path = blank(setting) ? "/" : String(setting).trim();
  if (!path.startsWith("/") || path.startsWith("//")) {
    throw new Error(`the ${what} is a path on the application's host, starting with /, not "${path}"`);
  }
  return `${(app && app.url) || ""}${path.replace(/\{key\}/g, encodeURIComponent(String(key)))}`;
}

/** The lines as charged, from the source row's formulas or its child rows'. */
async function chargedLines(db, configuration, table, key) {
  if ((configuration.lines || "one") === "children") {
    for (const s of ["child_table", "child_key", "unit_amount"]) {
      if (blank(configuration[s])) throw new Error(`a checkout of child rows needs the ${s.replace("_", " ")} setting`);
    }
    const select = {
      quantity: blank(configuration.quantity) ? "1" : configuration.quantity,
      unit_amount: configuration.unit_amount,
    };
    if (!blank(configuration.line_description)) select.description = configuration.line_description;
    if (!blank(configuration.item)) select.item = configuration.item;
    const rows = await db[configuration.child_table].where({ [configuration.child_key]: key }).select(select).rows();
    return rows.map((r, i) => ({
      description: blank(r.description) ? `Item ${i + 1}` : String(r.description),
      quantity: requireAmount(r.quantity, `line ${i + 1}'s quantity`),
      unit_amount: requireAmount(r.unit_amount, `line ${i + 1}'s unit amount`),
      item: r.item === undefined ? null : r.item,
    }));
  }
  if (blank(configuration.amount)) throw new Error("a checkout of one line needs the amount setting: a formula over the row");
  const select = { amount: configuration.amount };
  if (!blank(configuration.description)) select.description = configuration.description;
  const one = await db[table].select(select).get(key);
  return [
    {
      description: blank(one && one.description) ? `${table} ${key}` : String(one.description),
      quantity: 1,
      unit_amount: requireAmount(one && one.amount, "the amount"),
      item: null,
    },
  ];
}

/**
 * Everything a checkout charges and records, worked out from the trigger's configuration and
 * the row it is for. Refuses, by name, an account that cannot be used, a capability its adapter
 * lacks, a row the caller cannot read, and a total of nothing.
 */
async function prepareCheckout({ configuration = {}, payload, row, user, session, app, db, moduleConfiguration }) {
  const { account, adapter } = await usableAccount(db, configuration.account);
  const mode = configuration.mode || "payment";
  if (mode !== "payment" && mode !== "subscription") throw new Error(`a checkout's mode is payment or subscription, not "${mode}"`);
  if (mode === "subscription") {
    requireCapability(account, adapter, "subscriptions");
  } else {
    requireCapability(account, adapter, "one_off");
    requireCapability(account, adapter, "adhoc_prices");
  }

  const table = String(configuration.table || "").trim();
  if (!table) throw new Error("the checkout needs the table setting: the application table whose row is paid for");
  const pk = primaryKey(table);
  const key = sourceKey(configuration, payload, row, table, pk);

  // The same refusal whether the row is missing or only out of the caller's reach: which keys
  // exist is not the caller's to learn.
  const readable = configuration.read_check === false ? null : await db.asUser()[table].get(key);
  if (configuration.read_check !== false && !readable) {
    throw new Error(`there is no ${table} row ${JSON.stringify(key)} that you can read`);
  }
  const source = readable || (await db[table].get(key));
  if (!source) throw new Error(`there is no ${table} row ${JSON.stringify(key)}`);
  // The key as the row has it — a number for an integer key, text for a text or UUID one — so
  // that the application's `id === row.ref_key` finds the row again (§5).
  const refKey = source[pk] === undefined || source[pk] === null ? key : source[pk];

  let lines;
  let total = null;
  if (mode === "subscription") {
    lines = [await subscriptionLine(db, configuration, table, key, account, adapter, moduleConfiguration)];
  } else {
    lines = await chargedLines(db, configuration, table, key);
    total = 0;
    for (const l of lines) {
      total += l.quantity * l.unit_amount;
      if (!Number.isSafeInteger(total)) throw new Error("the checkout's total is too large to count exactly");
    }
    if (total === 0) throw new Error(`there is nothing to pay for ${table} ${JSON.stringify(refKey)}: its lines come to 0`);
  }

  // The row's other formulas, read in one go.
  const select = {};
  const setting = String(configuration.currency || "").trim();
  if (!setting) throw new Error("the checkout needs the currency setting: a code such as EUR, or a formula");
  if (!currencyIsLiteral(setting)) select.currency = setting;
  if (!blank(configuration.email)) select.email = configuration.email;
  if (!blank(configuration.grant_credits)) select.credits = configuration.grant_credits;
  const values = Object.keys(select).length ? (await db[table].select(select).get(key)) || {} : {};
  const currency = currencyCode(currencyIsLiteral(setting) ? setting : values.currency);

  const signedIn = user && user.id !== undefined && user.id !== null && !user.module;
  const email = signedIn && user.email ? user.email : blank(values.email) ? null : String(values.email);
  // A subscription is somebody's: its role is granted to the user who took it out.
  if (mode === "subscription" && !signedIn) {
    throw new Error("a subscription is taken out by a signed-in user: its role is granted to them, and a guest has no account to grant it to");
  }
  // The provider customer this user already is on the account, so that a second checkout (a
  // top-up, another plan) is the same customer, with the same saved card (§5).
  const known = signedIn ? await db.pay_objects.where({ account: account.id, kind: "customer", user_id: user.id }).first() : null;

  let tax = configuration.tax || "none";
  if (!TAX.includes(tax)) throw new Error(`tax is ${TAX.join(", ")}, not "${tax}"`);
  // A provider that is always the merchant of record (Polar) works out and pays the tax itself,
  // whatever the setting says.
  if (adapter.capabilities && adapter.capabilities.merchant_of_record === "always") tax = "merchant_of_record";
  const shipping = configuration.shipping === true;
  if (tax === "merchant_of_record") {
    const mor = adapter.capabilities && adapter.capabilities.merchant_of_record;
    if (!mor || mor === "no") {
      throw new Error(`the ${adapter.name} account "${account.name}" cannot act as merchant of record: its adapter does not support it`);
    }
    if (shipping) {
      throw new Error("a merchant of record sells digital goods only, so a checkout through one cannot collect a shipping address");
    }
  }
  if (shipping) requireCapability(account, adapter, "physical_goods");
  const countries = shipping
    ? String(configuration.shipping_countries || "")
        .split(",")
        .map((c) => c.trim().toUpperCase())
        .filter(Boolean)
    : [];
  if (shipping && !countries.length) throw new Error("a checkout that ships needs the countries it ships to");
  for (const c of countries) if (!/^[A-Z]{2}$/.test(c)) throw new Error(`"${c}" is not a two-letter country code`);

  const effects = {};
  if (mode === "subscription" && (!blank(configuration.grant_credits) || !blank(configuration.grant_role))) {
    throw new Error("effects are for one-off payments: a subscription's role and credits come from the plan binding in the payments module's settings");
  }
  if (!blank(configuration.grant_credits)) {
    effects.credits = { amount: requireAmount(values.credits, "the credits to grant") };
    if (!blank(configuration.credits_expire_days)) effects.credits.expire_days = requireAmount(Number(configuration.credits_expire_days), "the days credits last");
  }
  if (!blank(configuration.grant_role)) {
    effects.role = { role: requireAmount(Number(configuration.grant_role), "the role to grant") };
    if (!blank(configuration.role_days)) effects.role.days = requireAmount(Number(configuration.role_days), "the days the role lasts");
  }

  return {
    account,
    adapter,
    mode,
    table,
    key: refKey,
    lines,
    total,
    currency,
    customer: { email, user_id: signedIn ? user.id : null, ref: known ? known.provider_ref : null },
    session: session ? session.id : null,
    tax,
    shipping: shipping ? { countries } : null,
    success_url: appUrl(app, configuration.success_path, refKey, "path after paying"),
    cancel_url: appUrl(app, configuration.cancel_path, refKey, "path after cancelling"),
    base_url: (app && app.url) || "",
    effects: Object.keys(effects).length ? effects : null,
    // What the provider keeps with the checkout and its payment, so that an object the mirror
    // has never seen still says which row it was for (§5).
    // Provider metadata is text, so the key is kept as JSON: `12` and `"12"` stay apart.
    metadata: { sc_ref_table: table, sc_ref_key: JSON.stringify(refKey), ...(signedIn ? { sc_user_id: String(user.id) } : {}) },
  };
}

/** A subscription checkout's one line: the provider's price, from the configuration's formula
 * over the row — or, when the row is one of the plan binding's plans and no formula is given,
 * from the binding's price field for this account — its quantity, and its trial. */
async function subscriptionLine(db, configuration, table, key, account, adapter, moduleConfiguration) {
  const select = {};
  const binding = plans.planBinding(moduleConfiguration);
  if (!blank(configuration.price_ref)) select.price_ref = configuration.price_ref;
  else if (binding && binding.table === table && plans.priceField(binding, account.name)) select.price_ref = plans.priceField(binding, account.name);
  else {
    throw new Error(
      `a subscription checkout needs the price setting: a formula over the ${table} row giving the provider's price ` +
        `(or make the row a plan of the plan binding in the payments module's settings)`,
    );
  }
  select.quantity = blank(configuration.subscription_quantity) ? "1" : configuration.subscription_quantity;
  const trial = blank(configuration.trial_days) ? null : String(configuration.trial_days).trim();
  if (trial !== null && !/^\d+$/.test(trial)) select.trial_days = trial;
  if (!blank(configuration.description)) select.description = configuration.description;
  const values = (await db[table].select(select).get(key)) || {};
  if (blank(values.price_ref) || typeof values.price_ref !== "string") {
    throw new Error(`the ${table} row ${JSON.stringify(key)} has no price to subscribe to: its price formula gives ${JSON.stringify(values.price_ref)}`);
  }
  const quantity = requireAmount(values.quantity, "the subscription's quantity");
  if (quantity === 0) throw new Error("a subscription of quantity 0 subscribes to nothing");
  const trialDays = trial === null ? 0 : requireAmount(/^\d+$/.test(trial) ? Number(trial) : values.trial_days, "the trial's days");
  if (trialDays > 0) requireCapability(account, adapter, "trial_with_card");
  return {
    description: blank(values.description) ? null : String(values.description),
    price_ref: values.price_ref,
    quantity,
    trial_days: trialDays,
    item: null,
  };
}

/** The checkout's mirror row, written now so that the webhook finds which row it was for. An
 * upsert, because the provider's first webhook may have got there first. */
async function recordCheckout(db, prepared, created) {
  const now = new Date().toISOString();
  const object = created.object || {};
  const values = {
    kind: "checkout",
    status: object.status || "open",
    user_id: prepared.customer ? prepared.customer.user_id : null,
    email: prepared.customer ? prepared.customer.email : null,
    ref_table: prepared.table || null,
    ref_key: prepared.key || null,
    amount: prepared.total,
    currency: prepared.currency,
    lines: prepared.lines,
    effects: prepared.effects || null,
    provider_data: object.provider_data || null,
    updated_at: now,
  };
  const key = { account: prepared.account.id, provider_ref: String(created.id) };
  const existing = await db.pay_objects.where(key).first();
  if (existing) {
    await db.pay_objects.where({ id: existing.id }).update(values);
    return { ...existing, ...values };
  }
  return db.pay_objects.insert({ ...key, ...values, created_at: now });
}

/** `pay_checkout`: a hosted checkout for an application row, answered as its URL. For a
 * subscription the amount is the provider's price, known once the checkout is made. */
async function payCheckout(argument) {
  const prepared = await prepareCheckout(argument);
  const created = await prepared.adapter.createCheckout(prepared.account, {
    mode: prepared.mode,
    currency: prepared.currency,
    lines: prepared.lines,
    customer: prepared.customer,
    tax: prepared.tax,
    shipping: prepared.shipping,
    base_url: prepared.base_url,
    success_url: prepared.success_url,
    cancel_url: prepared.cancel_url,
    client_reference: `${prepared.table}:${prepared.key}`,
    metadata: prepared.metadata,
  });
  if (prepared.total === null && created.object && Number.isSafeInteger(created.object.amount)) {
    prepared.total = created.object.amount;
    prepared.currency = created.object.currency || prepared.currency;
  }
  const mirrored = await recordCheckout(argument.db, prepared, created);
  return { id: mirrored.id, checkout: created.id, url: created.url, amount: prepared.total, currency: prepared.currency };
}

module.exports = { CHECKOUT_FIELDS, TAX, prepareCheckout, recordCheckout, payCheckout, appUrl };
