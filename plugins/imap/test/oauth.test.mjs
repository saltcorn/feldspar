// OAuth2 / XOAUTH2 accounts (TODO-imap.md M10.3, `oauth.js`): the consent request an action
// asks the server for, renewing the access token, and logging in to IMAP and SMTP with it —
// against a fake token endpoint, a fake IMAP server that knows only AUTH=XOAUTH2 (as
// outlook.office365.com does) and the SMTP library with XOAUTH2 only.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { createServer as createHttp } from "node:http";
import { createServer as createTls } from "node:tls";
import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { fakeDb, fakeFiles, fakeLog, installFakeTable } from "./fake-host.mjs";

const require = createRequire(import.meta.url);
const { SMTPServer } = require("smtp-server");
const oauth = require("../oauth.js");
const { imapOptions, smtpOptions, redact, connectionSettings } = require("../connect.js");
const { AccountRunner } = require("../account.js");
const { actions } = require("../actions.js");

const here = dirname(fileURLToPath(import.meta.url));
const certs = join(here, "servers", "run", "certs");

const MS = { microsoft_client_id: "cid", microsoft_client_secret: "csecret", microsoft_tenant: "consumers" };

// ------------------------------------------------------------------------------ the request

test("the consent request: the provider's addresses, scopes, hint and where the tokens go", () => {
  const account = { id: 7, email: "ada@outlook.com", imap_host: null, smtp_host: "smtp.example.com" };
  const request = oauth.consentRequest(account, null, MS, "/settings/mail");
  assert.equal(request.authorization_url, "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize");
  assert.equal(request.token_url, "https://login.microsoftonline.com/consumers/oauth2/v2.0/token");
  assert.equal(request.client_id, "cid");
  assert.equal(request.client_secret, "csecret");
  assert.match(request.scope, /offline_access/);
  assert.match(request.scope, /IMAP\.AccessAsUser\.All/);
  assert.match(request.scope, /SMTP\.Send/);
  assert.deepEqual(request.params, { prompt: "select_account", login_hint: "ada@outlook.com" });
  assert.equal(request.return_to, "/settings/mail");
  assert.deepEqual(request.store, {
    table: "mail_accounts",
    id: 7,
    fields: { access_token: "oauth_access_token", refresh_token: "oauth_refresh_token", expires_at: "oauth_expires_at" },
    // The IMAP server the account lacked; its own SMTP server is kept.
    set: { auth: "oauth2", oauth_provider: "microsoft", imap_host: "outlook.office365.com", imap_port: 993, imap_security: "tls" },
  });

  const google = oauth.consentRequest({ id: 8, email: "bo@gmail.com" }, "google", { google_client_id: "g" }, null);
  assert.equal(google.scope, "https://mail.google.com/");
  assert.equal(google.params.access_type, "offline", "or Google grants no refresh token");
  assert.equal(google.return_to, "/");
  assert.equal(google.store.set.smtp_host, "smtp.gmail.com");

  assert.throws(() => oauth.consentRequest(account, "google", MS), /Google \(Gmail\) is not set up/);
  assert.throws(() => oauth.consentRequest(account, "yahoo", MS), /no OAuth provider "yahoo"/);
  assert.equal(oauth.providerOf({ email: "x@hotmail.de" }), "microsoft");
  assert.equal(oauth.providerOf({ imap_host: "outlook.office365.com" }), "microsoft");
  assert.equal(oauth.providerOf({ email: "x@googlemail.com" }), "google");
  assert.equal(oauth.providerOf({ email: "x@ionos.de" }), null);
  assert.equal(oauth.providerOf({ email: "x@gmail.com", oauth_provider: "microsoft" }), "microsoft");
});

test("imapflow and nodemailer are given the token, and no error shows it", () => {
  const target = { address: "1.2.3.4", port: 993, secure: true, servername: "outlook.office365.com" };
  const account = { auth: "oauth2", email: "ada@outlook.com", oauth_access_token: "AT-secret-123", oauth_refresh_token: "RT-secret-456", imap_password: "unused" };
  assert.deepEqual(imapOptions(account, target, {}).auth, { user: "ada@outlook.com", accessToken: "AT-secret-123" });
  assert.deepEqual(smtpOptions(account, { ...target, port: 587, secure: false }, {}).auth, {
    type: "OAuth2",
    user: "ada@outlook.com",
    accessToken: "AT-secret-123",
  });
  const said = redact(new Error("refused AT-secret-123 and RT-secret-456"), account);
  assert.equal(said, "refused ••• and •••");
  // A password account is as it was.
  assert.deepEqual(imapOptions({ email: "a@b.c", imap_password: "pw" }, target, {}).auth, { user: "a@b.c", pass: "pw" });
});

