// **The ingest-mode actions** (TODO-imap.md §5.6, M8.6): `mail_test_account`, `mail_sync_now`,
// `mail_set_flags` and `mail_attachment`.
//
// Each is exposed by the application as a trigger with event None, and called with
// `{ payload, user }`. **Each loads what it touches as the caller first**, through v1's `Table`
// with `forUser`, so the tables' ownership rules decide (§3): a message the caller may not read
// is "not found", exactly as a row that does not exist. Only then does it read the account's
// secrets and talk to the server. Its writes to the rows are the module's own afterwards: the
// flags, the stored file.
//
// `settings` is the module's configuration (`index.js` passes it; the argument's own
// `configuration` is the trigger's).
//
// The IMAP work queues on the account's running connection (`service.js` `runners`) when the
// service runs it, so the account never has a second connection; otherwise it opens one for the
// call and logs out.

"use strict";

const nodemailer = require("nodemailer");
const { connectionSettings, smtpTarget, smtpOptions, redact } = require("./connect.js");
const { withConnection } = require("./account.js");
const { runners } = require("./service.js");
const { FILE_STORE, FILE_FOLDER, safeName, downloadable } = require("./sync.js");
const { parse } = require("./parse.js");

/** A Secret field's value as clients see it (`sc-types` `SECRET_SENTINEL`): "keep what is stored". */
const SECRET_SENTINEL = "••••••••";

/** `mail_test_account` calls per user per minute (§5.6). A failed login costs seconds and can
 * lock an account (M1.4, 4). */
const TEST_LIMIT = 5;
const testCalls = new Map();

/** At most this many messages per `mail_set_flags` call. */
const MAX_IDS = 1000;

/** v1's `Table`, answered by the host (tutorial-modules Step 6). Required when first used, so the
 * module's other files load under plain Node for their tests. */
const table = (name) => {
  const Table = require("@saltcorn/data/models/table");
  const found = Table.findOne({ name });
  if (!found) throw new Error(`the table ${name} is missing; reinstall the mail module`);
  return found;
};

/** The caller, or a refusal: these actions act for somebody. */
function caller(user) {
  if (!user || user.id === undefined || user.id === null) throw new Error("sign in to use the mail actions");
  return user;
}

/** The ids in a payload value: one id, a list, or a comma-separated string. */
function idList(value, what) {
  const list = (Array.isArray(value) ? value : value === undefined || value === null ? [] : String(value).split(","))
    .map((v) => Number(v))
    .filter((v) => Number.isInteger(v) && v > 0);
  if (!list.length) throw new Error(`${what} is required`);
  if (list.length > MAX_IDS) throw new Error(`at most ${MAX_IDS} ${what} at once`);
  return [...new Set(list)];
}

/** One row the caller may read, or "not found". */
async function readable(name, id, user) {
  const row = await table(name).getRow({ id }, { forUser: user });
  if (!row) throw new Error(`${name.replace(/^mail_/, "").replace(/s$/, "")} ${id} was not found`);
  return row;
}

/** Run IMAP work on `path` of an account: on its running connection, or on one of its own. */
function onFolder(account, path, configuration, work) {
  const runner = runners.get(account.id);
  if (runner && runner.client && runner.client.usable) return runner.withFolder(path, work);
  return withConnection(account, connectionSettings(configuration), path, work);
}

// ------------------------------------------------------------------------------ mail_test_account

/** The fields `mail_test_account` reads from its payload. */
const ACCOUNT_FIELDS = [
  "email", "imap_host", "imap_port", "imap_security", "imap_user", "imap_password",
  "smtp_host", "smtp_port", "smtp_security", "smtp_user", "smtp_password",
];

/** Log in to IMAP and SMTP with settings that may not be saved yet. With `account_id`, a
 * password left empty (or as the sentinel) is the saved one, read as the caller. */
async function testAccount({ payload, user, settings: configuration }) {
  caller(user);
  const now = Date.now();
  const recent = (testCalls.get(user.id) || []).filter((t) => now - t < 60_000);
  if (recent.length >= TEST_LIMIT) throw new Error(`at most ${TEST_LIMIT} account tests a minute; wait a little and try again`);
  recent.push(now);
  testCalls.set(user.id, recent);

  const p = payload || {};
  const account = {};
  for (const key of ACCOUNT_FIELDS) if (p[key] !== undefined) account[key] = p[key];
  if (p.account_id) {
    const saved = await readable("mail_accounts", Number(p.account_id), user);
    for (const key of ACCOUNT_FIELDS) {
      const given = account[key];
      if (given === undefined || given === null || given === "" || given === SECRET_SENTINEL) account[key] = saved[key];
    }
  }
  const settings = connectionSettings(configuration);
  const result = { imap: false, smtp: null, error: null };
  const errors = [];
  try {
    await withConnection(account, settings, null, async () => {});
    result.imap = true;
  } catch (e) {
    errors.push(`IMAP: ${redact(e, account)}`);
  }
  if (account.smtp_host) {
    result.smtp = false;
    let transport;
    try {
      transport = nodemailer.createTransport(smtpOptions(account, await smtpTarget(account, settings), settings));
      await transport.verify();
      result.smtp = true;
    } catch (e) {
      errors.push(`SMTP: ${redact(e, account)}`);
    } finally {
      if (transport) transport.close();
    }
  }
  if (errors.length) result.error = errors.join("; ");
  return result;
}

