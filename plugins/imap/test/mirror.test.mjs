// Mirror mode and the client actions against a real Dovecot and an SMTP sink (TODO-imap.md
// M9.1–M9.6): `account.js`, `sync.js` and `actions.js` over an in-memory host (fake-host.mjs),
// with v1's `Table` faked so the actions read as their caller. Skipped unless the test Dovecot
// runs:
//
//   test/servers/dovecot.sh start && npm test
//
// Isolation between users through REST, GraphQL and live updates needs the real host:
// crates/sc-module/tests/imap_mirror.rs.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb, fakeFiles, fakeLog, installFakeTable } from "./fake-host.mjs";
import { PASSWORD, skip, up, configuration, settings, admin, as, reset, append, mail, until, sleep } from "./dovecot.mjs";
import { startSink, SENDER } from "./servers/smtp-sink.mjs";

const require = createRequire(import.meta.url);
const { ImapFlow } = require("imapflow");
const { AccountRunner } = require("../account.js");
const service = require("../service.js");
const { actions } = require("../actions.js");

const USER = "mirror";
const ann = { id: "u-ann", role: 80 };
const bob = { id: "u-bob", role: 80 };
const staff = { id: "u-staff", role: 40 };

/** Fast enough to test: reconnects in a second, polls and backfill only when the test says. */
const FAST = { backoffFirstMs: 300, backoffMaxMs: 1000, settleMs: 200, pollMs: 600_000, mirrorPollMs: 600_000, backfillMs: 600_000 };

let nextAccountId = 100;
function mirrorAccount(extra = {}) {
  return {
    id: nextAccountId++,
    owner: ann.id,
    min_role: 1,
    send_role: null,
    label: "ann@",
    email: "ann@example.com",
    display_name: "Ann Example",
    mode: "mirror",
    imap_host: "127.0.0.1",
    imap_port: 11993,
    imap_security: "tls",
    imap_user: USER,
    imap_password: PASSWORD,
    smtp_host: "127.0.0.1",
    smtp_port: 10465,
    smtp_security: "tls",
    smtp_user: SENDER,
    smtp_password: PASSWORD,
    body_days: 30,
    save_sent: null,
    folders: null,
    enabled: true,
    ...extra,
  };
}

/** A running mirror account over fresh fakes, registered where the actions find it. */
function start(account = mirrorAccount(), options = {}) {
  const fake = fakeDb();
  fake.tables.mail_accounts.push({ ...account });
  const files = fakeFiles();
  const log = fakeLog();
  const restore = installFakeTable(fake);
  const runner = new AccountRunner({ db: fake.db, files, log, settings: settings() }, account, { ...FAST, ...options });
  service.runners.set(account.id, runner);
  runner.start();
  const callerFiles = {
    stored: new Map([["quotes/q-17.pdf", Buffer.from("%PDF quote 17")]]),
    async stat(store, path) {
      const f = store === "uploads" && this.stored.get(path);
      return f ? { size: f.length, isDirectory: false, mimeType: "application/pdf" } : null;
    },
    async read(store, path) {
      if (store !== "uploads" || !this.stored.has(path)) throw new Error(`${path} does not exist in file store ${store}`);
      return this.stored.get(path);
    },
  };
  const act = (name, user, payload) => actions[name]({ payload, user, settings: configuration(), files, callerFiles });
  const stop = async () => {
    await runner.stop();
    service.runners.delete(account.id);
    restore();
  };
  const folder = (path) => fake.tables.mail_folders.find((f) => f.account === account.id && f.path === path);
  const rows = (path) => fake.tables.mail_messages.filter((m) => m.folder === (folder(path) || {}).id);
  const row = (subject) => fake.tables.mail_messages.find((m) => m.subject === subject);
  return { runner, account, ...fake, files, log, act, stop, folder, rows, row };
}

const idle = (s) => until("idle", () => s.runner.state === "idle" && !s.runner.passing, 30_000);

