// LLM credits on the users screen (TODO-payments P6.2).
//
// A user's credits are what a metered agent's model calls are charged to: one
// bucket per grant (an admin's by hand, a plan's allowance, a top-up), drawn
// down soonest-expiring first, with a debt when a call took more than was left.
// This module is the dialog's pure half: how an entry and a bucket read, and
// the bodies the two forms send. Amounts stay decimal strings throughout — the
// server keeps them exact, and a float on the way would make a balance drift.

import type {
  GetUserCreditsResponse,
  GrantUserCreditsRequest,
  SetUserCreditThresholdRequest,
} from "./client";

export type CreditBucketRow = GetUserCreditsResponse["buckets"][number];
export type CreditEntryRow = GetUserCreditsResponse["entries"][number];

/** A decimal string as typed: digits with at most one point, no sign. */
const AMOUNT = /^\d+(\.\d+)?$|^\.\d+$/;

/** Where a bucket stands: something left that counts, used up, or expired. */
export type BucketState = "live" | "spent" | "expired";

export function bucketState(bucket: CreditBucketRow): BucketState {
  if (!bucket.live) return "expired";
  return Number(bucket.remaining) > 0 ? "live" : "spent";
}

/** The badge words and tone for a bucket's state. */
export function bucketStateBadge(state: BucketState): { label: string; tone: "green" | "secondary" | "red" } {
  switch (state) {
    case "live":
      return { label: "Available", tone: "green" };
    case "spent":
      return { label: "Used up", tone: "secondary" };
    case "expired":
      return { label: "Expired", tone: "red" };
  }
}

/** Who granted a bucket, for a human. An admin's key is a random id that means
 * nothing to read; a module's (an invoice, a pack) is what tells two apart. */
export function bucketSourceLabel(bucket: CreditBucketRow): string {
  if (bucket.source === "admin") return "By an admin";
  return `${bucket.source}: ${bucket.key}`;
}

/** What an entry was, in a few words: a grant, a model call, or a notice. */
export function entryLabel(entry: CreditEntryRow): string {
  switch (entry.kind) {
    case "grant":
      return entry.source === "admin" ? "Granted by an admin" : `Granted by ${entry.source}`;
    case "debit": {
      const what = [entry.agent && `agent ${entry.agent}`, entry.model].filter(Boolean).join(", ");
      return what ? `Model call (${what})` : "Model call";
    }
    case "low_balance":
      return "Fell below the low-balance threshold";
    default:
      return entry.kind;
  }
}

/** An entry's tokens as one short phrase, or `null` for an entry with none. */
export function entryTokens(entry: CreditEntryRow): string | null {
  if (entry.input_tokens == null && entry.output_tokens == null) return null;
  return `${entry.input_tokens ?? 0} in, ${entry.output_tokens ?? 0} out`;
}

/** Whether a user is in debt: a debit took more than their buckets held. */
export function inDebt(credits: GetUserCreditsResponse): boolean {
  return Number(credits.debt) > 0;
}

/** The "Grant credits" form. Days blank means the credits never expire. */
export type CreditGrantForm = { amount: string; days: string; note: string };

export function newCreditGrantForm(): CreditGrantForm {
  return { amount: "", days: "", note: "" };
}

/** The request body, or a sentence saying what is wrong with the form. */
export function creditGrantBody(form: CreditGrantForm): GrantUserCreditsRequest | string {
  const amount = form.amount.trim();
  if (!AMOUNT.test(amount) || Number(amount) <= 0) {
    return "The amount must be a number of credits greater than 0, like 100 or 2.5.";
  }
  const days = form.days.trim();
  let expires_in_days: number | null = null;
  if (days !== "") {
    const n = Number(days);
    if (!Number.isInteger(n) || n < 1) {
      return "Days must be a whole number of at least 1, or blank for credits that never expire.";
    }
    expires_in_days = n;
  }
  const note = form.note.trim();
  return { amount, expires_in_days, note: note === "" ? null : note };
}

/** The threshold box's value as the server takes it: blank clears it. */
export function thresholdBody(text: string): SetUserCreditThresholdRequest | string {
  const value = text.trim();
  if (value === "") return { threshold: null };
  if (!AMOUNT.test(value)) {
    return "The threshold must be a number of credits, like 10, or blank for none.";
  }
  return { threshold: value };
}
