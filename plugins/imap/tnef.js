// **Outlook's `winmail.dat`** (TNEF, "Transport Neutral Encapsulation Format"), decoded in JS
// (TODO-imap.md §5.4, M7.4).
//
// Outlook sends one of these instead of, or next to, real MIME parts when a mail is in "Rich
// Text" format, which is what Outlook-to-Exchange mail usually is: the body (compressed RTF)
// and the files are packed inside one opaque attachment. `decode` unpacks it into
// `{ body_text, body_rtf, body_html, attachments: [{ name, type, cid, data }] }`. It never
// throws on damage: whatever was read before the damage is kept.
//
// Ported from feldspar-mailbridge's `tnef.py`, with three additions found against the
// verdammelt/tnef samples: an attachment whose bytes are a MAPI `PR_ATTACH_DATA_OBJ` (16 bytes
// of IID, then the file), multi-valued and unicode MAPI properties read as `tnef(1)` reads
// them, and HTML-in-RTF (`\fromhtml1`) de-encapsulated into `body_html`. npm's `node-tnef`
// was evaluated first and rejected: it misses four of the fifteen samples' names, has no
// compressed-RTF body, and brings `bunyan` and `yargs` (see CHANGELOG, M7.4).
//
// Layout (MS-OXTNEF): the signature 0x223E9F78, a 16-bit key, then attributes of
//   level (1 byte) | id (4: low 16 bits the attribute, high 16 bits its type) | length (4) |
//   data | checksum (2)
// Level 1 is the message, level 2 an attachment. `attAttachRenddata` starts an attachment; its
// bytes are `attAttachData`, its name `attAttachTitle` (often an 8.3 short name) or, better,
// `PR_ATTACH_LONG_FILENAME` inside the MAPI properties of `attAttachment`.

"use strict";

const iconv = require("iconv-lite");

const SIGNATURE = 0x223e9f78;
const ATT_ATTACH_RENDDATA = 0x00069002;
const ATT_ATTACH_DATA = 0x0006800f;
const ATT_ATTACH_TITLE = 0x00018010;
const ATT_ATTACHMENT = 0x00069005;
const ATT_MAPI_PROPS = 0x00069003;
const ATT_BODY = 0x0002800c;
const ATT_OEM_CODEPAGE = 0x00069007;

const PR_BODY = 0x1000;
const PR_RTF_COMPRESSED = 0x1009;
const PR_BODY_HTML = 0x1013;
const PR_ATTACH_DATA_OBJ = 0x3701;
const PR_ATTACH_FILENAME = 0x3704;
const PR_ATTACH_LONG_FILENAME = 0x3707;
const PR_ATTACH_MIME_TAG = 0x370e;
const PR_ATTACH_CONTENT_ID = 0x3712;
const PR_DISPLAY_NAME = 0x3001;

const PT_STRING8 = 0x001e;
const PT_UNICODE = 0x001f;
const PT_BINARY = 0x0102;
const PT_OBJECT = 0x000d;

const MAX_ATTACHMENTS = 100;

/** Sizes of the fixed-width MAPI property types. */
const FIXED = { 0x0002: 2, 0x0003: 4, 0x0004: 4, 0x0005: 8, 0x0006: 8, 0x0007: 8, 0x000a: 4, 0x000b: 4, 0x0014: 8, 0x0040: 8, 0x0048: 16 };

const pad = (n) => (n + 3) & ~3;

/** Whether `data` starts like a TNEF stream. */
function isTnef(data) {
  return Buffer.isBuffer(data) && data.length >= 6 && data.readUInt32LE(0) === SIGNATURE;
}

/** Bytes in a single-byte or ANSI code page as text; an unknown code page is read as windows-1252. */
function ansi(bytes, codepage) {
  const name = codepage && iconv.encodingExists(`cp${codepage}`) ? `cp${codepage}` : "windows-1252";
  return iconv.decode(bytes, name);
}

const stripNul = (s) => s.replace(/\0+$/, "");

/**
 * The properties of a MAPI property block: `Map<id, {type, values: [Buffer|number]}>`.
 * A named property (id ≥ 0x8000) is skipped over. A type this reader does not know ends the
 * block, because nothing after it can be located; what was read until then is kept.
 */
