// **The actions** (TODO-imap.md §5.6): `mail_test_account`, `mail_sync_now`, `mail_set_flags`
// and `mail_attachment` for both modes (M8.6); `mail_move` for both; `mail_fetch_body`,
// `mail_delete`, `mail_create_folder`, `mail_delete_folder` and `mail_save_draft` for mirror
// accounts (M9.2); and `mail_send` (§5.7, M9.3).
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
// call and logs out, one such call at a time per account (M9.6). An action that changes the
// server writes the rows **inside** that queued work, so the service's next sync of the folder
// already finds them and does not import the same mail a second time.
//
// Who may do what (§5.6): reading an account's mail is the ownership rule's business, and
// whoever may read it may also set flags, move, delete and file it. Sending, and saving a
// draft, is speaking for the account: a personal account's owner may, and on a shared account
// a user at or above its `send_role` (an admin only, when that is empty). Connecting an account
// to an OAuth2 provider (`mail_oauth_start`) is changing it: its owner, or an admin for a shared
// one.
//
// An OAuth2 account's access token is renewed before the action connects or sends with it, when
// it expires within 5 minutes (`oauth.js` `fresh`).

"use strict";

const crypto = require("node:crypto");
const nodemailer = require("nodemailer");
const MailComposer = require("nodemailer/lib/mail-composer");
const addressparser = require("nodemailer/lib/addressparser");
const { connectionSettings, smtpTarget, smtpOptions, redact, domainOf } = require("./connect.js");
const { withConnection, assignRoles } = require("./account.js");
const { runners } = require("./service.js");
const { FILE_STORE, FILE_FOLDER, safeName, downloadable, storeMessage, fetchAndParse, deleteMessages, deleteFolder, isMirror, headerNames } = require("./sync.js");
const { parse } = require("./parse.js");
const { fresh, isOAuth, consentRequest, providerOf } = require("./oauth.js");

/** A Secret field's value as clients see it (`sc-types` `SECRET_SENTINEL`): "keep what is stored". */
const SECRET_SENTINEL = "••••••••";

/** `mail_test_account` calls per user per minute (§5.6). A failed login costs seconds and can
 * lock an account (M1.4, 4). */
const TEST_LIMIT = 5;
const testCalls = new Map();

/** At most this many messages per `mail_set_flags`, `mail_move` or `mail_delete` call. */
const MAX_IDS = 1000;

/** `mail_send` (§5.7): recipients, attachments and their total size. */
const MAX_RECIPIENTS = 50;
const MAX_ATTACHMENTS = 10;
const MAX_ATTACHMENT_BYTES = 20 * 1024 * 1024;

/** The name a folder of each role gets when an action needs one and the server has none. */
const ROLE_NAMES = { sent: "Sent", drafts: "Drafts", trash: "Trash" };

/** Servers that file what is sent through their SMTP into Sent themselves (§5.7). */
const FILES_SENT_ITSELF = /(^|\.)(gmail\.com|googlemail\.com|office365\.com|outlook\.com)$/i;

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
async function onFolder(account, path, configuration, work) {
  const runner = runners.get(account.id);
  if (runner && runner.client && runner.client.usable) return runner.withFolder(path, work);
  await fresh(account, configuration, moduleDb());
  return withConnection(account, connectionSettings(configuration), path, work);
}

/** Run IMAP work that selects no folder (CREATE, APPEND) on an account's connection. */
async function onAccount(account, configuration, work) {
  const runner = runners.get(account.id);
  if (runner && runner.client && runner.client.usable) return runner.withClient(work);
  await fresh(account, configuration, moduleDb());
  return withConnection(account, connectionSettings(configuration), null, work);
}

/** The module's tables as the sync's `db` chain (`sync.js` writes through it), over v1's
 * `Table`, with no caller: what an action writes after the caller's read is the module's. */
