// **One account's life** (TODO-imap.md §5.1, M8.2): connect, find the folders, IDLE, poll,
// reconnect, and say how it is going in the account's status fields.
//
//   const runner = new AccountRunner({ db, files, log, settings }, accountRow);
//   runner.start();             // runs until stop()
//   runner.wake();              // sync now (mail_sync_now)
//   await runner.withFolder("INBOX", (client) => …);   // an action's IMAP work, queued
//   await runner.stop();
//
// One connection per account
// --------------------------
// §5.1 allows two: an IDLE connection and a short-lived one for everything else. An ingest
// account needs only the first. It IDLEs on the first watched folder; every 2 minutes, on
// `mail_sync_now` and when the server reports new mail, the runner takes the connection out of
// IDLE, syncs what is due (selecting the other watched folders in turn), selects the first one
// again and goes back to IDLE. Actions (`mail_set_flags`, `mail_attachment`) queue on the same
// connection through `withFolder`, so the account never has more than one connection open while
// the service runs, whatever the app does in parallel (M9.6).
//
// Failures
// --------
// - **imapflow does not reconnect by itself.** On `close` or `error` the runner waits and
//   connects again, with exponential backoff and jitter from 5 s up to 10 min. A connection that
//   lasted 10 minutes resets the backoff.
// - **Three login failures in a row** set `status = auth_failed` and stop the runner, because
//   repeated bad logins can get an account locked. The service starts it again when somebody
//   changes the row (§5.5).
// - **A refused server** (a private address, a wrong port: `connect.js`) is not retried either;
//   it sets `status = refused` and waits for the row to change.
// - Errors written to `last_error` have the passwords taken out (`connect.redact`).
//
// Status values: `connecting`, `syncing`, `idle`, `error` (and retrying), `auth_failed`,
// `refused`, `stopped`.

"use strict";

const { ImapFlow } = require("imapflow");
const { imapTarget, imapOptions, redact } = require("./connect.js");
const { syncFolder, folderRow, watchedFolders, applyFlags } = require("./sync.js");

const DEFAULTS = {
  /** How often the watched folders are synced in full, IDLE or not (§4: polled every 2 min). */
  pollMs: 2 * 60_000,
  /** Reconnect backoff (§5.1). */
  backoffFirstMs: 5_000,
  backoffMaxMs: 10 * 60_000,
  /** A connection that lasted this long resets the backoff. */
  backoffResetMs: 10 * 60_000,
  /** Login failures in a row before `auth_failed` (§5.1). */
  maxAuthFailures: 3,
  /** IMAP IDLE is restarted after this long (§5.1: 25 min; imapflow's `maxIdleTime`). */
  maxIdleMs: 25 * 60_000,
  /** How long the connection is quiet before IDLE starts. imapflow's default is 15 s, and mail
   * arriving in that time would wait for it: new mail must appear within seconds (§1). */
  autoIdleMs: 500,
  /** How long after a burst of flag or expunge notifications the folder is checked. */
  settleMs: 1_000,
};

/** Folder roles by SPECIAL-USE flag (RFC 6154). */
const SPECIAL_USE = {
  "\\Inbox": "inbox",
  "\\Sent": "sent",
  "\\Drafts": "drafts",
  "\\Trash": "trash",
  "\\Junk": "spam",
  "\\Archive": "archive",
  "\\All": null,
  "\\Flagged": null,
};

/** Folder roles by common name, after the flags (§7 `_special_folder`), compared on the last
 * segment of the path, lower-cased. */
const COMMON_NAMES = {
  sent: ["sent", "sent items", "sent messages", "sent mail", "gesendet", "gesendete objekte", "gesendete elemente", "envoyés", "éléments envoyés", "enviados", "inviata", "posta inviata", "verzonden", "skickat"],
  drafts: ["drafts", "draft", "entwürfe", "brouillons", "borradores", "bozze", "concepten", "utkast"],
  trash: ["trash", "deleted", "deleted items", "deleted messages", "bin", "papierkorb", "gelöschte objekte", "gelöschte elemente", "corbeille", "éléments supprimés", "papelera", "cestino", "prullenbak", "papperskorgen"],
  spam: ["spam", "junk", "junk e-mail", "junk email", "junk mail", "bulk mail", "spamverdacht", "unerwünscht", "courrier indésirable", "correo no deseado", "posta indesiderata", "ongewenste e-mail", "skräppost"],
  archive: ["archive", "archives", "archiv", "archivo", "archivio", "archief", "arkiv", "all mail"],
};

