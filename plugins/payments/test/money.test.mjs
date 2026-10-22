import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const { currencyCode, minorUnits, requireAmount, format, parseDecimal } = createRequire(import.meta.url)("../money.js");

test("amounts are whole minor units, formatted by integer arithmetic", () => {
  assert.equal(format(1999, "usd"), "19.99 USD");
  assert.equal(format(5, "EUR"), "0.05 EUR");
  assert.equal(format(500, "JPY"), "500 JPY");
  assert.equal(format(1500, "KWD"), "1.500 KWD");
  assert.equal(minorUnits("jpy"), 0);
  assert.equal(minorUnits("KWD"), 3);
  assert.equal(minorUnits("GBP"), 2);
});

test("a decimal is read exactly, and nothing that is not money is accepted", () => {
  assert.equal(parseDecimal("19.99", "USD"), 1999);
  assert.equal(parseDecimal("0.1", "USD"), 10);
  assert.equal(parseDecimal("1.5", "KWD"), 1500);
  assert.equal(parseDecimal("500", "JPY"), 500);
  assert.throws(() => parseDecimal("1.999", "USD"), /at most 2 decimals/);
  assert.throws(() => parseDecimal("1.5", "JPY"), /at most 0 decimals/);
  assert.throws(() => requireAmount(19.99, "the price"), /whole number/);
  assert.throws(() => requireAmount(-1, "the price"), /whole number/);
  assert.equal(requireAmount("42", "the price"), 42);
  assert.throws(() => currencyCode("dollars"), /three-letter ISO code/);
});