function mapiProps(data) {
  const out = new Map();
  try {
    const count = data.readUInt32LE(0);
    let pos = 4;
    for (let n = 0; n < Math.min(count, 5000); n++) {
      const type = data.readUInt16LE(pos);
      const id = data.readUInt16LE(pos + 2);
      pos += 4;
      if (id >= 0x8000) {
        pos += 16; // the GUID
        const kind = data.readUInt32LE(pos);
        pos += 4;
        if (kind === 0) pos += 4;
        else pos += 4 + pad(data.readUInt32LE(pos));
      }
      const multi = (type & 0x1000) !== 0;
      const base = type & 0x0fff;
      const values = [];
      if (base in FIXED) {
        let n = 1;
        if (multi) {
          n = data.readUInt32LE(pos);
          pos += 4;
        }
        for (let i = 0; i < Math.min(n, 5000); i++) {
          const size = FIXED[base];
          values.push(size <= 4 ? data.readUIntLE(pos, size) : data.subarray(pos, pos + size));
          pos += pad(size);
        }
      } else if (base === PT_STRING8 || base === PT_UNICODE || base === PT_BINARY || base === PT_OBJECT) {
        // Variable-length values always carry a count, multi-valued or not.
        const n = data.readUInt32LE(pos);
        pos += 4;
        for (let i = 0; i < Math.min(n, 5000); i++) {
          const length = data.readUInt32LE(pos);
          pos += 4;
          if (pos + length > data.length) throw new RangeError("property past the end");
          values.push(data.subarray(pos, pos + length));
          pos += pad(length);
        }
      } else {
        break;
      }
      if (!out.has(id)) out.set(id, { type: base, values });
    }
  } catch {
    // damaged: keep what was read
  }
  return out;
}

/** A string property's first value as text. */
function mapiString(props, id, codepage) {
  const prop = props.get(id);
  if (!prop || !prop.values.length || !Buffer.isBuffer(prop.values[0])) return "";
  const raw = prop.values[0];
  if (prop.type === PT_UNICODE) return stripNul(raw.toString("utf16le"));
  if (prop.type === PT_STRING8) return stripNul(ansi(raw, codepage));
  return "";
}

/** A binary or object property's first value. An object's value starts with a 16-byte IID. */
function mapiBytes(props, id) {
  const prop = props.get(id);
  if (!prop || !prop.values.length || !Buffer.isBuffer(prop.values[0])) return null;
  const raw = prop.values[0];
  return prop.type === PT_OBJECT ? raw.subarray(16) : raw;
}

// ------------------------------------------------------------------ compressed RTF

const LZ_DICT = Buffer.from(
  "{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor " +
    "MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx",
  "latin1",
);

/** MS-OXRTFCP: "LZFu" compressed RTF, or "MELA" (stored). Damaged input yields what decoded. */
function decompressRtf(data) {
  if (!Buffer.isBuffer(data) || data.length < 16) return Buffer.alloc(0);
  const compSize = data.readUInt32LE(0);
  const rawSize = data.readUInt32LE(4);
  const kind = data.readUInt32LE(8);
  const body = compSize + 4 <= data.length ? data.subarray(16, compSize + 4) : data.subarray(16);
  if (kind === 0x414c454d) return Buffer.from(body.subarray(0, rawSize)); // MELA
  if (kind !== 0x75465a4c) return Buffer.alloc(0); // not LZFu
  const window = Buffer.alloc(4096);
  LZ_DICT.copy(window);
  let wpos = LZ_DICT.length;
  const out = Buffer.alloc(rawSize);
  let o = 0;
  let i = 0;
  while (i < body.length && o < rawSize) {
    const control = body[i++];
    for (let bit = 0; bit < 8 && i < body.length && o < rawSize; bit++) {
      if (control & (1 << bit)) {
        if (i + 1 >= body.length) return out.subarray(0, o);
        const token = (body[i] << 8) | body[i + 1];
        i += 2;
        const offset = token >> 4;
        const length = (token & 0xf) + 2;
        if (offset === wpos) return out.subarray(0, o); // the end marker
        for (let k = 0; k < length && o < rawSize; k++) {
          const byte = window[(offset + k) % 4096];
          out[o++] = byte;
          window[wpos] = byte;
          wpos = (wpos + 1) % 4096;
        }
      } else {
        const byte = body[i++];
        out[o++] = byte;
        window[wpos] = byte;
        wpos = (wpos + 1) % 4096;
      }
    }
  }
  return out.subarray(0, o);
}

// ------------------------------------------------------------------ RTF to text

