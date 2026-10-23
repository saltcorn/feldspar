// **One folder's mail, into the tables** (TODO-imap.md §5.2, §5.3; M8.3, M8.4).
//
// Called with the folder selected and its mailbox lock held (`account.js` does both), with an
// `env` of `{ client, db, files, account, log, signal, extraHeaders }`: an imapflow client, the
// service's `ctx.db` and `ctx.files`, the account row, where to say what happened, and the
// header fields kept in `mail_messages.headers` (the module's "Headers to keep").
//
//   await syncFolder(env, folderRow, { flags: true });
//
// New mail (§5.2)
// ---------------
// - **UIDs only.** The cursor is `mail_folders.last_uid`. Each batch is `UID SEARCH UID
//   last+1:*` (with `SINCE start_from` when the account has one), filtered to `uid > last`,
//   because `N:*` always answers the highest message even when N is past it.
// - **At most 50 per batch**, oldest first, each source fetched on its own, so memory holds one
//   mail at a time. The loop goes on until nothing is left.
// - **Each message gets its own try.** A mail that does not parse is still stored, with
//   `import_error`. Only a failure to *write* stops the batch, and then the cursor stays before
//   that mail.
// - **A message is stored in two steps**, because its attachment rows refer to it: the row is
//   inserted with `body_state = "importing"`, the files are written and the attachment rows
//   inserted, and then `body_state` is set to what the parser said (`full`, `too_large`). An
//   Insert trigger sees the whole message but not yet its attachments. A trigger that needs
//   them listens for the update where `old.body_state === "importing"` (M8.8).
// - **A message deleted meanwhile is skipped.** Its attachment rows are inserted in one
//   statement; when that fails because the message row is gone (deleted by an action or by a
//   sync of the same folder on another path), the message counts as not stored, and the
//   folder's next sync looks at it again. It does not fail the batch.
// - **Nothing is stored twice.** A row already there for `(folder, uid)` is skipped; one left
//   at `importing` by a crash has its attachment rows written again. A row whose `uid` was
//   cleared (an expunge, a UIDVALIDITY reset) is matched again by `message_id` and given the
//   new UID instead of being imported a second time.
// - **Moves happen after the batch**, by UID: mail with an `import_error` to the account's
//   `error_folder`, the rest to `processed_folder`. The row stays, and records where the mail
//   went (its `folder` and its new `uid`, when the server says it with UIDPLUS).
// - **The cursor moves last**, past the messages that were stored and moved.
//
// Flags and expunges (§5.3)
// -------------------------
// With CONDSTORE, the flags that changed since `highestmodseq`; without it, the flags of the
// newest 2,000 messages. Keywords (`$label1`, `$Junk`, any flag without a backslash) are kept in
// `keywords`, sorted. Expunges by `UID SEARCH ALL` against the stored UIDs. In ingest mode an
// expunged message's row is never deleted: its `uid` is cleared, which records that the mail is
// no longer in that folder on the server. In mirror mode it is deleted, with its attachment rows
// and stored files.
//
// The selected folder
// -------------------
// Every step checks that the folder it was given is the one selected (`requireSelected`)
// before it searches, and again before it deletes what the search did not find: a UID SEARCH
// answered by another folder would make every row of this one look expunged.
//
// Mirror mode (§4, M9.1)
// ----------------------
// A mirror account is a two-way copy of every folder, and differs in four ways:
// - **Newest first.** A folder's first sync stores its newest 50 messages, sets `last_uid` to the
//   top and `backfill_uid` to the lowest UID stored. New mail is then `uid > last_uid` as above,
//   and `backfill` works downwards from `backfill_uid`, one batch at a time, as slowly as the
//   runner calls it (`account.js`: a batch every 15 s, 200 a minute). `backfill_uid` is 0 when
//   the folder is complete, and null before its first sync.
// - **Bodies for `body_days`.** Mail received in the last `body_days` days is fetched whole;
//   older mail is stored from its headers and structure alone (`body_state = "headers"`), and
//   `mail_fetch_body` fetches the rest when it is opened.
// - **Attachments are stored when opened** (`mail_attachment`), not on import.
// - **Nothing is kept that the server no longer has**: an expunge deletes the row, and a new
//   UIDVALIDITY deletes the folder's rows and syncs it again from the start.

