// What the first real use (a mail client on six accounts, 2026-10-22) ran into, at the level of
// one function each, with no server: a message deleted while its attachments are written, a
// sync answered by the wrong folder, keywords and the extra header fields, the module's "Headers
// to keep", and what mail_send accepts. The same behaviour against the test Dovecot is in
// mirror.test.mjs.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fakeDb, fakeFiles, fakeLog } from "./fake-host.mjs";

const require = createRequire(import.meta.url);
const { storeMessage, syncFolder, syncFlags, applyFlags, headerNames, DEFAULT_HEADERS, flagValues } = require("../sync.js");
const { parse } = require("../parse.js");
const { addressList, calendarOf } = require("../actions.js");

/** An account, a folder and a parsed message with two attachments, over fresh fakes. */
async function setup(mode) {
  const fake = fakeDb();
  const account = await fake.db.mail_accounts.insert({ label: "a@", mode });
  const folder = await fake.db.mail_folders.insert({ account: account.id, path: "INBOX", name: "INBOX" });
  const parsed = {
    message: { subject: "with files", body_state: "full" },
    attachments: [
      { part: "2", filename: "a.pdf", content_type: "application/pdf", size: 3, content: Buffer.from("%PD") },
      { part: "3", filename: "b.txt", content_type: "text/plain", size: 2, content: Buffer.from("hi") },
    ],
  };
  const files = fakeFiles();
  const env = { db: fake.db, files, account, log: fakeLog(), client: null };
  return { fake, account, folder, parsed, files, env };
}

for (const mode of ["mirror", "ingest"]) {
  test(`${mode}: a message deleted while its attachments are written is skipped, not a failed sync`, async () => {
    const { fake, folder, parsed, files, env } = await setup(mode);
    // What the field report saw: the row is gone between its insert and its attachments'.
    fake.hooks.beforeInsert = async (table) => {
      if (table === "mail_attachments") await fake.db.mail_messages.where({ subject: "with files" }).delete();
    };
    const row = await storeMessage(env, folder, { uid: 7 }, parsed, null);
    assert.equal(row, null);
    assert.equal(fake.tables.mail_attachments.length, 0);
    assert.equal(files.stored.size, 0, "an ingest account's files are removed again");

    // Without the delete, both attachment rows are written in one insert.
    fake.hooks.beforeInsert = null;
    const stored = await storeMessage(env, folder, { uid: 8 }, parsed, null);
    assert.equal(stored.body_state, "full");
    assert.deepEqual(
      fake.tables.mail_attachments.map((a) => [a.message, a.filename]),
      [
        [stored.id, "a.pdf"],
        [stored.id, "b.txt"],
      ],
    );
  });
}

test("an attachment insert that fails for another reason still fails", async () => {
  const { fake, folder, parsed, env } = await setup("mirror");
  fake.hooks.beforeInsert = async (table) => {
    if (table === "mail_attachments") throw new Error("disk full");
  };
  await assert.rejects(storeMessage(env, folder, { uid: 9 }, parsed, null), /disk full/);
});

test("a sync answered by another folder stops before it deletes anything", async () => {
  const { fake, account, env } = await setup("mirror");
  const archive = await fake.db.mail_folders.insert({ account: account.id, path: "Archive", name: "Archive", uidvalidity: 5, last_uid: 3, backfill_uid: 0 });
  for (const uid of [1, 2, 3]) await fake.db.mail_messages.insert({ account: account.id, folder: archive.id, uid, subject: `m${uid}` });
  // INBOX is selected (a reconnect selected it under the sync): its UIDs are not Archive's.
  const searched = [];
  env.client = {
    mailbox: { path: "INBOX", uidValidity: 5, exists: 1 },
    enabled: new Set(),
    async search(query) {
      searched.push(query);
      return [9];
    },
    async fetchAll() {
      return [];
    },
  };
  await assert.rejects(syncFolder(env, archive, { flags: true }), /Archive was to be synced, but INBOX is selected/);
  await assert.rejects(syncFlags(env, archive), /INBOX is selected/);
  assert.deepEqual(searched, [], "nothing was searched in the wrong folder");
  assert.equal(fake.tables.mail_messages.length, 3, "and nothing deleted");

  // Selected as itself, an INBOX path in any case is the same folder.
  env.client.mailbox.path = "INBOX";
  const inbox = fake.tables.mail_folders.find((f) => f.path === "INBOX");
  await syncFlags(env, { ...inbox, path: "Inbox" });
});