// ------------------------------------------------------------------------------ mail_sync_now

/** Wake the service for one account. With `wait`, answers after that sync (up to 60 s). */
async function syncNow({ payload, user }) {
  caller(user);
  const id = idList(payload && payload.account_id, "account_id")[0];
  const account = await readable("mail_accounts", id, user);
  const runner = runners.get(account.id);
  if (!runner) return { queued: false, status: account.status || null, error: account.enabled ? "the account is not running; see its status" : "the account is disabled" };
  const done = runner.settled();
  runner.wake();
  if (payload && payload.wait) await Promise.race([done, new Promise((r) => setTimeout(r, 60_000))]);
  return { queued: true, status: runner.state };
}

// ------------------------------------------------------------------------------ mail_set_flags

/** Set `seen` and/or `flagged` on messages: on the server, by UID, then on the rows. */
async function setFlags({ payload, user, settings: configuration }) {
  caller(user);
  const p = payload || {};
  const ids = idList(p.message_ids !== undefined ? p.message_ids : p.message_id, "message_ids");
  const change = {};
  for (const key of ["seen", "flagged"]) if (typeof p[key] === "boolean") change[key] = p[key];
  if (!Object.keys(change).length) throw new Error("say seen: true|false and/or flagged: true|false");

  const messages = await table("mail_messages").getRows({ id: { in: ids } }, { forUser: user });
  const missing = ids.filter((id) => !messages.some((m) => m.id === id));
  const offServer = messages.filter((m) => m.uid === null || m.uid === undefined || m.folder === null);
  // By account and folder: one STORE per folder and flag.
  const groups = new Map();
  for (const m of messages) {
    if (offServer.includes(m)) continue;
    const key = `${m.account}/${m.folder}`;
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(m);
  }
  const names = { seen: "\\Seen", flagged: "\\Flagged" };
  const updated = [];
  for (const group of groups.values()) {
    const account = await readable("mail_accounts", group[0].account, user);
    const folder = await table("mail_folders").getRow({ id: group[0].folder });
    const range = group.map((m) => m.uid).join(",");
    await onFolder(account, folder.path, configuration, async (client) => {
      for (const [key, value] of Object.entries(change)) {
        const ok = value
          ? await client.messageFlagsAdd(range, [names[key]], { uid: true })
          : await client.messageFlagsRemove(range, [names[key]], { uid: true });
        if (ok === false) throw new Error(`the server refused to change ${key} in ${folder.path}`);
      }
    });
    const rows = table("mail_messages");
    for (const m of group) {
      const error = await rows.updateRow(change, m.id);
      if (error) throw new Error(error);
      updated.push(m.id);
    }
  }
  return { updated, missing, not_on_server: offServer.map((m) => m.id) };
}

// ------------------------------------------------------------------------------ mail_attachment

/** The File path of an attachment, storing it first if it is not stored yet. */
async function attachment({ payload, user, settings: configuration, files }) {
  caller(user);
  const id = idList(payload && (payload.attachment_id !== undefined ? payload.attachment_id : payload.id), "attachment_id")[0];
  const att = await readable("mail_attachments", id, user);
  if (att.file) return { file: att.file, stored: false };
  const message = await readable("mail_messages", att.message, user);
  if (message.uid === null || message.uid === undefined || !message.folder) {
    throw new Error("the message is no longer on the server, and its attachment was not stored");
  }
  const account = await readable("mail_accounts", message.account, user);
  const folder = await table("mail_folders").getRow({ id: message.folder });
  const name = safeName(att.filename, `part-${att.part}`);
  const path = `${FILE_FOLDER}/${message.id}/${att.id}-${name}`;
  const write = (content) =>
    files.write(FILE_STORE, path, content, { contentType: att.content_type || "application/octet-stream", filename: att.filename || name });
  const file = await onFolder(account, folder.path, configuration, async (client) => {
    if (downloadable(att.part)) {
      const { content } = await client.download(String(message.uid), att.part, { uid: true });
      if (!content) throw new Error(`the server has no part ${att.part} in that message`);
      return write(content);
    }
    // Unpacked from inside another part (a winmail.dat, an encoded .eml): parse the source again.
    const fetched = await client.fetchOne(String(message.uid), { uid: true, source: true }, { uid: true });
    if (!fetched || !fetched.source) throw new Error("the server has no such message any more");
    const parsed = await parse(fetched.source, {});
    const found = parsed.attachments.find((a) => a.part === att.part);
    if (!found || !found.content) throw new Error(`no attachment ${att.part} in that message`);
    return write(found.content);
  });
  const error = await table("mail_attachments").updateRow({ file }, att.id);
  if (error) throw new Error(error);
  return { file, stored: true };
}

module.exports = {
  actions: {
    mail_test_account: testAccount,
    mail_sync_now: syncNow,
    mail_set_flags: setFlags,
    mail_attachment: attachment,
  },
  SECRET_SENTINEL,
};