"use strict";

const { parse, SIZE_CAP } = require("./parse.js");
const { parseLarge, parseHeaders } = require("./parse-large.js");

/** Messages per batch (§5.2). */
const BATCH = 50;
/** Without CONDSTORE, how many of the newest messages have their flags re-read (§5.3). */
const FLAG_WINDOW = 2000;
/** Rows read or written per database call when comparing UIDs. */
const CHUNK = 500;
/** The file store and folder the module's `mail_attachments.file` field declares (M4.5). */
const FILE_STORE = "mail_files";
const FILE_FOLDER = "attachments";

/** A message's flags as row values: the four system flags, and its keywords. */
function flagValues(flags) {
  const f = new Set([...(flags || [])].map((x) => String(x).toLowerCase()));
  return {
    seen: f.has("\\seen"),
    flagged: f.has("\\flagged"),
    answered: f.has("\\answered"),
    draft: f.has("\\draft"),
    keywords: keywordsOf(flags),
  };
}

/** The keywords among a message's flags (`$label1`, `$Forwarded`, `NonJunk`): every flag that
 * is not a system flag, sorted. `\Recent` and the other backslash flags are not keywords. */
function keywordsOf(flags) {
  return [...new Set([...(flags || [])].map(String).filter((x) => x && !x.startsWith("\\")))].sort();
}

/** A stored `keywords` value (an array, or JSON text) as a sorted array. */
function storedKeywords(value) {
  if (Array.isArray(value)) return value;
  if (typeof value === "string" && value) {
    try {
      const list = JSON.parse(value);
      return Array.isArray(list) ? list : [];
    } catch {
      return [];
    }
  }
  return [];
}

/** Throw unless `folder` is the one selected on the connection. */
function requireSelected(env, folder) {
  const selected = env.client && env.client.mailbox && env.client.mailbox.path;
  const same = (a, b) => a === b || (/^inbox$/i.test(a || "") && /^inbox$/i.test(b || ""));
  if (!same(selected, folder.path)) {
    throw new Error(`${folder.path} was to be synced, but ${selected || "no folder"} is selected; the sync of ${folder.path} stopped`);
  }
}