/** The server's view of a folder: `{ uid, subject, flags, source? }` by subject. */
async function serverView(path, { source = false } = {}) {
  return as(USER, async (c) => {
    const lock = await c.getMailboxLock(path);
    try {
      if (!c.mailbox.exists) return new Map();
      const all = await c.fetchAll("1:*", { uid: true, envelope: true, flags: true, source }, { uid: true });
      return new Map(all.map((m) => [m.envelope.subject, { uid: m.uid, flags: m.flags, source: m.source }]));
    } finally {
      lock.release();
    }
  });
}

const OLD = new Date("2020-03-01T10:00:00Z");

before(async () => {
  if (up) await reset(USER);
});

test("a mirror syncs every folder newest first, keeps bodies for body_days, and backfills the rest", { skip, timeout: 120_000 }, async () => {
  await reset(USER);
  const old = Array.from({ length: 120 }, (_, i) => mail({ subject: `old ${i}`, ...(i === 5 ? { attachment: { name: "old.pdf", data: "%PDF old" } } : {}) }));
  await append(USER, "INBOX", old, { date: OLD });
  const recent = Array.from({ length: 10 }, (_, i) => mail({ subject: `new ${i}`, ...(i === 3 ? { attachment: { name: "new.pdf", data: "%PDF new" } } : {}) }));
  await append(USER, "INBOX", recent);
  await as(USER, (c) => c.mailboxCreate("Projects"));
  await append(USER, "Projects", [mail({ subject: "p1" }), mail({ subject: "p2" })]);
  await append(USER, "Sent", [mail({ subject: "sent 1" })], { flags: ["\\Seen"] });

  const s = start();
  try {
    await idle(s);
    // Every folder, with its role, synced.
    assert.ok(["INBOX", "Sent", "Drafts", "Trash", "Junk", "Projects"].every((p) => s.folder(p) && s.folder(p).sync), JSON.stringify(s.tables.mail_folders));
    assert.equal(s.folder("Sent").role, "sent");
    assert.equal(s.rows("Projects").length, 2);
    assert.equal(s.rows("Sent").length, 1);
    assert.equal(s.row("sent 1").seen, true);

    // Newest first: the newest 50 of INBOX, the rest waiting for the backfill.
    const inbox = s.folder("INBOX");
    const subjects = new Set(s.rows("INBOX").map((m) => m.subject));
    assert.equal(subjects.size, 50);
    for (let i = 0; i < 10; i++) assert.ok(subjects.has(`new ${i}`));
    for (let i = 80; i < 120; i++) assert.ok(subjects.has(`old ${i}`), `old ${i}`);
    const uids = await serverView("INBOX");
    assert.equal(inbox.last_uid, uids.get("new 9").uid);
    assert.equal(inbox.backfill_uid, uids.get("old 80").uid);
    assert.equal(s.folder("Projects").backfill_uid, 0, "a small folder is complete at once");

    // Bodies for body_days; headers only for older mail.
    assert.equal(s.row("new 0").body_state, "full");
    assert.equal(s.row("new 0").body_text.trim(), "hello");
    assert.equal(s.row("old 100").body_state, "headers");
    assert.equal(s.row("old 100").body_text, null);
    assert.equal(s.row("old 100").subject, "old 100");
    assert.equal(s.row("old 100").thread_key, s.row("old 100").message_id);
    // Attachments are listed, and stored only when opened.
    const newPdf = s.tables.mail_attachments.find((a) => a.message === s.row("new 3").id);
    assert.equal(newPdf.filename, "new.pdf");
    assert.equal(newPdf.file, null);
    assert.equal(s.files.stored.size, 0);

    // The backfill: 50 a step, newest first, until the folder is complete.
    await s.runner.backfillStep();
    assert.equal(s.rows("INBOX").length, 100);
    assert.equal(s.folder("INBOX").backfill_uid, uids.get("old 30").uid);
    await s.runner.backfillStep();
    assert.equal(s.rows("INBOX").length, 130);
    assert.equal(s.folder("INBOX").backfill_uid, 0);
    assert.equal(s.runner.backfillStep(), null, "nothing left to do");
    assert.equal(new Set(s.rows("INBOX").map((m) => m.uid)).size, 130, "each stored once");

    // An old mail with an attachment, listed from its structure, then opened.
    const oldMail = s.row("old 5");
    assert.equal(oldMail.body_state, "headers");
    assert.equal(oldMail.has_attachments, true);
    const listed = s.tables.mail_attachments.find((a) => a.message === oldMail.id);
    assert.equal(listed.filename, "old.pdf");
    const fetched = await s.act("mail_fetch_body", ann, { message_id: oldMail.id });
    assert.deepEqual(fetched, { id: oldMail.id, body_state: "full", fetched: true });
    assert.equal(s.row("old 5").body_text.trim(), "hello");
    assert.equal(s.row("old 5").snippet, "hello");
    const after = s.tables.mail_attachments.filter((a) => a.message === oldMail.id);
    assert.equal(after.length, 1);
    assert.equal(after[0].id, listed.id, "the listed attachment keeps its row");
    assert.equal((await s.act("mail_fetch_body", ann, { message_id: oldMail.id })).fetched, false);
    const opened = await s.act("mail_attachment", ann, { attachment_id: listed.id });
    assert.equal(opened.stored, true);
    assert.equal(s.files.stored.get(opened.file).bytes.toString(), "%PDF old");
    await assert.rejects(s.act("mail_fetch_body", bob, { message_id: oldMail.id }), /not found/);
  } finally {
    await s.stop();
  }
});