// ------------------------------------------------------------------------------ renewing

/** A `fetch` that answers the token endpoint from `answer(form)`, recording each request. */
function fakeFetch(answer) {
  const calls = [];
  const fetch = async (url, init) => {
    const form = Object.fromEntries(new URLSearchParams(init.body));
    calls.push({ url, form });
    await new Promise((r) => setTimeout(r, 10));
    const [status, body] = answer(form);
    return { ok: status === 200, status, json: async () => body };
  };
  return { fetch, calls };
}

test("an access token is renewed when it is about to expire, once at a time, and written back", async () => {
  const fake = fakeDb();
  const now = Date.parse("2026-10-23T10:00:00Z");
  const account = {
    id: 1,
    auth: "oauth2",
    oauth_provider: "microsoft",
    oauth_access_token: "old",
    oauth_refresh_token: "rt-1",
    oauth_expires_at: new Date(now + 2 * 60_000).toISOString(),
  };
  fake.tables.mail_accounts.push({ ...account });
  const { fetch, calls } = fakeFetch((form) => [200, { access_token: `new-${form.refresh_token}`, refresh_token: "rt-2", expires_in: 3600 }]);

  // Good for another 10 minutes: used as it is.
  const later = { ...account, oauth_expires_at: new Date(now + 10 * 60_000).toISOString() };
  await oauth.fresh(later, MS, fake.db, { fetch, now: () => now });
  assert.equal(calls.length, 0);

  // Two minutes left: renewed, by one request however many ask at once.
  const [a, b] = await Promise.all([oauth.fresh(account, MS, fake.db, { fetch, now: () => now }), oauth.fresh(account, MS, fake.db, { fetch, now: () => now })]);
  assert.equal(calls.length, 1);
  assert.equal(a.oauth_access_token, "new-rt-1");
  assert.equal(b.oauth_refresh_token, "rt-2", "Microsoft's new refresh token is kept");
  assert.equal(calls[0].url, "https://login.microsoftonline.com/consumers/oauth2/v2.0/token");
  assert.deepEqual(calls[0].form, {
    grant_type: "refresh_token",
    refresh_token: "rt-1",
    client_id: "cid",
    client_secret: "csecret",
    scope: oauth.PROVIDERS.microsoft.scope,
  });
  const row = fake.tables.mail_accounts[0];
  assert.equal(row.oauth_access_token, "new-rt-1");
  assert.equal(row.oauth_refresh_token, "rt-2");
  assert.equal(row.oauth_expires_at, new Date(now + 3600_000).toISOString());

  // A refusal that only a new consent mends.
  const refused = fakeFetch(() => [400, { error: "invalid_grant", error_description: "AADSTS70000: the grant was revoked.\nTrace ID: 1" }]);
  await assert.rejects(
    oauth.fresh({ ...account, id: 2, oauth_expires_at: null, oauth_access_token: null }, MS, fake.db, { fetch: refused.fetch }),
    (e) => e.reconnect === true && e.authenticationFailed === true && /refused to renew .*AADSTS70000: the grant was revoked\.\); connect the account again/.test(e.message),
  );
  // One worth another try later is not that.
  const busy = fakeFetch(() => [503, { error: "temporarily_unavailable" }]);
  await assert.rejects(oauth.fresh({ ...account, id: 3, oauth_access_token: null }, MS, fake.db, { fetch: busy.fetch }), (e) => !e.reconnect && /did not renew/.test(e.message));
  // A password account is never touched.
  await oauth.fresh({ id: 4, auth: "password" }, MS, fake.db, { fetch: busy.fetch });
  assert.equal(busy.calls.length, 1);
});

// ------------------------------------------------------------------------------ servers

