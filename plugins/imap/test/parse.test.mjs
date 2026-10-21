// parse.js (TODO-imap.md M7.2) against the generated mails (M1.2): every `expect` in
// fixtures/generated/expected.json, as fixtures/README.md defines them, plus M7.5's (a)–(c) for
// each mail.
//
//   npm test            (in plugins/imap)

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const { parse, PLACEHOLDERS, LIMITS } = require("../parse.js");

const here = dirname(fileURLToPath(import.meta.url));
const generated = join(here, "fixtures/generated");
const expected = JSON.parse(readFileSync(join(generated, "expected.json"), "utf8"));

const INTERNAL_DATE = new Date("2025-10-15T12:00:00Z");
const norm = (s) => String(s ?? "").replace(/\s+/g, " ").trim();
const sha256 = (buf) => createHash("sha256").update(buf).digest("hex");

/** M7.5 (a)–(c), for any parsed mail: a body or a placeholder, and no NUL anywhere. */
export function assertSane({ message, attachments }, label) {
  assert.ok(norm(message.body_text) || norm(message.body_html), `${label}: no body and no placeholder`);
  const strings = [];
  const walk = (v) => {
    if (typeof v === "string") strings.push(v);
    else if (Array.isArray(v)) v.forEach(walk);
    else if (v && typeof v === "object" && !Buffer.isBuffer(v)) Object.values(v).forEach(walk);
  };
  walk(message);
  attachments.forEach((a) => walk({ ...a, content: null }));
  for (const s of strings) assert.ok(!s.includes("\0"), `${label}: a NUL byte in ${JSON.stringify(s.slice(0, 60))}`);
  assert.ok(message.body_text.length <= LIMITS.body, `${label}: body_text too long`);
  assert.ok(!message.body_html || message.body_html.length <= LIMITS.body, `${label}: body_html too long`);
  assert.ok(message.snippet.length <= LIMITS.snippet, `${label}: snippet too long`);
}

for (const mail of expected) {
  test(`${mail.file}: ${mail.description}`, async () => {
    const source = readFileSync(join(generated, mail.file));
    const parsed = await parse(source, { internalDate: INTERNAL_DATE, flags: ["\\Seen"], size: source.length });
    const { message: m, attachments } = parsed;
    const e = mail.expect;
    assertSane(parsed, mail.file);
    assert.equal(m.import_error, null);
    assert.equal(m.seen, true);
    assert.equal(m.flagged, false);

    for (const key of ["subject", "from_name", "from_addr", "message_id", "in_reply_to", "thread_key"]) {
      if (key in e) assert.equal(m[key] || null, e[key] === "" ? null : e[key], key);
    }
    if ("subject" in e && e.subject === "") assert.equal(m.subject, "");
    if ("sent_at" in e) assert.equal(m.sent_at, e.sent_at ?? INTERNAL_DATE.toISOString(), "sent_at");
    for (const key of ["to_addrs", "cc_addrs"]) {
      if (key in e) assert.deepEqual(m[key].map((a) => a.address), e[key], key);
    }
    if ("references" in e) assert.deepEqual(m.references, e.references, "references");
    for (const s of e.subject_includes || []) assert.ok(m.subject.includes(s), `subject ${JSON.stringify(m.subject)} lacks ${s}`);
    for (const s of e.body_text_includes || []) assert.ok(norm(m.body_text).includes(norm(s)), `body_text lacks ${JSON.stringify(s)}: ${JSON.stringify(m.body_text.slice(0, 300))}`);
    for (const s of e.body_text_excludes || []) assert.ok(!norm(m.body_text).includes(norm(s)), `body_text has ${JSON.stringify(s)}`);
    for (const s of e.body_html_includes || []) assert.ok(norm(m.body_html).includes(norm(s)), `body_html lacks ${JSON.stringify(s)}: ${JSON.stringify((m.body_html || "").slice(0, 300))}`);
    for (const s of e.body_html_excludes || []) assert.ok(!norm(m.body_html).includes(norm(s)), `body_html has ${JSON.stringify(s)}`);
    if ("body_html" in e && e.body_html === null) assert.equal(m.body_html, null, "body_html");
    if ("body_text_max_length" in e) assert.ok(m.body_text.length <= e.body_text_max_length);
    if ("placeholder" in e) assert.equal(m.body_text, PLACEHOLDERS[e.placeholder], "placeholder");
    if ("attachments" in e) {
      assert.deepEqual(
        attachments.map((a) => ({
          filename: a.filename,
          content_type: a.content_type,
          inline: a.inline,
          size: a.size,
          sha256: sha256(a.content),
          ...(a.cid ? { cid: a.cid } : {}),
        })),
        e.attachments,
      );
      for (const a of attachments) assert.equal(a.content.length, a.size);
    }
    if ("attachment_names" in e) assert.deepEqual(attachments.map((a) => a.filename), e.attachment_names);
    assert.equal(m.has_attachments, attachments.some((a) => !a.inline));
  });
}

