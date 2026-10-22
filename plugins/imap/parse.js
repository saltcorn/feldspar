// **One mail's source, parsed into a `mail_messages` row and its `mail_attachments` rows**
// (TODO-imap.md §5.4, M7.2). A pure function: no IMAP, no host, no database.
//
//   const { message, attachments } = await parse(source, { internalDate, flags, size });
//
// It never throws. A mail it cannot make sense of still yields a row, with what could be read
// and `import_error` set, so the sync can store it and move its cursor on (§5.2).
//
// Why not mailparser's `simpleParser`
// -----------------------------------
// mailparser was the plan (§2.1), and this file is built from its parts: the MIME splitter
// (`@zone-eu/mailsplit`), libmime, iconv-lite and html-to-text are what mailparser itself runs
// on. Its `simpleParser` was tried on the generated mails (M1.2) and got six of them wrong in
// ways that are not options: a latin-1 subject decoded as UTF-8, an unknown charset likewise,
// an invalid `Date` replaced with the current time, an inline `message/rfc822` kept as an
// attachment, a `winmail.dat` left packed, and no IMAP part numbers at all, which
// `mail_attachments.part` and the over-the-cap path (`parse-large.js`) need. So the splitter
// gives the tree, and the decisions below are this module's.
//
// The shape of the work
// ---------------------
// The splitter turns the source into MIME nodes, and `treeOf` into a tree of containers and
// `Leaf`s (`leafFromNode`), a description that does not care where it came from, so
// `parse-large.js` can build the same tree from an IMAP `BODYSTRUCTURE`. `assemble` then picks
// the body parts (`selectBodies`), decides for every other leaf what it is (`classify`), and
// writes the row. Everything that is a rule of §5.4 is in `classify`, `decodeText`,
// `selectBodies` and `assemble`.

"use strict";

const { Splitter } = require("@zone-eu/mailsplit");
const libmime = require("libmime");
const libcharset = require("libmime/lib/charset");
const iconv = require("iconv-lite");
const { convert: htmlToText } = require("html-to-text");
const addressparser = require("nodemailer/lib/addressparser");
const tnef = require("./tnef");

/** At or below this, the whole source is fetched and parsed here; above it, `parse-large.js`. */
const SIZE_CAP = 25 * 1024 * 1024;

/** Lengths a row's values are cut to. Bodies and the snippet are §5.4's; the header lengths
 * are feldspar-mailbridge's `_parse`, with address lists counted in addresses. */
const LIMITS = {
  body: 400_000,
  snippet: 200,
  subject: 1000,
  message_id: 500,
  name: 300,
  address: 300,
  to: 100,
  cc: 100,
  reply_to: 5,
  references: 50,
  filename: 200,
  content_type: 200,
  cid: 300,
  import_error: 1000,
};

/** What `body_text` says when a mail has no body part (§5.4). */
const PLACEHOLDERS = {
  encrypted: "(This message is encrypted. It cannot be shown here; open it in a mail program that has your key.)",
  calendar: "(Calendar invitation: see the attached .ics file.)",
  attachments_only: "(This message has no text, only attachments.)",
  empty: "(This message has no text.)",
};

/** Leaves whose text is a body when they are not marked as attachments. Delivery and read
 * reports are text a reader wants to see, as mailparser shows them too. */
const BODY_TYPES = new Set(["text/plain", "text/html", "text/enriched", "text/richtext", "message/delivery-status", "message/disposition-notification"]);

const TNEF_TYPES = new Set(["application/ms-tnef", "application/vnd.ms-tnef"]);
const ENCRYPTED_TYPES = new Set(["application/pkcs7-mime", "application/x-pkcs7-mime"]);

// ------------------------------------------------------------------------------ decoding

