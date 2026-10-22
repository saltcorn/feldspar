// The live-server matrix script (test/live-matrix.mjs, TODO-imap.md M10.2) run against the test
// Dovecot and SMTP sink, so the script itself is known to work before somebody points it at a
// real account. The sink delivers nowhere, so the test plays the mail server and APPENDs what was
// sent to the INBOX. Skipped unless the test Dovecot runs (test/servers/dovecot.sh start).

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { PASSWORD, CA, skip, reset, append, as } from "./dovecot.mjs";
import { startSink, SENDER } from "./servers/smtp-sink.mjs";
import { checkAccount, reportRow, REPORT_HEADER, CHECKS } from "./live-matrix.mjs";

const USER = "live";
// Ports of its own: mirror.test.mjs's sink may be running at the same time.
const SMTP_TLS = 10466;
const SMTP_STARTTLS = 10588;

const spec = {
  name: "Dovecot (test)",
  email: "live@example.com",
  display_name: "Live Test",
  imap_host: "127.0.0.1",
  imap_port: 11993,
  imap_security: "tls",
  imap_user: USER,
  imap_password: PASSWORD,
  smtp_host: "127.0.0.1",
  smtp_port: SMTP_TLS,
  smtp_security: "tls",
  smtp_user: SENDER,
  smtp_password: PASSWORD,
};

const configuration = () => ({
  trusted_servers: `127.0.0.1:11993\n127.0.0.1:${SMTP_TLS}`,
  ca_certificates: readFileSync(CA, "utf8"),
});

test("the live matrix passes every check against the test Dovecot, cleans up, and never prints the password", { skip, timeout: 120_000 }, async () => {
  await reset(USER);
  await append(USER, "INBOX", [Buffer.from("From: a@example.com\r\nTo: live@example.com\r\nSubject: already there\r\n\r\nhi\r\n")]);
  const sink = await startSink({ tls: SMTP_TLS, starttls: SMTP_STARTTLS });
  const lines = [];
  try {
    const result = await checkAccount(spec, {
      configuration: configuration(),
      progress: (l) => lines.push(l),
      deliver: async () => append(USER, "INBOX", sink.messages.map((m) => m.source)),
    });
    assert.equal(result.error, null, lines.join("\n"));
    for (const check of CHECKS) assert.equal(result.checks[check] && result.checks[check].ok, true, `${check}: ${JSON.stringify(result.checks[check])}\n${lines.join("\n")}`);
    assert.ok(result.checks.new.ms < 5000, `new mail took ${result.checks.new.ms} ms (§1: within seconds)`);
    assert.ok(result.checks.flags.ms < 10_000, `a server flag took ${result.checks.flags.ms} ms`);
    assert.match(result.checks.sent.note, /APPENDed by the module/);
    assert.ok(result.capabilities.includes("IDLE"), result.capabilities.join(" "));
    assert.equal(result.folders.sent, "Sent");
    assert.equal(sink.messages.length, 1, "one mail sent");

    // The report: a row under the header, with as many cells, and no password anywhere.
    const row = reportRow(result, "2026-10-22");
    const cells = (line) => line.split(/(?<!\\)\|/).length;
    assert.equal(cells(row), cells(REPORT_HEADER.split("\n")[0]));
    assert.match(row, /^\| Dovecot \(test\) \| 2026-10-22 \| ✓ \| ✓ \| ✓ \| ✓ \d+\.\d s \| ✓ \| ✓ \d+\.\d s \|/);
    for (const text of [row, JSON.stringify(result), ...lines]) assert.ok(!text.includes(PASSWORD), text);

    // Cleanup: the test mail is gone from INBOX and Sent; the mail that was there stays.
    const left = await as(USER, async (c) => {
      const out = {};
      for (const path of ["INBOX", "Sent"]) {
        const lock = await c.getMailboxLock(path);
        try {
          out[path] = c.mailbox.exists ? (await c.fetchAll("1:*", { envelope: true })).map((m) => m.envelope.subject) : [];
        } finally {
          lock.release();
        }
      }
      return out;
    });
    assert.deepEqual(left, { INBOX: ["already there"], Sent: [] });
  } finally {
    await sink.close();
  }
});

test("a failed login stops the matrix at login, with the server's reason and no password", { skip, timeout: 60_000 }, async () => {
  const wrong = "Wrong-pässwort-123";
  const result = await checkAccount({ ...spec, imap_password: wrong, smtp_host: "" }, { configuration: configuration() });
  assert.equal(result.checks.login.ok, false);
  assert.deepEqual(Object.keys(result.checks), ["login"]);
  const row = reportRow(result);
  assert.match(row, /\| ✗ \| – \| – \| – \| – \| – \|/);
  assert.match(row, /login: IMAP: /);
  assert.ok(!row.includes(wrong) && !JSON.stringify(result).includes(wrong));
});