test("the backfill runs by itself, a few batches a minute", { skip, timeout: 60_000 }, async () => {
  await reset(USER);
  await append(USER, "INBOX", Array.from({ length: 140 }, (_, i) => mail({ subject: `b${i}` })), { date: OLD });
  const s = start(mirrorAccount(), { backfillMs: 300 });
  try {
    await idle(s);
    assert.equal(s.rows("INBOX").length, 50);
    await until("the whole folder", () => s.rows("INBOX").length === 140 && s.folder("INBOX").backfill_uid === 0, 20_000);
    assert.ok(s.rows("INBOX").every((m) => m.body_state === "headers"));
  } finally {
    await s.stop();
  }
});

test("new mail, flags and expunges in both directions; a new UIDVALIDITY rebuilds; folders follow the server", { skip, timeout: 90_000 }, async () => {
  await reset(USER);
  await append(USER, "INBOX", [mail({ subject: "i1", attachment: { name: "a.txt", data: "attached" } }), mail({ subject: "i2" }), mail({ subject: "i3" })]);
  await as(USER, (c) => c.mailboxCreate("Projects"));
  await append(USER, "Projects", [mail({ subject: "p1" }), mail({ subject: "p2" })]);
  const s = start();
  try {
    await idle(s);
    assert.equal(s.tables.mail_messages.length, 5);
    await sleep(1000);

    // INBOX by IDLE.
    const sent = Date.now();
    await append(USER, "INBOX", [mail({ subject: "live" })]);
    await until("live", () => s.row("live") && s.row("live").body_state === "full", 5_000);
    console.log(`mirror: new mail stored ${Date.now() - sent} ms after delivery`);

    // Another folder by the poll: STATUS says whether it changed.
    const projects = s.folder("Projects");
    assert.equal(await s.runner.withClient((c) => s.runner.changedOnServer(c, projects)), false);
    await append(USER, "Projects", [mail({ subject: "p3" })]);
    assert.equal(await s.runner.withClient((c) => s.runner.changedOnServer(c, projects)), true);
    s.runner.wake({ quick: true });
    await until("p3", () => s.row("p3"));

    // Flags from the app to the server…
    const done = await s.act("mail_set_flags", ann, { message_ids: [s.row("i2").id, s.row("p1").id], seen: true, flagged: true });
    assert.deepEqual(done.updated.sort(), [s.row("i2").id, s.row("p1").id].sort());
    assert.ok((await serverView("INBOX")).get("i2").flags.has("\\Flagged"));
    assert.ok((await serverView("Projects")).get("p1").flags.has("\\Seen"));
    // …and from the server to the app: pushed in INBOX, polled elsewhere.
    await as(USER, async (c) => {
      await c.mailboxOpen("INBOX");
      await c.messageFlagsAdd({ uid: String(s.row("i3").uid) }, ["\\Flagged"], { uid: true });
      await c.mailboxOpen("Projects");
      await c.messageFlagsRemove({ uid: String(s.row("p1").uid) }, ["\\Seen"], { uid: true });
    });
    await until("i3 flagged", () => s.row("i3").flagged, 10_000);
    s.runner.wake({ quick: true });
    await until("p1 unread", () => s.row("p1").seen === false, 10_000);

    // An expunge deletes the row, its attachment rows and the stored file.
    const i1 = s.row("i1");
    const att = s.tables.mail_attachments.find((a) => a.message === i1.id);
    const { file } = await s.act("mail_attachment", ann, { attachment_id: att.id });
    assert.ok(s.files.stored.has(file));
    await as(USER, async (c) => {
      await c.mailboxOpen("INBOX");
      await c.messageDelete({ uid: String(i1.uid) }, { uid: true });
    });
    await until("i1 gone", () => !s.row("i1"), 10_000);
    assert.ok(!s.tables.mail_attachments.some((a) => a.message === i1.id));
    assert.ok(!s.files.stored.has(file), "its file is removed");

    // A new UIDVALIDITY: the folder's rows are deleted and it is synced again.
    await sleep(1100); // Dovecot's UIDVALIDITY is a timestamp
    await as(USER, async (c) => {
      await c.mailboxDelete("Projects");
      await c.mailboxCreate("Projects");
    });
    await append(USER, "Projects", [mail({ subject: "p-new" })]);
    s.runner.wake({ quick: true });
    await until("Projects rebuilt", () => s.rows("Projects").length === 1 && s.row("p-new"), 10_000);
    assert.ok(!s.row("p1") && !s.row("p3"));

    // Folders made and removed on the server.
    await as(USER, (c) => c.mailboxCreate("Clients"));
    await append(USER, "Clients", [mail({ subject: "c1" })]);
    s.runner.wake({ quick: true });
    await until("Clients", () => s.row("c1"), 10_000);
    assert.equal(s.folder("Clients").sync, true);
    await as(USER, (c) => c.mailboxDelete("Clients"));
    s.runner.wake({ quick: true });
    await until("Clients gone", () => !s.folder("Clients") && !s.row("c1"), 10_000);
    await idle(s);
    assert.equal(s.tables.mail_accounts[0].status, "idle");
  } finally {
    await s.stop();
  }
});