/** Destination groups that hold no body text. */
const SKIP_GROUPS = new Set([
  "fonttbl", "colortbl", "stylesheet", "info", "pict", "object", "header", "footer", "headerl", "headerr",
  "footerl", "footerr", "generator", "listtable", "listoverridetable", "rsidtbl", "datastore", "themedata",
  "latentstyles", "xmlnstbl", "filetbl", "revtbl", "fldinst", "bkmkstart", "bkmkend", "nonshppict",
]);

/**
 * The words of an RTF document, without its layout: enough to read a body that exists nowhere
 * else. With `{ html: true }` and an HTML-in-RTF document (`\fromhtml1`, MS-OXRTFEX), the HTML
 * that was encapsulated is returned instead, or `""` when the document is not one.
 */
function rtfToText(rtf, { html = false } = {}) {
  const s = Buffer.isBuffer(rtf) ? rtf.toString("latin1") : String(rtf || "");
  const fromHtml = /\\fromhtml1/.test(s.slice(0, 1000));
  if (html && !fromHtml) return "";
  const cpMatch = /\\ansicpg(\d+)/.exec(s.slice(0, 1000));
  const codepage = cpMatch ? Number(cpMatch[1]) : 1252;
  const out = [];
  let pendingBytes = [];
  const flushBytes = () => {
    if (pendingBytes.length) {
      out.push(ansi(Buffer.from(pendingBytes), codepage));
      pendingBytes = [];
    }
  };
  const emit = (text) => {
    flushBytes();
    out.push(text);
  };
  // Per group: [skip, htmlrtf, htmltag]
  const stack = [];
  let skip = false; // inside a group with no body text
  let htmlrtf = false; // HTML-in-RTF: inside \htmlrtf, RTF-only text that is not in the HTML
  let htmltag = false; // HTML-in-RTF: inside {\*\htmltagN …}, which holds HTML
  let ucSkip = 0;
  let uc = 1;
  const visible = () => (html ? !skip && (htmltag || !htmlrtf) : !skip && !htmltag);
  let i = 0;
  const n = s.length;
  while (i < n) {
    const c = s[i];
    if (c === "{") {
      stack.push([skip, htmlrtf, htmltag]);
      i++;
      const m = /^\\(\*)?\\?([a-z]+)(-?\d+)?/.exec(s.slice(i, i + 40));
      if (m && m[1] && /^(htmltag|mhtmltag)$/.test(m[2])) {
        htmltag = true;
        skip = false;
        i += m[0].length;
        if (s[i] === " ") i++;
      } else if (s.startsWith("\\*", i) || (m && SKIP_GROUPS.has(m[2]))) {
        skip = true;
      }
    } else if (c === "}") {
      [skip, htmlrtf, htmltag] = stack.length ? stack.pop() : [false, false, false];
      i++;
    } else if (c === "\\") {
      const m = /^\\([a-zA-Z]+)(-?\d+)? ?/.exec(s.slice(i, i + 40));
      if (m) {
        const word = m[1];
        const num = m[2];
        i += m[0].length;
        if (word === "htmlrtf") {
          htmlrtf = num !== "0";
          continue;
        }
        if (word === "uc" && num !== undefined) {
          uc = Number(num);
          continue;
        }
        if (word === "bin" && num !== undefined) {
          i += Math.max(0, Number(num));
          continue;
        }
        if (!visible()) continue;
        if (word === "par" || word === "line" || word === "sect" || word === "page") emit(html && htmltag ? "" : "\n");
        else if (word === "tab") emit("\t");
        else if (word === "u" && num !== undefined) {
          emit(String.fromCharCode(((Number(num) % 65536) + 65536) % 65536));
          ucSkip = uc;
        } else if (word === "lquote" || word === "rquote") emit("'");
        else if (word === "ldblquote" || word === "rdblquote") emit('"');
        else if (word === "endash") emit("–");
        else if (word === "emdash") emit("—");
        else if (word === "bullet") emit("•");
        else if (word === "emspace" || word === "enspace") emit(" ");
      } else {
        const next = s[i + 1];
        if (next === "'") {
          const hex = s.slice(i + 2, i + 4);
          i += 4;
          if (ucSkip > 0) {
            ucSkip--;
          } else if (visible() && /^[0-9a-fA-F]{2}$/.test(hex)) {
            pendingBytes.push(parseInt(hex, 16));
          }
        } else if (next === "\\" || next === "{" || next === "}") {
          if (visible()) emit(next);
          i += 2;
        } else if (next === "~") {
          if (visible()) emit(" ");
          i += 2;
        } else if (next === "\r" || next === "\n") {
          if (visible() && !html) emit("\n");
          i += 2;
        } else {
          i += 2;
        }
      }
    } else if (c === "\r" || c === "\n") {
      i++;
    } else {
      if (ucSkip > 0) ucSkip--; // the stand-in after \uN
      else if (visible()) emit(c);
      i++;
    }
  }
  flushBytes();
  const text = out.join("");
  return html ? text.trim() : text.replace(/[ \t]+\n/g, "\n").replace(/\n{3,}/g, "\n\n").trim();
}