test("keywords are stored and kept up to date with the flags", async () => {
  const { fake, account, folder } = await setup("mirror");
  assert.deepEqual(flagValues(new Set(["\\Seen", "$label2", "\\Recent", "$Forwarded", "$label2"])), {
    seen: true,
    flagged: false,
    answered: false,
    draft: false,
    keywords: ["$Forwarded", "$label2"],
  });
  const row = await fake.db.mail_messages.insert({ account: account.id, folder: folder.id, uid: 4, seen: true, keywords: [] });
  assert.equal(await applyFlags({ db: fake.db }, folder, [{ uid: 4, flags: new Set(["\\Seen", "$label1"]) }]), 1);
  assert.deepEqual(fake.tables.mail_messages.find((m) => m.id === row.id).keywords, ["$label1"]);
  assert.equal(await applyFlags({ db: fake.db }, folder, [{ uid: 4, flags: new Set(["\\Seen", "$label1"]) }]), 0, "unchanged: no write");
});

const NEWSLETTER = Buffer.from(
  [
    "From: News <news@example.com>",
    "To: ann@example.com",
    "Subject: Weekly",
    "Message-ID: <w1@example.com>",
    "Date: Wed, 21 Oct 2026 10:00:00 +0000",
    "List-Id: Weekly news <weekly.example.com>",
    "List-Unsubscribe: <https://example.com/u?x=1>,",
    " <mailto:unsub@example.com>",
    "List-Unsubscribe-Post: List-Unsubscribe=One-Click",
    "Authentication-Results: mx.example.net; spf=pass smtp.mailfrom=example.com",
    "Authentication-Results: mx.example.net; dkim=pass header.d=example.com",
    "X-Spam-Status: No, score=-0.1",
    "X-Spam-Flag: NO",
    "X-Mailer: =?utf-8?q?M=C3=A4iler?=",
    "MIME-Version: 1.0",
    "Content-Type: text/plain; charset=utf-8",
    "",
    "news",
    "",
  ].join("\r\n"),
);

test("the header fields asked for are stored by lower-case name; repeated ones together", async () => {
  const { message } = await parse(NEWSLETTER, { flags: new Set(["\\Seen", "$label3"]), extraHeaders: [...DEFAULT_HEADERS, "x-mailer", "x-missing"] });
  assert.deepEqual(message.keywords, ["$label3"]);
  assert.deepEqual(message.headers, {
    "list-id": "Weekly news <weekly.example.com>",
    "list-unsubscribe": "<https://example.com/u?x=1>, <mailto:unsub@example.com>",
    "list-unsubscribe-post": "List-Unsubscribe=One-Click",
    "authentication-results": "mx.example.net; spf=pass smtp.mailfrom=example.com\nmx.example.net; dkim=pass header.d=example.com",
    "x-spam-status": "No, score=-0.1",
    "x-spam-flag": "NO",
    "x-mailer": "Mäiler",
  });
  assert.equal((await parse(NEWSLETTER, {})).message.headers, null, "nothing asked for: null");
});

test("Headers to keep: the defaults when empty, none for -, names and prefixes otherwise", () => {
  assert.deepEqual(headerNames({}), DEFAULT_HEADERS);
  assert.deepEqual(headerNames({ extra_headers: "  " }), DEFAULT_HEADERS);
  assert.deepEqual(headerNames({ extra_headers: "-" }), []);
  // A trailing colon is forgiven; a name with one inside is not a header name, and is dropped.
  assert.deepEqual(headerNames({ extra_headers: "List-Unsubscribe:\nX-Spam-*, Received\nlist-unsubscribe\nx:y" }), ["list-unsubscribe", "x-spam-*", "received"]);
});

test("mail_send's addresses: localhost is one (a local test server), a dotless typo is not", () => {
  assert.deepEqual(addressList("x@localhost", "to"), [{ name: "", address: "x@localhost" }]);
  assert.throws(() => addressList("bob@gmail", "to"), /not a mail address/);
});

test("mail_send's calendar: an iTIP part whose method matches the calendar's", () => {
  const ics = ["BEGIN:VCALENDAR", "METHOD:REQUEST", "BEGIN:VEVENT", "UID:1@example.com", "END:VEVENT", "END:VCALENDAR"].join("\r\n");
  assert.deepEqual(calendarOf({ method: "request", content: ics }), { method: "REQUEST", content: ics, filename: "invite.ics" });
  assert.equal(calendarOf(undefined), undefined);
  assert.throws(() => calendarOf({ method: "REPLY", content: ics }), /says METHOD:REQUEST/);
  assert.throws(() => calendarOf({ method: "INVITE", content: ics }), /must be one of/);
  assert.throws(() => calendarOf({ method: "REQUEST", content: "hello" }), /iCalendar object/);
});