/** A transfer-encoded body as bytes. Lenient: junk in base64 is skipped, a bad `=XX` is kept. */
function decodeTransfer(raw, encoding) {
  switch ((encoding || "").toLowerCase().trim()) {
    case "base64": {
      // Each `=`-padded segment on its own, because some mailers concatenate encoded chunks;
      // and the rest of a line after its padding is junk, not the next chunk.
      const segments = [];
      let current = "";
      for (const line of raw.toString("latin1").split(/\r?\n/)) {
        const pad = line.indexOf("=");
        current += (pad < 0 ? line : line.slice(0, pad)).replace(/[^A-Za-z0-9+/]/g, "");
        if (pad >= 0) {
          segments.push(current);
          current = "";
        }
      }
      segments.push(current);
      return Buffer.concat(segments.filter(Boolean).map((s) => Buffer.from(s.length % 4 === 1 ? s.slice(0, -1) : s, "base64")));
    }
    case "quoted-printable": {
      const text = raw.toString("latin1").replace(/=[ \t]*\r?\n/g, "");
      const out = Buffer.alloc(text.length);
      let o = 0;
      for (let i = 0; i < text.length; i++) {
        const c = text.charCodeAt(i);
        if (c === 0x3d && /^[0-9A-Fa-f]{2}$/.test(text.slice(i + 1, i + 3))) {
          out[o++] = parseInt(text.slice(i + 1, i + 3), 16);
          i += 2;
        } else {
          out[o++] = c;
        }
      }
      return out.subarray(0, o);
    }
    default:
      return raw;
  }
}

const utf8 = new TextDecoder("utf-8", { fatal: true });

/** Bytes as UTF-8 if they are valid UTF-8, else `null`. */
function strictUtf8(bytes) {
  try {
    return utf8.decode(bytes);
  } catch {
    return null;
  }
}

