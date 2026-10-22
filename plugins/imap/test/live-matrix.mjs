// The live-server matrix (TODO-imap.md M10.2): run the module's own sync and actions against a
// real mail account and say what worked. It is a manual test, not part of `npm test`, because it
// needs real accounts and sends real mail:
//
//   node test/live-matrix.mjs accounts.json [--only <name>] [--keep] [--json]
//
// `accounts.json` is a list of accounts, each one a `mail_accounts` row's connection fields plus
// a `name` for the report (keep the file out of git; it holds passwords):
//
//   [{ "name": "Fastmail", "email": "me@fastmail.com", "display_name": "Me",
//      "imap_host": "imap.fastmail.com", "imap_port": 993, "imap_security": "tls",
//      "imap_user": "me@fastmail.com", "imap_password": "app password",
//      "smtp_host": "smtp.fastmail.com", "smtp_port": 465, "smtp_security": "tls",
//      "smtp_user": "me@fastmail.com", "smtp_password": "app password" }]
//
// An account on a server inside the network (Exchange on-premises) also needs the module's
// settings: add `"configuration": { "trusted_servers": "mail.corp.example", "ca_certificates":
// "-----BEGIN CERTIFICATE-----…" }` to its entry.
//
// For each account, as a mirror account over the in-memory host of the other tests
// (fake-host.mjs), with the module's own connection rules (public servers only):
//
//   1. login:   `mail_test_account`, IMAP and SMTP;
//   2. sync:    the service's runner connects, lists the folders and syncs, until `idle`;
//   3. send:    `mail_send` from the account to itself;
//   4. new:     that mail arrives in INBOX as a row by IDLE, without a poll (timed);
//   5. sent:    exactly one copy in the Sent folder on the server, whether the module APPENDed
//               it or the server filed it (Gmail, Microsoft 365), and whether `save_sent`'s
//               default guessed right;
//   6. flags:   \Flagged set on the server reaches the row by itself (timed), and `seen` set by
//               `mail_set_flags` reaches the server;
//   7. cleanup: the test mail is deleted for good, in INBOX and Sent, unless `--keep`.
//
// It prints one Markdown table row per account, for TODO-imap.md's M10.2 results. Passwords never
// appear in the output: errors go through `connect.redact`.

import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { randomUUID } from "node:crypto";
import { fakeDb, fakeFiles, fakeLog, installFakeTable } from "./fake-host.mjs";

const require = createRequire(import.meta.url);
const { ImapFlow } = require("imapflow");
const { AccountRunner } = require("../account.js");
const service = require("../service.js");
const { actions } = require("../actions.js");
const { connectionSettings, imapTarget, imapOptions, redact } = require("../connect.js");

/** The checks, in the order they run and are reported. */
export const CHECKS = ["login", "sync", "send", "new", "sent", "flags"];

const DEFAULT_TIMEOUTS = { syncMs: 180_000, newMailMs: 120_000, sentMs: 60_000, flagsMs: 60_000 };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(ready, ms) {
  const start = Date.now();
  for (;;) {
    const value = await ready();
    if (value) return value;
    if (Date.now() - start > ms) return null;
    await sleep(100);
  }
}

/** Our own connection to the account, playing the user's other mail program. */
async function otherClient(account, settings) {
  const client = new ImapFlow(imapOptions(account, await imapTarget(account, settings), settings));
  client.on("error", () => {});
  await client.connect();
  return client;
}

/** UIDs in `path` whose Message-ID header is `messageId`. */
async function findOnServer(client, path, messageId) {
  const lock = await client.getMailboxLock(path);
  try {
    return (await client.search({ header: { "message-id": messageId } }, { uid: true })) || [];
  } finally {
    lock.release();
  }
}

/** The flags of one UID in `path`, as a Set (empty when it is gone). */
async function flagsOnServer(client, path, uid) {
  const lock = await client.getMailboxLock(path);
  try {
    const found = await client.fetchOne(String(uid), { flags: true }, { uid: true });
    return found ? found.flags : new Set();
  } finally {
    lock.release();
  }
}

/**
 * Run the matrix against one account. Answers `{ name, checks: { <check>: { ok, ms?, note? } },
 * capabilities, folders, error }`; a check that could not run because an earlier one failed is
 * left out.
 *
 * `options.configuration` is the module's settings (trusted servers and CAs: only the tests'
 * local Dovecot needs them), `options.deliver` a hook called after sending, for a test whose SMTP
 * server delivers nowhere.
 */
