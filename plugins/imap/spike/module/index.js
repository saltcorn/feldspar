// Throwaway (TODO-imap.md M1.4): each action checks one claim §2.1 rests on,
// from inside feldspar's Deno worker. The harness is
// crates/sc-module/tests/imap_spike.rs; the findings are in TODO-imap.md M1.4.
//
// Every action takes its arguments as `row`.

// imapflow's logger, pino, calls os.hostname() when it is required. The host
// grants every module that one item of `sys`; before it did, this module could
// not load (TODO-imap.md M1.4, finding 1).

const { ImapFlow } = require("imapflow");
const { simpleParser } = require("mailparser");
const nodemailer = require("nodemailer");
const crypto = require("node:crypto");
const v8 = require("node:v8");

const heapMb = () => Math.round(v8.getHeapStatistics().used_heap_size / 1048576);

// §5.1's rules as far as a spike needs them: certificates verified (against
// the spike's own CA), the logger off, imapflow's timeouts set.
function imap({ port, ca, user, pass, loginMethod, servername }) {
  return new ImapFlow({
    host: "127.0.0.1",
    port,
    secure: port === 10993,
    doSTARTTLS: port === 10993 ? undefined : true,
    auth: { user, pass, ...(loginMethod ? { loginMethod } : {}) },
    // `servername`: connect to the checked IP, verify the certificate against
    // the name (§5.1, M3.3).
    tls: { ca, rejectUnauthorized: true, ...(servername ? { servername } : {}) },
    logger: false,
    connectionTimeout: 10_000,
    greetingTimeout: 10_000,
    socketTimeout: 5 * 60_000,
  });
}

// One IDLE connection held across calls, in module scope.
let idle = null;

