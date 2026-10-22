// An SMTP sink for the mail module's tests (TODO-imap.md M9.3, M9.5): 127.0.0.1, one port with
// TLS (465's role) and one with STARTTLS required (587's), AUTH PLAIN and LOGIN for the user
// `sender`, whose password has non-ASCII characters (§5.7: nodemailer must send it as UTF-8).
// The certificate is the test Dovecot's (`certs.sh`), so the same CA verifies both.
//
// In a test:
//
//   const sink = await startSink({ tls: 10465, starttls: 10587 });
//   … sink.messages: [{ port, secure, user, from, to, source }] …
//   await sink.close();
//
// As a process (the Rust test), writing each mail to <dir>/<n>.eml and a JSON line per mail
// to <dir>/log.jsonl; stops on SIGTERM:
//
//   node test/servers/smtp-sink.mjs <dir> [tls-port] [starttls-port]

import { SMTPServer } from "smtp-server";
import { readFileSync, writeFileSync, appendFileSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const here = dirname(fileURLToPath(import.meta.url));
export const SENDER = "sender";
export const PASSWORD = "Grüße-€-pässwort";

/** Start the sink. `dir`, when given, is where each mail is written as well. */
export async function startSink({ tls = 10465, starttls = 10587, dir = null } = {}) {
  const certs = join(here, "run", "certs");
  execFileSync(join(here, "certs.sh"), [certs]);
  if (dir) mkdirSync(dir, { recursive: true });
  const messages = [];
  const refused = [];
  const server = (port, secure) =>
    new Promise((resolve, reject) => {
      const smtp = new SMTPServer({
        secure,
        key: readFileSync(join(certs, "server.key")),
        cert: readFileSync(join(certs, "server.crt")),
        authMethods: ["PLAIN", "LOGIN"],
        // AUTH only after TLS, as a real submission server does.
        allowInsecureAuth: false,
        logger: false,
        onAuth(auth, session, done) {
          if (auth.username === SENDER && auth.password === PASSWORD) return done(null, { user: SENDER });
          refused.push({ port, method: auth.method, username: auth.username });
          done(new Error("Invalid username or password"));
        },
        onData(stream, session, done) {
          const chunks = [];
          stream.on("data", (c) => chunks.push(c));
          stream.on("end", () => {
            const message = {
              port,
              secure: session.secure,
              user: session.user,
              from: session.envelope.mailFrom.address,
              to: session.envelope.rcptTo.map((r) => r.address),
              source: Buffer.concat(chunks),
            };
            messages.push(message);
            if (dir) {
              const file = `${messages.length}.eml`;
              writeFileSync(join(dir, file), message.source);
              const { source, ...line } = message;
              appendFileSync(join(dir, "log.jsonl"), JSON.stringify({ ...line, file, size: source.length }) + "\n");
            }
            done();
          });
        },
      });
      smtp.on("error", () => {});
      smtp.listen(port, "127.0.0.1", (e) => (e ? reject(e) : resolve(smtp)));
    });
  const servers = [await server(tls, true), await server(starttls, false)];
  return {
    messages,
    refused,
    close: () => Promise.all(servers.map((s) => new Promise((r) => s.close(r)))),
  };
}

if (process.argv[1] === fileURLToPath(import.meta.url) && process.argv[2]) {
  const [dir, tls, starttls] = process.argv.slice(2);
  const sink = await startSink({ dir, tls: Number(tls) || 10465, starttls: Number(starttls) || 10587 });
  console.log("smtp sink ready");
  process.on("SIGTERM", async () => {
    await sink.close();
    process.exit(0);
  });
}
