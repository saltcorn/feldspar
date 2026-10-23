// **A mail above the size cap**, parsed from its structure and its text parts only
// (TODO-imap.md §5.4, M7.3).
//
// Above 25 MB the source is not fetched. Instead, as saltcorn/imap did:
// - the headers and IMAP's `BODYSTRUCTURE` are fetched;
// - the text parts are downloaded, each on its own;
// - the attachments are listed from the structure, without their bytes, to be downloaded when
//   they are opened.
//
// The result is the same row `parse.js` writes, with `body_state = "too_large"` and every
// attachment's `content` null. The structure becomes the same tree of containers and leaves
// `parse.js` builds from the source, so the body choice, the attachment names and the
// placeholders are one code path. A TNEF part at or below the cap is downloaded and unpacked,
// because the body of an Outlook mail may be nowhere else.
//
// The fetcher is passed in, so this file knows nothing of IMAP and is tested with a fake:
//
//   fetchPart(part, { maxBytes }) → Promise<Buffer>
//
// resolves to the part's body as the server stores it, still transfer-encoded (IMAP's
// `BODY.PEEK[<part>]`), and may stop after `maxBytes` (`BODY.PEEK[<part>]<0.maxBytes>`).

"use strict";

const libmime = require("libmime");
const { assemble, selectBodies, split, failed, SIZE_CAP } = require("./parse.js");

/** How much of a text part is downloaded: the body is cut at 400,000 characters anyway, which
 * is at most 1.6 MB of UTF-8, and base64 adds a third. */
const TEXT_FETCH_LIMIT = 4 * 1024 * 1024;

const decodeWords = (value) => {
  try {
    return libmime.decodeWords(value);
  } catch {
    return value;
  }
};

/**
 * imapflow's `bodyStructure` as the tree `assemble` takes.
 *
 * imapflow gives the body of a single-part message/rfc822 the wrapper's own part number; IMAP
 * addresses it as `<wrapper>.1` (RFC 3501 §6.4.5), which is what `parse.js` numbers it too.
 */
function treeOfStructure(node, { encrypted = false, inMessage = null } = {}) {
  const type = String(node.type || "").toLowerCase();
  const params = node.parameters || {};
  const dispositionParams = node.dispositionParameters || {};
  if (type.startsWith("multipart/") && Array.isArray(node.childNodes)) {
    const multipart = type.slice("multipart/".length);
    const inner = encrypted || multipart === "encrypted";
    return { multipart, encrypted, children: node.childNodes.map((child) => treeOfStructure(child, { encrypted: inner })) };
  }
  let disposition = node.disposition ? String(node.disposition).toLowerCase() : null;
  if (disposition && disposition !== "inline" && disposition !== "attachment") disposition = "attachment";
  const filename = decodeWords(String(dispositionParams.filename || params.name || "")).trim();
  if (type === "message/rfc822" && Array.isArray(node.childNodes) && node.childNodes.length && disposition !== "attachment" && !filename) {
    return { multipart: "mixed", message: true, encrypted, children: [treeOfStructure(node.childNodes[0], { encrypted, inMessage: node })] };
  }
  let path = node.part || "1";
  if (inMessage && path === (inMessage.part || "1")) path = `${path}.1`;
  return {
    path,
    type: type && type !== "/" ? type : "text/plain",
    typed: Boolean(type && type !== "/"),
    charset: params.charset || null,
    encoding: String(node.encoding || "").toLowerCase(),
    disposition,
    filename,
    cid: String(node.id || "").trim().replace(/^<|>$/g, "").trim(),
    flowed: /^flowed$/i.test(params.format || ""),
    delsp: /^yes$/i.test(params.delsp || ""),
    encrypted,
    size: Number(node.size) || 0,
    raw: null,
  };
}

/** Every leaf of a tree. */
function leavesOf(node, out = []) {
  if (node.children) node.children.forEach((child) => leavesOf(child, out));
  else out.push(node);
  return out;
}

/**
 * Parse a mail above the size cap from its structure (§5.4).
 *
 * @param {object} bodyStructure  imapflow's `bodyStructure` of the message
 * @param {(part: string, opts: { maxBytes?: number }) => Promise<Buffer>} fetchPart
 * @param {{ headers?: Buffer, internalDate?: Date|string, flags?: Iterable<string>, size?: number, extraHeaders?: string[] }} opts
 *   `headers` is the raw header block (`BODY.PEEK[HEADER]`)
 * @returns {Promise<{ message: object, attachments: object[] }>} as `parse` returns, with
 *   `body_state` `"too_large"` and each attachment's `content` null. A part that could not be
 *   downloaded is named in `import_error`; the row is written all the same.
 */
