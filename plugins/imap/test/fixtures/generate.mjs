#!/usr/bin/env node
// Generate the awkward mails the public corpus does not cover (TODO-imap.md M1.2).
//
//   node generate.mjs [outDir]      (default: ./generated next to this file)
//
// Every mail is built by hand, byte by byte, rather than by a mail library: the
// point is MIME that a library would never write — an unknown charset, raw 8-bit
// headers, a NUL byte, a multipart with no closing boundary. The output is
// deterministic (no clocks, no randomness), so `fixtures.test.mjs` can check
// that the committed files are what this script writes.
//
// Next to the mails it writes `expected.json`: for each one, the lessons (§6)
// and parsing rules (§5.4) it covers, the MIME tree it must parse into, and what
// `parse.js` (M7.2) must make of it. README.md describes the vocabulary.

import { writeFileSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";

const CRLF = "\r\n";
const DATE = "Tue, 14 Oct 2025 09:30:00 +0200";
const DATE_ISO = "2025-10-14T07:30:00.000Z";

const B = (s, enc = "utf8") => (Buffer.isBuffer(s) ? s : Buffer.from(s, enc));
const sha256 = (buf) => createHash("sha256").update(buf).digest("hex");

// ---------------------------------------------------------------------------
// Charsets Node cannot encode by itself.

const CP1252_EXTRA = { "€": 0x80, "‚": 0x82, "„": 0x84, "…": 0x85, "–": 0x96, "—": 0x97, "“": 0x93, "”": 0x94, "‘": 0x91, "’": 0x92 };
function cp1252(str) {
  return Buffer.from(
    [...str].map((ch) => {
      if (CP1252_EXTRA[ch] !== undefined) return CP1252_EXTRA[ch];
      const code = ch.codePointAt(0);
      if (code > 0xff) throw new Error(`not in windows-1252: ${ch}`);
      return code;
    }),
  );
}
function latin9(str) {
  return Buffer.from(
    [...str].map((ch) => {
      if (ch === "€") return 0xa4;
      const code = ch.codePointAt(0);
      if (code > 0xff) throw new Error(`not in iso-8859-15: ${ch}`);
      return code;
    }),
  );
}

// ---------------------------------------------------------------------------
// Transfer encodings.

function base64(buf) {
  return B(buf.toString("base64").replace(/.{1,76}/g, (line) => line + CRLF));
}

function quotedPrintable(buf) {
  let out = "";
  let line = "";
  const push = (token) => {
    if (line.length + token.length > 75) {
      out += line + "=" + CRLF;
      line = "";
    }
    line += token;
  };
  for (let i = 0; i < buf.length; i++) {
    const byte = buf[i];
    if (byte === 0x0d && buf[i + 1] === 0x0a) {
      // A trailing space or tab before a line break must be encoded.
      if (/[ \t]$/.test(line)) line = line.slice(0, -1) + "=" + line.at(-1).charCodeAt(0).toString(16).toUpperCase().padStart(2, "0");
      out += line + CRLF;
      line = "";
      i++;
    } else if ((byte >= 33 && byte <= 126 && byte !== 61) || byte === 32 || byte === 9) {
      push(String.fromCharCode(byte));
    } else {
      push("=" + byte.toString(16).toUpperCase().padStart(2, "0"));
    }
  }
  return B(out + line + CRLF);
}

const encodedWord = (str, charset = "UTF-8") => `=?${charset}?B?${B(str).toString("base64")}?=`;

// ---------------------------------------------------------------------------
// The MIME tree. A node is one of
//
//   leaf:      { type, params, headers, disposition, cte, body }
//   multipart: { type: "multipart/…", children, boundary, close = true, preamble }
//   message:   { type: "message/rfc822", disposition, message: node }
//
// `type: null` leaves the Content-Type header out (RFC 2045 then says
// text/plain). `headers` are written first, in order; values may be Buffers,
// for raw 8-bit bytes.

const crlf = (s) => s.replace(/\r?\n/g, CRLF);

function headerLines(pairs) {
  return Buffer.concat(pairs.map(([name, value]) => Buffer.concat([B(`${name}: `), B(value), B(CRLF)])));
}

function contentType(node) {
  let value = node.type;
  for (const [k, v] of Object.entries(node.params || {})) value += `; ${k}=${v}`;
  if (node.children) value += `; boundary="${node.boundary}"`;
  return value;
}

function render(node) {
  const head = [...(node.headers || [])];
  if (node.type !== null) head.push(["Content-Type", contentType(node)]);
  if (node.disposition) head.push(["Content-Disposition", node.disposition]);
  if (node.cte) head.push(["Content-Transfer-Encoding", node.cte]);
  let body;
  if (node.children) {
    const pieces = [];
    if (node.preamble) pieces.push(B(crlf(node.preamble) + CRLF));
    for (const child of node.children) {
      pieces.push(B(`--${node.boundary}${CRLF}`), render(child), B(CRLF));
    }
    if (node.close !== false) pieces.push(B(`--${node.boundary}--${CRLF}`));
    body = Buffer.concat(pieces);
  } else if (node.message) {
    body = render(node.message);
  } else {
    const raw = typeof node.body === "string" ? B(crlf(node.body)) : node.body;
    body = node.cte === "base64" ? base64(raw) : node.cte === "quoted-printable" ? quotedPrintable(raw) : raw;
  }
  return Buffer.concat([headerLines(head), B(CRLF), body]);
}

// The content types Python's `email` (policy.default) yields from `walk()`: the
// node, then its children; a message/rfc822 part, then the message inside it.
function structure(node) {
  const own = node.type === null ? "text/plain" : node.type.split(";")[0].toLowerCase();
  if (node.children) return [own, ...node.children.flatMap(structure)];
  if (node.message) return [own, ...structure(node.message)];
  return [own];
}

const envelope = (extra = []) => [
  ["From", "Ada Sender <ada@feldspar.test>"],
  ["To", "orders@feldspar.test"],
  ["Date", DATE],
  ...extra,
  ["MIME-Version", "1.0"],
];

const text = (body, params = { charset: "utf-8" }, extra = {}) => ({ type: "text/plain", params, cte: "8bit", body, ...extra });
const html = (body, params = { charset: "utf-8" }, extra = {}) => ({ type: "text/html", params, cte: "8bit", body, ...extra });
const file = (type, filename, data, extra = {}) => ({
  type,
  params: { name: `"${filename}"` },
  disposition: `attachment; filename="${filename}"`,
  cte: "base64",
  body: data,
  ...extra,
});

// ---------------------------------------------------------------------------
// Compressed RTF (MS-OXRTFCP) and TNEF (MS-OXTNEF), enough to write what
// Outlook writes.

const RTF_PREBUF = B(
  "{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx",
  "latin1",
);

const CRC_TABLE = Array.from({ length: 256 }, (_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
// The LZFu CRC: CRC-32's table, but starting at 0 and with no final inversion.
export function lzfuCrc(buf) {
  let crc = 0;
  for (const byte of buf) crc = (CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8)) >>> 0;
  return crc;
}

// Greedy LZ77 against the 4 KiB ring buffer, which starts out holding the
// prebuffer — so `{\rtf1\ansi` and friends compress to references into it.
function lzfuCompress(raw) {
  if (RTF_PREBUF.length + raw.length >= 4096) throw new Error("the generator's RTF must fit the ring buffer unwrapped");
  const hist = Buffer.concat([RTF_PREBUF, raw]);
  const at = (i) => hist[i];
  const tokens = [];
  let pos = 0;
  while (pos < raw.length) {
    const writePos = RTF_PREBUF.length + pos;
    let best = 0;
    let bestOffset = 0;
    for (let o = 0; o < writePos; o++) {
      let len = 0;
      while (len < 17 && pos + len < raw.length && at(o + len) === raw[pos + len]) len++;
      if (len > best) {
        best = len;
        bestOffset = o;
      }
    }
    if (best >= 2) {
      tokens.push({ ref: (bestOffset << 4) | (best - 2) });
      pos += best;
    } else {
      tokens.push({ literal: raw[pos] });
      pos += 1;
    }
  }
  // The end marker: a reference to the current write position.
  tokens.push({ ref: ((RTF_PREBUF.length + raw.length) & 0xfff) << 4 });
  const out = [];
  for (let i = 0; i < tokens.length; i += 8) {
    const run = tokens.slice(i, i + 8);
    out.push(run.reduce((ctl, t, k) => (t.ref !== undefined ? ctl | (1 << k) : ctl), 0));
    for (const t of run) {
      if (t.ref !== undefined) out.push(t.ref >> 8, t.ref & 0xff);
      else out.push(t.literal);
    }
  }
  const data = Buffer.from(out);
  const header = Buffer.alloc(16);
  header.writeUInt32LE(data.length + 12, 0);
  header.writeUInt32LE(raw.length, 4);
  header.write("LZFu", 8, "latin1");
  header.writeUInt32LE(lzfuCrc(data), 12);
  return Buffer.concat([header, data]);
}

function tnefAttribute(level, id, data) {
  const head = Buffer.alloc(9);
  head.writeUInt8(level, 0);
  head.writeUInt32LE(id, 1);
  head.writeUInt32LE(data.length, 5);
  const sum = Buffer.alloc(2);
  sum.writeUInt16LE(data.reduce((s, b) => (s + b) & 0xffff, 0), 0);
  return Buffer.concat([head, data, sum]);
}
const u32 = (...values) => {
  const buf = Buffer.alloc(4 * values.length);
  values.forEach((v, i) => buf.writeUInt32LE(v >>> 0, 4 * i));
  return buf;
};
const pad4 = (buf) => Buffer.concat([buf, Buffer.alloc((4 - (buf.length % 4)) % 4)]);
// A MAPI property block of binary (0x0102) and 8-bit string (0x001E) values.
function mapiProps(props) {
  const parts = [u32(props.length)];
  for (const { type, id, value } of props) {
    const tag = Buffer.alloc(4);
    tag.writeUInt16LE(type, 0);
    tag.writeUInt16LE(id, 2);
    const data = type === 0x001e ? Buffer.concat([B(value, "latin1"), Buffer.from([0])]) : value;
    parts.push(tag, u32(1, data.length), pad4(data));
  }
  return Buffer.concat(parts);
}

function tnef({ rtf, attachments = [] }) {
  const parts = [u32(0x223e9f78), Buffer.from([0x01, 0x00])];
  parts.push(tnefAttribute(1, 0x00089006, u32(0x00010000))); // attTnefVersion
  parts.push(tnefAttribute(1, 0x00069007, u32(1252, 0))); // attOemCodepage
  parts.push(tnefAttribute(1, 0x00078008, B("IPM.Microsoft Mail.Note\0"))); // attMessageClass
  parts.push(tnefAttribute(1, 0x00069003, mapiProps([{ type: 0x0102, id: 0x1009, value: lzfuCompress(B(rtf, "latin1")) }]))); // attMAPIProps: PR_RTF_COMPRESSED
  for (const a of attachments) {
    const rend = Buffer.alloc(14);
    rend.writeUInt16LE(1, 0); // a file
    rend.writeUInt32LE(0xffffffff, 2);
    parts.push(tnefAttribute(2, 0x00069002, rend)); // attAttachRendData starts an attachment
    parts.push(tnefAttribute(2, 0x00018010, B(`${a.shortName}\0`, "latin1"))); // attAttachTitle
    parts.push(tnefAttribute(2, 0x0006800f, a.data)); // attAttachData
    parts.push(
      tnefAttribute(
        2,
        0x00069005, // attAttachment
        mapiProps([
          { type: 0x001e, id: 0x3707, value: a.name }, // PR_ATTACH_LONG_FILENAME
          { type: 0x001e, id: 0x370e, value: a.type }, // PR_ATTACH_MIME_TAG
        ]),
      ),
    );
  }
  return Buffer.concat(parts);
}

// ---------------------------------------------------------------------------
// The mails.

const PNG = Buffer.from(
  "89504e470d0a1a0a0000000d4948445200000001000000010806000000" + "1f15c4890000000d49444154789c6360f80f0000010101005b8d6a1a0000000049454e44ae426082",
  "hex",
);
const PNG2 = Buffer.concat([PNG.subarray(0, 16), Buffer.from([0, 0, 0, 2]), PNG.subarray(20)]);
const PDF = B("%PDF-1.4\n1 0 obj << /Type /Catalog >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n");
const ALL_BYTES = Buffer.from(Array.from({ length: 1024 }, (_, i) => i % 256));

const att = (filename, content_type, data, extra = {}) => ({ filename, content_type, inline: false, size: data.length, sha256: sha256(data), ...extra });

const innerMail = (subject, body) => ({
  headers: [
    ["From", "Bo Inner <bo@feldspar.test>"],
    ["To", "ada@feldspar.test"],
    ["Subject", subject],
    ["Date", "Mon, 13 Oct 2025 08:00:00 +0000"],
    ["Message-ID", `<${subject.toLowerCase().replace(/\W+/g, "-")}@feldspar.test>`],
  ],
  ...text(body),
});

const RTF_PLAIN = "{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0\\fswiss Calibri;}}\\f0\\fs22 Hello from Outlook.\\par Gr\\'fc\\'dfe aus K\\'f6ln.\\par}";
const RTF_FROMHTML =
  "{\\rtf1\\ansi\\ansicpg1252\\fromhtml1 \\deff0{\\fonttbl{\\f0\\fswiss Arial;}}{\\*\\htmltag19 <html>}{\\*\\htmltag34 <body>}\\htmlrtf {\\htmlrtf0 Hello in HTML-in-RTF.{\\*\\htmltag116 <br>}\\par}\\htmlrtf0 {\\*\\htmltag58 </body>}{\\*\\htmltag27 </html>}}";
const REPORT = B("Quarterly numbers: 1, 2, 3.\r\n");

const LONG_LINE = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod.\n";

const MAILS = [
  {
    file: "base64-windows-1252.eml",
    covers: ["§6.7", "§5.4 charsets"],
    description: "A base64 text body in windows-1252, with characters only that charset has (€ – „ “).",
    tree: { headers: envelope([["Subject", "Preis"]]), ...text(cp1252("Grüße aus Köln – der Preis beträgt 20 € „netto“.\n"), { charset: "windows-1252" }, { cte: "base64" }) },
    expect: { subject: "Preis", body_text_includes: ["Grüße aus Köln – der Preis beträgt 20 € „netto“."], body_html: null },
  },
  {
    file: "qp-iso-8859-15.eml",
    covers: ["§6.7", "§5.4 charsets"],
    description: "A quoted-printable body in iso-8859-15, where 0xA4 is € (in latin-1 it would be ¤).",
    tree: { headers: envelope([["Subject", "Euro"]]), ...text(latin9("Der Betrag: 99 € (neunundneunzig Euro), fällig am 1. November.\n"), { charset: "iso-8859-15" }, { cte: "quoted-printable" }) },
    expect: { body_text_includes: ["99 €", "fällig"], body_text_excludes: ["¤"] },
  },
  {
    file: "base64-html-iso-2022-jp.eml",
    covers: ["§6.7", "§5.4 charsets"],
    description: "A base64 HTML body and a subject in ISO-2022-JP, a stateful 7-bit charset.",
    tree: {
      headers: envelope([["Subject", `=?ISO-2022-JP?B?${B("\x1b$B$3$s$K$A$O\x1b(B", "latin1").toString("base64")}?=`]]),
      ...html(B("<html><body><p>\x1b$B$3$s$K$A$O\x1b(B</p></body></html>\n", "latin1"), { charset: "iso-2022-jp" }, { cte: "base64" }),
    },
    expect: { subject: "こんにちは", body_html_includes: ["<p>こんにちは</p>"], body_text_includes: ["こんにちは"] },
  },
  {
    file: "unknown-charset-utf8.eml",
    covers: ["§5.4 charsets"],
    description: "A charset nobody knows, over bytes that are valid UTF-8: decoded as UTF-8.",
    tree: { headers: envelope([["Subject", "Unknown charset"]]), ...text("Grüße, ein Gruß in UTF-8.\n", { charset: "x-feldspar-unknown" }) },
    expect: { body_text_includes: ["Grüße, ein Gruß in UTF-8."] },
  },
  {
    file: "unknown-charset-latin1.eml",
    covers: ["§5.4 charsets"],
    description: "A charset nobody knows, over bytes that are not valid UTF-8: decoded as latin-1.",
    tree: { headers: envelope([["Subject", "Unknown charset, latin-1 bytes"]]), ...text(B("Gr\xfc\xdfe aus M\xfcnchen.\n", "latin1"), { charset: '"bogus-8"' }) },
    expect: { body_text_includes: ["Grüße aus München."] },
  },
  {
    file: "charset-aliases.eml",
    covers: ["§5.4 charsets"],
    description: "Charset names as real mailers spell them: utf8 (no dash) and latin1.",
    tree: {
      headers: envelope([["Subject", "Aliases"]]),
      type: "multipart/alternative",
      boundary: "alias-b",
      children: [text("Straße in utf8.\n", { charset: "utf8" }), html(B("<p>Stra\xdfe in latin1.</p>\n", "latin1"), { charset: "latin1" })],
    },
    expect: { body_text_includes: ["Straße in utf8."], body_html_includes: ["Straße in latin1."] },
  },
  {
    file: "undeclared-8bit-headers-utf8.eml",
    covers: ["§5.4 charsets (undeclared 8-bit headers)"],
    description: "Raw UTF-8 bytes in Subject and From, with no encoded words.",
    tree: {
      headers: [
        ["From", B("Jörg Müller <joerg@feldspar.test>")],
        ["To", "orders@feldspar.test"],
        ["Subject", B("Grüße aus Köln")],
        ["Date", DATE],
        ["MIME-Version", "1.0"],
      ],
      ...text("Body.\n"),
    },
    expect: { subject: "Grüße aus Köln", from_name: "Jörg Müller", from_addr: "joerg@feldspar.test" },
  },
  {
    file: "undeclared-8bit-headers-latin1.eml",
    covers: ["§5.4 charsets (undeclared 8-bit headers)"],
    description: "Raw latin-1 bytes in Subject and From, which are not valid UTF-8.",
    tree: {
      headers: [
        ["From", B("J\xf6rg M\xfcller <joerg@feldspar.test>", "latin1")],
        ["To", "orders@feldspar.test"],
        ["Subject", B("Gr\xfc\xdfe aus K\xf6ln", "latin1")],
        ["Date", DATE],
        ["MIME-Version", "1.0"],
      ],
      ...text("Body.\n"),
    },
    expect: { subject: "Grüße aus Köln", from_name: "Jörg Müller", from_addr: "joerg@feldspar.test" },
  },
  {
    file: "encoded-word-subject.eml",
    covers: ["§5.4 charsets"],
    description: "A subject of B and Q encoded words (whitespace between them dropped), then a malformed one left as it is.",
    tree: { headers: envelope([["Subject", "=?UTF-8?B?R3LDvMOfZQ==?= =?UTF-8?Q?_aus_K=C3=B6ln?= and =?bogus?X?broken?="]]), ...text("Body.\n") },
    expect: { subject_includes: ["Grüße aus Köln"] },
  },
  {
    file: "single-part-html.eml",
    covers: ["§6.9", "§5.4 body choice (single-part text/html keeps its markup)"],
    description: "The whole mail is one text/html part, quoted-printable.",
    tree: {
      headers: envelope([["Subject", "Single-part HTML"]]),
      ...html("<html><body><h1>Order 4711</h1><p>Two <b>blue</b> widgets.</p></body></html>\n", { charset: "utf-8" }, { cte: "quoted-printable" }),
    },
    expect: { body_html_includes: ["<h1>Order 4711</h1>", "<b>blue</b>"], body_text_includes: ["Order 4711", "Two blue widgets."], body_text_excludes: ["<b>"] },
  },
  {
    file: "html-only-with-attachment.eml",
    covers: ["§6.8", "§5.4 body choice (HTML only: body_text derived)"],
    description: "multipart/mixed of an HTML part and a PDF, with no text/plain anywhere.",
    tree: {
      headers: envelope([["Subject", "HTML only"]]),
      type: "multipart/mixed",
      boundary: "mixed-html-only",
      children: [html("<div>Please find the <i>invoice</i> attached.</div>\n"), file("application/pdf", "invoice.pdf", PDF)],
    },
    expect: { body_text_includes: ["Please find the invoice attached."], body_html_includes: ["<i>invoice</i>"], attachments: [att("invoice.pdf", "application/pdf", PDF)] },
  },
  {
    file: "text-only.eml",
    covers: ["§6.8", "§5.4 body choice (text only: body_html empty)"],
    description: "A plain text mail with no MIME headers at all.",
    tree: { headers: [["From", "ada@feldspar.test"], ["To", "orders@feldspar.test"], ["Subject", "Plain"], ["Date", DATE]], type: null, body: "Just text.\n" },
    expect: { body_text_includes: ["Just text."], body_html: null, from_addr: "ada@feldspar.test", from_name: null },
  },
  {
    file: "alternative-both.eml",
    covers: ["§5.4 body choice (keep both)"],
    description: "multipart/alternative with a text/plain and a text/html that differ.",
    tree: {
      headers: envelope([["Subject", "Both"]]),
      type: "multipart/alternative",
      boundary: "alt-both",
      children: [text("The plain version.\n"), html("<p>The <em>HTML</em> version.</p>\n")],
    },
    expect: { body_text_includes: ["The plain version."], body_html_includes: ["The <em>HTML</em> version."] },
  },
  {
    file: "apple-mail-nested.eml",
    covers: ["§5.4 body choice", "§5.4 inline images"],
    description: "Apple Mail's shape: mixed[alternative[text, related[html, cid image]], PDF].",
    tree: {
      headers: envelope([["Subject", "Nested"]]),
      type: "multipart/mixed",
      boundary: "apple-mixed",
      children: [
        {
          type: "multipart/alternative",
          boundary: "apple-alt",
          children: [
            text("See the picture.\n"),
            {
              type: "multipart/related",
              params: { type: '"text/html"' },
              boundary: "apple-rel",
              children: [
                html('<p>See the picture.</p><img src="cid:pic-1@feldspar.test">\n'),
                { type: "image/png", params: { name: "pic.png" }, headers: [["Content-ID", "<pic-1@feldspar.test>"]], disposition: 'inline; filename="pic.png"', cte: "base64", body: PNG },
              ],
            },
          ],
        },
        file("application/pdf", "terms.pdf", PDF),
      ],
    },
    expect: {
      body_text_includes: ["See the picture."],
      body_html_includes: ['src="cid:pic-1@feldspar.test"'],
      attachments: [att("pic.png", "image/png", PNG, { inline: true, cid: "pic-1@feldspar.test" }), att("terms.pdf", "application/pdf", PDF)],
    },
  },
  {
    file: "no-from-no-date.eml",
    covers: ["§6.11", "§5.4 every header is optional"],
    description: "No From and no Date: sent_at falls back to INTERNALDATE.",
    tree: { headers: [["To", "orders@feldspar.test"], ["Subject", "Anonymous"], ["MIME-Version", "1.0"]], ...text("Who sent this?\n") },
    expect: { from_addr: null, from_name: null, sent_at: null, body_text_includes: ["Who sent this?"] },
  },
  {
    file: "no-to-no-cc.eml",
    covers: ["§6.11"],
    description: "No To and no Cc (a Bcc'd copy).",
    tree: { headers: [["From", "ada@feldspar.test"], ["Subject", "Bcc only"], ["Date", DATE], ["MIME-Version", "1.0"]], ...text("You were Bcc'd.\n") },
    expect: { to_addrs: [], cc_addrs: [], sent_at: DATE_ISO },
  },
  {
    file: "invalid-date.eml",
    covers: ["§5.4 every header is optional (an invalid date)"],
    description: "A Date header that is not a date.",
    tree: { headers: [["From", "ada@feldspar.test"], ["To", "orders@feldspar.test"], ["Subject", "When?"], ["Date", "yesterday afternoon"], ["MIME-Version", "1.0"]], ...text("Some time.\n") },
    expect: { sent_at: null },
  },
  {
    file: "no-subject.eml",
    covers: ["§5.4 every header is optional (subject falls back to empty)"],
    description: "No Subject header.",
    tree: { headers: envelope(), ...text("No subject.\n") },
    expect: { subject: "" },
  },
  {
    file: "no-headers.eml",
    covers: ["§6.11", "§5.4 every header is optional"],
    description: "No headers at all: the mail starts with the blank line.",
    tree: { headers: [], type: null, body: "Only a body.\n" },
    expect: { subject: "", from_addr: null, sent_at: null, to_addrs: [], body_text_includes: ["Only a body."] },
  },
  {
    file: "rfc822-inline-and-attached.eml",
    covers: ["§6.12", "§5.4 message/rfc822"],
    description: "An inline message/rfc822 (recursed into) next to an attached one with a filename and one without.",
    tree: {
      headers: envelope([["Subject", "Two forwards"]]),
      type: "multipart/mixed",
      boundary: "rfc822-mix",
      children: [
        text("Outer text.\n"),
        { type: "message/rfc822", disposition: "inline", message: innerMail("Inline inner", "Inline inner body.\n") },
        { type: "message/rfc822", disposition: 'attachment; filename="forwarded.eml"', message: innerMail("Attached inner", "Attached inner body.\n") },
        { type: "message/rfc822", disposition: "attachment", message: innerMail("Nameless inner", "Nameless inner body.\n") },
      ],
    },
    expect: {
      subject: "Two forwards",
      body_text_includes: ["Outer text.", "Inline inner body."],
      body_text_excludes: ["Attached inner body.", "Nameless inner body."],
      attachment_names: ["forwarded.eml", "part-4.eml"],
    },
  },
  {
    file: "rfc822-forward-only.eml",
    covers: ["§6.12", "§5.4 message/rfc822 (an attached body never replaces the outer one)"],
    description: "Forward-as-attachment where the outer mail has no text of its own.",
    tree: {
      headers: envelope([["Subject", "Fwd: Attached inner"]]),
      type: "multipart/mixed",
      boundary: "fwd-only",
      children: [{ type: "message/rfc822", disposition: 'attachment; filename="original.eml"', message: innerMail("Attached inner", "The forwarded body.\n") }],
    },
    expect: { subject: "Fwd: Attached inner", body_text_excludes: ["The forwarded body."], placeholder: "attachments_only", attachment_names: ["original.eml"] },
  },
  {
    file: "journal-report.eml",
    covers: ["§6.12", "§5.4 message/rfc822 (inline, e.g. Exchange journal reports)"],
    description: "An Exchange journal report: a text envelope and the journaled mail inline, with no disposition.",
    tree: {
      headers: envelope([["Subject", "Journal report"], ["X-MS-Journal-Report", ""]]),
      type: "multipart/mixed",
      boundary: "journal",
      children: [text("Sender: bo@feldspar.test\nRecipient: ada@feldspar.test\n"), { type: "message/rfc822", message: innerMail("Journaled", "The journaled body.\n") }],
    },
    expect: { body_text_includes: ["Sender: bo@feldspar.test", "The journaled body."] },
  },
  {
    file: "cid-images.eml",
    covers: ["§6.10", "§5.4 inline images"],
    description: "cid: references in single and double quotes, one image referenced twice, and a Content-ID image the HTML never mentions.",
    tree: {
      headers: envelope([["Subject", "Logos"]]),
      type: "multipart/related",
      boundary: "cid-rel",
      children: [
        html("<p><img src='cid:logo@feldspar.test'> and again <img src=\"cid:logo@feldspar.test\"> and <img src=\"cid:chart@feldspar.test\"></p>\n"),
        { type: "image/png", params: { name: "logo.png" }, headers: [["Content-ID", "<logo@feldspar.test>"]], cte: "base64", body: PNG },
        { type: "image/png", headers: [["Content-ID", "<chart@feldspar.test>"]], disposition: "inline", cte: "base64", body: PNG2 },
        { type: "image/png", params: { name: "unused.png" }, headers: [["Content-ID", "<unused@feldspar.test>"]], cte: "base64", body: PNG },
      ],
    },
    expect: {
      body_html_includes: ["src='cid:logo@feldspar.test'", 'src="cid:logo@feldspar.test"', 'src="cid:chart@feldspar.test"'],
      body_html_excludes: ["data:image"],
      attachments: [
        att("logo.png", "image/png", PNG, { inline: true, cid: "logo@feldspar.test" }),
        att("part-3.png", "image/png", PNG2, { inline: true, cid: "chart@feldspar.test" }),
        att("unused.png", "image/png", PNG, { inline: true, cid: "unused@feldspar.test" }),
      ],
    },
  },
  {
    file: "inline-disposition-without-cid.eml",
    covers: ["§6.10", "§5.4 attachments (a filename and no Content-ID)"],
    description: "Apple Mail's inline PDF and image: disposition inline with a filename, no Content-ID — so attachments, not inline images.",
    tree: {
      headers: envelope([["Subject", "Inline PDF"]]),
      type: "multipart/mixed",
      boundary: "inline-nocid",
      children: [
        text("Here it is.\n"),
        { ...file("application/pdf", "Rechnung.pdf", PDF), disposition: 'inline; filename="Rechnung.pdf"' },
        { ...file("image/png", "photo.png", PNG), disposition: 'inline; filename="photo.png"' },
      ],
    },
    expect: { attachments: [att("Rechnung.pdf", "application/pdf", PDF), att("photo.png", "image/png", PNG)] },
  },
  {
    file: "nul-byte.eml",
    covers: ["§5.4 remove NUL bytes"],
    description: "A NUL byte in the body (8bit) and in the subject (an encoded word).",
    tree: { headers: envelope([["Subject", "=?UTF-8?Q?before=00after?="]]), ...text(B("text before\x00text after\n", "latin1")) },
    expect: { subject: "beforeafter", body_text_includes: ["text before", "text after"] },
  },
  {
    file: "nul-byte-html-base64.eml",
    covers: ["§5.4 remove NUL bytes"],
    description: "NUL bytes inside a base64 HTML part.",
    tree: { headers: envelope([["Subject", "NUL in HTML"]]), ...html(B("<p>a\x00b\x00c</p>\n", "latin1"), { charset: "utf-8" }, { cte: "base64" }) },
    expect: { body_html_includes: ["<p>abc</p>"] },
  },
  {
    file: "tnef-rtf-only.eml",
    covers: ["§6.13", "§5.4 TNEF (body)"],
    description: "Outlook to Exchange: no text part, a winmail.dat holding only a compressed RTF body.",
    tree: {
      headers: envelope([["Subject", "Outlook RTF"]]),
      type: "multipart/mixed",
      boundary: "tnef-only",
      children: [{ type: "application/ms-tnef", params: { name: '"winmail.dat"' }, disposition: 'attachment; filename="winmail.dat"', cte: "base64", body: tnef({ rtf: RTF_PLAIN }) }],
    },
    expect: { body_text_includes: ["Hello from Outlook.", "Grüße aus Köln."], attachments: [] },
    tnef: { rtf: RTF_PLAIN, attachments: [] },
  },
  {
    file: "tnef-with-attachment.eml",
    covers: ["§6.13", "§5.4 TNEF (body and attachments)"],
    description: "A text part, and a winmail.dat with an HTML-in-RTF body and one attached file.",
    tree: {
      headers: envelope([["Subject", "Outlook with a file"]]),
      type: "multipart/mixed",
      boundary: "tnef-att",
      children: [
        text("See the report.\n"),
        {
          type: "application/ms-tnef",
          params: { name: '"winmail.dat"' },
          disposition: 'attachment; filename="winmail.dat"',
          cte: "base64",
          body: tnef({ rtf: RTF_FROMHTML, attachments: [{ shortName: "REPORT~1.TXT", name: "quarterly report.txt", type: "text/plain", data: REPORT }] }),
        },
      ],
    },
    expect: { body_text_includes: ["See the report."], attachments: [att("quarterly report.txt", "text/plain", REPORT)] },
    tnef: { rtf: RTF_FROMHTML, attachments: ["quarterly report.txt"] },
  },
  {
    file: "attachment-missing-params.eml",
    covers: ["§6.5", "§5.4 attachments (no name and no type: skipped; no name: part-<path>.<ext>)"],
    description: "A part with a parameterless disposition and no Content-Type, and one with a type but no name.",
    tree: {
      headers: envelope([["Subject", "Nameless parts"]]),
      type: "multipart/mixed",
      boundary: "nameless",
      children: [
        text("See the parts.\n"),
        { type: null, disposition: "attachment", cte: "base64", body: B("no name, no type\n") },
        { type: "application/octet-stream", disposition: "attachment", cte: "base64", body: ALL_BYTES.subarray(0, 16) },
      ],
    },
    expect: { attachments: [att("part-3.bin", "application/octet-stream", ALL_BYTES.subarray(0, 16))] },
  },
  {
    file: "attachment-filename-encodings.eml",
    covers: ["§5.4 attachments (the name is filename, then name)"],
    description: "RFC 2231 filename*, a name only in Content-Type as an encoded word, and an RFC 2231 continuation.",
    tree: {
      headers: envelope([["Subject", "File names"]]),
      type: "multipart/mixed",
      boundary: "names",
      children: [
        text("Three files.\n"),
        { type: "application/pdf", disposition: "attachment; filename*=UTF-8''%E2%82%AC-Rechnung%20M%C3%A4rz.pdf", cte: "base64", body: PDF },
        { type: "image/png", params: { name: `"${encodedWord("Bild Größe.png")}"` }, cte: "base64", body: PNG },
        { type: "text/plain", disposition: 'attachment; filename*0="very-long-file"; filename*1="name.txt"', cte: "base64", body: REPORT },
      ],
    },
    expect: {
      attachments: [att("€-Rechnung März.pdf", "application/pdf", PDF), att("Bild Größe.png", "image/png", PNG), att("very-long-filename.txt", "text/plain", REPORT)],
    },
  },
  {
    file: "binary-attachment.eml",
    covers: ["§6.4", "§5.4 attachments (binary-safe)"],
    description: "An attachment holding every byte value four times, which a string round trip would corrupt.",
    tree: {
      headers: envelope([["Subject", "Bytes"]]),
      type: "multipart/mixed",
      boundary: "bytes",
      children: [text("All 256 bytes.\n"), file("application/octet-stream", "all-bytes.bin", ALL_BYTES)],
    },
    expect: { attachments: [att("all-bytes.bin", "application/octet-stream", ALL_BYTES)] },
  },
  {
    file: "encrypted-pgp.eml",
    covers: ["§5.4 no body part (encrypted)"],
    description: "PGP/MIME: multipart/encrypted.",
    tree: {
      headers: envelope([["Subject", "Secret"]]),
      type: "multipart/encrypted",
      params: { protocol: '"application/pgp-encrypted"' },
      boundary: "pgp",
      children: [
        { type: "application/pgp-encrypted", cte: "7bit", body: "Version: 1\n" },
        { type: "application/octet-stream", params: { name: '"encrypted.asc"' }, cte: "7bit", body: "-----BEGIN PGP MESSAGE-----\n\nhQEMA0xxxxxxxxxxAQf/not-really-ciphertext\n=abcd\n-----END PGP MESSAGE-----\n" },
      ],
    },
    expect: { placeholder: "encrypted" },
  },
  {
    file: "encrypted-smime.eml",
    covers: ["§5.4 no body part (encrypted: pkcs7)"],
    description: "S/MIME: a single application/pkcs7-mime part.",
    tree: {
      headers: envelope([["Subject", "Secret S/MIME"]]),
      type: "application/pkcs7-mime",
      params: { "smime-type": "enveloped-data", name: '"smime.p7m"' },
      disposition: 'attachment; filename="smime.p7m"',
      cte: "base64",
      body: ALL_BYTES.subarray(0, 300),
    },
    expect: { placeholder: "encrypted" },
  },
  {
    file: "calendar-only.eml",
    covers: ["§5.4 no body part (text/calendar only)"],
    description: "An invitation that is nothing but text/calendar.",
    tree: {
      headers: envelope([["Subject", "Invitation: Review"]]),
      type: "text/calendar",
      params: { charset: "utf-8", method: "REQUEST" },
      cte: "8bit",
      body: "BEGIN:VCALENDAR\nVERSION:2.0\nMETHOD:REQUEST\nBEGIN:VEVENT\nUID:review-1@feldspar.test\nDTSTART:20251020T090000Z\nSUMMARY:Review\nEND:VEVENT\nEND:VCALENDAR\n",
    },
    expect: { placeholder: "calendar" },
  },
  {
    file: "attachments-only.eml",
    covers: ["§5.4 no body part (attachments only)"],
    description: "multipart/mixed holding one PDF and nothing else.",
    tree: { headers: envelope([["Subject", "Scan"]]), type: "multipart/mixed", boundary: "only-att", children: [file("application/pdf", "scan.pdf", PDF)] },
    expect: { placeholder: "attachments_only", attachments: [att("scan.pdf", "application/pdf", PDF)] },
  },
  {
    file: "missing-closing-boundary.eml",
    covers: ["§5.4 every header is optional (malformed structure)"],
    description: "A multipart whose closing boundary never comes.",
    tree: { headers: envelope([["Subject", "Truncated"]]), type: "multipart/mixed", boundary: "open-ended", close: false, preamble: "This is a multi-part message in MIME format.", children: [text("First part.\n"), file("application/pdf", "doc.pdf", PDF)] },
    expect: { body_text_includes: ["First part."], attachment_names: ["doc.pdf"] },
  },
  {
    file: "bare-lf.eml",
    covers: ["§6.9"],
    description: "Unix line endings (LF only) throughout, as a mail saved from mbox has.",
    lf: true,
    tree: { headers: envelope([["Subject", "Bare LF"]]), type: "multipart/alternative", boundary: "lf-alt", children: [text("Line one.\nLine two.\n"), html("<p>Line one.<br>Line two.</p>\n")] },
    expect: { subject: "Bare LF", body_text_includes: ["Line one.", "Line two."], body_html_includes: ["<br>"] },
  },
  {
    file: "reply-threading.eml",
    covers: ["M9.4 thread_key"],
    description: "A reply with a folded References header: thread_key is its first entry.",
    tree: {
      headers: envelope([
        ["Subject", "Re: Re: Order 4711"],
        ["Message-ID", "<reply-2@feldspar.test>"],
        ["In-Reply-To", "<reply-1@feldspar.test>"],
        ["References", "<root-0@feldspar.test>\r\n <reply-1@feldspar.test>"],
      ]),
      ...text("Agreed.\n"),
    },
    expect: { message_id: "<reply-2@feldspar.test>", in_reply_to: "<reply-1@feldspar.test>", references: ["<root-0@feldspar.test>", "<reply-1@feldspar.test>"], thread_key: "<root-0@feldspar.test>" },
  },
  {
    file: "no-message-id.eml",
    covers: ["M9.4 thread_key"],
    description: "No Message-ID, In-Reply-To or References.",
    tree: { headers: envelope([["Subject", "No id"]]), ...text("No id.\n") },
    expect: { message_id: null, in_reply_to: null, references: [] },
  },
  {
    file: "long-body.eml",
    covers: ["§5.4 size limits"],
    description: "A text body of 410,000 characters, past the 400,000 limit.",
    tree: { headers: envelope([["Subject", "Long"]]), ...text(LONG_LINE.repeat(Math.ceil(410000 / LONG_LINE.length)), { charset: "us-ascii" }, { cte: "7bit" }) },
    expect: { body_text_max_length: 400000, body_text_includes: ["Lorem ipsum"] },
  },
];

// ---------------------------------------------------------------------------

export function generate(outDir) {
  mkdirSync(outDir, { recursive: true });
  for (const name of readdirSync(outDir)) rmSync(join(outDir, name));
  const expected = [];
  for (const mail of MAILS) {
    let bytes = render(mail.tree);
    if (mail.lf) bytes = B(bytes.toString("latin1").replace(/\r\n/g, "\n"), "latin1");
    writeFileSync(join(outDir, mail.file), bytes);
    expected.push({ file: mail.file, covers: mail.covers, description: mail.description, structure: structure(mail.tree), expect: mail.expect, ...(mail.tnef ? { tnef: mail.tnef } : {}) });
  }
  writeFileSync(join(outDir, "expected.json"), JSON.stringify(expected, null, 2) + "\n");
  return expected;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const out = process.argv[2] || join(dirname(fileURLToPath(import.meta.url)), "generated");
  const expected = generate(out);
  console.log(`${expected.length} mails written to ${out}`);
}