function moduleDb() {
  const chain = (name) => {
    const where = (w, opts = {}) => ({
      first: () => table(name).getRow(w),
      rows: () => table(name).getRows(w, opts),
      select: () => where(w, opts),
      limit: (n) => where(w, { ...opts, limit: n }),
      orderBy: (field) => where(w, { ...opts, orderBy: field }),
      async update(values) {
        const t = table(name);
        const rows = w.id !== undefined && typeof w.id !== "object" ? [{ id: w.id }] : await t.getRows(w);
        for (const r of rows) {
          const error = await t.updateRow(values, r.id);
          if (error) throw new Error(error);
        }
        return { updated: rows.length };
      },
      delete: () => table(name).deleteRows(w),
    });
    return {
      where,
      async insert(values) {
        if (Array.isArray(values)) {
          const out = [];
          for (const v of values) out.push({ ...v, id: await table(name).insertRow(v) });
          return out;
        }
        const id = await table(name).insertRow(values);
        return { ...values, id };
      },
    };
  };
  return {
    mail_accounts: chain("mail_accounts"),
    mail_messages: chain("mail_messages"),
    mail_attachments: chain("mail_attachments"),
    mail_folders: chain("mail_folders"),
  };
}

/** What `sync.js` needs to write rows from an action. */
function moduleEnv(account, files, configuration) {
  const log = Object.assign((...a) => console.log(...a), { info: console.log, warn: console.warn, error: console.error, debug: () => {} });
  return { db: moduleDb(), files, account, log, client: null, extraHeaders: headerNames(configuration) };
}

/** The rows of `ids` the caller may read, those it may not (or that do not exist), and those
 * no longer on the server. */
async function messagesOf(ids, user) {
  const messages = await table("mail_messages").getRows({ id: { in: ids } }, { forUser: user });
  const missing = ids.filter((id) => !messages.some((m) => m.id === id));
  const offServer = messages.filter((m) => m.uid === null || m.uid === undefined || m.folder === null || m.folder === undefined);
  return { messages: messages.filter((m) => !offServer.includes(m)), missing, offServer };
}

/** Messages by account and folder, each group one IMAP command. */
function byFolder(messages) {
  const groups = new Map();
  for (const m of messages) {
    const key = `${m.account}/${m.folder}`;
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(m);
  }
  return [...groups.values()];
}

const folderById = (id) => table("mail_folders").getRow({ id });
const label = (account) => account.label || account.email || `account ${account.id}`;

/** Mirror-only actions refuse an ingest account: ingest keeps every mail (§4). */
function requireMirror(account, what) {
  if (!isMirror(account)) throw new Error(`${what} is for mirror accounts; ${label(account)} is an ingest account, which keeps every mail`);
}

/** Whether the caller may speak for the account (§5.6): its owner, or on a shared account a
 * user at or above `send_role`. */
function requireSender(account, user) {
  if (account.owner === null || account.owner === undefined) {
    const role = account.send_role === null || account.send_role === undefined ? 1 : Number(account.send_role);
    if (!(Number(user.role) <= role)) throw new Error(`your role may not send from the shared account ${label(account)}`);
  } else if (String(account.owner) !== String(user.id)) {
    throw new Error(`only its owner may send from ${label(account)}`);
  }
}

/** The account's folder with `role`, made on the server (and in the table) when it has none and
 * `ROLE_NAMES` names one. */
async function roleFolder(account, role, configuration, files) {
  const found = await table("mail_folders").getRow({ account: account.id, role });
  if (found) return found;
  const name = ROLE_NAMES[role];
  if (!name) throw new Error(`${label(account)} has no ${role} folder`);
  return onAccount(account, configuration, async (client) => {
    const made = await client.mailboxCreate(name).catch((e) => {
      // Already there (but not known as that role): use it.
      if (!/exist/i.test(`${e.message} ${e.responseText || ""}`)) throw e;
      return { path: name };
    });
    const path = (made && made.path) || name;
    const db = moduleDb();
    const existing = await db.mail_folders.where({ account: account.id, path }).first();
    if (existing) {
      await db.mail_folders.where({ id: existing.id }).update({ role });
      return { ...existing, role };
    }
    return db.mail_folders.insert({ account: account.id, path, name: path, role, sync: isMirror(account) });
  });
}

/** A move's target: `role:<role>` or a folder path of the account. */
async function targetFolder(account, target, configuration, files) {
  const role = /^role:(.+)$/i.exec(target);
  if (role) return roleFolder(account, role[1].trim().toLowerCase(), configuration, files);
  const found = await table("mail_folders").getRow({ account: account.id, path: target });
  if (!found) throw new Error(`${label(account)} has no folder ${target}`);
  return found;
}