test("attachments carry their IMAP part numbers", async () => {
  const { attachments } = await parse(readFileSync(join(generated, "apple-mail-nested.eml")));
  assert.deepEqual(attachments.map((a) => a.part), ["1.2.2", "2"]);
  const single = await parse(Buffer.from("Content-Type: application/pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\n\r\n%PDF"));
  assert.deepEqual(single.attachments.map((a) => a.part), ["1"]);
  const tnef = await parse(readFileSync(join(generated, "tnef-with-attachment.eml")));
  assert.deepEqual(tnef.attachments.map((a) => a.part), ["2:tnef:0"]);
});

test("an inline message/rfc822 is opened and its body kept; an attached one is kept whole", async () => {
  const { message, attachments } = await parse(readFileSync(join(generated, "rfc822-inline-and-attached.eml")));
  assert.equal(message.subject, "Two forwards");
  const forwarded = attachments.find((a) => a.filename === "forwarded.eml");
  assert.equal(forwarded.content_type, "message/rfc822");
  assert.match(forwarded.content.toString(), /^From: Bo Inner[\s\S]*Attached inner body\./);
});

test("a TNEF body with only RTF becomes body_text, and body_html stays empty", async () => {
  const { message } = await parse(readFileSync(join(generated, "tnef-rtf-only.eml")));
  assert.match(message.body_text, /Hello from Outlook\.\s+Grüße aus Köln\./);
  assert.equal(message.body_html, null);
});

test("every header is optional, and nothing throws on junk", async () => {
  for (const source of ["", "\r\n\r\n", "garbage without a colon", "Content-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\n", Buffer.alloc(1000, 0)]) {
    const parsed = await parse(Buffer.from(source));
    assertSane(parsed, JSON.stringify(source).slice(0, 30));
    assert.equal(parsed.message.sent_at, null);
    assert.deepEqual(parsed.message.to_addrs, []);
  }
});

test("a declared UTF-8 body that is not UTF-8 is read as windows-1252, not as replacement characters", async () => {
  const source = Buffer.concat([Buffer.from("Subject: x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"), Buffer.from("Gr\xfc\xdfe 20 \x80", "latin1")]);
  const { message } = await parse(source);
  assert.equal(message.body_text, "Grüße 20 €");
});

test("an HTML part with no declared charset uses its <meta charset>", async () => {
  const html = Buffer.concat([Buffer.from('<html><head><meta charset="iso-8859-1"></head><body><p>M'), Buffer.from([0xfc]), Buffer.from("nchen</p></body></html>")]);
  const source = Buffer.concat([Buffer.from("Subject: x\r\nContent-Type: text/html\r\n\r\n"), html]);
  const { message } = await parse(source);
  assert.match(message.body_html, /München/);
  assert.match(message.body_text, /München/);
});

test("format=flowed text is unwrapped", async () => {
  const source = Buffer.from("Content-Type: text/plain; charset=utf-8; format=flowed\r\n\r\nA long line that was \r\nwrapped by the sender.\r\n");
  const { message } = await parse(source);
  assert.match(message.body_text, /A long line that was wrapped by the sender\./);
});

test("the snippet is the body's start, with whitespace collapsed", async () => {
  const { message } = await parse(readFileSync(join(generated, "long-body.eml")));
  assert.equal(message.snippet.length, 200);
  assert.ok(message.snippet.startsWith("Lorem ipsum dolor sit amet"));
  assert.ok(!/\s{2}|\n/.test(message.snippet));
});

// Fixes found on the corpus (M7.5).

const mail = (...lines) => Buffer.from(lines.join("\r\n"), "latin1");

test("a Q-encoded word with a raw 8-bit byte inside is decoded once (Outlook Express)", async () => {
  const { message } = await parse(mail("Subject: =?iso-8859-1?Q?Die_Hasen_und_die_Fr\xf6sche?=", "", "x"));
  assert.equal(message.subject, "Die Hasen und die Frösche");
});

