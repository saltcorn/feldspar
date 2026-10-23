// The module's three tables (TODO-payments §5). All infrastructure: the module keeps no orders,
// customers or plans — those are the application's — only the provider accounts, a mirror of
// the provider's objects, and the log of what happened.

"use strict";

/** The event kinds a `pay_events` row may have (§5). */
const EVENT_KINDS = [
  "checkout.completed",
  "payment.processing",
  "payment.succeeded",
  "payment.failed",
  "payment.refunded",
  "dispute.opened",
  "dispute.closed",
  "subscription.started",
  "subscription.renewed",
  "subscription.changed",
  "subscription.past_due",
  "subscription.canceled",
  "subscription.ended",
  "connected_account.updated",
  "other",
];

/** The kinds of provider object the mirror holds (§4). */
const OBJECT_KINDS = ["customer", "checkout", "payment", "subscription", "refund", "dispute", "connected_account"];

const tables = [
  {
    name: "pay_accounts",
    label: "Payment accounts",
    description: "One configured payment provider each: its adapter, its mode and its keys. Admin-only.",
    fields: [
      { name: "name", type: "text", required: true, unique: true, label: "Name", description: "Letters, digits, - and _: it is part of the account's webhook URL" },
      { name: "adapter", type: "text", required: true, label: "Adapter", description: "The provider: stripe, polar, or fake (a test provider in this server)" },
      { name: "mode", type: "text", required: true, default: "test", label: "Mode", description: "test or live" },
      { name: "secret_key", secret: true, label: "Secret key" },
      { name: "webhook_secret", secret: true, label: "Webhook signing secret" },
      { name: "publishable_key", type: "text", label: "Publishable key" },
      { name: "settings", type: "json", label: "Settings", description: "What the adapter asks for beyond the keys" },
      { name: "enabled", type: "bool", required: true, default: true, label: "Enabled" },
      { name: "synced_at", type: "timestamp", label: "Last swept", description: "When the reconcile sweep last read the provider's events" },
    ],
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "pay_objects",
    label: "Payment objects",
    description: "A mirror of the provider's checkouts, payments, customers and subscriptions, as the provider last said they were. The module writes it; a user may read their own, so an application can show when their plan renews.",
    fields: [
      { name: "account", references: "pay_accounts", required: true, label: "Account" },
      { name: "kind", type: "text", required: true, label: "Kind", description: OBJECT_KINDS.join(", ") },
      { name: "provider_ref", type: "text", required: true, label: "Provider id" },
      { name: "user_id", references: "users", label: "User" },
      { name: "email", type: "text", label: "Email" },
      { name: "ref_table", type: "text", label: "For table", description: "The application table of the row it is for" },
      { name: "ref_key", type: "json", label: "For row", description: "That row's key, as the row has it: a number, or text" },
      { name: "status", type: "text", label: "Status" },
      { name: "amount", type: "int", label: "Amount", description: "In the currency's smallest unit" },
      { name: "currency", type: "text", label: "Currency" },
      { name: "price_ref", type: "text", label: "Price", description: "Subscriptions: the provider's price or product" },
      { name: "current_period_end", type: "timestamp", label: "Period ends", description: "Subscriptions: when the period paid for ends — the renewal date" },
      { name: "trial_end", type: "timestamp", label: "Trial ends", description: "Subscriptions: when a trial ends, or ended" },
      { name: "cancel_at_period_end", type: "bool", label: "Ends at period end", description: "Subscriptions: canceled, and running to the end of the period paid for" },
      { name: "lines", type: "json", label: "Lines", description: "The lines as charged" },
      { name: "effects", type: "json", label: "Effects", description: "What the module grants once it is paid" },
      { name: "provider_data", type: "json", label: "Provider data" },
      { name: "issue", type: "text", label: "Issue", description: "Why the module could not do what it should have for it: a subscription whose price is no plan's, a role it may not grant" },
      { name: "created_at", type: "timestamp", label: "Created" },
      { name: "updated_at", type: "timestamp", label: "Updated" },
    ],
    unique: [["account", "provider_ref"]],
    indexes: [["ref_table", "ref_key"]],
    // A user may read their own, and nobody but the module writes.
    ownership: "_read && user_id === user.id",
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "pay_events",
    label: "Payment events",
    description: "One row per thing that happened at a provider, in the module's own words. Put your triggers here: an insert with kind payment.succeeded and ref_table orders is an order paid.",
    fields: [
      { name: "account", references: "pay_accounts", required: true, label: "Account" },
      { name: "event_id", type: "text", required: true, label: "Provider event id" },
      { name: "kind", type: "text", required: true, label: "Kind", description: EVENT_KINDS.join(", ") },
      { name: "object", references: "pay_objects", label: "Object" },
      { name: "user_id", references: "users", label: "User" },
      { name: "email", type: "text", label: "Email" },
      { name: "ref_table", type: "text", label: "For table" },
      { name: "ref_key", type: "json", label: "For row", description: "That row's key, so that id === row.ref_key finds it" },
      { name: "amount", type: "int", label: "Amount" },
      { name: "currency", type: "text", label: "Currency" },
      { name: "status", type: "text", label: "Status" },
      { name: "provider_type", type: "text", label: "Provider event type" },
      { name: "raw", type: "json", label: "Raw event" },
      { name: "created_at", type: "timestamp", label: "Received" },
    ],
    unique: [["account", "event_id", "kind"]],
    indexes: [["ref_table", "ref_key"]],
    min_role_read: 1,
    min_role_write: 1,
  },
];

module.exports = { tables, EVENT_KINDS, OBJECT_KINDS };
