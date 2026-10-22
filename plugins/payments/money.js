// Money (TODO-payments §3.11): an amount is an integer in the currency's minor unit, beside
// an ISO 4217 code. Never a float. The adapters convert where a provider counts otherwise.

"use strict";

/** Currencies with no minor unit: 500 JPY is `500`. */
const ZERO_DECIMAL = new Set([
  "BIF", "CLP", "DJF", "GNF", "ISK", "JPY", "KMF", "KRW", "PYG", "RWF", "UGX", "UYI", "VND",
  "VUV", "XAF", "XOF", "XPF",
]);

/** Currencies with three decimals: 1.500 KWD is `1500`. */
const THREE_DECIMAL = new Set(["BHD", "IQD", "JOD", "KWD", "LYD", "OMR", "TND"]);

/** An ISO 4217 code, upper-cased, or a refusal naming what was given. */
function currencyCode(value) {
  const code = String(value === undefined || value === null ? "" : value).trim().toUpperCase();
  if (!/^[A-Z]{3}$/.test(code)) {
    throw new Error(`"${value}" is not a currency: give its three-letter ISO code, such as USD or EUR`);
  }
  return code;
}

/** How many decimals the currency's minor unit has: 0, 2 or 3. */
function minorUnits(currency) {
  const code = currencyCode(currency);
  if (ZERO_DECIMAL.has(code)) return 0;
  if (THREE_DECIMAL.has(code)) return 3;
  return 2;
}

/** An amount in minor units, or a refusal naming `what`: a whole number, never negative, and
 * small enough to count exactly. */
function requireAmount(value, what) {
  if (typeof value === "string" && /^\d+$/.test(value.trim())) value = Number(value.trim());
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error(
      `${what} must be a whole number of the currency's smallest unit (1999 for 19.99), not ${JSON.stringify(value)}`,
    );
  }
  return value;
}

/** `1999, "USD"` as `19.99 USD`, by integer arithmetic. */
function format(amount, currency) {
  const code = currencyCode(currency);
  const units = minorUnits(code);
  const sign = amount < 0 ? "-" : "";
  const digits = String(Math.abs(amount));
  if (units === 0) return `${sign}${digits} ${code}`;
  const padded = digits.padStart(units + 1, "0");
  return `${sign}${padded.slice(0, -units)}.${padded.slice(-units)} ${code}`;
}

/** `"19.99", "USD"` as `1999`, from the decimal text, exactly. */
function parseDecimal(text, currency) {
  const units = minorUnits(currency);
  const match = /^(\d+)(?:\.(\d+))?$/.exec(String(text).trim());
  if (!match || (match[2] || "").length > units) {
    throw new Error(`"${text}" is not an amount of ${currencyCode(currency)} (at most ${units} decimals)`);
  }
  const whole = Number(match[1]) * 10 ** units;
  const fraction = Number((match[2] || "").padEnd(units, "0") || "0");
  return requireAmount(whole + fraction, "the amount");
}

module.exports = { currencyCode, minorUnits, requireAmount, format, parseDecimal };