module.exports = {
  sc_plugin_api_version: 1,
  plugin_name: "imap-spike",
  actions: {
    // imapflow over node:tls / STARTTLS, logging in with a UTF-8 password.
    imap_login: {
      run: async ({ row }) => {
        const client = imap(row);
        client.on("error", () => {});
        const t0 = Date.now();
        await client.connect();
        const result = {
          ms: Date.now() - t0,
          authenticated: client.authenticated,
          secure: client.secureConnection,
          capabilities: [...client.capabilities.keys()].sort(),
          folders: (await client.list()).map((m) => ({ path: m.path, specialUse: m.specialUse || null })),
        };
        await client.logout();
        return result;
      },
    },

    // A wrong password: the error, and whether the password leaks into it.
    imap_bad_login: {
      run: async ({ row }) => {
        const client = imap(row);
        client.on("error", () => {});
        try {
          await client.connect();
          return { error: null };
        } catch (e) {
          return { error: String(e.message), code: e.authenticationFailed ? "auth_failed" : e.code || null, response: e.responseText || null };
        } finally {
          try {
            client.close();
          } catch {}
        }
      },
    },

    // Open INBOX and IDLE; events are recorded with the time they arrived, so
    // the harness can tell "arrived while no call was running" from "arrived
    // when the next call woke the worker".
    idle_start: {
      run: async ({ row }) => {
        if (idle) throw new Error("already idling");
        const client = imap(row);
        const events = [];
        client.on("exists", (d) => events.push({ type: "exists", count: d.count, prevCount: d.prevCount, at: Date.now() }));
        client.on("close", () => events.push({ type: "close", at: Date.now() }));
        client.on("error", (e) => events.push({ type: "error", message: String(e.message), at: Date.now() }));
        await client.connect();
        const mailbox = await client.mailboxOpen("INBOX");
        idle = { client, events, startedAt: Date.now() };
        // Resolves only when IDLE ends; imapflow restarts IDLE by itself.
        client.idle().catch((e) => events.push({ type: "idle_error", message: String(e.message), at: Date.now() }));
        return { exists: mailbox.exists, uidValidity: String(mailbox.uidValidity), uidNext: mailbox.uidNext };
      },
    },

    idle_poll: {
      run: async () => ({
        now: Date.now(),
        usable: idle ? idle.client.usable : false,
        idling: idle ? idle.client.idling : false,
        events: idle ? idle.events : [],
      }),
    },

    idle_stop: {
      run: async () => {
        if (!idle) return { stopped: false };
        await idle.client.logout();
        idle = null;
        return { stopped: true };
      },
    },

    // UID FETCH the newest message's source on a second connection and parse
    // it: UIDs, a mailbox lock released in `finally`, mailparser on a Buffer.
    fetch_newest: {
      run: async ({ row }) => {
        const client = imap(row);
        client.on("error", () => {});
        await client.connect();
        const lock = await client.getMailboxLock("INBOX");
        try {
          const uids = await client.search({ all: true }, { uid: true });
          const uid = Math.max(...uids);
          const msg = await client.fetchOne(String(uid), { source: true, internalDate: true, flags: true }, { uid: true });
          const parsed = await simpleParser(msg.source);
          return { uid, size: msg.source.length, subject: parsed.subject, text: (parsed.text || "").trim(), internalDate: msg.internalDate };
        } finally {
          lock.release();
          try {
            await client.logout();
          } catch {}
        }
      },
    },

    // mailparser on a generated mail of `mb` megabytes (one binary attachment),
    // with the heap measured before, after and at the peak we can see.
    parse_big: {
      run: async ({ row }) => {
        const payload = crypto.randomBytes(Math.round(row.mb * 1048576 * 0.74)); // base64 grows it by 4/3
        const b64 = payload.toString("base64").replace(/.{76}/g, "$&\r\n");
        const source = Buffer.from(
          "From: a@feldspar.test\r\nTo: b@feldspar.test\r\nSubject: big\r\nMIME-Version: 1.0\r\n" +
            'Content-Type: multipart/mixed; boundary="b"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nA big one.\r\n' +
            '--b\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename="big.bin"\r\nContent-Transfer-Encoding: base64\r\n\r\n' +
            b64 +
            "\r\n--b--\r\n",
        );
        const before = heapMb();
        let peak = before;
        const sampler = setInterval(() => (peak = Math.max(peak, heapMb())), 5);
        const t0 = Date.now();
        const parsed = await simpleParser(source);
        const ms = Date.now() - t0;
        clearInterval(sampler);
        peak = Math.max(peak, heapMb());
        const att = parsed.attachments[0];
        const intact = att.content.equals(payload);
        const heapLimitMb = Math.round(v8.getHeapStatistics().heap_size_limit / 1048576);
        const rss = typeof process.memoryUsage === "function" ? Math.round(process.memoryUsage().rss / 1048576) : null;
        return { sourceMb: +(source.length / 1048576).toFixed(1), ms, heapBeforeMb: before, heapPeakMb: peak, heapAfterMb: heapMb(), heapLimitMb, rssMb: rss, text: parsed.text.trim(), attachmentBytes: att.size, intact };
      },
    },

    // nodemailer through 465 (TLS) or 587 (STARTTLS required), AUTH with a
    // UTF-8 password.
    smtp_send: {
      run: async ({ row }) => {
        const transport = nodemailer.createTransport({
          host: "127.0.0.1",
          port: row.port,
          secure: row.port === 10465,
          requireTLS: row.port !== 10465,
          auth: { user: row.user, pass: row.pass, ...(row.method ? { method: row.method } : {}) },
          tls: { ca: row.ca, rejectUnauthorized: true, ...(row.servername ? { servername: row.servername } : {}) },
          name: "feldspar.test",
          logger: false,
          connectionTimeout: 10_000,
          greetingTimeout: 10_000,
          socketTimeout: 60_000,
        });
        try {
          const info = await transport.sendMail({
            from: '"Ada Sender" <ada@feldspar.test>',
            to: "orders@feldspar.test",
            subject: `Grüße über ${row.port}`,
            text: "Ein Test mit Umlauten: äöü ß €.",
            html: "<p>Ein Test mit <b>Umlauten</b>: äöü ß €.</p>",
            attachments: [{ filename: "größe.bin", content: Buffer.from([0, 1, 2, 253, 254, 255]) }],
            messageId: `<spike-${row.port}@feldspar.test>`,
          });
          return { accepted: info.accepted, rejected: info.rejected, response: info.response, messageId: info.messageId };
        } catch (e) {
          return { error: String(e.message), code: e.code || null };
        } finally {
          transport.close();
        }
      },
    },
  },
};