test("move, delete to Trash and for good, create folders, save and replace drafts", { skip, timeout: 90_000 }, async () => {
  await reset(USER);
  await append(USER, "INBOX", ["m1", "m2", "m3", "m4", "m5"].map((subject) => mail({ subject })));
  const s = start();
  try {
    await idle(s);

    // A folder, and one inside it.
    const made = await s.act("mail_create_folder", ann, { account_id: s.account.id, name: "Projects" });
    assert.equal(made.path, "Projects");
    const delimiter = s.folder("INBOX").delimiter;
    const sub = await s.act("mail_create_folder", ann, { account_id: s.account.id, name: "2026", parent: "Projects" });
    assert.equal(sub.path, `Projects${delimiter}2026`);
    const listed = await as(USER, (c) => c.list());
    assert.ok(listed.some((b) => b.path === sub.path), "made on the server");
    await assert.rejects(s.act("mail_create_folder", ann, { account_id: s.account.id, name: "Projects" }), /already/);
    await assert.rejects(s.act("mail_create_folder", bob, { account_id: s.account.id, name: "Mine" }), /not found/);

    // Move, by path and by role; the rows follow, with the new UIDs, and are not imported again.
    const m2 = s.row("m2");
    const moved = await s.act("mail_move", ann, { message_ids: [m2.id], target: "Projects" });
    assert.deepEqual(moved.moved, [m2.id]);
    assert.equal(s.row("m2").folder, s.folder("Projects").id);
    const there = (await serverView("Projects")).get("m2");
    assert.equal(s.row("m2").uid, there.uid);
    await s.act("mail_move", ann, { message_ids: [s.row("m5").id], target: "role:spam" });
    assert.equal(s.row("m5").folder, s.folder("Junk").id);
    s.runner.wake();
    await idle(s);
    assert.equal(s.tables.mail_messages.filter((m) => m.subject === "m2").length, 1, "not imported twice");
    assert.equal((await serverView("INBOX")).has("m2"), false);
    const refused = await s.act("mail_move", bob, { message_ids: [s.row("m1").id], target: "Projects" });
    assert.deepEqual(refused.missing, [s.row("m1").id]);
    await assert.rejects(s.act("mail_move", ann, { message_ids: [s.row("m1").id], target: "Nowhere" }), /no folder Nowhere/);

    // Delete: to Trash, then from Trash for good; and for good at once.
    const m3 = s.row("m3");
    assert.deepEqual((await s.act("mail_delete", ann, { message_ids: [m3.id] })).trashed, [m3.id]);
    assert.equal(s.row("m3").folder, s.folder("Trash").id);
    assert.ok((await serverView("Trash")).has("m3"));
    assert.deepEqual((await s.act("mail_delete", ann, { message_ids: [m3.id] })).deleted, [m3.id]);
    assert.equal(s.row("m3"), undefined);
    assert.equal((await serverView("Trash")).size, 0);
    const m4 = s.row("m4");
    assert.deepEqual((await s.act("mail_delete", ann, { message_ids: [m4.id], permanent: true })).deleted, [m4.id]);
    assert.equal(s.row("m4"), undefined);
    assert.equal((await serverView("INBOX")).has("m4"), false);
    s.runner.wake();
    await idle(s);
    assert.equal(s.row("m3"), undefined, "the sync does not bring it back");

    // Drafts: saved with \Draft, and a second save replaces the first.
    const first = await s.act("mail_save_draft", ann, { account_id: s.account.id, to: "Bob <bob@example.com>", subject: "Draft one", text: "first try" });
    const draft = s.tables.mail_messages.find((m) => m.id === first.id);
    assert.equal(draft.folder, s.folder("Drafts").id);
    assert.equal(draft.draft, true);
    assert.equal(draft.body_text.trim(), "first try");
    assert.ok((await serverView("Drafts")).get("Draft one").flags.has("\\Draft"));
    const second = await s.act("mail_save_draft", ann, { account_id: s.account.id, to: "bob@example.com", subject: "Draft two", text: "better", replaces: first.id });
    assert.equal(second.replaced, first.id);
    const drafts = await serverView("Drafts");
    assert.deepEqual([...drafts.keys()], ["Draft two"]);
    assert.equal(s.tables.mail_messages.find((m) => m.id === first.id), undefined);
    assert.equal(s.rows("Drafts").length, 1);
    await assert.rejects(s.act("mail_save_draft", ann, { account_id: s.account.id, to: "not an address" }), /not a mail address/);
  } finally {
    await s.stop();
  }
});