/** The charset name iconv-lite or libmime can decode, or `null`. */
function knownCharset(declared) {
  if (!declared) return null;
  const name = String(declared).trim().replace(/^["']|["']$/g, "");
  if (!name) return null;
  if (/^(us-?)?ascii$|^utf-?8$|^unicode-1-1-utf-8$/i.test(name)) return "utf-8";
  if (/^(iso-?2022-?jp|jis|euc-?jp|shift[-_]?jis|sjis)/i.test(name)) return name; // libmime, via encoding-japanese
  const normal = libcharset.normalizeCharset(name);
  if (iconv.encodingExists(normal)) return normal;
  if (iconv.encodingExists(name)) return name;
  return null;
}

/**
 * Bytes as text, by the declared charset (§5.4 "Charsets").
 *
 * An unknown or missing charset is read as UTF-8 if the bytes are valid UTF-8, else as
 * latin-1, and so is a declared UTF-8 (or US-ASCII) that the bytes are not: that is the
 * commonest lie in real mail. Latin-1 is read as windows-1252, its superset, as every browser
 * and mail client reads it: the bytes 0x80–0x9F are `€`, `„`, `“` and the like there, and
 * invisible control characters in ISO 8859-1. Never throws.
 */
function decodeText(bytes, declared) {
  const charset = knownCharset(declared);
  if (charset && charset !== "utf-8") {
    try {
      if (/^(iso-?2022-?jp|jis|euc-?jp|shift[-_]?jis|sjis)/i.test(charset)) return libcharset.decode(bytes, charset);
      return iconv.decode(bytes, charset);
    } catch {
      // fall through to the guess
    }
  }
  const text = strictUtf8(bytes);
  return text !== null ? text : iconv.decode(bytes, "windows-1252");
}

/** An HTML part's own `<meta charset>`, for a part whose headers declare none. */
function sniffHtmlCharset(bytes) {
  const head = bytes.subarray(0, 4096).toString("latin1");
  const m = /<meta[^>]+charset\s*=\s*["']?\s*([A-Za-z0-9._:-]+)/i.exec(head);
  return m ? m[1] : null;
}

const htmlToTextOptions = {
  wordwrap: false,
  selectors: [
    { selector: "a", options: { ignoreHref: true } },
    { selector: "img", format: "skip" },
    ...["h1", "h2", "h3", "h4", "h5", "h6"].map((h) => ({ selector: h, options: { uppercase: false } })),
    { selector: "table", format: "dataTable", options: { uppercaseHeaderCells: false } },
  ],
  limits: { maxInputLength: 4_000_000 },
};

/** The text a reader sees in an HTML body. */
function textOfHtml(html) {
  try {
    return htmlToText(html, htmlToTextOptions);
  } catch {
    return html.replace(/<[^>]*>/g, " ");
  }
}

/** text/enriched (RFC 1896) as plain text: the formatting commands dropped. */
function textOfEnriched(text) {
  return text
    .replace(/<param>[\s\S]*?<\/param>/gi, "")
    .replace(/<\/?[A-Za-z0-9-]+>/g, "")
    .replace(/<</g, "<");
}

/** The value with NUL characters removed, because Postgres refuses them in text (§5.4), and
 * lone UTF-16 surrogates replaced, because they cannot be written as UTF-8 at all. */
const noNul = (s) => (typeof s === "string" ? s.replace(/\0/g, "").toWellFormed() : s);

/** At most `n` characters, never splitting a surrogate pair. */
function cut(s, n) {
  if (typeof s !== "string" || s.length <= n) return s;
  const code = s.charCodeAt(n - 1);
  return s.slice(0, code >= 0xd800 && code <= 0xdbff ? n - 1 : n);
}

// ------------------------------------------------------------------------------ headers

/**
 * A header's value, unfolded, with encoded words left for the caller.
 *
 * Raw 8-bit bytes are read as UTF-8 if they are valid UTF-8, else in `charset`, the charset
 * the mail declares for its text, else as latin-1 (§5.4 "Undeclared 8-bit headers"). A mail
 * that writes its subject in raw ISO-8859-9 or EUC-KR almost always declares that charset for
 * its body too, which is how mail clients read such headers. A raw 8-bit byte inside a
 * Q-encoded word (`=?iso-8859-1?Q?Fr\xf6sche?=`, as Outlook Express wrote it) is written as
 * `=XX` first, so the word's own charset decodes it.
 */
function rawHeader(headers, key, charset) {
  if (!headers) return "";
  let line;
  try {
    const found = headers.getList().find((h) => h.key === key);
    line = found && found.line;
  } catch {
    return "";
  }
  if (!line) return "";
  // The splitter keeps header lines as `binary` strings: one character per byte.
  let value = line
    .slice(line.indexOf(":") + 1)
    .replace(/\r?\n[ \t]*/g, " ")
    .trim();
  if (!/[^\x00-\x7f]/.test(value)) return value;
  value = value.replace(/=\?([^?\s]+)\?([Qq])\?([^?]*)\?=/g, (word, cs, q, text) =>
    /[^\x00-\x7f]/.test(text) ? `=?${cs}?${q}?${text.replace(/[^\x00-\x7f]/g, (ch) => `=${ch.charCodeAt(0).toString(16).toUpperCase().padStart(2, "0")}`)}?=` : word,
  );
  const bytes = Buffer.from(value, "latin1");
  const asUtf8 = strictUtf8(bytes);
  if (asUtf8 !== null) return asUtf8;
  return decodeText(bytes, knownCharset(charset) && knownCharset(charset) !== "utf-8" ? charset : "windows-1252");
}

/** Encoded words decoded; on a malformed one, the words as they are. */
function decodeWords(value) {
  try {
    return libmime.decodeWords(value);
  } catch {
    return value;
  }
}

/** An address header as `[{ name, address }]`. Groups are flattened; entries with no address are dropped. */
function addresses(headers, key, limit, charset) {
  const raw = rawHeader(headers, key, charset);
  if (!raw) return [];
  let parsed;
  try {
    parsed = addressparser(raw.replace(/;\s*(?=\S)/g, ", "), { flatten: true });
  } catch {
    return [];
  }
  const out = [];
  for (const a of parsed) {
    let address = decodeWords(String(a.address || "")).trim();
    if (!address || !address.includes("@")) continue;
    let name = decodeWords(String(a.name || "")).replace(/\s+/g, " ").trim();
    if (name === address) name = "";
    out.push({ name: cut(name, LIMITS.name), address: cut(address, LIMITS.address) });
    if (out.length >= limit) break;
  }
  return out;
}

/** The `<…>` ids in a Message-ID, In-Reply-To or References header. A bare id is taken as it is. */
function messageIds(value) {
  const ids = (value.match(/<[^<>\s]+>/g) || []).map((id) => cut(id, LIMITS.message_id));
  if (ids.length) return ids;
  const bare = value.trim().split(/\s+/)[0];
  return bare ? [cut(bare, LIMITS.message_id)] : [];
}

/** A `Date` header as a Date, or `null` when it is missing or not a date. */
function parseDate(value) {
  if (!value) return null;
  const tries = [value, value.replace(/\([^)]*\)/g, " ").replace(/\s+/g, " ").trim(), value.replace(/^[A-Za-z]+,?\s*/, "")];
  for (const t of tries) {
    const d = new Date(t);
    if (!Number.isNaN(d.getTime()) && d.getUTCFullYear() >= 1970 && d.getUTCFullYear() < 10000) return d;
  }
  return null;
}

const iso = (d) => (d instanceof Date && !Number.isNaN(d.getTime()) ? d.toISOString() : null);

function toDate(value) {
  if (value === undefined || value === null || value === "") return null;
  const d = value instanceof Date ? value : new Date(value);
  return Number.isNaN(d.getTime()) ? null : d;
}

/** `flags` as a Set of lower-case flag names, from a Set, an array or nothing. */
function flagSet(flags) {
  return new Set([...(flags || [])].map((f) => String(f).toLowerCase()));
}

/** The header columns of a row, from the root's headers. */
function headerFields(headers, { internalDate, flags, charset }) {
  const from = addresses(headers, "from", 1, charset)[0] || addresses(headers, "sender", 1, charset)[0] || null;
  const messageId = messageIds(rawHeader(headers, "message-id"))[0] || null;
  const inReplyTo = messageIds(rawHeader(headers, "in-reply-to"))[0] || null;
  let references = messageIds(rawHeader(headers, "references"));
  if (references.length > LIMITS.references) references = [references[0], ...references.slice(-(LIMITS.references - 1))];
  const internal = toDate(internalDate);
  const sent = parseDate(rawHeader(headers, "date")) || internal;
  const f = flagSet(flags);
  return {
    message_id: messageId,
    in_reply_to: inReplyTo,
    references,
    thread_key: references[0] || inReplyTo || messageId || null,
    subject: cut(decodeWords(rawHeader(headers, "subject", charset)).replace(/\s+/g, " ").trim(), LIMITS.subject),
    from_name: from && from.name ? from.name : null,
    from_addr: from ? from.address : null,
    to_addrs: addresses(headers, "to", LIMITS.to, charset),
    cc_addrs: addresses(headers, "cc", LIMITS.cc, charset),
    reply_to: addresses(headers, "reply-to", LIMITS.reply_to, charset),
    sent_at: iso(sent),
    internal_date: iso(internal),
    seen: f.has("\\seen"),
    flagged: f.has("\\flagged"),
    answered: f.has("\\answered"),
    draft: f.has("\\draft"),
  };
}

// ------------------------------------------------------------------------------ leaves

/**
 * @typedef {object} Leaf  One MIME leaf, wherever it was read from.
 * @property {string} path         the IMAP part number, e.g. `"2.1"`
 * @property {string} type         the content type, lower case; `text/plain` when undeclared
 * @property {boolean} typed       whether a Content-Type was declared at all
 * @property {string|null} charset
 * @property {string} encoding     the Content-Transfer-Encoding
 * @property {string|null} disposition  `inline`, `attachment` or `null`
 * @property {string} filename     decoded; `""` when it has none
 * @property {string} cid          the Content-ID without angle brackets; `""` when it has none
 * @property {boolean} flowed
 * @property {boolean} delsp
 * @property {boolean} encrypted   inside a multipart/encrypted
 * @property {number} size         the transfer-encoded size, when known
 * @property {Buffer|null} raw     the transfer-encoded body, or `null` when not fetched
 */

/** The IMAP part number of a splitter node. The splitter writes `TEXT` for a message's own
 * body; a single-part message's body is part `1`, at the top and inside a message/rfc822. */
function partPath(partNr) {
  const nums = (partNr || []).filter((n) => typeof n === "number");
  if (partNr && partNr[partNr.length - 1] === "TEXT") nums.push(1);
  return nums.join(".") || "1";
}

/** The Content-ID of a node's headers, without angle brackets. */
function contentId(value) {
  return cut(String(value || "").trim().replace(/^<|>$/g, "").trim(), LIMITS.cid);
}

function leafFromNode(node, raw, encrypted) {
  const headers = node.headers;
  let params = {};
  try {
    params = libmime.parseHeaderValue(rawHeader(headers, "content-type")).params || {};
  } catch {
    params = {};
  }
  let disposition = node.disposition ? String(node.disposition).toLowerCase() : null;
  if (disposition && disposition !== "inline" && disposition !== "attachment") disposition = "attachment";
  return {
    path: partPath(node.partNr),
    type: (node.contentType || "text/plain").toLowerCase(),
    typed: Boolean(rawHeader(headers, "content-type")),
    charset: node.charset || null,
    encoding: node.encoding || "",
    disposition,
    filename: node.filename ? decodeWords(String(node.filename)).trim() : "",
    cid: contentId(rawHeader(headers, "content-id")),
    flowed: Boolean(node.flowed),
    delsp: Boolean(node.delSp) || /^yes$/i.test(params.delsp || ""),
    encrypted,
    size: raw ? raw.length : 0,
    raw,
  };
}

/** `body`, `tnef`, `attachment` or `skip`: what a leaf is (§5.4 "Body choice", "Attachments"). */
function classify(leaf) {
  if (TNEF_TYPES.has(leaf.type) || /^winmail\.dat$/i.test(leaf.filename)) return "tnef";
  if (BODY_TYPES.has(leaf.type) && leaf.disposition !== "attachment" && !leaf.filename && !leaf.encrypted) return "body";
  // A part with no name and no type is nothing anybody could open (saltcorn/imap f422f71).
  if (!leaf.typed && !leaf.filename) return "skip";
  return "attachment";
}

/** The text of a body leaf. */
function leafText(leaf) {
  const bytes = decodeTransfer(leaf.raw, leaf.encoding);
  let charset = leaf.charset;
  if (!charset && leaf.type === "text/html") charset = sniffHtmlCharset(bytes);
  let text = decodeText(bytes, charset);
  if (leaf.flowed && leaf.type === "text/plain") {
    try {
      text = libmime.decodeFlowed(text, leaf.delsp);
    } catch {
      // keep the text as it is
    }
  }
  if (leaf.type === "text/enriched" || leaf.type === "text/richtext") text = textOfEnriched(text);
  return text.replace(/\r\n?/g, "\n");
}

/** A name for a part that has none: `part-<path>.<extension of its type>`. A type libmime does
 * not know, such as `audio/mp3`, gives its subtype when that looks like an extension. */
function nameFor(leaf) {
  let ext = libmime.detectExtension(leaf.type);
  const subtype = leaf.type.split("/")[1] || "";
  if (ext === "bin" && leaf.type !== "application/octet-stream" && /^[a-z0-9]{2,5}$/.test(subtype)) ext = subtype;
  return `part-${leaf.path}.${ext}`;
}

/** A filename that is safe to show and to store under: no NUL, control characters or slashes. */
function cleanFilename(name) {
  return cut(String(name).replace(/[\0-\x1f\x7f/\\]+/g, "_").trim(), LIMITS.filename);
}

// ------------------------------------------------------------------------------ the row

/**
 * @typedef {object} Container  A multipart, or an inline message/rfc822 that was opened.
 * @property {string} multipart  its subtype (`mixed`, `alternative`, `related`, …); `mixed` for a message
 * @property {Array<Container|Leaf>} children
 */

/**
 * Which body leaves make the text view and which the HTML view: RFC 8621's `parseStructure`
 * (§4.1.4), the algorithm JMAP servers use, which is also what Stalwart's expected output is.
 *
 * A multipart/alternative gives each view one branch, so the plain part of an alternative is
 * not shown twice, and a text/enriched alternative is dropped when a plain or HTML one exists.
 * In a multipart/related, only the first part can be a body. A text part that is a body by
 * `classify` but none of these is returned in `others`, with the leaves that are not bodies.
 */
function selectBodies(container, text, html, others, inAlternative = false) {
  const type = container.multipart;
  const textLength = text ? text.length : -1;
  const htmlLength = html ? html.length : -1;
  container.children.forEach((part, i) => {
    if (part.children) {
      selectBodies(part, text, html, others, inAlternative || part.multipart === "alternative");
      return;
    }
    if (classify(part) !== "body" || (type === "related" && i > 0)) {
      others.push(part);
      return;
    }
    if (type === "alternative") {
      if (part.type === "text/plain") text && text.push(part);
      else if (part.type === "text/html") html && html.push(part);
      // else: another rendering of the same content (text/enriched), which neither view needs
      return;
    }
    if (inAlternative) {
      if (part.type === "text/plain") html = null;
      if (part.type === "text/html") text = null;
    }
    if (text) text.push(part);
    if (html) html.push(part);
  });
  if (type === "alternative" && text && html) {
    // A branch that had only HTML gives the text view its HTML, and the other way round.
    if (textLength === text.length && htmlLength !== html.length) text.push(...html.slice(htmlLength));
    if (htmlLength === html.length && textLength !== text.length) html.push(...text.slice(textLength));
  }
}

/** The first charset the mail declares for a text part, which its raw 8-bit headers are
 * most likely in too. */
function textCharset(tree) {
  if (!tree.children) return tree.type.startsWith("text/") ? tree.charset : null;
  for (const child of tree.children) {
    const found = textCharset(child);
    if (found) return found;
  }
  return null;
}

const escapeHtml = (s) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

/**
 * The row and attachment rows, from the root headers and the MIME tree.
 *
 * `raw` is `null` on a leaf that was not fetched (the over-the-cap path): a body leaf then
 * contributes nothing, and an attachment is listed with `content: null`.
 *
 * @param {object} headers  the root's headers (a mailsplit `Headers`)
 * @param {Container|Leaf} tree
 */
function assemble(headers, tree, { internalDate, flags, size, bodyState = "full" }) {
  const message = headerFields(headers, { internalDate, flags, charset: textCharset(tree) });
  const textParts = [];
  const htmlParts = [];
  const others = [];
  selectBodies({ multipart: "mixed", children: [tree] }, textParts, htmlParts, others);

  const texts = new Map();
  const textOf = (leaf) => {
    if (!texts.has(leaf)) texts.set(leaf, leaf.raw ? leafText(leaf) : "");
    return texts.get(leaf);
  };
  let bodyText = textParts.map((leaf) => (leaf.type === "text/html" ? textOfHtml(textOf(leaf)) : textOf(leaf))).join("\n");
  // The HTML view is kept only when there is HTML in it; a text part next to the HTML is shown
  // as preformatted text.
  let bodyHtml = htmlParts.some((leaf) => leaf.type === "text/html" && textOf(leaf).trim())
    ? htmlParts.map((leaf) => (leaf.type === "text/html" ? textOf(leaf) : `<div style="white-space: pre-wrap">${escapeHtml(textOf(leaf))}</div>`)).join("\n")
    : "";
  if (!bodyText.trim()) bodyText = "";

  const attachments = [];
  let tnefBody = null;
  for (const leaf of others) {
    const kind = classify(leaf);
    if (kind === "skip") continue;
    if (kind === "tnef" && leaf.raw) {
      const unpacked = tnef.decode(decodeTransfer(leaf.raw, leaf.encoding));
      if (unpacked) {
        if (!tnefBody && (unpacked.body_text || unpacked.body_html)) tnefBody = unpacked;
        unpacked.attachments.forEach((a, i) => {
          const type = (a.type || libmime.detectMimeType(a.name || "") || "application/octet-stream").toLowerCase();
          attachments.push({
            part: `${leaf.path}:tnef:${i}`,
            filename: cleanFilename(a.name || `part-${leaf.path}-${i + 1}.${libmime.detectExtension(type)}`),
            content_type: cut(type, LIMITS.content_type),
            size: a.data.length,
            cid: contentId(a.cid) || null,
            inline: false,
            content: a.data,
          });
        });
        continue;
      }
    }
    const content = leaf.raw ? decodeTransfer(leaf.raw, leaf.encoding) : null;
    attachments.push({
      part: leaf.path,
      filename: cleanFilename(leaf.filename || nameFor(leaf)),
      content_type: cut(leaf.type, LIMITS.content_type),
      size: content ? content.length : leaf.size || 0,
      cid: leaf.cid || null,
      // A Content-ID makes an inline image (§5.4), unless the part says it is an attachment
      // and the HTML never shows it (checked below).
      inline: Boolean(leaf.cid) && leaf.disposition !== "attachment",
      content,
    });
  }

  if (!bodyText && !bodyHtml && tnefBody) {
    bodyText = tnefBody.body_text;
    bodyHtml = tnefBody.body_html;
    if (!bodyText && bodyHtml) bodyText = textOfHtml(bodyHtml);
  }
  if (bodyHtml) {
    for (const a of attachments) {
      if (a.cid && !a.inline && bodyHtml.includes(`cid:${a.cid}`)) a.inline = true;
    }
  }
  if (!bodyText.trim() && !bodyHtml) {
    const leaves = [];
    const collect = (node) => (node.children ? node.children.forEach(collect) : leaves.push(node));
    collect(tree);
    const types = leaves.map((l) => l.type);
    let placeholder = "empty";
    if (leaves.some((l) => l.encrypted) || types.some((t) => ENCRYPTED_TYPES.has(t))) placeholder = "encrypted";
    else if (types.includes("text/calendar")) placeholder = "calendar";
    else if (attachments.length) placeholder = "attachments_only";
    bodyText = PLACEHOLDERS[placeholder];
  }

  message.body_text = cut(noNul(bodyText), LIMITS.body);
  message.body_html = bodyHtml ? cut(noNul(bodyHtml), LIMITS.body) : null;
  message.snippet = cut(message.body_text.replace(/\s+/g, " ").trim(), LIMITS.snippet);
  message.size = Number.isFinite(size) ? size : null;
  message.has_attachments = attachments.some((a) => !a.inline);
  message.body_state = bodyState;
  message.import_error = null;
  return { message: clean(message), attachments: attachments.map(clean) };
}

/** Every string in a row, and in its lists, without NUL characters. */
function clean(row) {
  const out = {};
  for (const [k, v] of Object.entries(row)) {
    if (typeof v === "string") out[k] = noNul(v);
    else if (Array.isArray(v)) out[k] = v.map((x) => (typeof x === "string" ? noNul(x) : x && typeof x === "object" ? clean(x) : x));
    else out[k] = v;
  }
  return out;
}

// ------------------------------------------------------------------------------ splitting

/**
 * The splitter's nodes, each with its raw body, in document order. Inline `message/rfc822`
 * parts are opened (Exchange journal reports, §5.4); attached ones stay leaves, kept whole.
 */
function split(source) {
  return new Promise((resolve, reject) => {
    const splitter = new Splitter({ defaultInlineEmbedded: true, maxHeadSize: 2 * 1024 * 1024, maxChildNodes: 10000 });
    const nodes = [];
    const bodies = new Map();
    splitter.on("data", (data) => {
      if (data.type === "node") {
        nodes.push(data);
        bodies.set(data, []);
      } else if (data.type === "body" && data.node && bodies.has(data.node)) {
        bodies.get(data.node).push(data.value);
      }
    });
    splitter.on("end", () => resolve(nodes.map((node) => ({ node, raw: Buffer.concat(bodies.get(node)) }))));
    splitter.on("error", reject);
    splitter.end(source);
  });
}

/**
 * The source with transport padding taken off its boundary lines.
 *
 * RFC 2046 allows whitespace after a boundary delimiter (`--b  `), and mailers write it; the
 * splitter does not recognise such a line, and reads the rest of the mail as one part's text.
 * Only lines that are a delimiter of a boundary the mail declares are changed, so no other line
 * (a signature's `-- `, an attachment's bytes) is touched.
 */
function stripBoundaryPadding(source) {
  const text = source.toString("latin1");
  const boundaries = new Set();
  for (const m of text.matchAll(/boundary\s*=\s*(?:"([^"\r\n]+)"|([^\s;"]+))/gi)) boundaries.add(m[1] || m[2]);
  if (!boundaries.size) return source;
  const escaped = [...boundaries].map((b) => b.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|");
  const padded = new RegExp(`^(--(?:${escaped})(?:--)?)[ \\t]+(?=\\r?$)`, "gm");
  if (!padded.test(text)) return source;
  return Buffer.from(text.replace(padded, "$1"), "latin1");
}

