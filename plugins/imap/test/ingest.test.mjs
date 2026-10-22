// Ingest mode against a real Dovecot (TODO-imap.md M8.2–M8.5, M8.7): `account.js`, `sync.js` and
// `service.js` over an in-memory `ctx` (fake-host.mjs). Skipped unless the test Dovecot runs:
//
//   test/servers/dovecot.sh start && npm test
//
// The same module on the real host, with ownership and REST, is crates/sc-module/tests/
// imap_ingest.rs.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { fakeCtx, fakeDb, fakeFiles, fakeLog } from "./fake-host.mjs";

const require = createRequire(import.meta.url);
const { ImapFlow } = require("imapflow");
const { AccountRunner } = require("../account.js");
const { connectionSettings } = require("../connect.js");
const service = require("../service.js");

const here = dirname(fileURLToPath(import.meta.url));
const DOVECOT = join(here, "servers", "dovecot.sh");
const CA = join(here, "servers", "run", "certs", "ca.crt");
const PASSWORD = "Grüße-€-pässwort";

const up = (() => {
  try {
    execFileSync(DOVECOT, ["running"], { stdio: "ignore" });
    return existsSync(CA);
  } catch {
    return false;
  }
})();
const skip = up ? false : "the test Dovecot is not running (test/servers/dovecot.sh start)";

const configuration = () => ({
  trusted_servers: "127.0.0.1:11993\n127.0.0.1:11143",
  ca_certificates: readFileSync(CA, "utf8"),
});
const settings = () => connectionSettings(configuration());

/** Fast backoff, so a reconnect is seconds, not minutes. */
const FAST = { backoffFirstMs: 300, backoffMaxMs: 1000, settleMs: 200, pollMs: 60_000 };

let nextAccountId = 1;
function accountRow(extra = {}) {
  return {
    id: nextAccountId++,
    label: "orders",
    email: "ingest@localhost",
    mode: "ingest",
    imap_host: "127.0.0.1",
    imap_port: 11993,
    imap_security: "tls",
    imap_user: "ingest",
    imap_password: PASSWORD,
    folders: ["INBOX"],
    processed_folder: null,
    error_folder: null,
    start_from: null,
    enabled: true,
    ...extra,
  };
}

/** A client of our own, as another mail program or the mail server delivering. */
async function admin(user = "ingest") {
  const client = new ImapFlow({
    host: "127.0.0.1",
    port: 11993,
    secure: true,
    auth: { user, pass: PASSWORD },
    tls: { ca: readFileSync(CA, "utf8") },
    logger: false,
  });
  client.on("error", () => {});
  await client.connect();
  return client;
}

/** Empty every mailbox the tests use, and remove the ones they create. */
async function reset() {
  const c = await admin();
  try {
    for (const box of await c.list()) {
      if (box.path === "Orders" || box.path === "Made") {
        await c.mailboxDelete(box.path);
        continue;
      }
      const lock = await c.getMailboxLock(box.path);
      try {
        if (c.mailbox.exists) await c.messageDelete("1:*");
      } finally {
        lock.release();
      }
    }
  } finally {
    await c.logout();
  }
}

let serial = 0;
function mail({ subject, body = "hello", messageId, attachment, date } = {}) {
  serial += 1;
  const id = messageId || `<t${Date.now()}.${serial}@test.example>`;
  const head = [
    "From: Shop <shop@example.com>",
    "To: orders@example.com",
    `Subject: ${subject || `mail ${serial}`}`,
    `Message-ID: ${id}`,
    `Date: ${(date || new Date()).toUTCString()}`,
    "MIME-Version: 1.0",
  ];
  if (!attachment) return Buffer.from([...head, "Content-Type: text/plain; charset=utf-8", "", body, ""].join("\r\n"));
  return Buffer.from(
    [
      ...head,
      'Content-Type: multipart/mixed; boundary="b1"',
      "",
      "--b1",
      "Content-Type: text/plain; charset=utf-8",
      "",
      body,
      "--b1",
      `Content-Type: application/octet-stream; name="${attachment.name}"`,
      `Content-Disposition: attachment; filename="${attachment.name}"`,
      "Content-Transfer-Encoding: base64",
      "",
      Buffer.from(attachment.data).toString("base64"),
      "--b1--",
      "",
    ].join("\r\n"),
  );
}