/** Move one folder's messages, by UID, and record where they went, inside the queued work. A
 * new UID the server does not report (no UIDPLUS) is left empty, and the target folder's next
 * sync matches the mail to its row by Message-ID. */
async function moveGroup(account, from, to, group, configuration) {
  const range = group.map((m) => m.uid).join(",");
  const db = moduleDb();
  await onFolder(account, from.path, configuration, async (client) => {
    const result = await client.messageMove(range, to.path, { uid: true });
    if (!result) throw new Error(`the server refused to move mail from ${from.path} to ${to.path}`);
    const uidMap = result.uidMap instanceof Map ? result.uidMap : new Map();
    for (const m of group) {
      const uid = uidMap.get(Number(m.uid));
      await db.mail_messages.where({ id: m.id }).update({ folder: to.id, uid: uid === undefined ? null : uid });
    }
  });
}

/** Delete one folder's messages for good: `\Deleted` and UID EXPUNGE, then the rows. */
async function expungeGroup(account, from, group, configuration, files) {
  const range = group.map((m) => m.uid).join(",");
  await onFolder(account, from.path, configuration, async (client) => {
    const ok = await client.messageDelete(range, { uid: true });
    if (ok === false) throw new Error(`the server refused to delete mail in ${from.path}`);
    await deleteMessages(moduleEnv(account, files), group.map((m) => m.id));
  });
}

// ------------------------------------------------------------------------------ mail_test_account

/** The fields `mail_test_account` reads from its payload. */
const ACCOUNT_FIELDS = [
  "email", "imap_host", "imap_port", "imap_security", "imap_user", "imap_password",
  "smtp_host", "smtp_port", "smtp_security", "smtp_user", "smtp_password",
];

/** Log in to IMAP and SMTP with settings that may not be saved yet. With `account_id`, a
 * password left empty (or as the sentinel) is the saved one, read as the caller; a saved
 * OAuth2 account logs in with its token. */
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
    if (isOAuth(saved)) {
      for (const key of ["id", "auth", "oauth_provider", "oauth_access_token", "oauth_refresh_token", "oauth_expires_at"]) account[key] = saved[key];
    }
  }
  const settings = connectionSettings(configuration);
  const result = { imap: false, smtp: null, error: null };
  const errors = [];
  try {
    await fresh(account, configuration, moduleDb());
  } catch (e) {
    return { ...result, error: redact(e, account) };
  }
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

/** Wake the service for one account: its folders listed again (a folder made or deleted by
 * another client is seen now), then every folder synced. With `wait`, answers after that sync
 * (up to 60 s). */