test("mail_send: a reply reaches the SMTP server with its threading headers, lands in Sent, and its draft goes", { skip, timeout: 60_000 }, async () => {
  await reset(USER);
  const sink = await startSink();
  await append(USER, "INBOX", [
    mail({ subject: "Question", messageId: "<question@example.net>", references: "<root@example.net>", inReplyTo: "<root@example.net>", attachment: { name: "spec.txt", data: "the spec" } }),
  ]);
  const s = start();
  try {
    await idle(s);
    const question = s.row("Question");
    assert.equal(question.thread_key, "<root@example.net>", "M9.4: the root of References");
    const spec = s.tables.mail_attachments.find((a) => a.message === question.id);
    const draft = await s.act("mail_save_draft", ann, { account_id: s.account.id, to: "shop@example.com", subject: "Re: Question", text: "draft" });

    const result = await s.act("mail_send", ann, {
      account_id: s.account.id,
      to: "Shop Ümlaut <shop@example.com>",
      cc: ["carol@example.com"],
      bcc: "secret@example.com",
      subject: "Re: Question",
      text: "The answer.",
      html: "<p>The <b>answer</b>.</p>",
      in_reply_to: question.id,
      draft_id: draft.id,
      attachments: [{ attachment_id: spec.id }, { store: "uploads", path: "quotes/q-17.pdf" }],
    });
    assert.equal(result.saved, true, result.save_error);
    assert.equal(result.draft_deleted, true);
    assert.equal(result.warnings, undefined);
    assert.match(result.message_id, /^<[0-9a-f-]+@example\.com>$/);
    assert.deepEqual(result.accepted.sort(), ["carol@example.com", "secret@example.com", "shop@example.com"]);

    // At the SMTP server: TLS, the UTF-8 password, the threading headers, no Bcc header.
    assert.equal(sink.messages.length, 1);
    const got = sink.messages[0];
    assert.equal(got.secure, true);
    assert.equal(got.user, SENDER);
    assert.equal(got.from, "ann@example.com");
    assert.deepEqual(got.to.sort(), ["carol@example.com", "secret@example.com", "shop@example.com"]);
    const source = got.source.toString();
    const header = (name) => (source.split("\r\n\r\n")[0].match(new RegExp(`^${name}: (.*(?:\\r\\n[ \\t].*)*)`, "mi")) || [])[1];
    assert.equal(header("In-Reply-To"), "<question@example.net>");
    assert.equal(header("References").replace(/\s+/g, " "), "<root@example.net> <question@example.net>");
    assert.equal(header("Message-ID"), result.message_id);
    assert.match(header("From"), /<ann@example\.com>$/);
    assert.equal(header("Bcc"), undefined);
    assert.match(source, /spec\.txt/);
    assert.match(source, /q-17\.pdf/);
    assert.ok(source.includes(Buffer.from("%PDF quote 17").toString("base64")));

    // In Sent: the server's copy keeps the Bcc, and the row is there with the same Message-ID.
    const sentCopy = (await serverView("Sent", { source: true })).get("Re: Question");
    assert.ok(sentCopy.flags.has("\\Seen"));
    assert.match(sentCopy.source.toString(), /^Bcc: secret@example\.com/m);
    const sentRow = s.tables.mail_messages.find((m) => m.id === result.id);
    assert.equal(sentRow.folder, s.folder("Sent").id);
    assert.equal(sentRow.message_id, result.message_id);
    assert.equal(sentRow.in_reply_to, "<question@example.net>");
    assert.equal(sentRow.thread_key, "<root@example.net>", "the reply joins the question's thread");
    assert.equal(sentRow.uid, sentCopy.uid);
    s.runner.wake();
    await idle(s);
    assert.equal(s.rows("Sent").length, 1, "the service does not import the copy again");

    // The question is answered, on the server and in the row; the draft is gone.
    assert.equal(s.row("Question").answered, true);
    assert.ok((await serverView("INBOX")).get("Question").flags.has("\\Answered"));
    assert.equal(s.rows("Drafts").length, 0);
    assert.equal((await serverView("Drafts")).size, 0);

    // What is refused before anything is sent.
    const before = sink.messages.length;
    const many = Array.from({ length: 51 }, (_, i) => `r${i}@example.com`);
    await assert.rejects(s.act("mail_send", ann, { account_id: s.account.id, to: many, subject: "x" }), /at most 50 recipients/);
    await assert.rejects(s.act("mail_send", ann, { account_id: s.account.id, to: "nobody", subject: "x" }), /not a mail address/);
    await assert.rejects(s.act("mail_send", ann, { account_id: s.account.id, subject: "x" }), /who the message is to/);
    const eleven = Array.from({ length: 11 }, () => ({ store: "uploads", path: "quotes/q-17.pdf" }));
    await assert.rejects(s.act("mail_send", ann, { account_id: s.account.id, to: "a@example.com", attachments: eleven }), /at most 10 attachments/);
    await assert.rejects(s.act("mail_send", ann, { account_id: s.account.id, to: "a@example.com", attachments: [{ store: "uploads", path: "secret.pdf" }] }), /not a file/);
    await assert.rejects(s.act("mail_send", bob, { account_id: s.account.id, to: "a@example.com" }), /not found/);
    assert.equal(sink.messages.length, before);

    // A wrong SMTP password: refused, and the error does not carry it.
    s.tables.mail_accounts[0].smtp_password = "wrong-Pässwort";
    const e = await s.act("mail_send", ann, { account_id: s.account.id, to: "a@example.com", subject: "x" }).catch((err) => err);
    assert.match(e.message, /sending through 127\.0\.0\.1 failed/);
    assert.ok(!e.message.includes("wrong-Pässwort"));
  } finally {
    await s.stop();
    await sink.close();
  }
});