export async function checkAccount(spec, options = {}) {
  const timeouts = { ...DEFAULT_TIMEOUTS, ...(options.timeouts || {}) };
  const configuration = options.configuration || spec.configuration || {};
  const settings = connectionSettings(configuration);
  const progress = options.progress || (() => {});
  const user = { id: `live-${randomUUID()}`, role: 80 };
  const account = {
    id: 1,
    owner: user.id,
    min_role: 1,
    send_role: null,
    label: spec.name,
    mode: "mirror",
    // Only today's mail is fetched whole; older mail is headers, and no backfill runs.
    body_days: 1,
    save_sent: null,
    folders: null,
    enabled: true,
    ...Object.fromEntries(Object.entries(spec).filter(([k]) => k !== "name" && k !== "configuration")),
  };
  const result = { name: spec.name, checks: {}, capabilities: [], folders: {}, error: null };
  const record = (check, ok, extra = {}) => {
    result.checks[check] = { ok, ...extra };
    progress(`${spec.name}: ${check} ${ok ? "ok" : "FAILED"}${extra.ms !== undefined ? ` (${extra.ms} ms)` : ""}${extra.note ? `: ${extra.note}` : ""}`);
    return ok;
  };

  const fake = fakeDb();
  fake.tables.mail_accounts.push({ ...account });
  const files = fakeFiles();
  const log = fakeLog();
  const restore = installFakeTable(fake);
  const act = (name, payload) => actions[name]({ payload, user, settings: configuration, files, callerFiles: null });
  let runner = null;
  let other = null;
  const subject = `feldspar live test ${new Date().toISOString()} ${randomUUID().slice(0, 8)}`;
  const folderOf = (role) => fake.tables.mail_folders.find((f) => f.role === role);
  const rowIn = (folder) => folder && fake.tables.mail_messages.find((m) => m.folder === folder.id && m.subject === subject);

  try {
    // 1. login
    const tested = await act("mail_test_account", account);
    if (!record("login", tested.imap && tested.smtp === true, tested.error ? { note: tested.error } : {})) return result;

    other = await otherClient(account, settings);
    result.capabilities = [...other.capabilities.keys()].filter((c) => /^(IDLE|CONDSTORE|QRESYNC|SPECIAL-USE|UIDPLUS|MOVE|ENABLE|UTF8=ACCEPT|COMPRESS=DEFLATE)$/.test(c)).sort();

    // 2. sync
    let started = Date.now();
    runner = new AccountRunner({ db: fake.db, files, log, settings }, account, { backfillMs: 24 * 3600_000, mirrorPollMs: 24 * 3600_000, pollMs: 24 * 3600_000 });
    service.runners.set(account.id, runner);
    runner.start();
    const synced = await until(() => runner.state === "idle" && !runner.passing, timeouts.syncMs);
    const status = fake.tables.mail_accounts[0];
    for (const role of ["inbox", "sent", "drafts", "trash", "spam", "archive"]) {
      const f = folderOf(role);
      if (f) result.folders[role] = f.path;
    }
    const sync = `${fake.tables.mail_folders.length} folders, ${fake.tables.mail_messages.length} messages`;
    if (!record("sync", Boolean(synced), { ms: Date.now() - started, note: synced ? sync : `status ${status.status}: ${status.last_error || "no error"}` })) return result;

    // 3. send
    started = Date.now();
    const sent = await act("mail_send", { account_id: account.id, to: account.email, subject, text: "Sent by plugins/imap/test/live-matrix.mjs. Delete me.", html: "<p>Sent by <b>live-matrix.mjs</b>. Grüße. Delete me.</p>" });
    const sentAt = Date.now();
    const sendOk = Array.isArray(sent.accepted) && sent.accepted.length > 0;
    if (!record("send", sendOk, { ms: sentAt - started, note: sent.saved ? "the module saved the Sent copy" : sent.save_error ? `copy: ${sent.save_error}` : "no copy saved (server files it)" })) return result;
    if (options.deliver) await options.deliver();

    // 4. new mail, by IDLE: no wake(), no poll.
    const inbox = folderOf("inbox") || fake.tables.mail_folders.find((f) => f.path === "INBOX");
    const arrived = await until(() => rowIn(inbox), timeouts.newMailMs);
    record("new", Boolean(arrived), arrived ? { ms: Date.now() - sentAt } : { note: `no row in INBOX after ${timeouts.newMailMs / 1000} s` });

    // 5. the Sent copy on the server: exactly one, either way.
    const sentFolder = folderOf("sent");
    if (!sentFolder) {
      record("sent", false, { note: "no Sent folder found (SPECIAL-USE or name)" });
    } else {
      const copies = await until(async () => {
        const uids = await findOnServer(other, sentFolder.path, sent.message_id);
        return uids.length ? uids : null;
      }, timeouts.sentMs);
      await sleep(2000);
      const uids = copies ? await findOnServer(other, sentFolder.path, sent.message_id) : [];
      const how = sent.saved ? "APPENDed by the module" : "filed by the server";
      record("sent", uids.length === 1, {
        note: uids.length === 1 ? `one copy in ${sentFolder.path}, ${how}` : uids.length ? `${uids.length} copies in ${sentFolder.path}: set save_sent = false for this server` : `no copy in ${sentFolder.path}: set save_sent = true for this server`,
      });
    }

    // 6. flags, both ways, on the INBOX copy.
    if (arrived) {
      started = Date.now();
      const lock = await other.getMailboxLock(inbox.path);
      try {
        await other.messageFlagsAdd(String(arrived.uid), ["\\Flagged"], { uid: true });
      } finally {
        lock.release();
      }
      const fromServer = await until(() => (fake.tables.mail_messages.find((m) => m.id === arrived.id) || {}).flagged === true, timeouts.flagsMs);
      const serverMs = Date.now() - started;
      await act("mail_set_flags", { message_ids: [arrived.id], seen: true });
      const toServer = (await flagsOnServer(other, inbox.path, arrived.uid)).has("\\Seen");
      record("flags", Boolean(fromServer) && toServer, {
        ms: fromServer ? serverMs : undefined,
        note: [fromServer ? "server → app by itself" : "server → app did not arrive", toServer ? "app → server" : "app → server failed"].join(", "),
      });
    }

    // 7. cleanup
    if (!options.keep) {
      await act("mail_sync_now", { account_id: account.id, wait: true });
      const ours = fake.tables.mail_messages.filter((m) => m.subject === subject && m.uid !== null && m.uid !== undefined).map((m) => m.id);
      if (ours.length) await act("mail_delete", { message_ids: ours, permanent: true }).catch((e) => progress(`${spec.name}: cleanup: ${redact(e, account)}`));
    }
  } catch (e) {
    result.error = redact(e, account);
    progress(`${spec.name}: ${result.error}`);
  } finally {
    if (runner) {
      await runner.stop().catch(() => {});
      service.runners.delete(account.id);
    }
    if (other) await other.logout().catch(() => other.close());
    restore();
  }
  return result;
}