/** The role of one LIST entry: SPECIAL-USE first, then the name. */
function folderRole(entry) {
  if (String(entry.path).toUpperCase() === "INBOX") return "inbox";
  const flags = [...(entry.flags || []), entry.specialUse].filter(Boolean);
  for (const flag of flags) {
    const role = SPECIAL_USE[flag];
    if (role) return role;
  }
  const leaf = String(entry.name || entry.path.split(entry.delimiter || "/").pop()).toLowerCase();
  for (const [role, names] of Object.entries(COMMON_NAMES)) if (names.includes(leaf)) return role;
  return null;
}

/** Each role given to one folder only: the SPECIAL-USE one wins, then the shorter path. */
function assignRoles(entries) {
  const out = new Map();
  const taken = new Map();
  const ranked = entries
    .map((e) => ({ e, role: folderRole(e), flagged: Boolean(e.specialUse || (e.flags && [...e.flags].some((f) => SPECIAL_USE[f]))) }))
    .sort((a, b) => Number(b.flagged) - Number(a.flagged) || a.e.path.length - b.e.path.length);
  for (const { e, role } of ranked) {
    if (role && !taken.has(role)) {
      taken.set(role, e.path);
      out.set(e.path, role);
    } else {
      out.set(e.path, null);
    }
  }
  return out;
}

const sleep = (ms, signal) =>
  new Promise((resolve) => {
    if (signal && signal.aborted) return resolve();
    const timer = setTimeout(done, ms);
    function done() {
      clearTimeout(timer);
      if (signal) signal.removeEventListener("abort", done);
      resolve();
    }
    if (signal) signal.addEventListener("abort", done, { once: true });
  });

/** The next backoff delay: doubled, capped, with up to 25 % jitter either way. */
function nextBackoff(previous, opts) {
  const base = previous ? Math.min(previous * 2, opts.backoffMaxMs) : opts.backoffFirstMs;
  const jitter = base * 0.25 * (Math.random() * 2 - 1);
  return { base, delay: Math.max(1000, Math.round(base + jitter)) };
}

class AccountRunner {
  /**
   * @param {{ db, files, log, settings, now?: () => Date }} host the service's `ctx.db`,
   *   `ctx.files`, `ctx.log`, and the connection settings (`connect.connectionSettings`)
   * @param {object} account the `mail_accounts` row, secrets revealed
   * @param {object} [options] overrides of `DEFAULTS`, for tests
   */
  constructor(host, account, options = {}) {
    this.host = host;
    this.account = account;
    this.opts = { ...DEFAULTS, ...options };
    this.controller = new AbortController();
    this.client = null;
    this.state = "stopped";
    this.authFailures = 0;
    this.queue = Promise.resolve();
    this.folders = new Map();
    this.due = { new: new Set(), flags: new Set() };
    this.passScheduled = null;
    this.running = null;
    this.idleFolder = watchedFolders(account)[0];
    this.listeners = new Set();
  }

  get id() {
    return this.account.id;
  }

  get label() {
    return this.account.label || this.account.email || `account ${this.account.id}`;
  }

  /** Start the loop; answers at once. */
  start() {
    if (!this.running) this.running = this.loop().catch((e) => this.host.log.error(`${this.label}: ${e.stack || e}`));
    return this;
  }

  /** Stop: no more reconnects, the connection logged out, the loop finished. */
  async stop() {
    this.controller.abort();
    this.clearTimers();
    const client = this.client;
    if (client) {
      try {
        await Promise.race([client.logout(), sleep(5000)]);
      } catch (_) {
        // Logout is wrapped in its own try (§5.2): a dead connection is already logged out.
      }
      try {
        client.close();
      } catch (_) {}
    }
    await this.running;
    await this.setStatus("stopped").catch(() => {});
  }

  /** Sync every watched folder now (`mail_sync_now`). */
  wake() {
    for (const path of watchedFolders(this.account)) {
      this.due.new.add(path);
      this.due.flags.add(path);
    }
    this.schedulePass(0);
  }

  /** Wait for the next pass to finish; for tests and `mail_sync_now`'s `wait`. */
  settled() {
    return new Promise((resolve) => this.listeners.add(resolve));
  }

  // ---------------------------------------------------------------------------- status