test("sending from a shared account needs send_role; an ingest account refuses the mirror actions", { skip, timeout: 60_000 }, async () => {
  await reset(USER);
  const sink = await startSink();
  // Not running: the actions open a connection of their own when they need one.
  const shared = mirrorAccount({ owner: null, min_role: 80, send_role: 40, mode: "ingest", label: "orders@", email: "orders@example.com", save_sent: false });
  const fake = fakeDb();
  fake.tables.mail_accounts.push(shared);
  const restore = installFakeTable(fake);
  const act = (name, user, payload) => actions[name]({ payload, user, settings: configuration(), files: fakeFiles() });
  try {
    await assert.rejects(act("mail_send", bob, { account_id: shared.id, to: "a@example.com", subject: "x" }), /your role may not send/);
    const sent = await act("mail_send", staff, { account_id: shared.id, to: "a@example.com", subject: "From staff", text: "hi" });
    assert.equal(sent.saved, false, "save_sent is off");
    assert.equal(sink.messages.length, 1);
    assert.match(sink.messages[0].source.toString(), /^From: Ann Example <orders@example\.com>/m);
    fake.tables.mail_folders.push({ id: 1, account: shared.id, path: "INBOX" });
    fake.tables.mail_messages.push({ id: 1, account: shared.id, folder: 1, uid: 1, subject: "x" });
    await assert.rejects(act("mail_delete", staff, { message_ids: [1] }), /for mirror accounts/);
    await assert.rejects(act("mail_create_folder", staff, { account_id: shared.id, name: "x" }), /for mirror accounts/);
    await assert.rejects(act("mail_save_draft", staff, { account_id: shared.id, to: "a@example.com" }), /for mirror accounts/);
  } finally {
    restore();
    await sink.close();
  }
});