// ---------------------------------------------------------------------- the container

/**
 * The body and attachments of a TNEF stream, or `null` when `data` is not one.
 *
 * `body_text` is `PR_BODY`, else the RTF body as text. `body_rtf` is the decompressed RTF.
 * `body_html` is `PR_BODY_HTML`, else the HTML an HTML-in-RTF body encapsulates. An
 * attachment whose data attribute is missing is dropped; one whose data is empty is kept, as
 * `tnef(1)` keeps it.
 */
function decode(data) {
  if (!isTnef(data)) return null;
  const result = { body_text: "", body_rtf: "", body_html: "", attachments: [] };
  let current = null;
  let codepage = 1252;
  let pos = 6;
  let rtfCompressed = null;
  try {
    while (pos + 9 <= data.length) {
      const level = data[pos];
      const attr = data.readUInt32LE(pos + 1);
      const length = data.readUInt32LE(pos + 5);
      const start = pos + 9;
      if (start + length > data.length) break;
      const chunk = data.subarray(start, start + length);
      pos = start + length + 2; // and the checksum
      if (level === 1 && attr === ATT_OEM_CODEPAGE && chunk.length >= 4) {
        codepage = chunk.readUInt32LE(0) || 1252;
      } else if (level === 2 && attr === ATT_ATTACH_RENDDATA) {
        if (result.attachments.length >= MAX_ATTACHMENTS) break;
        current = { name: "", title: "", type: "", cid: "", data: null };
        result.attachments.push(current);
      } else if (level === 2 && current) {
        if (attr === ATT_ATTACH_DATA) {
          current.data = Buffer.from(chunk);
        } else if (attr === ATT_ATTACH_TITLE) {
          current.title = stripNul(ansi(chunk, codepage));
        } else if (attr === ATT_ATTACHMENT) {
          const props = mapiProps(chunk);
          current.name = mapiString(props, PR_ATTACH_LONG_FILENAME, codepage) || mapiString(props, PR_ATTACH_FILENAME, codepage) || current.name;
          current.type = mapiString(props, PR_ATTACH_MIME_TAG, codepage) || current.type;
          current.cid = mapiString(props, PR_ATTACH_CONTENT_ID, codepage).replace(/^<|>$/g, "") || current.cid;
          if (current.data === null) {
            const obj = mapiBytes(props, PR_ATTACH_DATA_OBJ);
            if (obj) current.data = Buffer.from(obj);
          }
          if (!current.name && !current.title) current.title = mapiString(props, PR_DISPLAY_NAME, codepage);
        }
      } else if (level === 1 && attr === ATT_BODY && !result.body_text) {
        result.body_text = stripNul(ansi(chunk, codepage));
      } else if (level === 1 && attr === ATT_MAPI_PROPS) {
        const props = mapiProps(chunk);
        const body = mapiString(props, PR_BODY, codepage);
        if (body) result.body_text = body;
        const html = props.get(PR_BODY_HTML);
        if (html && Buffer.isBuffer(html.values[0])) {
          result.body_html = html.type === PT_UNICODE ? stripNul(html.values[0].toString("utf16le")) : stripNul(html.values[0].toString("utf8"));
        }
        const rtf = mapiBytes(props, PR_RTF_COMPRESSED);
        if (rtf) rtfCompressed = rtf;
      }
    }
  } catch {
    // damaged: keep what was read
  }
  if (rtfCompressed) {
    const rtf = decompressRtf(rtfCompressed);
    result.body_rtf = rtf.toString("latin1");
    if (!result.body_html) result.body_html = rtfToText(rtf, { html: true });
    if (!result.body_text) result.body_text = rtfToText(rtf);
  }
  result.attachments = result.attachments
    .filter((a) => a.data !== null)
    .map((a) => ({ name: (a.name || a.title).replace(/[\0/\\]/g, "_").trim(), type: a.type, cid: a.cid, data: a.data }));
  return result;
}

module.exports = { decode, isTnef, decompressRtf, rtfToText };