/** A token endpoint on a real port: renews `rt-*` refresh tokens into `good`, refuses `dead`. */
async function tokenEndpoint() {
  const seen = [];
  const server = createHttp((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const form = Object.fromEntries(new URLSearchParams(body));
      seen.push(form);
      res.setHeader("content-type", "application/json");
      if (form.refresh_token === "dead") {
        res.statusCode = 400;
        return res.end(JSON.stringify({ error: "invalid_grant", error_description: "The refresh token has expired" }));
      }
      res.end(JSON.stringify({ access_token: "good", refresh_token: "rt-next", expires_in: 3600 }));
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return { url: `http://127.0.0.1:${server.address().port}/token`, seen, close: () => new Promise((r) => server.close(r)) };
}

/** An IMAP server that offers only AUTH=XOAUTH2 and accepts the access token `good`: enough
 * of IMAP for a login, one empty INBOX and IDLE. */
async function xoauthImap() {
  const tokens = [];
  const sockets = new Set();
  const server = createTls({ key: readFileSync(join(certs, "server.key")), cert: readFileSync(join(certs, "server.crt")) }, (socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => {});
    const say = (line) => socket.write(`${line}\r\n`);
    let idling = null;
    let buffer = "";
    say("* OK [CAPABILITY IMAP4rev1 SASL-IR AUTH=XOAUTH2 LOGINDISABLED] fake Outlook ready");
    socket.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      let at;
      while ((at = buffer.indexOf("\r\n")) >= 0) {
        const line = buffer.slice(0, at);
        buffer = buffer.slice(at + 2);
        if (idling) {
          if (line === "DONE") say(`${idling} OK IDLE done`);
          idling = null;
          continue;
        }
        const [tag, command = "", ...args] = line.split(" ");
        const ok = (text = "done") => say(`${tag} OK ${text}`);
        switch (command.toUpperCase()) {
          case "CAPABILITY":
            say("* CAPABILITY IMAP4rev1 SASL-IR AUTH=XOAUTH2 LOGINDISABLED");
            ok();
            break;
          case "LOGIN":
            say(`${tag} NO LOGIN failed: basic authentication is disabled`);
            break;
          case "AUTHENTICATE": {
            const fields = Buffer.from(args[1] || "", "base64").toString("utf8").split("\x01");
            const user = (fields.find((f) => f.startsWith("user=")) || "").slice(5);
            const token = (fields.find((f) => f.startsWith("auth=Bearer ")) || "").slice(12);
            tokens.push({ user, token });
            if (token === "good") say(`${tag} OK [CAPABILITY IMAP4rev1 IDLE UIDPLUS] AUTHENTICATE completed`);
            else say(`${tag} NO [AUTHENTICATIONFAILED] AUTHENTICATE failed`);
            break;
          }
          case "LIST":
            say('* LIST (\\HasNoChildren) "/" INBOX');
            ok();
            break;
          case "SELECT":
          case "EXAMINE":
            say("* 0 EXISTS");
            say("* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)");
            say("* OK [UIDVALIDITY 1] UIDs valid");
            say("* OK [UIDNEXT 1] Predicted next UID");
            ok("[READ-WRITE] SELECT completed");
            break;
          case "STATUS":
            say("* STATUS INBOX (MESSAGES 0 UIDNEXT 1 UIDVALIDITY 1)");
            ok();
            break;
          case "IDLE":
            idling = tag;
            say("+ idling");
            break;
          case "LOGOUT":
            say("* BYE fake signing off");
            ok();
            socket.end();
            break;
          default:
            ok();
        }
      }
    });
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return {
    port: server.address().port,
    tokens,
    close: () =>
      new Promise((r) => {
        for (const s of sockets) s.destroy();
        server.close(r);
      }),
  };
}

/** An SMTP submission server with XOAUTH2 only, accepting the token `good`. */
async function xoauthSmtp() {
  const logins = [];
  const smtp = new SMTPServer({
    secure: true,
    key: readFileSync(join(certs, "server.key")),
    cert: readFileSync(join(certs, "server.crt")),
    authMethods: ["XOAUTH2"],
    logger: false,
    onAuth(auth, session, done) {
      logins.push({ method: auth.method, user: auth.username, token: auth.accessToken });
      if (auth.accessToken === "good") return done(null, { user: auth.username });
      done(null, { data: { status: "401", schemes: "bearer" } });
    },
    onData(stream, session, done) {
      stream.resume();
      stream.on("end", () => done());
    },
  });
  await new Promise((r) => smtp.listen(0, "127.0.0.1", r));
  return { port: smtp.server.address().port, logins, close: () => new Promise((r) => smtp.close(r)) };
}

/** Everything a test below needs: the servers, the module's settings that trust them, and an
 * OAuth2 account on them. */
async function world() {
  execFileSync(join(here, "servers", "certs.sh"), [certs]);
  const token = await tokenEndpoint();
  const imap = await xoauthImap();
  const smtp = await xoauthSmtp();
  const configuration = {
    ...MS,
    trusted_servers: `127.0.0.1:${imap.port}\n127.0.0.1:${smtp.port}`,
    ca_certificates: readFileSync(join(certs, "ca.crt"), "utf8"),
    oauth_endpoints: { microsoft: { authorization_url: "http://127.0.0.1/authorize", token_url: token.url } },
  };
  const account = (extra = {}) => ({
    id: 1,
    owner: 5,
    email: "ada@outlook.com",
    mode: "mirror",
    enabled: true,
    auth: "oauth2",
    oauth_provider: "microsoft",
    imap_host: "127.0.0.1",
    imap_port: imap.port,
    imap_security: "tls",
    smtp_host: "127.0.0.1",
    smtp_port: smtp.port,
    smtp_security: "tls",
    oauth_access_token: "stale",
    oauth_refresh_token: "rt-1",
    oauth_expires_at: new Date(Date.now() + 3600_000).toISOString(),
    ...extra,
  });
  const close = () => Promise.all([token.close(), imap.close(), smtp.close()]);
  return { token, imap, smtp, configuration, account, close };
}

const until = async (what, check, ms = 15_000) => {
  const end = Date.now() + ms;
  while (!check()) {
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 25));
  }
};

