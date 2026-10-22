// What the tests that run against the test Dovecot (test/servers/dovecot.sh) share: whether it
// runs, the module's settings that trust it, another IMAP client to play the user's other mail
// program (or the mail server delivering), a mail to send, and waiting for a condition.

import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const require = createRequire(import.meta.url);
const { ImapFlow } = require("imapflow");
const { connectionSettings } = require("../connect.js");

const here = dirname(fileURLToPath(import.meta.url));
export const DOVECOT = join(here, "servers", "dovecot.sh");
export const CA = join(here, "servers", "run", "certs", "ca.crt");
export const PASSWORD = "Grüße-€-pässwort";

export const up = (() => {
  try {
    execFileSync(DOVECOT, ["running"], { stdio: "ignore" });
    return existsSync(CA);
  } catch {
    return false;
  }
})();
export const skip = up ? false : "the test Dovecot is not running (test/servers/dovecot.sh start)";

/** The module's configuration: the local Dovecot and the SMTP sink trusted, with their CA. */
export const configuration = () => ({
  trusted_servers: "127.0.0.1:11993\n127.0.0.1:11143\n127.0.0.1:10465\n127.0.0.1:10587",
  ca_certificates: readFileSync(CA, "utf8"),
});
export const settings = () => connectionSettings(configuration());

/** A client of our own for `user`. */
export async function admin(user) {
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

/** Run `work(client)` as `user`, logged out afterwards. */
export async function as(user, work) {
  const c = await admin(user);
  try {
    return await work(c);
  } finally {
    await c.logout().catch(() => c.close());
  }
}

/** `user`'s mailboxes emptied, and every one not in `keep` deleted. */
export async function reset(user, keep = ["INBOX", "Sent", "Drafts", "Trash", "Junk", "Processed", "Errors"]) {
  await as(user, async (c) => {
    const boxes = (await c.list()).sort((a, b) => b.path.length - a.path.length);
    for (const box of boxes) {
      if (!keep.includes(box.path)) {
        await c.mailboxDelete(box.path).catch(() => {});
        continue;
      }
      const lock = await c.getMailboxLock(box.path);
      try {
        if (c.mailbox.exists) await c.messageDelete("1:*");
      } finally {
        lock.release();
      }
    }
  });
}

/** Append `sources` to `folder` of `user`, with `flags` and an INTERNALDATE of `date`. */
export async function append(user, folder, sources, { flags = [], date } = {}) {
  await as(user, async (c) => {
    for (const s of sources) await c.append(folder, s, flags, date);
  });
}

let serial = 0;
/** A mail's source: plain text, or with one attachment. */
export function mail({ subject, body = "hello", messageId, attachment, date, inReplyTo, references } = {}) {
  serial += 1;
  const id = messageId || `<t${Date.now()}.${serial}@test.example>`;
  const head = [
    "From: Shop <shop@example.com>",
    "To: someone@example.com",
    `Subject: ${subject || `mail ${serial}`}`,
    `Message-ID: ${id}`,
    `Date: ${(date || new Date()).toUTCString()}`,
    "MIME-Version: 1.0",
  ];
  if (inReplyTo) head.push(`In-Reply-To: ${inReplyTo}`);
  if (references) head.push(`References: ${references}`);
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

/** Wait until `ready()` answers something truthy, and answer it. */
export async function until(what, ready, ms = 15_000) {
  const start = Date.now();
  for (;;) {
    const value = await ready();
    if (value) return value;
    if (Date.now() - start > ms) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