  async setStatus(status, error) {
    const values = { status };
    if (error !== undefined) values.last_error = error;
    if (status === "idle") values.last_sync_at = new Date().toISOString();
    this.state = status;
    await this.host.db.mail_accounts.where({ id: this.account.id }).update(values);
  }

  // ---------------------------------------------------------------------------- the loop

  async loop() {
    const signal = this.controller.signal;
    let backoff = 0;
    while (!signal.aborted) {
      const started = Date.now();
      try {
        await this.setStatus("connecting");
        await this.session();
        // The connection closed by itself (the server, the network).
        if (signal.aborted) break;
        throw new Error("the connection was closed");
      } catch (e) {
        if (signal.aborted) break;
        const message = redact(e, this.account);
        if (e.refused) {
          await this.setStatus("refused", message).catch(() => {});
          this.host.log.warn(`${this.label}: ${message}`);
          return;
        }
        if (e.authenticationFailed) {
          this.authFailures += 1;
          if (this.authFailures >= this.opts.maxAuthFailures) {
            await this.setStatus("auth_failed", `login failed ${this.authFailures} times in a row; fix the password and save the account to try again: ${message}`).catch(() => {});
            this.host.log.warn(`${this.label}: login failed ${this.authFailures} times; not retrying until the account changes`);
            return;
          }
        }
        if (Date.now() - started >= this.opts.backoffResetMs) backoff = 0;
        const next = nextBackoff(backoff, this.opts);
        backoff = next.base;
        await this.setStatus("error", `${message} (retrying in ${Math.round(next.delay / 1000)} s)`).catch(() => {});
        this.host.log.warn(`${this.label}: ${message}; reconnecting in ${Math.round(next.delay / 1000)} s`);
        await sleep(next.delay, signal);
      }
    }
  }

  /** One connection's life: answers when it closes. */
  async session() {
    const settings = this.host.settings || {};
    const target = await imapTarget(this.account, settings);
    const client = new ImapFlow(
      imapOptions(this.account, target, settings, {
        maxIdleTime: this.opts.maxIdleMs,
        autoIdleDelay: this.opts.autoIdleMs,
        qresync: false,
      }),
    );
    // Always: without a listener a refused login's late `error` ends the worker (M1.4, 3).
    let closedResolve;
    const closed = new Promise((resolve) => (closedResolve = resolve));
    client.on("error", (e) => {
      this.host.log.debug(`${this.label}: ${redact(e, this.account)}`);
    });
    client.on("close", () => closedResolve());
    client.on("exists", (event) => {
      this.due.new.add(event.path);
      this.schedulePass(0);
    });
    client.on("expunge", (event) => {
      this.due.flags.add(event.path);
      this.schedulePass(this.opts.settleMs);
    });
    client.on("flags", (event) => {
      if (event.uid) this.flagEvent(event).catch((e) => this.host.log.warn(`${this.label}: ${e.message}`));
      else {
        this.due.flags.add(event.path);
        this.schedulePass(this.opts.settleMs);
      }
    });
    this.client = client;
    try {
      await client.connect();
      this.authFailures = 0;
      await this.discoverFolders();
      await client.mailboxOpen(this.idleFolder);
      this.wake();
      this.pollTimer = setInterval(() => this.wake(), this.opts.pollMs);
      await closed;
    } finally {
      this.clearTimers();
      this.client = null;
      try {
        client.close();
      } catch (_) {}
    }
  }

  clearTimers() {
    clearInterval(this.pollTimer);
    clearTimeout(this.passScheduled);
    this.passScheduled = null;
  }

  // ---------------------------------------------------------------------------- folders

  /** LIST, roles, and an upserted `mail_folders` row for each folder (§7). */
  async discoverFolders() {
    const { db } = this.host;
    const entries = (await this.client.list()).filter((e) => !(e.flags && e.flags.has && e.flags.has("\\Noselect")));
    const roles = assignRoles(entries);
    const watched = new Set(watchedFolders(this.account));
    for (const entry of entries) {
      const values = {
        name: entry.name,
        role: roles.get(entry.path),
        delimiter: entry.delimiter || null,
        sync: watched.has(entry.path),
      };
      let row = await folderRow(db, this.account, entry.path, values);
      if (["name", "role", "delimiter", "sync"].some((k) => row[k] !== values[k])) {
        await db.mail_folders.where({ id: row.id }).update(values);
        row = { ...row, ...values };
      }
      this.folders.set(entry.path, row);
    }
    for (const path of watched) {
      if (!this.folders.has(path)) {
        throw new Error(`the folder ${path} is not on the server (the account's folders list names it)`);
      }
    }
  }