/** The splitter's nodes as a tree of containers and leaves. */
function treeOf(nodes) {
  const made = new Map();
  for (const { node, raw } of nodes) {
    const parent = node.parentNode ? made.get(node.parentNode) : null;
    let item;
    if (node.multipart) item = { multipart: String(node.multipart).toLowerCase(), children: [] };
    else if (node.messageNode) item = { multipart: "mixed", message: true, children: [] };
    else item = leafFromNode(node, raw, Boolean(parent && (parent.multipart === "encrypted" || parent.encrypted)));
    if (item.children && parent) item.encrypted = parent.multipart === "encrypted" || Boolean(parent.encrypted);
    made.set(node, item);
    if (parent && parent.children) parent.children.push(item);
  }
  return made.get(nodes[0].node);
}

/**
 * Opens the inline message/rfc822 leaves the splitter could not open because they are
 * transfer-encoded (base64 or quoted-printable, which RFC 2046 forbids and mailers still write).
 * Their parts are numbered `<path>:eml:<part in the message>`: IMAP cannot address them, so
 * only this parse can produce their bytes.
 */
async function openEncodedMessages(container, depth = 0) {
  if (!container.children || depth > 3) return;
  for (let i = 0; i < container.children.length; i++) {
    const part = container.children[i];
    if (part.children) {
      await openEncodedMessages(part, depth);
      continue;
    }
    const encoded = /^(base64|quoted-printable)$/i.test(part.encoding);
    if (part.type !== "message/rfc822" || !encoded || part.disposition === "attachment" || part.filename || !part.raw) continue;
    const nodes = await split(decodeTransfer(part.raw, part.encoding));
    if (!nodes.length) continue;
    const inner = treeOf(nodes);
    const renumber = (node) => {
      if (node.children) node.children.forEach(renumber);
      else node.path = `${part.path}:eml:${node.path}`;
    };
    renumber(inner);
    await openEncodedMessages(inner.children ? inner : { children: [inner] }, depth + 1);
    container.children[i] = { multipart: "mixed", message: true, encrypted: part.encrypted, children: [inner] };
  }
}