async function syncNow({ payload, user }) {
  caller(user);
  const id = idList(payload && payload.account_id, "account_id")[0];
  const account = await readable("mail_accounts", id, user);
  const runner = runners.get(account.id);
  if (!runner) return { queued: false, status: account.status || null, error: account.enabled ? "the account is not running; see its status" : "the account is disabled" };
  const done = runner.settled();
  runner.wake({ discover: true });
  if (payload && payload.wait) {
    let timer;
    await Promise.race([done, new Promise((r) => (timer = setTimeout(r, 60_000)))]);
    clearTimeout(timer);
  }
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

// ------------------------------------------------------------------------------ mail_fetch_body

/** The body of a message stored with its headers only (mirror mode's older mail): fetched,
 * written to the row, and its attachment rows made from the whole parse. */
async function fetchBody({ payload, user, settings: configuration }) {
  caller(user);
  const id = idList(payload && (payload.message_id !== undefined ? payload.message_id : payload.id), "message_id")[0];
  const message = await readable("mail_messages", id, user);
  if (message.body_state !== "headers") return { id, body_state: message.body_state, fetched: false };
  if (message.uid === null || message.uid === undefined || !message.folder) throw new Error("the message is no longer on the server");
  const account = await readable("mail_accounts", message.account, user);
  const folder = await folderById(message.folder);
  const meta = { uid: Number(message.uid), size: message.size, internalDate: message.internal_date };
  const parsed = await onFolder(account, folder.path, configuration, (client) => fetchAndParse(client, meta, { extraHeaders: headerNames(configuration) }));
  if (!parsed) throw new Error("the message is no longer on the server");
  const values = {};
  for (const key of ["body_text", "body_html", "snippet", "has_attachments", "body_state", "import_error", "headers"]) values[key] = parsed.message[key];
  const messages = table("mail_messages");
  const error = await messages.updateRow(values, id);
  if (error) throw new Error(error);
  // The structure's attachment rows become the parse's: the same part keeps its row (and a file
  // stored already), a part only the parse sees (unpacked from a winmail.dat) is added.
  const attachments = table("mail_attachments");
  const rows = await attachments.getRows({ message: id });
  for (const a of parsed.attachments) {
    const same = rows.find((r) => r.part === a.part);
    const row = { part: a.part, filename: a.filename, content_type: a.content_type, size: a.size, cid: a.cid, inline: !!a.inline };
    if (same) {
      const e = await attachments.updateRow(row, same.id);
      if (e) throw new Error(e);
    } else await attachments.insertRow({ message: id, ...row, file: null });
  }
  for (const r of rows) {
    if (!r.file && !parsed.attachments.some((a) => a.part === r.part)) await attachments.deleteRows({ id: r.id });
  }
  return { id, body_state: values.body_state, fetched: true };
}

// ------------------------------------------------------------------------------ mail_move

/** Move messages to `target` (a path, or `role:trash`, `role:archive`, …) of their account. */
async function move({ payload, user, settings: configuration, files }) {
  caller(user);
  const p = payload || {};
  const ids = idList(p.message_ids !== undefined ? p.message_ids : p.message_id, "message_ids");
  const target = String(p.target || "").trim();
  if (!target) throw new Error("say the target folder: a path, or role:trash, role:archive, …");
  const { messages, missing, offServer } = await messagesOf(ids, user);
  const moved = [];
  for (const group of byFolder(messages)) {
    const account = await readable("mail_accounts", group[0].account, user);
    const from = await folderById(group[0].folder);
    const to = await targetFolder(account, target, configuration, files);
    if (to.id !== from.id) await moveGroup(account, from, to, group, configuration);
    moved.push(...group.map((m) => m.id));
  }
  return { moved, missing, not_on_server: offServer.map((m) => m.id) };
}

// ------------------------------------------------------------------------------ mail_delete

/** Delete messages: to the account's Trash, or for good when `permanent` or already there. */
async function remove({ payload, user, settings: configuration, files }) {
  caller(user);
  const p = payload || {};
  const ids = idList(p.message_ids !== undefined ? p.message_ids : p.message_id, "message_ids");
  const { messages, missing, offServer } = await messagesOf(ids, user);
  const trashed = [];
  const deleted = [];
  for (const group of byFolder(messages)) {
    const account = await readable("mail_accounts", group[0].account, user);
    requireMirror(account, "mail_delete");
    const from = await folderById(group[0].folder);
    const trash = p.permanent ? null : await roleFolder(account, "trash", configuration, files);
    if (!trash || trash.id === from.id) {
      await expungeGroup(account, from, group, configuration, files);
      deleted.push(...group.map((m) => m.id));
    } else {
      await moveGroup(account, from, trash, group, configuration);
      trashed.push(...group.map((m) => m.id));
    }
  }
  return { trashed, deleted, missing, not_on_server: offServer.map((m) => m.id) };
}

// ------------------------------------------------------------------------------ mail_create_folder

/** Create a folder on a mirror account's server: `name`, under `parent` (a path) if given. */
async function createFolder({ payload, user, settings: configuration }) {
  caller(user);
  const p = payload || {};
  const account = await readable("mail_accounts", idList(p.account_id, "account_id")[0], user);
  requireMirror(account, "mail_create_folder");
  const name = String(p.name || "").trim();
  if (!name) throw new Error("say the folder's name");
  const folders = table("mail_folders");
  let parent = null;
  if (p.parent) {
    parent = await folders.getRow({ account: account.id, path: String(p.parent) });
    if (!parent) throw new Error(`${label(account)} has no folder ${p.parent}`);
  }
  const delimiter = (parent && parent.delimiter) || ((await folders.getRow({ account: account.id, path: "INBOX" })) || {}).delimiter || "/";
  if (name.includes(delimiter) || /[\u0000-\u001f*%]/.test(name)) throw new Error(`a folder name may not contain ${JSON.stringify(delimiter)}, * or %`);
  const path = parent ? `${parent.path}${delimiter}${name}` : name;
  if (await folders.getRow({ account: account.id, path })) throw new Error(`${label(account)} has a folder ${path} already`);
  const row = await onAccount(account, configuration, async (client) => {
    const made = await client.mailboxCreate(path);
    const created = (made && made.path) || path;
    // Its role by name (a "Sent" made on a server that had none), unless a folder has it.
    const others = await folders.getRows({ account: account.id });
    const role = assignRoles([{ path: created, name, delimiter }]).get(created);
    const taken = role && others.some((f) => f.role === role);
    return moduleDb().mail_folders.insert({ account: account.id, path: created, name, delimiter, role: taken ? null : role || null, sync: true });
  });
  // The service syncs it from its next LIST, which this asks for now.
  const runner = runners.get(account.id);
  if (runner) runner.wake({ discover: true });
  return { id: row.id, path: row.path, role: row.role };
}

// ------------------------------------------------------------------------------ mail_delete_folder

/** Delete a folder of a mirror account: on the server, then its messages (with their stored
 * attachments) and its row. Not INBOX, and not a folder that has folders inside it. */
async function removeFolder({ payload, user, settings: configuration, files }) {
  caller(user);
  const p = payload || {};
  const folders = table("mail_folders");
  let folder;
  if (p.folder_id !== undefined && p.folder_id !== null && p.folder_id !== "") {
    folder = await readable("mail_folders", idList(p.folder_id, "folder_id")[0], user);
  } else {
    const accountId = idList(p.account_id, "account_id")[0];
    const path = String(p.path || "").trim();
    if (!path) throw new Error("say the folder: { folder_id } or { account_id, path }");
    folder = await folders.getRow({ account: accountId, path }, { forUser: user });
    if (!folder) throw new Error(`folder ${path} was not found`);
  }
  const account = await readable("mail_accounts", folder.account, user);
  requireMirror(account, "mail_delete_folder");
  if (folder.role === "inbox" || /^inbox$/i.test(folder.path)) throw new Error("INBOX cannot be deleted");
  const delimiter = folder.delimiter || "/";
  const children = (await folders.getRows({ account: account.id })).filter((f) => f.path.startsWith(folder.path + delimiter));
  if (children.length) throw new Error(`${folder.path} has folders inside it (${children.map((f) => f.path).join(", ")}); delete those first`);
  const runner = runners.get(account.id);
  await onAccount(account, configuration, async (client) => {
    try {
      await client.mailboxDelete(folder.path);
    } catch (e) {
      // Gone from the server already (another client): only the rows are left to delete.
      if (!/nonexistent|not exist|no such|unknown mailbox|doesn't exist/i.test(`${e.message} ${e.responseText || ""}`)) throw e;
    }
    // Inside the queued work, so the service's next pass does not select it.
    if (runner) runner.forgetFolder(folder.path);
    await deleteFolder(moduleEnv(account, files, configuration), folder);
  });
  return { id: folder.id, path: folder.path, deleted: true };
}

// ------------------------------------------------------------------------------ composing

/** A list of addresses from a payload value: a string ("A <a@x>, b@y"), or a list of strings
 * or `{ name, address }`. Each must look like an address. */
function addressList(value, what) {
  if (value === undefined || value === null || value === "") return [];
  const items = Array.isArray(value) ? value : [value];
  const out = [];
  for (const item of items) {
    const parsed = typeof item === "string" ? addressparser(item, { flatten: true }) : [{ name: item.name || "", address: item.address }];
    for (const a of parsed) {
      const address = String(a.address || "").trim();
      // A domain with a dot, or `localhost` (a local test server).
      if (!/^[^\s@"<>(),;:\\]+@([^\s@"<>(),;:\\]+\.[^\s@"<>(),;:\\]+|localhost)$/i.test(address)) {
        throw new Error(`${what}: ${JSON.stringify(a.address || item)} is not a mail address`);
      }
      out.push({ name: String(a.name || "").trim(), address });
    }
  }
  return out;
}

/** The files a message carries, read as the caller (§5.7): `{ store, path, filename? }`, a file
 * of the application's that the caller may read, or `{ attachment_id }`, a mail attachment the
 * caller may read (forwarding). At most 10, 20 MB in all. */
async function attachmentsOf(list, { user, callerFiles, files, configuration }) {
  const items = list === undefined || list === null || list === "" ? [] : Array.isArray(list) ? list : [list];
  if (items.length > MAX_ATTACHMENTS) throw new Error(`at most ${MAX_ATTACHMENTS} attachments`);
  const out = [];
  let total = 0;
  const count = (n) => {
    total += n;
    if (total > MAX_ATTACHMENT_BYTES) throw new Error(`the attachments come to more than ${MAX_ATTACHMENT_BYTES / 1024 / 1024} MB`);
  };
  for (const item of items) {
    if (item && item.attachment_id !== undefined) {
      const att = await readable("mail_attachments", Number(item.attachment_id), user);
      count(Number(att.size) || 0);
      const { file } = await attachment({ payload: { attachment_id: att.id }, user, settings: configuration, files });
      const content = await files.read(FILE_STORE, file);
      count(content.length - (Number(att.size) || 0));
      out.push({ filename: item.filename || att.filename, content, contentType: att.content_type || undefined, cid: item.cid || undefined });
      continue;
    }
    const store = item && item.store;
    const path = item && item.path;
    if (!store || !path) throw new Error("an attachment is { store, path } (a file of the application's) or { attachment_id }");
    if (!callerFiles) throw new Error("this server cannot read the application's files for an action");
    const stat = await callerFiles.stat(store, path);
    if (!stat || stat.isDirectory) throw new Error(`${store}/${path} is not a file`);
    count(Number(stat.size) || 0);
    const content = await callerFiles.read(store, path);
    out.push({
      filename: item.filename || String(path).split("/").pop(),
      content,
      contentType: item.content_type || stat.mimeType || undefined,
      cid: item.cid || undefined,
    });
  }
  return out;
}

/** iTIP methods (RFC 5546) a `calendar` part may carry. */
const ITIP_METHODS = new Set(["PUBLISH", "REQUEST", "REPLY", "ADD", "CANCEL", "REFRESH", "COUNTER", "DECLINECOUNTER"]);
const MAX_CALENDAR_BYTES = 1024 * 1024;

/** A payload's `calendar` as nodemailer's `icalEvent`: a `text/calendar; method=…` alternative
 * next to the text and HTML (and an .ics attachment), which is how a mail client shows an
 * invitation with Accept and Decline. `content` is the whole VCALENDAR, its METHOD the same. */
function calendarOf(calendar) {
  if (calendar === undefined || calendar === null || calendar === "") return undefined;
  const method = String(calendar.method || "").trim().toUpperCase();
  if (!ITIP_METHODS.has(method)) throw new Error(`calendar.method must be one of ${[...ITIP_METHODS].join(", ")}`);
  const content = String(calendar.content || "");
  if (!/BEGIN:VCALENDAR/i.test(content)) throw new Error("calendar.content must be an iCalendar object (BEGIN:VCALENDAR …)");
  if (Buffer.byteLength(content) > MAX_CALENDAR_BYTES) throw new Error("calendar.content is larger than 1 MB");
  const declared = /^METHOD:(.+)$/im.exec(content);
  if (declared && declared[1].trim().toUpperCase() !== method) {
    throw new Error(`calendar.method is ${method}, but the calendar says METHOD:${declared[1].trim()}`);
  }
  return { method, content, filename: calendar.filename ? String(calendar.filename) : "invite.ics" };
}

/**
 * The message `mail_send` and `mail_save_draft` build (§5.7): From the account, a Message-ID on
 * its domain, the threading headers of the message answered, and the attachments.
 */
async function compose(account, p, { user, callerFiles, files, configuration, draft = false }) {
  const to = addressList(p.to, "to");
  const cc = addressList(p.cc, "cc");
  const bcc = addressList(p.bcc, "bcc");
  const count = to.length + cc.length + bcc.length;
  if (!draft && !count) throw new Error("say who the message is to");
  if (count > MAX_RECIPIENTS) throw new Error(`at most ${MAX_RECIPIENTS} recipients`);
  const address = String(account.email || account.smtp_user || "").trim();
  const domain = domainOf(address);
  if (!domain) throw new Error(`${label(account)} has no email address to send from`);
  let original = null;
  let inReplyTo;
  let references;
  if (p.in_reply_to !== undefined && p.in_reply_to !== null && p.in_reply_to !== "") {
    original = await readable("mail_messages", idList(p.in_reply_to, "in_reply_to")[0], user);
    if (original.message_id) {
      inReplyTo = original.message_id;
      let refs = Array.isArray(original.references) ? original.references : typeof original.references === "string" ? JSON.parse(original.references || "[]") : [];
      references = [...refs.filter((r) => r !== original.message_id), original.message_id];
    }
  }
  const mail = {
    from: { name: account.display_name || "", address },
    to,
    cc,
    bcc,
    subject: String(p.subject || ""),
    text: p.text === undefined || p.text === null ? undefined : String(p.text),
    html: p.html === undefined || p.html === null ? undefined : String(p.html),
    messageId: `<${crypto.randomUUID()}@${domain}>`,
    date: new Date(),
    inReplyTo,
    references,
    attachments: await attachmentsOf(p.attachments, { user, callerFiles, files, configuration }),
    icalEvent: calendarOf(p.calendar),
  };
  return { mail, original };
}

/** A message's source as a copy keeps it: with its Bcc, which the copy sent leaves out. */
async function sourceOf(mail) {
  const node = new MailComposer(mail).compile();
  node.keepBcc = true;
  return node.build();
}

/** APPEND `source` to `folder` with `flags`, and store its row, on the account's connection.
 * Answers `{ uid, row }`; `row` is null when the server does not say the UID (no UIDPLUS): the
 * folder's next sync stores it then. */
async function appendCopy(account, folder, source, flags, date, configuration, files) {
  return onAccount(account, configuration, async (client) => {
    const appended = await client.append(folder.path, source, flags, date);
    if (!appended) throw new Error(`the server refused to save it in ${folder.path}`);
    if (!appended.uid) return { uid: null, row: null };
    const parsed = await parse(source, { flags, size: source.length, internalDate: date, extraHeaders: headerNames(configuration) });
    const env = moduleEnv(account, files, configuration);
    let row;
    try {
      row = await storeMessage(env, folder, { uid: appended.uid }, parsed, null);
    } catch (e) {
      // Stored by the service in the meantime.
      row = await env.db.mail_messages.where({ folder: folder.id, uid: appended.uid }).first();
      if (!row) throw e;
    }
    return { uid: appended.uid, row };
  });
}

/** Delete a draft for good, on the server and its row; answers whether it was one. */
async function removeDraft(id, account, user, configuration, files) {
  const draft = await readable("mail_messages", id, user);
  if (draft.account !== account.id) throw new Error(`message ${id} is not a draft of ${label(account)}`);
  const folder = draft.folder ? await folderById(draft.folder) : null;
  if (!draft.draft && !(folder && folder.role === "drafts")) throw new Error(`message ${id} is not a draft`);
  if (draft.uid === null || draft.uid === undefined || !folder) {
    await deleteMessages(moduleEnv(account, files), [id]);
  } else {
    await expungeGroup(account, folder, [draft], configuration, files);
  }
  return true;
}

// ------------------------------------------------------------------------------ mail_save_draft

/** Save a draft in the Drafts folder (`\Draft`, `\Seen`), and delete the one it `replaces`. */
async function saveDraft({ payload, user, settings: configuration, files, callerFiles }) {
  caller(user);
  const p = payload || {};
  const account = await readable("mail_accounts", idList(p.account_id, "account_id")[0], user);
  requireMirror(account, "mail_save_draft");
  requireSender(account, user);
  const { mail } = await compose(account, p, { user, callerFiles, files, configuration, draft: true });
  const source = await sourceOf(mail);
  const drafts = await roleFolder(account, "drafts", configuration, files);
  const { uid, row } = await appendCopy(account, drafts, source, ["\\Draft", "\\Seen"], mail.date, configuration, files);
  let replaced = null;
  if (p.replaces !== undefined && p.replaces !== null && p.replaces !== "") {
    replaced = idList(p.replaces, "replaces")[0];
    await removeDraft(replaced, account, user, configuration, files);
  }
  return { id: row ? row.id : null, uid, message_id: mail.messageId, replaced };
}

// ------------------------------------------------------------------------------ mail_send

/** Whether a copy of what is sent is APPENDed to Sent: the account's `save_sent`, or, when that
 * is empty, unless the server files it there itself (§5.7). */
function savesSent(account) {
  if (account.save_sent === true || account.save_sent === false) return account.save_sent;
  return ![account.smtp_host, account.imap_host].some((h) => FILES_SENT_ITSELF.test(String(h || "").trim()));
}

/** Send a message from an account (§5.7): SMTP, then the copy in Sent, the answered flag on
 * the message replied to, and the draft it came from deleted. A failed copy does not fail the
 * send; it is reported as `save_error`. */
async function send({ payload, user, settings: configuration, files, callerFiles }) {
  caller(user);
  const p = payload || {};
  const account = await readable("mail_accounts", idList(p.account_id, "account_id")[0], user);
  requireSender(account, user);
  if (!account.smtp_host) throw new Error(`${label(account)} has no SMTP server`);
  const { mail, original } = await compose(account, p, { user, callerFiles, files, configuration });
  const source = await sourceOf(mail);

  const settings = connectionSettings(configuration);
  await fresh(account, configuration, moduleDb());
  const transport = nodemailer.createTransport(smtpOptions(account, await smtpTarget(account, settings), settings));
  let info;
  try {
    info = await transport.sendMail(mail);
  } catch (e) {
    throw new Error(`sending through ${account.smtp_host} failed: ${redact(e, account)}`);
  } finally {
    transport.close();
  }
  const result = {
    message_id: mail.messageId,
    id: null,
    saved: false,
    save_error: null,
    accepted: (info.accepted || []).map(String),
    rejected: (info.rejected || []).map(String),
  };

  if (savesSent(account)) {
    try {
      const sent = await roleFolder(account, "sent", configuration, files);
      const { row } = await appendCopy(account, sent, source, ["\\Seen"], mail.date, configuration, files);
      result.saved = true;
      result.id = row ? row.id : null;
    } catch (e) {
      result.save_error = redact(e, account);
    }
  }
  const problems = [];
  if (original && original.uid !== null && original.uid !== undefined && original.folder) {
    try {
      const repliedAccount = original.account === account.id ? account : await readable("mail_accounts", original.account, user);
      const folder = await folderById(original.folder);
      await onFolder(repliedAccount, folder.path, configuration, (client) => client.messageFlagsAdd(String(original.uid), ["\\Answered"], { uid: true }));
      const e = await table("mail_messages").updateRow({ answered: true }, original.id);
      if (e) throw new Error(e);
    } catch (e) {
      problems.push(`could not mark the message answered: ${redact(e, account)}`);
    }
  }
  if (p.draft_id !== undefined && p.draft_id !== null && p.draft_id !== "") {
    try {
      await removeDraft(idList(p.draft_id, "draft_id")[0], account, user, configuration, files);
      result.draft_deleted = true;
    } catch (e) {
      result.draft_deleted = false;
      problems.push(`could not delete the draft: ${redact(e, account)}`);
    }
  }
  if (problems.length) result.warnings = problems;
  return result;
}

// ------------------------------------------------------------------------------ mail_oauth_start

/** Connect an account to its OAuth2 provider (`oauth.js`): the consent flow's address, where
 * the application sends the browser. The account's owner, or an admin for a shared account. */
async function oauthStart({ payload, user, settings: configuration, oauth }) {
  caller(user);
  const p = payload || {};
  const account = await readable("mail_accounts", idList(p.account_id, "account_id")[0], user);
  const shared = account.owner === null || account.owner === undefined;
  if (shared ? Number(user.role) !== 1 : String(account.owner) !== String(user.id)) {
    throw new Error(shared ? `only an admin may connect the shared account ${label(account)}` : `only its owner may connect ${label(account)}`);
  }
  if (!oauth) throw new Error("this server cannot run a consent flow for a module");
  const provider = String(p.provider || "").trim().toLowerCase() || providerOf(account);
  if (!provider) throw new Error(`say which provider ${label(account)} is with: microsoft or google`);
  const { url } = await oauth.begin(consentRequest(account, provider, configuration, p.return_to));
  return { url, provider };
}

module.exports = {
  actions: {
    mail_oauth_start: oauthStart,
    mail_test_account: testAccount,
    mail_sync_now: syncNow,
    mail_set_flags: setFlags,
    mail_attachment: attachment,
    mail_fetch_body: fetchBody,
    mail_move: move,
    mail_delete: remove,
    mail_create_folder: createFolder,
    mail_delete_folder: removeFolder,
    mail_save_draft: saveDraft,
    mail_send: send,
  },
  SECRET_SENTINEL,
  savesSent,
  addressList,
  calendarOf,
};