  // ---------------------------------------------------------------------------- passes

  /** Run a pass in `ms`, unless one is already scheduled sooner. */
  schedulePass(ms) {
    if (this.controller.signal.aborted || !this.client) return;
    if (this.passScheduled && ms > 0) return;
    clearTimeout(this.passScheduled);
    this.passScheduled = setTimeout(() => {
      this.passScheduled = null;
      this.enqueue(() => this.pass()).catch((e) => this.passFailed(e));
    }, ms);
  }

  /** Queue `work` on the connection, after whatever is running. */
  enqueue(work) {
    const run = this.queue.then(work, work);
    this.queue = run.catch(() => {});
    return run;
  }

  passFailed(e) {
    const message = redact(e, this.account);
    this.host.log.warn(`${this.label}: sync failed: ${message}`);
    this.setStatus("error", message).catch(() => {});
    // A failed pass on a connection that is still up is retried at the next poll; one whose
    // connection went is retried by the loop's reconnect.
  }

  /** Sync what is due: the folders with new mail, then those whose flags are due. */
  async pass() {
    const client = this.client;
    if (!client || !client.usable) return;
    const due = this.due;
    this.due = { new: new Set(), flags: new Set() };
    const paths = [...new Set([...due.new, ...due.flags])].filter((p) => watchedFolders(this.account).includes(p));
    if (!paths.length) return;
    await this.setStatus("syncing");
    const env = { client, db: this.host.db, files: this.host.files, account: this.account, log: this.host.log, signal: this.controller.signal };
    try {
      for (const path of paths) {
        const folder = this.folders.get(path);
        if (!folder) continue;
        const lock = await client.getMailboxLock(path);
        try {
          const result = await syncFolder(env, folder, { flags: due.flags.has(path) });
          if (result.stored || result.expunged || result.flags) {
            this.host.log.debug(`${this.label}: ${path}: ${result.stored} stored, ${result.flags} flag changes, ${result.expunged} expunged`);
          }
        } finally {
          // Released in `finally` (§5.2), even when the sync threw.
          lock.release();
        }
      }
      // Back to the IDLE folder, where imapflow IDLEs once the connection is quiet.
      if (client.usable && (!client.mailbox || client.mailbox.path !== this.idleFolder)) {
        await client.mailboxOpen(this.idleFolder);
      }
      await this.setStatus("idle", null);
    } finally {
      for (const listener of this.listeners) listener();
      this.listeners.clear();
    }
  }

  /** A FETCH FLAGS the server pushed for one message: written to its row straight away. */
  async flagEvent(event) {
    const folder = this.folders.get(event.path);
    if (!folder) return;
    await applyFlags({ db: this.host.db }, folder, [{ uid: event.uid, flags: event.flags }]);
  }

  // ---------------------------------------------------------------------------- actions

  /** Run `work(client)` with `path` selected and locked, queued on this account's connection.
   * Throws when the account is not connected. */
  withFolder(path, work) {
    return this.enqueue(async () => {
      const client = this.client;
      if (!client || !client.usable) throw new Error(`${this.label} is not connected (${this.state})`);
      const lock = await client.getMailboxLock(path);
      try {
        return await work(client);
      } finally {
        lock.release();
        if (client.usable && client.mailbox && client.mailbox.path !== this.idleFolder) {
          await client.mailboxOpen(this.idleFolder).catch(() => {});
        }
      }
    });
  }
}

/**
 * Run `work(client)` on a connection of its own, for an account the service is not running:
 * connected, `path` selected if given, logged out afterwards whatever happens.
 */
async function withConnection(account, settings, path, work) {
  const target = await imapTarget(account, settings);
  const client = new ImapFlow(imapOptions(account, target, settings, { disableAutoIdle: true }));
  client.on("error", () => {});
  try {
    await client.connect();
    if (!path) return await work(client);
    const lock = await client.getMailboxLock(path);
    try {
      return await work(client);
    } finally {
      lock.release();
    }
  } finally {
    try {
      await client.logout();
    } catch (_) {
      client.close();
    }
  }
}

module.exports = { AccountRunner, withConnection, folderRole, assignRoles, nextBackoff, DEFAULTS };