async function append(folder, sources, opts = {}) {
  const c = await admin();
  try {
    for (const s of sources) await c.append(folder, s, opts.flags || [], opts.date);
  } finally {
    await c.logout();
  }
}

async function until(what, ready, ms = 15_000) {
  const start = Date.now();
  for (;;) {
    const value = await ready();
    if (value) return value;
    if (Date.now() - start > ms) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

function runnerFor(account, fake = fakeDb(), files = fakeFiles(), options = {}) {
  fake.tables.mail_accounts.push({ ...account });
  const log = fakeLog();
  const runner = new AccountRunner({ db: fake.db, files, log, settings: settings() }, account, { ...FAST, ...options });
  return { runner, ...fake, files, log };
}

const messagesIn = (tables, folderPath) => {
  const folder = tables.mail_folders.find((f) => f.path === folderPath);
  return folder ? tables.mail_messages.filter((m) => m.folder === folder.id) : [];
};
const accountOf = (tables, id) => tables.mail_accounts.find((a) => a.id === id);

before(async () => {
  if (up) await reset();
});

test("folders are found with their roles, and an existing mailbox of 1,000 mails is stored once each", { skip, timeout: 180_000 }, async () => {
  await reset();
  const sources = [];
  for (let i = 0; i < 1000; i++) {
    sources.push(i % 100 === 0 ? mail({ subject: `order ${i}`, attachment: { name: `order-${i}.pdf`, data: `%PDF-${i}` } }) : mail({ subject: `mail ${i}` }));
  }
  await append("INBOX", sources);
  const account = accountRow();
  const { runner, tables, events, files } = runnerFor(account);
  const started = Date.now();
  runner.start();
  try {
    await until("1,000 messages", () => tables.mail_messages.length >= 1000 && runner.state === "idle", 150_000);
    console.log(`1,000 mails ingested in ${Date.now() - started} ms`);
    // Roles from SPECIAL-USE, then names.
    const role = (p) => tables.mail_folders.find((f) => f.path === p)?.role;
    assert.equal(role("INBOX"), "inbox");
    assert.equal(role("Sent"), "sent");
    assert.equal(role("Drafts"), "drafts");
    assert.equal(role("Trash"), "trash");
    assert.equal(role("Junk"), "spam");
    assert.equal(role("Processed"), null);
    const inbox = tables.mail_folders.find((f) => f.path === "INBOX");
    assert.equal(inbox.sync, true);
    assert.equal(tables.mail_folders.find((f) => f.path === "Sent").sync, false);

    const rows = messagesIn(tables, "INBOX");
    assert.equal(rows.length, 1000);
    assert.equal(new Set(rows.map((r) => r.uid)).size, 1000);
    assert.equal(inbox.last_uid, Math.max(...rows.map((r) => r.uid)));
    assert.ok(inbox.uidvalidity > 0);
    assert.equal(inbox.total, 1000);
    assert.ok(rows.every((r) => r.body_state === "full" && !r.import_error && r.account === account.id));
    const order = rows.find((r) => r.subject === "order 500");
    assert.equal(order.has_attachments, true);
    const att = tables.mail_attachments.filter((a) => a.message === order.id);
    assert.equal(att.length, 1);
    assert.equal(att[0].filename, "order-500.pdf");
    assert.equal(files.stored.get(att[0].file).bytes.toString(), "%PDF-500");
    assert.equal(tables.mail_attachments.length, 10);

    // Two steps per message: inserted as `importing`, then finished after the attachment rows.
    const inserts = events.filter((e) => e.table === "mail_messages" && e.op === "insert");
    assert.ok(inserts.every((e) => e.row.body_state === "importing"));
    const finished = events.filter((e) => e.table === "mail_messages" && e.op === "update" && e.old.body_state === "importing" && e.row.body_state === "full");
    assert.equal(finished.length, 1000);
    const finishOrder = events.findIndex((e) => e.op === "update" && e.row.id === order.id && e.old.body_state === "importing");
    const attInsert = events.findIndex((e) => e.table === "mail_attachments" && e.row?.message === order.id);
    assert.ok(attInsert >= 0 && attInsert < finishOrder, "the attachment rows are there when the message is finished");

    const acc = accountOf(tables, account.id);
    assert.equal(acc.status, "idle");
    assert.equal(acc.last_error, null);
    assert.ok(acc.last_sync_at);
  } finally {
    await runner.stop();
  }
});

test("new mail during IDLE appears within 5 s", { skip, timeout: 60_000 }, async () => {
  await reset();
  const { runner, tables } = runnerFor(accountRow());
  runner.start();
  try {
    await until("idle", () => runner.state === "idle");
    // Let the connection settle into IDLE.
    await new Promise((r) => setTimeout(r, 1500));
    for (const n of [1, 2]) {
      const sent = Date.now();
      await append("INBOX", [mail({ subject: `live ${n}` })]);
      await until(`live ${n}`, () => tables.mail_messages.some((m) => m.subject === `live ${n}` && m.body_state === "full"), 5_000);
      console.log(`live ${n} stored ${Date.now() - sent} ms after delivery`);
    }
  } finally {
    await runner.stop();
  }
});

test("a server restart during a batch loses and duplicates nothing", { skip, timeout: 120_000 }, async () => {
  await reset();
  await append("INBOX", Array.from({ length: 400 }, (_, i) => mail({ subject: `r${i}` })));
  const { runner, tables, events } = runnerFor(accountRow());
  runner.start();
  try {
    await until("the first batch", () => tables.mail_messages.length >= 60, 30_000);
    execFileSync(DOVECOT, ["restart"], { stdio: "ignore" });
    const atRestart = tables.mail_messages.length;
    assert.ok(atRestart < 400, `the restart came during the import (${atRestart} stored)`);
    await until("every message after the restart", () => tables.mail_messages.length >= 400 && runner.state === "idle", 90_000);
    const subjects = tables.mail_messages.map((m) => m.subject);
    assert.equal(subjects.length, 400);
    assert.equal(new Set(subjects).size, 400, "no duplicates");
    assert.ok(tables.mail_messages.every((m) => m.body_state === "full"));
    const statuses = events.filter((e) => e.table === "mail_accounts").map((e) => e.row.status);
    assert.ok(statuses.includes("error"), `it reconnected: ${statuses.join(" ")}`);
    assert.ok(statuses.filter((s) => s === "connecting").length >= 2);
  } finally {
    await runner.stop();
  }
});

test("a UIDVALIDITY reset matches mail again by Message-ID and deletes nothing", { skip, timeout: 60_000 }, async () => {
  await reset();
  let c = await admin();
  await c.mailboxCreate("Orders");
  await c.logout();
  const ids = ["<a@orders>", "<b@orders>", "<c@orders>"];
  await append("Orders", ids.map((messageId, i) => mail({ subject: `order ${i}`, messageId })));
  const account = accountRow({ folders: ["INBOX", "Orders"] });
  const { runner, tables } = runnerFor(account);
  runner.start();
  try {
    await until("three orders", () => messagesIn(tables, "Orders").length === 3 && runner.state === "idle");
    const before = new Map(messagesIn(tables, "Orders").map((m) => [m.message_id, m]));
    const validity = tables.mail_folders.find((f) => f.path === "Orders").uidvalidity;

    // Recreated: a new UIDVALIDITY. Two old mails come back, one new one arrives.
    await new Promise((r) => setTimeout(r, 1100)); // Dovecot's UIDVALIDITY is a timestamp
    c = await admin();
    await c.mailboxDelete("Orders");
    await c.mailboxCreate("Orders");
    await c.logout();
    await append("Orders", [mail({ subject: "order 1 again", messageId: ids[1] }), mail({ subject: "order 0 again", messageId: ids[0] }), mail({ subject: "order 3", messageId: "<d@orders>" })]);
    runner.wake();
    await until("the new order", () => messagesIn(tables, "Orders").some((m) => m.message_id === "<d@orders>") && runner.state === "idle");

    const folder = tables.mail_folders.find((f) => f.path === "Orders");
    assert.notEqual(folder.uidvalidity, validity);
    const rows = messagesIn(tables, "Orders");
    assert.equal(rows.length, 4, "nothing deleted, nothing imported twice");
    const byId = new Map(rows.map((m) => [m.message_id, m]));
    // Matched: the same rows, with their new UIDs.
    assert.equal(byId.get(ids[0]).id, before.get(ids[0]).id);
    assert.equal(byId.get(ids[1]).id, before.get(ids[1]).id);
    assert.ok(byId.get(ids[0]).uid > 0 && byId.get(ids[1]).uid > 0);
    // Gone from the server: kept, without a UID.
    assert.equal(byId.get(ids[2]).uid, null);
    assert.equal(folder.last_uid, 3);
  } finally {
    await runner.stop();
  }
});

test("a broken mail gets import_error, goes to the error folder, and does not block the batch; the rest go to the processed folder", { skip, timeout: 60_000 }, async () => {
  await reset();
  await append("INBOX", [
    mail({ subject: "good 1" }),
    mail({ subject: "broken", attachment: { name: "boom.bin", data: "x" } }),
    mail({ subject: "good 2", attachment: { name: "fine.txt", data: "fine" } }),
  ]);
  const files = fakeFiles();
  const write = files.write.bind(files);
  files.write = async (store, path, data, opts) => {
    if (path.endsWith("boom.bin")) throw new Error("the disk is full");
    return write(store, path, data, opts);
  };
  const { runner, tables } = runnerFor(accountRow({ processed_folder: "Processed", error_folder: "Errors" }), fakeDb(), files);
  runner.start();
  try {
    await until("three messages moved", () => tables.mail_messages.length === 3 && tables.mail_messages.every((m) => m.folder !== tables.mail_folders.find((f) => f.path === "INBOX")?.id) && runner.state === "idle");
    const bySubject = new Map(tables.mail_messages.map((m) => [m.subject, m]));
    const broken = bySubject.get("broken");
    assert.match(broken.import_error, /boom\.bin: the disk is full/);
    assert.equal(broken.body_state, "full");
    assert.equal(tables.mail_attachments.find((a) => a.message === broken.id).file, null);
    const folderOf = (m) => tables.mail_folders.find((f) => f.id === m.folder).path;
    assert.equal(folderOf(broken), "Errors");
    assert.equal(folderOf(bySubject.get("good 1")), "Processed");
    assert.equal(folderOf(bySubject.get("good 2")), "Processed");
    // The rows record where the mail went, with the new UIDs (UIDPLUS).
    const c = await admin();
    try {
      const lock = await c.getMailboxLock("Processed");
      try {
        const there = await c.fetchAll("1:*", { uid: true, envelope: true }, { uid: true });
        for (const m of there) assert.equal(bySubject.get(m.envelope.subject).uid, m.uid);
      } finally {
        lock.release();
      }
      const inbox = await c.status("INBOX", { messages: true, uidNext: true });
      assert.equal(inbox.messages, 0);
      assert.equal((await c.status("Errors", { messages: true })).messages, 1);
      // The cursor is past all three, the broken one included.
      assert.equal(tables.mail_folders.find((f) => f.path === "INBOX").last_uid, inbox.uidNext - 1);
    } finally {
      await c.logout();
    }
  } finally {
    await runner.stop();
  }
});

test("flags changed on the server are updated; an expunge is recorded, never deleted", { skip, timeout: 60_000 }, async () => {
  await reset();
  await append("INBOX", [mail({ subject: "f1" }), mail({ subject: "f2" }), mail({ subject: "f3" })]);
  const { runner, tables } = runnerFor(accountRow());
  runner.start();
  try {
    await until("three", () => tables.mail_messages.length === 3 && runner.state === "idle");
    await new Promise((r) => setTimeout(r, 1000));
    const c = await admin();
    try {
      const lock = await c.getMailboxLock("INBOX");
      try {
        await c.messageFlagsAdd("1", ["\\Seen", "\\Flagged"]);
        await c.messageDelete("3");
      } finally {
        lock.release();
      }
    } finally {
      await c.logout();
    }
    const row = (s) => tables.mail_messages.find((m) => m.subject === s);
    await until("the flags", () => row("f1").seen && row("f1").flagged, 10_000);
    await until("the expunge", () => row("f3").uid === null, 10_000);
    assert.equal(tables.mail_messages.length, 3, "the expunged mail's row stays");
    assert.equal(row("f2").seen, false);

    // Without a notification: the poll (here, a wake) re-reads the flags.
    const c2 = await admin();
    await c2.mailboxOpen("INBOX");
    await c2.messageFlagsAdd({ uid: String(row("f2").uid) }, ["\\Answered"], { uid: true });
    await c2.logout();
    runner.wake();
    await until("answered", () => row("f2").answered, 10_000);
    const inbox = tables.mail_folders.find((f) => f.path === "INBOX");
    assert.ok(inbox.highestmodseq, "CONDSTORE's HIGHESTMODSEQ is kept");
    assert.equal(inbox.unread, 1);
  } finally {
    await runner.stop();
  }
});

test("start_from imports mail on or after that date only", { skip, timeout: 60_000 }, async () => {
  await reset();
  await append("INBOX", [mail({ subject: "old" })], { date: new Date("2020-01-05T10:00:00Z") });
  await append("INBOX", [mail({ subject: "new" })], { date: new Date("2026-06-01T10:00:00Z") });
  const { runner, tables } = runnerFor(accountRow({ start_from: "2026-01-01T00:00:00Z" }));
  runner.start();
  try {
    await until("idle", () => runner.state === "idle" && tables.mail_messages.length >= 1);
    assert.deepEqual(tables.mail_messages.map((m) => m.subject), ["new"]);
  } finally {
    await runner.stop();
  }
});

test("three failed logins set auth_failed and stop retrying; the password never shows", { skip, timeout: 60_000 }, async () => {
  const account = accountRow({ imap_password: "wrong-Pässwort" });
  const { runner, tables } = runnerFor(account);
  runner.start();
  try {
    await until("auth_failed", () => accountOf(tables, account.id).status === "auth_failed", 30_000);
    const row = accountOf(tables, account.id);
    assert.match(row.last_error, /login failed 3 times/);
    assert.ok(!row.last_error.includes("wrong-Pässwort"));
    await runner.running; // the loop has ended: no more logins
  } finally {
    await runner.stop();
  }
});

test("a private server that is not trusted is refused, not retried", { skip, timeout: 30_000 }, async () => {
  const account = accountRow();
  const fake = fakeDb();
  fake.tables.mail_accounts.push({ ...account });
  const runner = new AccountRunner({ db: fake.db, files: fakeFiles(), log: fakeLog(), settings: connectionSettings({}) }, account, FAST);
  runner.start();
  await runner.running;
  const row = accountOf(fake.tables, account.id);
  assert.equal(row.status, "refused");
  assert.match(row.last_error, /not 11993|not a public address/);
  await runner.stop();
});

test("the service runs each enabled account and follows changes to mail_accounts", { skip, timeout: 60_000 }, async () => {
  await reset();
  await append("INBOX", [mail({ subject: "for the service" })]);
  const { ctx, tables, listeners } = fakeCtx({ ...configuration(), runner_options: FAST });
  const enabled = accountRow({ id: 900 });
  const disabled = accountRow({ id: 901, enabled: false });
  tables.mail_accounts.push(enabled, disabled);
  const stop = await service.start(ctx);
  const change = (op, row, old) => Promise.all(listeners.filter((l) => l.table === "mail_accounts").map((l) => l.callback({ table: "mail_accounts", op, row, old, own: false })));
  try {
    await until("the mail", () => tables.mail_messages.some((m) => m.subject === "for the service"));
    assert.ok(service.runners.has(900));
    assert.ok(!service.runners.has(901));

    // Disabled by a user: stopped.
    const off = { ...accountOf(tables, 900), enabled: false };
    Object.assign(accountOf(tables, 900), off);
    await change("update", off, enabled);
    await until("stopped", () => !service.runners.has(900) && accountOf(tables, 900).status === "disabled");

    // A wrong password is saved: restarted, and it fails until fixed.
    const wrong = { ...accountOf(tables, 900), enabled: true, imap_password: "nope" };
    Object.assign(accountOf(tables, 900), wrong);
    await change("update", wrong, off);
    await until("auth_failed", () => accountOf(tables, 900).status === "auth_failed", 30_000);
    // Saved again with the right password: running again.
    const fixed = { ...wrong, imap_password: PASSWORD };
    Object.assign(accountOf(tables, 900), fixed);
    await change("update", fixed, wrong);
    await until("idle again", () => accountOf(tables, 900).status === "idle", 30_000);

    // A change to its own status fields only is no change.
    const runner = service.runners.get(900);
    await change("update", { ...fixed, status: "whatever" }, fixed);
    assert.equal(service.runners.get(900), runner);

    // A mode that is neither ingest nor mirror is refused.
    const odd = accountRow({ id: 902, mode: "archive" });
    tables.mail_accounts.push(odd);
    await change("insert", odd, null);
    await until("refused", () => accountOf(tables, 902).status === "refused");
    assert.match(accountOf(tables, 902).last_error, /ingest or mirror/);

    // Deleted: stopped.
    await change("delete", null, fixed);
    await until("gone", () => !service.runners.has(900));
  } finally {
    await stop();
  }
  assert.equal(service.runners.size, 0);
});

// M1.4's finding 5: imapflow restarts IDLE every 25 minutes (§5.1). Held for 30 minutes, once,
// when asked: IMAP_LONG_IDLE=1 npm test.
test("an IDLE held for 30 minutes still delivers new mail", { skip: skip || (process.env.IMAP_LONG_IDLE ? false : "set IMAP_LONG_IDLE=1"), timeout: 35 * 60_000 }, async () => {
  await reset();
  const { runner, tables, events } = runnerFor(accountRow());
  runner.start();
  try {
    await until("idle", () => runner.state === "idle");
    const connects = () => events.filter((e) => e.table === "mail_accounts" && e.row.status === "connecting").length;
    const before = connects();
    await new Promise((r) => setTimeout(r, 30 * 60_000));
    const sent = Date.now();
    await append("INBOX", [mail({ subject: "after 30 minutes" })]);
    await until("the late mail", () => tables.mail_messages.some((m) => m.subject === "after 30 minutes"), 5_000);
    console.log(`after 30 minutes of IDLE: stored ${Date.now() - sent} ms after delivery, ${connects() - before} reconnects`);
    assert.equal(connects(), before, "the same connection throughout");
  } finally {
    await runner.stop();
  }
});

after(async () => {
  if (up) await reset().catch(() => {});
});