test("a raw 8-bit header is read in the charset the body declares", async () => {
  const subject = Buffer.from([0xbf, 0xa9, 0xb1, 0xe2]).toString("latin1"); // 여기 in EUC-KR
  const { message } = await parse(mail(`Subject: ${subject}`, "Content-Type: text/plain; charset=euc-kr", "", "x"));
  assert.equal(message.subject, "여기");
});

test("base64 stops at the padding of a line: junk after it is not the next chunk", async () => {
  const { message } = await parse(mail("Content-Type: text/plain; charset=utf-8", "Content-Transfer-Encoding: base64", "", "ecO2dMOkIHZhYW4uCg== ignored", ""));
  assert.equal(message.body_text.trim(), "yötä vaan.");
});

test("a boundary line with transport padding is still a boundary (RFC 2046)", async () => {
  const { message, attachments } = await parse(
    mail('Content-Type: multipart/mixed; boundary="1"', "", "--1  ", "Content-Type: text/plain", "", "A", "--1\t", "Content-Type: image/png", "Content-Disposition: attachment; filename=a.png", "", "PNG", "--1-- ", ""),
  );
  assert.equal(message.body_text.trim(), "A");
  assert.deepEqual(attachments.map((a) => a.filename), ["a.png"]);
  // A signature delimiter, `-- `, is not a boundary and keeps its space.
  const sig = await parse(mail("Content-Type: text/plain", "", "Hi", "-- ", "Ada"));
  assert.match(sig.message.body_text, /\n-- \n/);
});

test("multipart/alternative gives each view one branch; text/enriched is dropped", async () => {
  const enriched = await parse(
    mail("Content-Type: multipart/alternative; boundary=a", "", "--a", "Content-Type: text/plain", "", "plain", "--a", "Content-Type: text/enriched", "", "<bold>rich</bold>", "--a--"),
  );
  assert.equal(enriched.message.body_text.trim(), "plain");
  assert.deepEqual(enriched.attachments, []);
  const related = await parse(
    mail("Content-Type: multipart/alternative; boundary=a", "", "--a", "Content-Type: text/plain", "", "plain", "--a", "Content-Type: multipart/related; boundary=r", "", "--r", "Content-Type: text/html", "", "<p>html</p>", "--r", "Content-Type: image/png", "Content-ID: <i@x>", "", "PNG", "--r--", "--a--"),
  );
  assert.equal(related.message.body_text.trim(), "plain");
  assert.equal(related.message.body_html.trim(), "<p>html</p>");
  assert.deepEqual(related.attachments.map((a) => [a.cid, a.inline]), [["i@x", true]]);
});

test("a text part next to an HTML part in multipart/mixed is in both views", async () => {
  const { message } = await parse(
    mail("Content-Type: multipart/mixed; boundary=m", "", "--m", "Content-Type: text/html", "", "<p>first</p>", "--m", "Content-Type: text/plain", "", "a < b", "--m--"),
  );
  assert.match(message.body_text, /first\s+a < b/);
  assert.match(message.body_html, /<p>first<\/p>\s*<div style="white-space: pre-wrap">a &lt; b<\/div>/);
});

test("an inline message/rfc822 that is base64-encoded is opened, and its parts numbered inside it", async () => {
  const inner = Buffer.from("Subject: inner\r\nContent-Type: multipart/mixed; boundary=i\r\n\r\n--i\r\nContent-Type: text/plain\r\n\r\ninner body\r\n--i\r\nContent-Type: image/gif\r\nContent-Disposition: attachment; filename=a.gif\r\n\r\nGIF89a\r\n--i--\r\n");
  const { message, attachments } = await parse(mail("Subject: outer", "Content-Type: message/rfc822", "Content-Transfer-Encoding: base64", "", inner.toString("base64")));
  assert.equal(message.subject, "outer");
  assert.equal(message.body_text.trim(), "inner body");
  assert.deepEqual(attachments.map((a) => [a.filename, a.part, a.content.toString()]), [["a.gif", "1:eml:2", "GIF89a"]]);
});

test("a nameless part of a type libmime does not know gets its subtype as the extension", async () => {
  const { attachments } = await parse(mail("Content-Type: multipart/mixed; boundary=m", "", "--m", "Content-Type: text/plain", "", "x", "--m", "Content-Type: audio/mp3", "", "ID3", "--m--"));
  assert.deepEqual(attachments.map((a) => a.filename), ["part-2.mp3"]);
});