async function parseLarge(bodyStructure, fetchPart, { headers, internalDate, flags, size, extraHeaders } = {}) {
  const opts = { internalDate, flags, size, bodyState: "too_large", extraHeaders };
  let rootHeaders = null;
  try {
    if (headers && headers.length) {
      const nodes = await split(Buffer.concat([headers, Buffer.from("\r\n\r\n")]));
      rootHeaders = nodes.length ? nodes[0].node.headers : null;
    }
    const tree = treeOfStructure(bodyStructure || {});

    // Download what the row needs: the parts that make the body, and TNEF containers.
    const textParts = [];
    const htmlParts = [];
    selectBodies({ multipart: "mixed", children: [tree] }, textParts, htmlParts, []);
    const wanted = new Set([...textParts, ...htmlParts]);
    for (const leaf of leavesOf(tree)) {
      const tnef = leaf.type === "application/ms-tnef" || leaf.type === "application/vnd.ms-tnef" || /^winmail\.dat$/i.test(leaf.filename);
      if (tnef && leaf.size <= SIZE_CAP) wanted.add(leaf);
    }
    const errors = [];
    for (const leaf of wanted) {
      try {
        const tnef = !textParts.includes(leaf) && !htmlParts.includes(leaf);
        const raw = await fetchPart(leaf.path, tnef ? {} : { maxBytes: TEXT_FETCH_LIMIT });
        leaf.raw = Buffer.isBuffer(raw) ? raw : Buffer.from(raw || "");
      } catch (err) {
        errors.push(`part ${leaf.path}: ${(err && err.message) || err}`);
      }
    }
    const result = assemble(rootHeaders, tree, opts);
    // A listed attachment's size is the server's, which is its encoded size.
    for (const a of result.attachments) {
      const leaf = leavesOf(tree).find((l) => l.path === a.part && !l.raw);
      if (leaf && leaf.encoding === "base64") a.size = Math.floor((leaf.size * 3) / 4);
    }
    if (errors.length) result.message.import_error = `could not download ${errors.join("; ")}`.slice(0, 1000);
    return result;
  } catch (err) {
    return failed(rootHeaders, err, opts);
  }
}

/**
 * A mail's row from its headers and structure alone, with no body (TODO-imap.md §4, M9.1):
 * mirror mode keeps older mail this way until it is opened (`mail_fetch_body`).
 *
 * The header columns are filled as `parse` fills them, `body_text`, `body_html` and `snippet`
 * are null, and `body_state` is `"headers"`. The attachments are listed from the structure, as
 * above the cap, with `content` null, so a client can show a paper clip and offer them.
 *
 * @param {object} bodyStructure  imapflow's `bodyStructure`
 * @param {{ headers?: Buffer, internalDate?: Date|string, flags?: Iterable<string>, size?: number, extraHeaders?: string[] }} opts
 */
async function parseHeaders(bodyStructure, { headers, internalDate, flags, size, extraHeaders } = {}) {
  const opts = { internalDate, flags, size, bodyState: "headers", extraHeaders };
  let rootHeaders = null;
  let result;
  try {
    if (headers && headers.length) {
      const nodes = await split(Buffer.concat([headers, Buffer.from("\r\n\r\n")]));
      rootHeaders = nodes.length ? nodes[0].node.headers : null;
    }
    const tree = treeOfStructure(bodyStructure || {});
    result = assemble(rootHeaders, tree, opts);
    const leaves = leavesOf(tree);
    for (const a of result.attachments) {
      const leaf = leaves.find((l) => l.path === a.part);
      if (leaf && leaf.encoding === "base64") a.size = Math.floor((leaf.size * 3) / 4);
    }
  } catch (err) {
    result = failed(rootHeaders, err, opts);
  }
  // `assemble` explains a missing body with a placeholder; here nothing was fetched, which is
  // not the same thing.
  Object.assign(result.message, { body_text: null, body_html: null, snippet: null });
  return result;
}

module.exports = { parseLarge, parseHeaders, treeOfStructure, TEXT_FETCH_LIMIT };
