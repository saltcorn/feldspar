// The SMTP sink of the M1.4 spike: 127.0.0.1:10465 (TLS) and 127.0.0.1:10587
// (STARTTLS required), AUTH PLAIN and LOGIN for one user whose password has
// non-ASCII characters. Every accepted mail is written to run/sink/<n>.eml,
// and one JSON line per mail to run/sink/log.jsonl.
//
//   node servers/smtp-sink.mjs     (stops on SIGTERM)

import { SMTPServer } from "smtp-server";
import { readFileSync, writeFileSync, appendFileSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const run = join(dirname(fileURLToPath(import.meta.url)), "../run");
const sink = join(run, "sink");
mkdirSync(sink, { recursive: true });
const USER = "spike";
const PASSWORD = "Grüße-€-pässwort";
let count = 0;

function server(port, secure) {
  const smtp = new SMTPServer({
    secure,
    key: readFileSync(join(run, "certs/server.key")),
    cert: readFileSync(join(run, "certs/server.crt")),
    authMethods: ["PLAIN", "LOGIN"],
    // Refuse AUTH before TLS on 587, as a real submission server does.
    allowInsecureAuth: false,
    logger: false,
    onAuth(auth, session, done) {
      if (auth.username === USER && auth.password === PASSWORD) return done(null, { user: USER });
      appendFileSync(join(sink, "log.jsonl"), JSON.stringify({ port, refused: auth.method, username: auth.username }) + "\n");
      done(new Error("Invalid username or password"));
    },
    onData(stream, session, done) {
      const chunks = [];
      stream.on("data", (c) => chunks.push(c));
      stream.on("end", () => {
        const file = `${++count}.eml`;
        const source = Buffer.concat(chunks);
        writeFileSync(join(sink, file), source);
        appendFileSync(
          join(sink, "log.jsonl"),
          JSON.stringify({ port, file, secure: session.secure, auth: session.user, from: session.envelope.mailFrom.address, to: session.envelope.rcptTo.map((r) => r.address), size: source.length }) + "\n",
        );
        done();
      });
    },
  });
  smtp.on("error", (e) => console.error(`smtp ${port}:`, e.message));
  smtp.listen(port, "127.0.0.1", () => console.log(`smtp sink on ${port} (${secure ? "tls" : "starttls"})`));
  return smtp;
}

const servers = [server(10465, true), server(10587, false)];
process.on("SIGTERM", () => {
  for (const s of servers) s.close();
  process.exit(0);
});
