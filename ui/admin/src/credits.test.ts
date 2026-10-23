/**
 * LLM credits on the users screen (TODO-payments P6.2): how a bucket and an
 * entry read, and the bodies the grant and threshold forms send.
 */

import { describe, expect, it } from "vitest";

import {
  bucketSourceLabel,
  bucketState,
  creditGrantBody,
  entryLabel,
  entryTokens,
  inDebt,
  newCreditGrantForm,
  thresholdBody,
  type CreditBucketRow,
  type CreditEntryRow,
} from "./credits";

function bucket(over: Partial<CreditBucketRow> = {}): CreditBucketRow {
  return {
    id: "b1",
    amount: "100",
    remaining: "40",
    kind: "allowance",
    source: "@feldspar/payments",
    key: "invoice:in_1",
    expires_at: "2026-11-01T00:00:00Z",
    note: null,
    created_at: "2026-10-01T00:00:00Z",
    live: true,
    ...over,
  };
}

function entry(over: Partial<CreditEntryRow> = {}): CreditEntryRow {
  return {
    id: "e1",
    kind: "debit",
    amount: "-0.25",
    balance: "39.75",
    source: "agent",
    key: "run:1",
    run: "r1",
    agent: "helper",
    model: "claude-sonnet-5",
    input_tokens: 1200,
    output_tokens: 300,
    cost: "0.0081",
    note: null,
    created_at: "2026-10-02T00:00:00Z",
    ...over,
  };
}

describe("buckets", () => {
  it("are available, used up or expired", () => {
    expect(bucketState(bucket())).toBe("live");
    expect(bucketState(bucket({ remaining: "0" }))).toBe("spent");
    expect(bucketState(bucket({ live: false }))).toBe("expired");
  });

  it("name a module's key but not an admin's random one", () => {
    expect(bucketSourceLabel(bucket())).toBe("@feldspar/payments: invoice:in_1");
    expect(bucketSourceLabel(bucket({ source: "admin", key: "5f0c…" }))).toBe("By an admin");
  });
});

describe("entries", () => {
  it("say what happened and what was used", () => {
    expect(entryLabel(entry())).toBe("Model call (agent helper, claude-sonnet-5)");
    expect(entryTokens(entry())).toBe("1200 in, 300 out");
    expect(entryLabel(entry({ kind: "grant", source: "admin" }))).toBe("Granted by an admin");
    expect(entryLabel(entry({ kind: "low_balance" }))).toContain("low-balance");
    expect(entryTokens(entry({ kind: "grant", input_tokens: null, output_tokens: null }))).toBeNull();
  });

  it("show debt when a call took more than was left", () => {
    const view = { balance: "-1.5", available: "0", debt: "1.5", buckets: [], entries: [] };
    expect(inDebt(view)).toBe(true);
    expect(inDebt({ ...view, debt: "0" })).toBe(false);
  });
});

describe("the forms", () => {
  it("send the amount exactly, as typed", () => {
    expect(creditGrantBody({ amount: " 2.50 ", days: "", note: "" })).toEqual({
      amount: "2.50",
      expires_in_days: null,
      note: null,
    });
    expect(creditGrantBody({ amount: "100", days: "30", note: " welcome " })).toEqual({
      amount: "100",
      expires_in_days: 30,
      note: "welcome",
    });
  });

  it("refuse what the server would", () => {
    expect(typeof creditGrantBody(newCreditGrantForm())).toBe("string");
    expect(typeof creditGrantBody({ amount: "0", days: "", note: "" })).toBe("string");
    expect(typeof creditGrantBody({ amount: "-5", days: "", note: "" })).toBe("string");
    expect(typeof creditGrantBody({ amount: "1e3", days: "", note: "" })).toBe("string");
    expect(typeof creditGrantBody({ amount: "5", days: "1.5", note: "" })).toBe("string");
  });

  it("clear the threshold when it is blank", () => {
    expect(thresholdBody("")).toEqual({ threshold: null });
    expect(thresholdBody(" 10 ")).toEqual({ threshold: "10" });
    expect(typeof thresholdBody("ten")).toBe("string");
  });
});