/**
 * Parse a mail's whole source (§5.4, at or below the size cap).
 *
 * @param {Buffer} source
 * @param {{ internalDate?: Date|string, flags?: Iterable<string>, size?: number }} [opts]
 * @returns {Promise<{ message: object, attachments: object[] }>} `message` is a
 *   `mail_messages` row without `account`, `folder` and `uid`; each attachment is a
 *   `mail_attachments` row without `message` and `file`, plus `content`, its bytes.
 */
async function parse(source, { internalDate, flags, size } = {}) {
  const buf = Buffer.isBuffer(source) ? source : Buffer.from(source || "");
  const opts = { internalDate, flags, size: size ?? buf.length };
  let nodes;
  try {
    nodes = await split(stripBoundaryPadding(buf));
    if (!nodes.length) throw new Error("no MIME structure");
    const tree = { multipart: "mixed", children: [treeOf(nodes)] };
    await openEncodedMessages(tree);
    return assemble(nodes[0].node.headers, tree.children[0], opts);
  } catch (err) {
    return failed(nodes && nodes.length ? nodes[0].node.headers : null, err, opts);
  }
}

/** The row for a mail that could not be parsed: its headers if they were read, and the error. */
function failed(headers, err, opts) {
  let message;
  try {
    message = headerFields(headers, opts);
  } catch {
    message = headerFields(null, opts);
  }
  Object.assign(message, {
    body_text: "",
    body_html: null,
    snippet: "",
    size: Number.isFinite(opts.size) ? opts.size : null,
    has_attachments: false,
    body_state: opts.bodyState || "full",
    import_error: cut(noNul(`could not parse the message: ${(err && err.message) || err}`), LIMITS.import_error),
  });
  return { message: clean(message), attachments: [] };
}

module.exports = {
  parse,
  assemble,
  selectBodies,
  classify,
  decodeText,
  decodeTransfer,
  rawHeader,
  failed,
  split,
  leafText,
  SIZE_CAP,
  LIMITS,
  PLACEHOLDERS,
};
