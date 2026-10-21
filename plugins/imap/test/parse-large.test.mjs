// parse-large.js (TODO-imap.md M7.3) with a fake fetcher: every generated mail (M1.2) is turned
// into the `bodyStructure` imapflow would report for it, and the over-the-cap parse must agree
// with the whole-source parse on everything but the attachments' bytes.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { parse, split } = require("../parse.js");
const { parseLarge, TEXT_FETCH_LIMIT } = require("../parse-large.js");

const here = dirname(fileURLToPath(import.meta.url));
const generated = join(here, "fixtures/generated");
const expected = JSON.parse(readFileSync(join(generated, "expected.json"), "utf8"));

/**
 * What a server reports for a mail: imapflow's `bodyStructure` (built the way imapflow's
 * `parseBodystructure` numbers parts, including giving an embedded message's single body part
 * the wrapper's number), the header block, and `BODY[<part>]` for each IMAP part number.
 */
async function serverView(source) {
  const nodes = await split(source);
  const children = new Map(nodes.map(({ node }) => [node, []]));
  const raws = new Map(nodes.map(({ node, raw }) => [node, raw]));
  for (const { node } of nodes) if (node.parentNode) children.get(node.parentNode).push(node);
  const bodies = new Map();
  const headerOf = (node, name) => {
    const line = node.headers.getList().find((h) => h.key === name);
    return line ? line.line.slice(line.line.indexOf(":") + 1).trim() : "";
  };
  const walk = (node, path, inMessage) => {
    const out = {};
    if (path.length) out.part = path.join(".");
    if (node.multipart) {
      out.type = `multipart/${node.multipart}`;
      out.childNodes = children.get(node).map((child, i) => walk(child, path.concat(i + 1)));
      return out;
    }
    out.type = node.contentType || "text/plain";
    const params = {};
    if (node.charset) params.charset = node.charset;
    if (node.flowed) params.format = "flowed";
    if (node.filename && node.disposition !== "attachment") params.name = node.filename;
    out.parameters = params;
    const id = headerOf(node, "content-id");
    if (id) out.id = id;
    out.encoding = node.encoding || "7bit";
    if (node.disposition) {
      out.disposition = node.disposition;
      if (node.filename) out.dispositionParameters = { filename: node.filename };
    }
    if (node.messageNode) {
      out.size = 0;
      out.childNodes = [walk(children.get(node)[0], path, true)];
      return out;
    }
    out.size = raws.get(node).length;
    // The address IMAP gives this body.
    const imapPath = !path.length ? "1" : inMessage ? `${path.join(".")}.1` : path.join(".");
    bodies.set(imapPath, raws.get(node));
    return out;
  };
  const end = source.indexOf("\r\n\r\n");
  const lf = source.indexOf("\n\n");
  const cut = end >= 0 && (lf < 0 || end < lf) ? end + 4 : lf >= 0 ? lf + 2 : 0;
  return { bodyStructure: walk(nodes[0].node, []), headers: source.subarray(0, cut), bodies };
}

/** A fetcher over `bodies` that records what it was asked for. */
function fakeFetcher(bodies) {
  const asked = [];
  const fetchPart = async (part, { maxBytes } = {}) => {
    asked.push({ part, maxBytes });
    if (!bodies.has(part)) throw new Error(`no part ${part}`);
    const body = bodies.get(part);
    return maxBytes ? body.subarray(0, maxBytes) : body;
  };
  return { fetchPart, asked };
}

// The mails whose parts this structure cannot express the way the source does: a part with no
// Content-Type at all (a server reports text/plain), and a multipart whose closing boundary
// never comes (a server's structure is its own guess).
const DIFFERENT = new Set(["attachment-missing-params.eml", "missing-closing-boundary.eml"]);

for (const mail of expected.filter((m) => !DIFFERENT.has(m.file))) {
  test(`${mail.file}: the over-the-cap parse agrees with the whole-source parse`, async () => {
    const source = readFileSync(join(generated, mail.file));
    const opts = { internalDate: new Date("2025-10-15T12:00:00Z"), flags: ["\\Seen"], size: 30_000_000 };
    const whole = await parse(source, opts);
    const { bodyStructure, headers, bodies } = await serverView(source);
    const { fetchPart, asked } = fakeFetcher(bodies);
    const large = await parseLarge(bodyStructure, fetchPart, { ...opts, headers });

    assert.equal(large.message.import_error, null);
    assert.equal(large.message.body_state, "too_large");
    for (const key of ["subject", "from_addr", "from_name", "message_id", "thread_key", "sent_at", "body_text", "body_html", "snippet", "has_attachments", "seen", "size"]) {
      assert.deepEqual(large.message[key], whole.message[key], key);
    }
    const shape = (a) => [a.part, a.filename, a.content_type, a.cid, a.inline];
    assert.deepEqual(large.attachments.map(shape), whole.attachments.map(shape));
    // Attachments are listed, not downloaded; only a TNEF container's contents have bytes.
    for (const a of large.attachments) assert.equal(a.content === null, !a.part.includes(":tnef:"), a.filename);
    // Only the body parts and TNEF containers were downloaded, the text ones with a limit.
    const attachmentParts = new Set(whole.attachments.filter((a) => !a.part.includes(":")).map((a) => a.part));
    for (const { part, maxBytes } of asked) {
      assert.ok(!attachmentParts.has(part), `downloaded the attachment ${part}`);
      if (maxBytes !== undefined) assert.equal(maxBytes, TEXT_FETCH_LIMIT);
    }
  });
}

test("a part that cannot be downloaded is named in import_error, and the row is still written", async () => {
  const source = readFileSync(join(generated, "alternative-both.eml"));
  const { bodyStructure, headers } = await serverView(source);
  const large = await parseLarge(bodyStructure, async () => { throw new Error("connection lost"); }, { headers, size: 30_000_000 });
  assert.equal(large.message.subject, "Both");
  assert.match(large.message.import_error, /part 1: connection lost/);
  assert.ok(large.message.body_text, "a placeholder at least");
});

test("an embedded single-part message's body is part <n>.1, as IMAP numbers it", async () => {
  const source = readFileSync(join(generated, "journal-report.eml"));
  const { bodyStructure, headers, bodies } = await serverView(source);
  assert.equal(bodyStructure.childNodes[1].childNodes[0].part, "2", "imapflow's numbering");
  const { fetchPart, asked } = fakeFetcher(bodies);
  const large = await parseLarge(bodyStructure, fetchPart, { headers });
  assert.deepEqual(asked.map((a) => a.part), ["1", "2.1"]);
  assert.match(large.message.body_text, /The journaled body\./);
});

test("no structure and no headers still give a row", async () => {
  const large = await parseLarge(null, async () => Buffer.alloc(0), {});
  assert.equal(large.message.body_state, "too_large");
  assert.ok(large.message.body_text);
});