test("a token the server refuses is renewed once, and the account connects with the new one", async () => {
  const w = await world();
  const fake = fakeDb();
  const account = w.account();
  fake.tables.mail_accounts.push({ ...account });
  const host = { db: fake.db, files: fakeFiles(), log: fakeLog(), settings: connectionSettings(w.configuration), configuration: w.configuration };
  const runner = new AccountRunner(host, account, { backoffFirstMs: 60_000 });
  try {
    runner.start();
    await until("idle", () => runner.state === "idle");
    // `stale` looked good by its expiry, and was refused; `good` came from the renewal.
    assert.deepEqual(
      w.imap.tokens.map((t) => t.token),
      ["stale", "good"],
    );
    assert.equal(w.imap.tokens[1].user, "ada@outlook.com");
    assert.equal(w.token.seen.length, 1);
    assert.equal(w.token.seen[0].refresh_token, "rt-1");
    const row = fake.tables.mail_accounts[0];
    assert.equal(row.oauth_access_token, "good");
    assert.equal(row.oauth_refresh_token, "rt-next");
    assert.ok(fake.tables.mail_folders.some((f) => f.path === "INBOX"));
  } finally {
    await runner.stop();
    await w.close();
  }
});

test("a renewal the provider refuses stops the account at auth_failed, saying to connect it again", async () => {
  const w = await world();
  const fake = fakeDb();
  const account = w.account({ oauth_refresh_token: "dead", oauth_expires_at: new Date(Date.now() - 1000).toISOString() });
  fake.tables.mail_accounts.push({ ...account });
  const host = { db: fake.db, files: fakeFiles(), log: fakeLog(), settings: connectionSettings(w.configuration), configuration: w.configuration };
  const runner = new AccountRunner(host, account, { backoffFirstMs: 60_000 });
  try {
    runner.start();
    await until("auth_failed", () => runner.state === "auth_failed");
    const row = fake.tables.mail_accounts[0];
    assert.match(row.last_error, /refused to renew the account's access \(The refresh token has expired\); connect the account again/);
    assert.equal(w.imap.tokens.length, 0, "no login with a token known to be dead");
  } finally {
    await runner.stop();
    await w.close();
  }
});

test("the actions renew an expired token and log in to IMAP and SMTP with it", async () => {
  const w = await world();
  const fake = fakeDb();
  const user = { id: 5, role: 80 };
  fake.tables.mail_accounts.push(w.account({ oauth_access_token: "expired", oauth_expires_at: new Date(Date.now() - 1000).toISOString() }));
  const restore = installFakeTable(fake);
  try {
    const result = await actions.mail_test_account({ payload: { account_id: 1 }, user, settings: w.configuration });
    assert.deepEqual(result, { imap: true, smtp: true, error: null });
    assert.deepEqual(w.imap.tokens, [{ user: "ada@outlook.com", token: "good" }]);
    assert.deepEqual(w.smtp.logins, [{ method: "XOAUTH2", user: "ada@outlook.com", token: "good" }]);
    assert.equal(fake.tables.mail_accounts[0].oauth_access_token, "good", "written back");
    assert.equal(w.token.seen.length, 1);
  } finally {
    restore();
    await w.close();
  }
});

test("a shared account's saved token or password goes only to its saved servers", async () => {
  const w = await world();
  const fake = fakeDb();
  // Shared (no owner), so a staff user who may read it may test it.
  fake.tables.mail_accounts.push(w.account({ owner: null, min_role: 80, oauth_access_token: "good" }));
  fake.tables.mail_accounts.push({
    id: 2, owner: null, min_role: 80, email: "team@example.com", auth: "password",
    imap_host: "127.0.0.1", imap_port: w.imap.port, imap_security: "tls", imap_password: "imap-secret",
    smtp_host: "127.0.0.1", smtp_port: w.smtp.port, smtp_security: "tls", smtp_password: "smtp-secret",
  });
  const restore = installFakeTable(fake);
  const test_ = (payload) => actions.mail_test_account({ payload, user: { id: 6, role: 40 }, settings: w.configuration });
  try {
    await assert.rejects(test_({ account_id: 1, imap_host: "imap.attacker.example" }), /only sent to the saved servers, and imap_host differ/);
    await assert.rejects(test_({ account_id: 1, smtp_port: 2525 }), /smtp_port differ/);
    await assert.rejects(test_({ account_id: 2, smtp_host: "smtp.attacker.example", smtp_password: "••••••••" }), /smtp_host differ/);
    await assert.rejects(test_({ account_id: 2, email: "someone@attacker.example" }), /email differ/);
    assert.equal(w.imap.tokens.length, 0, "nothing was sent anywhere");
    assert.equal(w.smtp.logins.length, 0);
    assert.equal(w.token.seen.length, 0, "not even a renewal");

    // The saved settings, given again (in another case), are not a change.
    const result = await test_({ account_id: 1, imap_host: "127.0.0.1", imap_port: String(w.imap.port), smtp_security: "TLS" });
    assert.deepEqual(result, { imap: true, smtp: true, error: null });
  } finally {
    restore();
    await w.close();
  }
});

test("mail_oauth_start asks the server for a consent flow, for the account's owner only", async () => {
  const fake = fakeDb();
  fake.tables.mail_accounts.push(
    { id: 1, owner: 5, email: "ada@outlook.com", auth: "password", min_role: 1 },
    { id: 2, owner: null, email: "team@outlook.com", auth: "password", min_role: 80 },
    { id: 3, owner: 5, email: "ada@ionos.de", auth: "password", min_role: 1 },
  );
  const restore = installFakeTable(fake);
  const asked = [];
  const begin = async (request) => {
    asked.push(request);
    return { url: "/_feldspar/oauth/consent/abc", expires_in: 600 };
  };
  const start = (user, payload) => actions.mail_oauth_start({ payload, user, settings: MS, oauth: { begin } });
  try {
    assert.deepEqual(await start({ id: 5, role: 80 }, { account_id: 1, return_to: "/mail/settings" }), {
      url: "/_feldspar/oauth/consent/abc",
      provider: "microsoft",
    });
    assert.equal(asked[0].store.id, 1);
    assert.equal(asked[0].return_to, "/mail/settings");
    assert.equal(asked[0].params.login_hint, "ada@outlook.com");

    // Another user does not see the account at all; a shared one is an admin's to connect.
    await assert.rejects(start({ id: 6, role: 80 }, { account_id: 1 }), /account 1 was not found/);
    await assert.rejects(start({ id: 6, role: 40 }, { account_id: 2 }), /only an admin may connect the shared account/);
    await start({ id: 9, role: 1 }, { account_id: 2 });
    // A provider is named, or found from the address.
    await assert.rejects(start({ id: 5, role: 80 }, { account_id: 3 }), /say which provider/);
    await assert.rejects(start({ id: 5, role: 80 }, { account_id: 3, provider: "google" }), /Google \(Gmail\) is not set up/);
    assert.equal(asked.length, 2);
  } finally {
    restore();
  }
});