/** A name that is safe as one path segment of a stored file. */
function safeName(name, fallback) {
  const cleaned = String(name || "")
    .normalize("NFC")
    .replace(/[\u0000-\u001f\u007f/\\:*?"<>|]+/g, "_")
    .replace(/^\.+/, "_")
    .trim()
    .slice(0, 120);
  return cleaned || fallback;
}

/** Whether an attachment can be downloaded by its part number. `<part>:tnef:<n>` and
 * `<part>:eml:<n>` were unpacked from inside another part (M7). */
const downloadable = (part) => typeof part === "string" && /^[0-9]+(\.[0-9]+)*$/.test(part);

/** A Readable (or anything async-iterable) read into one Buffer. */
async function bytesOf(stream) {
  const chunks = [];
  for await (const chunk of stream) chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
  return Buffer.concat(chunks);
}

/** The folder row's numbers, as numbers. */
const num = (v) => (v === null || v === undefined || v === "" ? null : Number(v));

/** The header fields kept in `mail_messages.headers` when the module's "Headers to keep" is
 * empty: what a client needs for unsubscribe links, sender checks and spam marks. */
const DEFAULT_HEADERS = ["list-id", "list-unsubscribe", "list-unsubscribe-post", "authentication-results", "x-spam-*", "precedence", "auto-submitted"];

/** The module's "Headers to keep" (`extra_headers`): one name per line or comma-separated, a
 * trailing `*` for a prefix; empty for the defaults, `-` for none. */
function headerNames(configuration) {
  const text = String((configuration && configuration.extra_headers) || "").trim();
  if (!text) return DEFAULT_HEADERS;
  if (text === "-") return [];
  return [...new Set(text.split(/[\s,]+/).map((n) => n.trim().replace(/:$/, "").toLowerCase()).filter((n) => /^[!-9;-~]+$/.test(n)))];
}

/** Whether an account is a two-way mirror (§4) rather than an ingest mailbox. */
const isMirror = (account) => Boolean(account) && account.mode === "mirror";

/** Mirror mode: mail received before this time is stored with its headers only (`body_days`,
 * default 90). */
function bodyCutoff(account, now = Date.now()) {
  const days = num(account && account.body_days);
  return now - (days !== null && days >= 0 ? days : 90) * 86_400_000;
}

// ------------------------------------------------------------------------------ deleting

/** Remove the stored files of attachment rows. */
async function removeFiles(env, attachments) {
  const { files, log } = env;
  for (const a of attachments) {
    if (a.file && files && files.remove) {
      await files.remove(FILE_STORE, a.file).catch((e) => log.warn(`could not remove ${a.file}: ${e.message}`));
    }
  }
}

/** Delete messages' rows, with their attachment rows and stored files (mirror mode, and the
 * mirror actions). A row the database will not delete — the application's own data refers to
 * it — keeps existing with its `uid` cleared, as ingest mode keeps one, and is logged. */
async function deleteMessages(env, ids) {
  const { db, log } = env;
  let deleted = 0;
  for (let i = 0; i < ids.length; i += CHUNK) {
    const part = ids.slice(i, i + CHUNK);
    const attachments = await db.mail_attachments.where({ message: { in: part } }).rows();
    await removeFiles(env, attachments);
    if (attachments.length) await db.mail_attachments.where({ message: { in: part } }).delete();
    try {
      await db.mail_messages.where({ id: { in: part } }).delete();
      deleted += part.length;
    } catch (_) {
      // One at a time, to find the rows something else refers to.
      for (const id of part) {
        try {
          await db.mail_messages.where({ id }).delete();
          deleted += 1;
        } catch (e) {
          log.warn(`message ${id} is gone from the server but is kept: ${e.message}`);
          await db.mail_messages.where({ id }).update({ uid: null });
        }
      }
    }
  }
  return deleted;
}

/** A folder that is gone from the server (mirror mode): its messages and its row. */
async function deleteFolder(env, folder) {
  const { db } = env;
  for (;;) {
    const ids = (await db.mail_messages.where({ folder: folder.id }).select("id").limit(CHUNK).rows()).map((r) => r.id);
    if (!ids.length) break;
    const deleted = await deleteMessages(env, ids);
    if (deleted < ids.length) {
      // Some are kept: they cannot stay in a folder that is going, so they leave it.
      await db.mail_messages.where({ folder: folder.id }).update({ folder: null, uid: null });
      break;
    }
  }
  await db.mail_folders.where({ id: folder.id }).delete();
}

// ------------------------------------------------------------------------------ folder rows

/** The `mail_folders` row of `path` for an account, inserted when it is missing. */
async function folderRow(db, account, path, values = {}) {
  const found = await db.mail_folders.where({ account: account.id, path }).first();
  if (found) return found;
  try {
    return await db.mail_folders.insert({ account: account.id, path, name: path.split(/[./]/).pop(), sync: false, ...values });
  } catch (e) {
    // Inserted by somebody else in between (the unique constraint): read it again.
    const again = await db.mail_folders.where({ account: account.id, path }).first();
    if (again) return again;
    throw e;
  }
}

// ------------------------------------------------------------------------------ uidvalidity

/** UIDVALIDITY changed (§5.2): every UID of the folder means something else now. In ingest mode
 * the rows are kept and their `uid` cleared, so new mail can be matched to them by `message_id`
 * as it is found again; in mirror mode they are deleted and the folder is synced again from the
 * start. The cursor starts over either way. */
async function resetFolder(env, folder, uidValidity) {
  const { db, log } = env;
  const mirror = isMirror(env.account);
  log.warn(
    `${env.account.label || env.account.email}: ${folder.path} has a new UIDVALIDITY (${folder.uidvalidity} → ${uidValidity}); ` +
      (mirror ? "it is synced again" : "its messages will be matched again by Message-ID"),
  );
  for (;;) {
    const ids = (await db.mail_messages.where({ folder: folder.id, uid: { gt: 0 } }).select("id").limit(CHUNK).rows()).map((r) => r.id);
    if (!ids.length) break;
    if (mirror) {
      if ((await deleteMessages(env, ids)) < ids.length) continue;
    } else {
      await db.mail_messages.where({ id: { in: ids } }).update({ uid: null });
    }
  }
  delete folder.backlog;
  const values = { uidvalidity: uidValidity, last_uid: 0, highestmodseq: null, backfill_uid: null };
  await db.mail_folders.where({ id: folder.id }).update(values);
  Object.assign(folder, values);
}

// ------------------------------------------------------------------------------ one message

/** The body of one part as the server stores it, still transfer-encoded (`parse-large.js`). */
async function fetchRawPart(client, uid, part, maxBytes) {
  const query = { uid: true, bodyParts: [maxBytes ? { key: part, start: 0, maxLength: maxBytes } : part] };
  const message = await client.fetchOne(String(uid), query, { uid: true });
  if (!message || !message.bodyParts || !message.bodyParts.size) throw new Error("the server sent no data for it");
  // The key is the part, with `<0>` when partial: there is only one.
  return [...message.bodyParts.values()][0];
}

/** One message, parsed: the whole source at or below the cap, the structure above it, and
 * (`headersOnly`, mirror mode's older mail) the headers and structure alone. */
async function fetchAndParse(client, meta, { headersOnly = false, extraHeaders } = {}) {
  const opts = { internalDate: meta.internalDate, flags: meta.flags, size: meta.size, extraHeaders };
  if (headersOnly) {
    let { bodyStructure, headers } = meta;
    if (!bodyStructure) {
      const message = await client.fetchOne(String(meta.uid), { uid: true, bodyStructure: true, headers: true }, { uid: true });
      if (!message) return null;
      ({ bodyStructure, headers } = message);
    }
    return parseHeaders(bodyStructure, { ...opts, headers });
  }
  if (!(meta.size > SIZE_CAP)) {
    const message = await client.fetchOne(String(meta.uid), { uid: true, source: true }, { uid: true });
    if (!message || !message.source) return null;
    return parse(message.source, opts);
  }
  const message = await client.fetchOne(String(meta.uid), { uid: true, bodyStructure: true, headers: true }, { uid: true });
  if (!message) return null;
  return parseLarge(message.bodyStructure, (part, { maxBytes } = {}) => fetchRawPart(client, meta.uid, part, maxBytes), {
    headers: message.headers,
    internalDate: meta.internalDate,
    flags: meta.flags,
    size: meta.size,
    extraHeaders,
  });
}

/** Store one message: the row, its files, its attachment rows, then its final `body_state`.
 * `existing` is a row left at `importing` by an earlier attempt, to be finished. Answers the
 * row. Throws only when the database or the file store refuses. */
async function storeMessage(env, folder, meta, parsed, existing) {
  const { db, files, client } = env;
  const { message, attachments } = parsed;
  const finalState = message.body_state || "full";
  const values = { ...message, account: env.account.id, folder: folder.id, uid: meta.uid, body_state: "importing" };
  let row;
  if (existing) {
    await db.mail_attachments.where({ message: existing.id }).delete();
    await db.mail_messages.where({ id: existing.id }).update(values);
    row = { ...existing, ...values };
  } else {
    row = await db.mail_messages.insert(values);
  }
  const problems = [];
  // Mirror mode stores an attachment's file when it is first opened (§4), ingest on import.
  const storeFiles = !isMirror(env.account) && files;
  const rows = [];
  for (let i = 0; i < attachments.length; i++) {
    const a = attachments[i];
    let file = null;
    if (storeFiles) {
      try {
        let content = a.content;
        // Above the cap the parser lists attachments without their bytes; ingest stores them
        // now (§4), straight from the server as a stream.
        if (!content && downloadable(a.part)) content = (await client.download(String(meta.uid), a.part, { uid: true })).content;
        if (content) {
          const name = safeName(a.filename, `part-${i + 1}`);
          file = await files.write(FILE_STORE, `${FILE_FOLDER}/${row.id}/${i + 1}-${name}`, content, {
            contentType: a.content_type || "application/octet-stream",
            filename: a.filename || name,
          });
        }
      } catch (e) {
        problems.push(`attachment ${a.filename || a.part}: ${e.message}`);
      }
    }
    rows.push({
      message: row.id,
      part: a.part,
      filename: a.filename,
      content_type: a.content_type,
      size: a.size,
      cid: a.cid,
      inline: !!a.inline,
      file,
    });
  }
  if (rows.length) {
    try {
      await db.mail_attachments.insert(rows);
    } catch (e) {
      // Deleted in the meantime (its folder reset, an action): nothing to attach them to.
      if (await db.mail_messages.where({ id: row.id }).first()) throw e;
      await removeFiles(env, rows);
      return null;
    }
  }
  const done = { body_state: finalState };
  if (problems.length) done.import_error = [message.import_error, ...problems].filter(Boolean).join("; ").slice(0, 1000);
  await db.mail_messages.where({ id: row.id }).update(done);
  return { ...row, ...done };
}

/** The row for a message that could not be fetched or parsed at all, so it is visible and the
 * cursor can pass it (§5.2). */
function errorRow(meta, error) {
  return {
    message: {
      subject: "",
      body_text: "",
      snippet: "",
      size: meta.size || null,
      internal_date: meta.internalDate ? new Date(meta.internalDate).toISOString() : null,
      sent_at: meta.internalDate ? new Date(meta.internalDate).toISOString() : null,
      has_attachments: false,
      body_state: "full",
      ...flagValues(meta.flags),
      import_error: `could not import the message: ${(error && error.message) || error}`.slice(0, 1000),
    },
    attachments: [],
  };
}

// ------------------------------------------------------------------------------ new mail

/** Import one batch of UIDs. Answers `[{ uid, row }]` for every message that is stored (new or
 * already there), in UID order, and the UID it stopped before when a write failed. */
async function importBatch(env, folder, uids) {
  const { client, db, log } = env;
  const mirror = isMirror(env.account);
  const cutoff = mirror ? bodyCutoff(env.account) : null;
  // Collected first: imapflow must not be given another command while a FETCH is streaming.
  // A mirror also takes the structure and headers here, for the mail it keeps headers-only.
  const query = { uid: true, flags: true, size: true, internalDate: true, envelope: true };
  if (mirror) Object.assign(query, { bodyStructure: true, headers: true });
  const metas = await client.fetchAll(uids.join(","), query, { uid: true });
  const byUid = new Map(metas.map((m) => [m.uid, m]));
  const stored = [];
  for (const uid of uids) {
    if (env.signal && env.signal.aborted) return { stored, stoppedAt: uid };
    const meta = byUid.get(uid);
    if (!meta) continue; // expunged since the search
    try {
      const existing = await db.mail_messages.where({ folder: folder.id, uid }).first();
      if (existing && existing.body_state !== "importing") {
        stored.push({ uid, row: existing });
        continue;
      }
      // Seen before under another UID (an expunge undone, a UIDVALIDITY reset): give it this one.
      const messageId = meta.envelope && meta.envelope.messageId;
      if (!existing && messageId) {
        const known = await db.mail_messages.where({ folder: folder.id, uid: null, message_id: messageId, body_state: { ne: "importing" } }).first();
        if (known) {
          const values = { uid, ...flagValues(meta.flags) };
          await db.mail_messages.where({ id: known.id }).update(values);
          stored.push({ uid, row: { ...known, ...values }, relinked: true });
          continue;
        }
      }
      let parsed;
      try {
        const old = mirror && meta.internalDate && new Date(meta.internalDate).getTime() < cutoff;
        parsed = await fetchAndParse(client, meta, { headersOnly: Boolean(old), extraHeaders: env.extraHeaders });
        if (!parsed) continue; // gone in the meantime
      } catch (e) {
        if (!client.usable) throw e; // the connection went: retried after the reconnect
        parsed = errorRow(meta, e);
      }
      const row = await storeMessage(env, folder, meta, parsed, existing);
      if (!row) {
        log.info(`${folder.path} UID ${uid}: its row was deleted while it was being stored; skipped`);
        continue;
      }
      if (row.import_error) log.warn(`${folder.path} UID ${uid}: ${row.import_error}`);
      stored.push({ uid, row });
    } catch (e) {
      e.stoppedAt = uid;
      e.stored = stored;
      throw e;
    }
  }
  return { stored, stoppedAt: null };
}

/** After a batch: move stored mail to the processed or error folder, by UID, and record where it
 * went. Mail that was already there before this batch is moved too (a crash between the import
 * and the move). A refused move is logged and leaves the mail where it is. */
async function moveAfterBatch(env, folder, stored) {
  const { account, client, db, log } = env;
  if (isMirror(account)) return;
  const watched = new Set(watchedFolders(account));
  const groups = new Map();
  for (const s of stored) {
    if (s.relinked) continue;
    const target = s.row.import_error ? account.error_folder || account.processed_folder : account.processed_folder;
    if (!target || target === folder.path || watched.has(target)) continue;
    if (!groups.has(target)) groups.set(target, []);
    groups.get(target).push(s);
  }
  for (const [target, items] of groups) {
    const range = items.map((s) => s.uid).join(",");
    let result;
    try {
      result = await client.messageMove(range, target, { uid: true });
      if (!result) {
        // Most often the folder is not there: make it, and try once more.
        await client.mailboxCreate(target).catch(() => {});
        result = await client.messageMove(range, target, { uid: true });
      }
    } catch (e) {
      if (!client.usable) throw e;
      log.warn(`could not move ${items.length} message(s) from ${folder.path} to ${target}: ${e.message}`);
      continue;
    }
    if (!result) {
      log.warn(`could not move ${items.length} message(s) from ${folder.path} to ${target}`);
      continue;
    }
    const destination = await folderRow(db, account, target);
    const uidMap = result.uidMap instanceof Map ? result.uidMap : new Map();
    for (const s of items) {
      const newUid = uidMap.get(s.uid);
      await db.mail_messages.where({ id: s.row.id }).update({ folder: destination.id, uid: newUid === undefined ? null : newUid });
    }
  }
}

/** New mail in the selected folder, in batches, until there is none (§5.2). Answers how many
 * messages were stored. */
async function syncNewMail(env, folder) {
  const { client, db, account } = env;
  let last = num(folder.last_uid) || 0;
  const since = account.start_from ? new Date(account.start_from) : null;
  let count = 0;
  for (;;) {
    if (env.signal && env.signal.aborted) break;
    const query = { uid: `${last + 1}:*` };
    if (since && !isNaN(since)) query.since = since;
    const found = (await client.search(query, { uid: true })) || [];
    const uids = found.filter((u) => u > last).sort((a, b) => a - b);
    if (!uids.length) {
      // Every UID below UIDNEXT was there when the folder was selected, and none above the
      // cursor is wanted: the cursor can pass them, so a deleted top message is not looked for
      // again on every pass.
      const next = num(client.mailbox && client.mailbox.uidNext);
      if (next && next - 1 > last && !since) {
        await db.mail_folders.where({ id: folder.id }).update({ last_uid: next - 1 });
        folder.last_uid = next - 1;
      }
      break;
    }
    const batch = uids.slice(0, BATCH);
    let outcome;
    let failure = null;
    try {
      outcome = await importBatch(env, folder, batch);
    } catch (e) {
      failure = e;
      outcome = { stored: e.stored || [], stoppedAt: e.stoppedAt };
    }
    if (client.usable) await moveAfterBatch(env, folder, outcome.stored);
    // Past every UID before the one it stopped at; all of the batch when it did not stop.
    const next = outcome.stoppedAt ? Math.max(last, ...batch.filter((u) => u < outcome.stoppedAt)) : batch[batch.length - 1];
    if (next > last) {
      await db.mail_folders.where({ id: folder.id }).update({ last_uid: next });
      folder.last_uid = next;
      last = next;
    }
    count += outcome.stored.length;
    if (failure) throw failure;
    if (outcome.stoppedAt) break;
  }
  return count;
}

// ------------------------------------------------------------------------------ flags, expunges

/** Write the flags of `[{ uid, flags }]` to the folder's rows that differ. */
async function applyFlags(env, folder, changes) {
  const { db } = env;
  let updated = 0;
  for (let i = 0; i < changes.length; i += CHUNK) {
    const part = changes.slice(i, i + CHUNK);
    const rows = await db.mail_messages
      .where({ folder: folder.id, uid: { in: part.map((c) => c.uid) } })
      .select("id", "uid", "seen", "flagged", "answered", "draft", "keywords")
      .rows();
    const byUid = new Map(rows.map((r) => [Number(r.uid), r]));
    for (const change of part) {
      const row = byUid.get(change.uid);
      if (!row) continue;
      const values = flagValues(change.flags);
      const changed =
        ["seen", "flagged", "answered", "draft"].some((k) => !!row[k] !== values[k]) ||
        JSON.stringify(storedKeywords(row.keywords)) !== JSON.stringify(values.keywords);
      if (changed) {
        await db.mail_messages.where({ id: row.id }).update(values);
        updated += 1;
      }
    }
  }
  return updated;
}

/** Flags and expunges of the selected folder (§5.3). Answers `{ flags, expunged }`. */
async function syncFlags(env, folder) {
  const { client, db } = env;
  requireSelected(env, folder);
  const mailbox = client.mailbox;
  const last = num(folder.last_uid) || 0;
  const present = ((await client.search({ all: true }, { uid: true })) || []).filter((u) => u <= last).sort((a, b) => a - b);

  // Flags: what changed since the stored HIGHESTMODSEQ, or the newest window.
  let changes = [];
  const modseq = mailbox && mailbox.highestModseq !== undefined && mailbox.highestModseq !== null ? String(mailbox.highestModseq) : null;
  const condstore = client.enabled && client.enabled.has("CONDSTORE") && modseq;
  if (condstore && folder.highestmodseq) {
    if (folder.highestmodseq !== modseq && last > 0) {
      changes = await client.fetchAll(`1:${last}`, { uid: true, flags: true }, { uid: true, changedSince: BigInt(folder.highestmodseq) });
    }
  } else if (present.length) {
    const window = present.slice(-FLAG_WINDOW);
    changes = await client.fetchAll(`${window[0]}:${window[window.length - 1]}`, { uid: true, flags: true }, { uid: true });
  }
  const flags = await applyFlags(env, folder, changes.map((m) => ({ uid: m.uid, flags: m.flags })));

  // Expunges: stored UIDs the server no longer has. Recorded by clearing the UID (ingest), or
  // deleted (mirror). Only when the search above was this folder's.
  requireSelected(env, folder);
  const have = new Set(present);
  let expunged = 0;
  let after = 0;
  for (;;) {
    const rows = await db.mail_messages
      .where({ folder: folder.id, uid: { gt: after } })
      .select("id", "uid")
      .orderBy("uid")
      .limit(CHUNK)
      .rows();
    if (!rows.length) break;
    after = Number(rows[rows.length - 1].uid);
    const gone = rows.filter((r) => Number(r.uid) <= last && !have.has(Number(r.uid))).map((r) => r.id);
    if (gone.length) {
      if (isMirror(env.account)) await deleteMessages(env, gone);
      else await db.mail_messages.where({ id: { in: gone } }).update({ uid: null });
      expunged += gone.length;
    }
    if (rows.length < CHUNK) break;
  }

  const unseen = (await client.search({ seen: false }, { uid: true })) || [];
  const values = { highestmodseq: modseq, total: mailbox ? mailbox.exists : null, unread: unseen.length };
  await db.mail_folders.where({ id: folder.id }).update(values);
  Object.assign(folder, values);
  return { flags, expunged };
}

// ------------------------------------------------------------------------------ watched folders

/** The folders an ingest account watches (default `["INBOX"]`). */
function watchedFolders(account) {
  let list = account.folders;
  if (typeof list === "string") {
    try {
      list = JSON.parse(list);
    } catch {
      list = list.split(",");
    }
  }
  list = (Array.isArray(list) ? list : []).map((f) => String(f).trim()).filter(Boolean);
  return list.length ? list : ["INBOX"];
}

// ------------------------------------------------------------------------------ mirror

/** A mirror folder's first sync: its newest batch, the cursor at the top, and the backfill
 * below what was stored. Answers how many messages were stored. */
async function startMirror(env, folder) {
  const { client, db } = env;
  const all = ((await client.search({ all: true }, { uid: true })) || []).sort((a, b) => a - b);
  const newest = all.slice(-BATCH);
  let stored = 0;
  if (newest.length) {
    const outcome = await importBatch(env, folder, newest);
    stored = outcome.stored.length;
    // Stopped (the service is stopping): the next pass starts again; nothing is stored twice.
    if (outcome.stoppedAt) return stored;
  }
  const values = {
    last_uid: Math.max(all.length ? all[all.length - 1] : 0, num(folder.last_uid) || 0),
    backfill_uid: all.length > newest.length ? newest[0] : 0,
  };
  await db.mail_folders.where({ id: folder.id }).update(values);
  Object.assign(folder, values);
  return stored;
}

/** Whether a mirror folder has older mail still to store. */
const backfillPending = (folder) => (num(folder.backfill_uid) || 0) > 1;

/**
 * One step of a mirror folder's backfill (§4: "slow backfill"): the next `limit` messages below
 * `backfill_uid`, newest first, which is selected and locked. Answers how many were stored.
 * The UIDs still to do are read once and kept on the folder object (`backlog`), not searched
 * for again at every step.
 */
async function backfill(env, folder, limit = BATCH) {
  const { client, db } = env;
  requireSelected(env, folder);
  if (!(await checkValidity(env, folder)) || !backfillPending(folder)) return 0;
  const below = num(folder.backfill_uid);
  if (!folder.backlog) {
    folder.backlog = ((await client.search({ uid: `1:${below - 1}` }, { uid: true })) || []).filter((u) => u < below).sort((a, b) => a - b);
  }
  const batch = folder.backlog.slice(-limit);
  let stored = 0;
  if (batch.length) {
    const outcome = await importBatch(env, folder, batch);
    stored = outcome.stored.length;
    if (outcome.stoppedAt) return stored;
    folder.backlog.splice(-batch.length);
  }
  const next = folder.backlog.length ? batch[0] : 0;
  if (!next) delete folder.backlog;
  await db.mail_folders.where({ id: folder.id }).update({ backfill_uid: next });
  folder.backfill_uid = next;
  return stored;
}

// ------------------------------------------------------------------------------ the folder

/** The selected folder's UIDVALIDITY against the stored one: recorded the first time, and the
 * folder reset when it changed. Answers false when it was reset. */
async function checkValidity(env, folder) {
  const mailbox = env.client.mailbox;
  const uidValidity = mailbox && mailbox.uidValidity !== undefined ? Number(mailbox.uidValidity) : null;
  if (uidValidity === null) return true;
  if (folder.uidvalidity === null || folder.uidvalidity === undefined) {
    await env.db.mail_folders.where({ id: folder.id }).update({ uidvalidity: uidValidity });
    folder.uidvalidity = uidValidity;
    return true;
  }
  if (Number(folder.uidvalidity) !== uidValidity) {
    await resetFolder(env, folder, uidValidity);
    return false;
  }
  return true;
}

/**
 * Sync one folder, which is selected and locked. Answers `{ stored, flags, expunged }`.
 *
 * @param {object} env `{ client, db, files, account, log, signal }`
 * @param {object} folder its `mail_folders` row; updated in place
 * @param {{ flags?: boolean }} [opts] also re-read flags and look for expunges
 */
async function syncFolder(env, folder, opts = {}) {
  requireSelected(env, folder);
  await checkValidity(env, folder);
  let stored = 0;
  if (isMirror(env.account) && (folder.backfill_uid === null || folder.backfill_uid === undefined)) {
    stored += await startMirror(env, folder);
  }
  stored += await syncNewMail(env, folder);
  let result = { stored, flags: 0, expunged: 0 };
  if (opts.flags) Object.assign(result, await syncFlags(env, folder));
  return result;
}

module.exports = {
  BATCH,
  FLAG_WINDOW,
  FILE_STORE,
  FILE_FOLDER,
  syncFolder,
  syncNewMail,
  syncFlags,
  backfill,
  backfillPending,
  applyFlags,
  storeMessage,
  fetchAndParse,
  deleteMessages,
  deleteFolder,
  folderRow,
  isMirror,
  bodyCutoff,
  flagValues,
  keywordsOf,
  requireSelected,
  headerNames,
  DEFAULT_HEADERS,
  watchedFolders,
  safeName,
  downloadable,
  bytesOf,
};