test("M9.6: at most two IMAP connections per account, whatever the app calls in parallel", { skip, timeout: 90_000 }, async () => {
  await reset(USER);
  await append(USER, "INBOX", Array.from({ length: 30 }, (_, i) => mail({ subject: `c${i}`, attachment: { name: `c${i}.txt`, data: `c${i}` } })), { date: OLD });
  // Every connection the module opens for the account, counted while it is open.
  let open = 0;
  let most = 0;
  const connect = ImapFlow.prototype.connect;
  ImapFlow.prototype.connect = async function (...args) {
    const counted = this.options && this.options.auth && this.options.auth.user === USER && this.options.clientInfo && this.options.clientInfo.name === "feldspar";
    if (counted) {
      open += 1;
      most = Math.max(most, open);
      this.once("close", () => (open -= 1));
    }
    return connect.apply(this, args);
  };
  const s = start();
  try {
    await idle(s);
    most = open;
    const ids = s.rows("INBOX").map((m) => m.id);
    const atts = s.tables.mail_attachments.map((a) => a.id);
    const calls = [];
    for (let i = 0; i < 10; i++) {
      calls.push(s.act("mail_set_flags", ann, { message_ids: [ids[i]], seen: i % 2 === 0 }));
      calls.push(s.act("mail_fetch_body", ann, { message_id: ids[10 + i] }));
      calls.push(s.act("mail_attachment", ann, { attachment_id: atts[i] }));
    }
    calls.push(s.act("mail_sync_now", ann, { account_id: s.account.id, wait: true }));
    calls.push(s.runner.backfillStep());
    await Promise.all(calls);
    assert.equal(most, 1, "with the service running, its one connection serves every action");

    // Without the service: the actions' own connections, one at a time.
    await s.runner.stop();
    service.runners.delete(s.account.id);
    most = open;
    const own = [];
    for (let i = 0; i < 10; i++) own.push(s.act("mail_set_flags", ann, { message_ids: [ids[i]], flagged: true }));
    for (let i = 10; i < 15; i++) own.push(s.act("mail_attachment", ann, { attachment_id: atts[i] }));
    const results = await Promise.all(own);
    assert.ok(results.slice(0, 10).every((r) => r.updated.length === 1));
    assert.equal(most, 1, "one connection at a time when the service is not running");
    assert.ok(open <= 0, "and each is closed");
  } finally {
    ImapFlow.prototype.connect = connect;
    await s.stop();
  }
});

after(async () => {
  if (up) await reset(USER).catch(() => {});
});
