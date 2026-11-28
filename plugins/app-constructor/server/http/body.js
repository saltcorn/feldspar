// Reading a request's body fields, refusing ones of the wrong type.

"use strict";

const { invalid } = require("../rules/model.js");

const bodyOf = (req) =>
  req.body && typeof req.body === "object" ? req.body : {};

function text(body, name) {
  if (typeof body[name] !== "string") throw invalid(`\`${name}\` must be text`);
  return body[name];
}

/** An optional text field: absent or null is none. */
function optionalText(body, name) {
  const value = body[name];
  if (value === undefined || value === null) return null;
  if (typeof value !== "string")
    throw invalid(`\`${name}\` must be text, got ${JSON.stringify(value)}`);
  return value;
}

/** A list of `{ <idField>, text }`, refused whole with `message` when it is not one. */
function pairs(value, idField, message) {
  if (!Array.isArray(value)) throw invalid(message);
  return value.map((item) => {
    const id = item && item[idField];
    const ok =
      item &&
      typeof item === "object" &&
      (Number.isInteger(id) || (typeof id === "string" && id)) &&
      typeof item.text === "string";
    if (!ok) throw invalid(message);
    return { [idField]: id, text: item.text };
  });
}

module.exports = { bodyOf, text, optionalText, pairs };
