// Checks a value against a JSON Schema: the shapes the agent's tools declare (`model.js`), so a
// route that takes the same input checks it the same way. The agent loop checks a tool call's
// arguments against these schemas before the tool runs; a route has no such check, so it calls
// this one.
//
// Only the keywords the shapes use are known: `type`, `nullable`, `enum`, `properties`,
// `required`, `additionalProperties: false`, `items`, `minItems`, `maxItems` and `minimum`.
// `title` on a list's item schema names its items in messages ("phase 2", not "item 2").

"use strict";

/** What `value` is, as a message says it. */
function kindOf(value) {
  if (value === null || value === undefined) return "null";
  if (Array.isArray(value)) return "a list";
  if (typeof value === "string") return "a text";
  if (typeof value === "number") return Number.isInteger(value) ? "an integer" : "a number";
  if (typeof value === "boolean") return "a boolean";
  return "an object";
}

/** The words for a schema `type` in "must be …". */
const TYPE_WORDS = {
  string: "a text",
  integer: "an integer",
  number: "a number",
  boolean: "a boolean",
  object: "an object",
  array: "a list",
};

/** Whether `value` is of the schema type `type`. */
function isType(type, value) {
  switch (type) {
    case "string":
      return typeof value === "string";
    case "integer":
      return Number.isInteger(value);
    case "number":
      return typeof value === "number" && Number.isFinite(value);
    case "boolean":
      return typeof value === "boolean";
    case "object":
      return value !== null && typeof value === "object" && !Array.isArray(value);
    case "array":
      return Array.isArray(value);
    default:
      return true;
  }
}

const quoted = (names) => names.map((n) => `\`${n}\``).join(", ");

/** The first way `value` does not match `schema`, as a message starting with `where` (what the
 * value is, e.g. "`phases`" or "phase 2: `title`"), or `null` when it matches. `parent` is what
 * holds it ("phase 2"), which a list's titled items are named after. */
function mismatch(schema, value, where, parent = null) {
  if (value === null && schema.nullable) return null;
  if (schema.type && !isType(schema.type, value))
    return `${where} must be ${TYPE_WORDS[schema.type]}, not ${kindOf(value)}`;
  if (schema.enum && !schema.enum.includes(value))
    return `${where} must be one of ${quoted(schema.enum)}, not ${JSON.stringify(value)}`;
  if (typeof schema.minimum === "number" && typeof value === "number" && value < schema.minimum)
    return `${where} must be at least ${schema.minimum}, not ${value}`;
  if (Array.isArray(value)) return listMismatch(schema, value, where, parent);
  if (isType("object", value) && (schema.properties || schema.required))
    return objectMismatch(schema, value, where);
  return null;
}

/** The first way the list `value` does not match `schema`, or `null`. */
function listMismatch(schema, value, where, parent) {
  const { minItems = 0, maxItems = Infinity } = schema;
  if (value.length < minItems || value.length > maxItems) {
    const bound =
      maxItems === Infinity ? `at least ${minItems}` : `between ${minItems} and ${maxItems}`;
    return `${where} must have ${bound} items, not ${value.length}`;
  }
  if (!schema.items) return null;
  const title = schema.items.title;
  for (let i = 0; i < value.length; i++) {
    // A titled item says which list it is in ("phase 2: requirement 1"), so the list's own
    // name is left out; an untitled one is named by the list ("`depends_on` item 1").
    const label = title
      ? `${parent ? `${parent}: ` : ""}${title} ${i + 1}`
      : `${where} item ${i + 1}`;
    const found = mismatch(schema.items, value[i], label, label);
    if (found) return found;
  }
  return null;
}

/** The first way the object `value` does not match `schema`, or `null`. */
function objectMismatch(schema, value, where) {
  const properties = schema.properties || {};
  const names = Object.keys(properties);
  if (schema.additionalProperties === false) {
    const extra = Object.keys(value).find((key) => !names.includes(key));
    if (extra !== undefined)
      return `${where} has the unknown field \`${extra}\`; its fields are ${quoted(names)}`;
  }
  for (const name of schema.required || []) {
    if (value[name] === undefined) return `${where} has no \`${name}\``;
  }
  for (const name of names) {
    if (value[name] === undefined) continue;
    const found = mismatch(properties[name], value[name], `${where}: \`${name}\``, where);
    if (found) return found;
  }
  return null;
}

/** Throw `fail(message)` for the first way `value` does not match `schema`. `name` is what the
 * value is called in the message, e.g. "`phases`". */
function check(schema, value, name, fail) {
  const found = mismatch(schema, value, name);
  if (found) throw fail(found);
}

module.exports = { check };