/** One Markdown table row for TODO-imap.md M10.2: `| name | date | login | … | capabilities |`. */
export function reportRow(result, date = new Date().toISOString().slice(0, 10)) {
  const cell = (check) => {
    const c = result.checks[check];
    if (!c) return "–";
    const time = c.ms !== undefined && (check === "new" || check === "flags") ? ` ${(c.ms / 1000).toFixed(1)} s` : "";
    return `${c.ok ? "✓" : "✗"}${time}`;
  };
  const notes = [...CHECKS.map((c) => result.checks[c] && !result.checks[c].ok && result.checks[c].note && `${c}: ${result.checks[c].note}`), result.error].filter(Boolean);
  const sent = result.checks.sent && result.checks.sent.ok ? result.checks.sent.note : null;
  if (sent) notes.push(sent);
  const pipe = (s) => String(s).replace(/\|/g, "\\|").replace(/\n/g, " ");
  return `| ${pipe(result.name)} | ${date} | ${CHECKS.map(cell).join(" | ")} | ${result.capabilities.join(" ") || "–"} | ${pipe(notes.join("; ")) || ""} |`;
}

export const REPORT_HEADER = [
  "| server | date | login | sync | send | new mail | Sent copy | flags | capabilities | notes |",
  "|---|---|---|---|---|---|---|---|---|---|",
].join("\n");

async function main(argv) {
  const args = argv.slice(2);
  const file = args.find((a) => !a.startsWith("--") && args[args.indexOf(a) - 1] !== "--only");
  if (!file) {
    console.error("usage: node test/live-matrix.mjs accounts.json [--only <name>] [--keep] [--json]");
    return 2;
  }
  const only = args.includes("--only") ? args[args.indexOf("--only") + 1] : null;
  const accounts = JSON.parse(readFileSync(file, "utf8")).filter((a) => !only || a.name === only);
  const results = [];
  for (const spec of accounts) {
    results.push(await checkAccount(spec, { keep: args.includes("--keep"), progress: (line) => console.error(line) }));
  }
  if (args.includes("--json")) console.log(JSON.stringify(results, null, 2));
  console.log(REPORT_HEADER);
  for (const r of results) console.log(reportRow(r));
  return results.every((r) => !r.error && CHECKS.every((c) => !r.checks[c] || r.checks[c].ok)) ? 0 : 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv).then((code) => process.exit(code));
}
