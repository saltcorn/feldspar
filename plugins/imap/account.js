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
// §5.1 allows two: an IDLE connection and a short-lived one for everything else. An account
// needs only the first. It IDLEs on one folder (ingest: the first watched folder; mirror:
// INBOX); on a poll, on `mail_sync_now` and when the server reports new mail, the runner takes
// the connection out of IDLE, syncs what is due (selecting the other folders in turn), selects
// the IDLE folder again and goes back to IDLE. Actions queue on the same connection through
// `withFolder` and `withClient`, one folder's sync at a time, so an action waits for at most one
// folder (or one backfill batch), and the account never has more than one connection open while
// the service runs, whatever the app does in parallel (M9.6).
//
// Mirror mode (§4, M9.1)
// ----------------------
// - **Every folder** is synced. INBOX by IDLE; the others every 5 minutes, when a STATUS says
//   something changed (a new UIDNEXT, a different count, a new HIGHESTMODSEQ).
// - **Folders are listed again** at every poll and on `mail_sync_now`: a new one is synced, and
//   one gone from the server is deleted with its messages. The table is read again too, so a
//   folder row somebody deleted is made again rather than synced into.
// - **Backfill** (`sync.js` `backfill`): every 15 s, one batch of 50 older messages of the
//   first folder that has some left, INBOX first and Trash and Junk last.
//
// One connection's work
// ---------------------
// Everything that selects a folder runs on the queue, the first LIST and SELECT of a new
// connection included. A pass, a backfill step and a folder's sync are bound to the connection
// they started on: after a reconnect, work left over from the old connection stops instead of
// running on the new one, where another folder may be selected under it.
//
// Failures
// --------
// - **imapflow does not reconnect by itself.** On `close` or `error` the runner waits and
//   connects again, with exponential backoff and jitter from 5 s up to 10 min. A connection that
//   lasted 10 minutes resets the backoff.
// - **Three login failures in a row** set `status = auth_failed` and stop the runner, because
//   repeated bad logins can get an account locked. The service starts it again when somebody
//   changes the row (§5.5).
// - **OAuth2 accounts** (`oauth.js`): the access token is renewed before each connection when
//   it expires within 5 minutes. A login refused with a token that looked good renews it once
//   and connects again at once; a renewal the provider refuses sets `auth_failed` straight away,
//   because only a new consent mends it.
// - **A refused server** (a private address, a wrong port: `connect.js`) is not retried either;
//   it sets `status = refused` and waits for the row to change.
// - Errors written to `last_error` have the passwords taken out (`connect.redact`).
//
// Status values: `connecting`, `syncing`, `idle`, `error` (and retrying), `auth_failed`,
// `refused`, `stopped`.

"use strict";

const { ImapFlow } = require("imapflow");
const { imapTarget, imapOptions, redact } = require("./connect.js");
const { fresh, isOAuth } = require("./oauth.js");
const { syncFolder, folderRow, watchedFolders, applyFlags, backfill, backfillPending, deleteFolder, isMirror } = require("./sync.js");

const DEFAULTS = {
  /** How often the watched folders are synced in full, IDLE or not (§4: polled every 2 min). */
  pollMs: 2 * 60_000,
  /** Mirror mode: how often every folder is polled (§4: the others are polled every 5 min). */
  mirrorPollMs: 5 * 60_000,
  /** Mirror mode: how often a backfill batch runs, and its size (§4: a few hundred a minute). */
  backfillMs: 15_000,
  backfillBatch: 50,
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

/** The order folders are synced and backfilled in, by role: what a user looks at first. */
const ROLE_ORDER = ["inbox", "sent", "drafts", "archive", null, "spam", "trash"];
const roleRank = (role) => {
  const i = ROLE_ORDER.indexOf(role || null);
  return i < 0 ? ROLE_ORDER.indexOf(null) : i;
};

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
   * @param {{ db, files, log, settings, configuration, now?: () => Date }} host the service's
   *   `ctx.db`, `ctx.files`, `ctx.log`, the connection settings (`connect.connectionSettings`)
   *   and the module's configuration (an OAuth2 account's provider, `oauth.js`)
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
    /** An OAuth2 login was refused: renew the token before the next connection, whatever its
     * expiry says. */
    this.forceRefresh = false;
    this.queue = Promise.resolve();
    this.folders = new Map();
    this.due = emptyDue();
    this.passScheduled = null;
    this.passing = null;
    this.backfilling = false;
    this.running = null;
    this.mirror = isMirror(account);
    this.idleFolder = this.mirror ? "INBOX" : watchedFolders(account)[0];
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

  /** Sync every folder now. A poll passes `quick`: a mirror then lists the folders again, and
   * skips a folder whose STATUS says nothing changed. `discover` lists the folders again in
   * either mode first (`mail_sync_now`, `mail_create_folder`). */
  wake({ quick = false, discover = false } = {}) {
    for (const path of this.syncedPaths()) {
      this.due.new.add(path);
      this.due.flags.add(path);
      if (quick) this.due.quick.add(path);
    }
    if ((this.mirror && quick) || discover) this.due.discover = true;
    this.schedulePass(0);
  }

  /** Forget a folder that was deleted (`mail_delete_folder`): not synced again. */
  forgetFolder(path) {
    this.folders.delete(path);
    this.due.new.delete(path);
    this.due.flags.delete(path);
    this.due.quick.delete(path);
  }

  /** The folders this account syncs: an ingest account's watched folders, or (mirror) every
   * selectable folder on the server. */
  syncedPaths() {
    if (!this.mirror) return watchedFolders(this.account);
    return [...this.folders.values()].sort((a, b) => roleRank(a.role) - roleRank(b.role) || a.path.localeCompare(b.path)).map((f) => f.path);
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
        if (e.reconnect) {
          await this.setStatus("auth_failed", message).catch(() => {});
          this.host.log.warn(`${this.label}: ${message}`);
          return;
        }
        if (e.authenticationFailed && isOAuth(this.account) && !this.forceRefresh && !e.refreshed) {
          // Revoked early, or the clock is off: one renewal and another try, now.
          this.forceRefresh = true;
          this.host.log.info(`${this.label}: the access token was refused; renewing it`);
          continue;
        }
        if (e.authenticationFailed) {
          this.authFailures += 1;
          if (this.authFailures >= this.opts.maxAuthFailures) {
            const fix = isOAuth(this.account) ? "connect the account to its provider again" : "fix the password and save the account to try again";
            await this.setStatus("auth_failed", `login failed ${this.authFailures} times in a row; ${fix}: ${message}`).catch(() => {});
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
    const forced = this.forceRefresh;
    if (isOAuth(this.account)) {
      await fresh(this.account, this.host.configuration, this.host.db, { force: forced });
      this.forceRefresh = false;
    }
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
      try {
        await client.connect();
      } catch (e) {
        // A token renewed just now and refused all the same: not worth renewing again.
        if (forced && e && typeof e === "object") e.refreshed = true;
        throw e;
      }
      this.authFailures = 0;
      // On the queue, behind whatever an action or the last connection's pass left there.
      await this.enqueue(async () => {
        await this.discoverFolders(client);
        await client.mailboxOpen(this.idleFolder);
      });
      this.wake();
      this.pollTimer = setInterval(() => this.wake({ quick: true }), this.mirror ? this.opts.mirrorPollMs : this.opts.pollMs);
      if (this.mirror) this.backfillTimer = setInterval(() => this.backfillStep(), this.opts.backfillMs);
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
    clearInterval(this.backfillTimer);
    clearTimeout(this.passScheduled);
    this.passScheduled = null;
  }

  /** What a sync needs: the connection, the service's tables and files, the account. */
  env(client) {
    return {
      client,
      db: this.host.db,
      files: this.host.files,
      account: this.account,
      log: this.host.log,
      signal: this.controller.signal,
      extraHeaders: this.host.extraHeaders,
    };
  }

  /** Whether `client` is still this runner's connection, and up. */
  current(client) {
    return Boolean(client) && client === this.client && client.usable;
  }

  // ---------------------------------------------------------------------------- folders

  /** LIST, roles, and an upserted `mail_folders` row for each folder (§7). A mirror deletes
   * the folders that are gone from the server, with their messages, and forgets them. A folder
   * whose row is gone from the table gets a new one. Runs on the queue. */
  async discoverFolders(client = this.client) {
    const { db } = this.host;
    if (!this.current(client)) return;
    const entries = (await client.list()).filter((e) => !(e.flags && e.flags.has && (e.flags.has("\\Noselect") || e.flags.has("\\NonExistent"))));
    const roles = assignRoles(entries);
    const watched = new Set(watchedFolders(this.account));
    const listed = new Set(entries.map((e) => e.path));
    const stored = await db.mail_folders.where({ account: this.account.id }).rows();
    const storedPaths = new Map(stored.map((r) => [r.path, r]));
    if (this.mirror) {
      for (const row of stored) {
        if (listed.has(row.path)) continue;
        this.host.log.info(`${this.label}: the folder ${row.path} is gone from the server`);
        await deleteFolder(this.env(client), row);
        storedPaths.delete(row.path);
        this.forgetFolder(row.path);
      }
      for (const path of [...this.folders.keys()]) if (!listed.has(path)) this.forgetFolder(path);
    }
    for (const entry of entries) {
      const values = {
        name: entry.name,
        role: roles.get(entry.path),
        delimiter: entry.delimiter || null,
        sync: this.mirror || watched.has(entry.path),
      };
      let known = this.folders.get(entry.path);
      const row0 = storedPaths.get(entry.path);
      if (known && (!row0 || row0.id !== known.id)) {
        // Its row was deleted (or made again) by somebody else: the table's is the one.
        this.forgetFolder(entry.path);
        known = null;
      }
      let row = known || row0 || (await folderRow(db, this.account, entry.path, values));
      if (["name", "role", "delimiter", "sync"].some((k) => row[k] !== values[k])) {
        await db.mail_folders.where({ id: row.id }).update(values);
        Object.assign(row, values);
      }
      if (!known) {
        this.folders.set(entry.path, row);
        // Found by a later LIST: synced at once.
        if (this.mirror && this.client) {
          this.due.new.add(entry.path);
          this.due.flags.add(entry.path);
        }
      }
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
      this.pass().catch((e) => this.passFailed(e));
    }, ms);
  }

  /** Queue `work` on the connection, after whatever is running. */
  enqueue(work) {
    const run = this.queue.then(work, work);
    this.queue = run.catch(() => {});
    return run;
  }

  passFailed(e) {
    // Stopped meanwhile: the connection was closed under the sync on purpose, and the account's
    // status is the service's to write now.
    if (this.controller.signal.aborted) return;
    const message = redact(e, this.account);
    this.host.log.warn(`${this.label}: sync failed: ${message}`);
    this.setStatus("error", message).catch(() => {});
    // A failed pass on a connection that is still up is retried at the next poll; one whose
    // connection went is retried by the loop's reconnect.
  }

  /**
   * Sync what is due: the folders with new mail, and those whose flags are due. Each folder is
   * its own item on the queue, so an action waits for one folder, not for the whole pass. A
   * pass that is running takes on what becomes due meanwhile; answers when nothing is left.
   */
  pass() {
    if (this.passing) return this.passing;
    this.passing = this.runPass().finally(() => {
      this.passing = null;
      for (const listener of this.listeners) listener();
      this.listeners.clear();
      // Due after the last round looked: another pass.
      if (this.due.new.size || this.due.flags.size || this.due.discover) this.schedulePass(0);
    });
    return this.passing;
  }

  async runPass() {
    // The connection this pass started on: after a reconnect, the new one runs its own.
    const client = this.client;
    let synced = false;
    for (;;) {
      if (!this.current(client)) return;
      const due = this.due;
      this.due = emptyDue();
      if (due.discover) await this.enqueue(() => this.discoverFolders(client));
      const paths = this.syncedPaths().filter((p) => due.new.has(p) || due.flags.has(p));
      if (!paths.length) break;
      if (!synced) await this.setStatus("syncing");
      synced = true;
      for (const path of paths) {
        await this.enqueue(() => this.syncPath(client, path, { flags: due.flags.has(path), quick: due.quick.has(path) }));
      }
    }
    if (!synced || !this.current(client)) return;
    await this.enqueue(() => this.returnToIdle(client));
    await this.setStatus("idle", null);
  }

  /** One folder's sync, on the queue, on the connection its pass started on. */
  async syncPath(client, path, { flags, quick }) {
    // Left over from a connection that is gone: the next one syncs every folder anyway.
    if (!this.current(client)) return;
    const folder = this.folders.get(path);
    if (!folder) return;
    if (quick && this.mirror && path !== this.idleFolder && !(await this.changedOnServer(client, folder))) return;
    const lock = await client.getMailboxLock(path);
    try {
      const result = await syncFolder(this.env(client), folder, { flags });
      if (result.stored || result.expunged || result.flags) {
        this.host.log.debug(`${this.label}: ${path}: ${result.stored} stored, ${result.flags} flag changes, ${result.expunged} expunged`);
      }
    } finally {
      // Released in `finally` (§5.2), even when the sync threw.
      lock.release();
    }
  }

  /** Whether a folder that is not selected has changed since it was synced, by STATUS: cheaper
   * than selecting each of a mirror's folders every 5 minutes. Without CONDSTORE a flag change
   * cannot be seen this way, so such a server's folders are always synced. */
  async changedOnServer(client, folder) {
    if (folder.backfill_uid === null || folder.backfill_uid === undefined || !folder.uidvalidity) return true;
    let status;
    try {
      status = await client.status(folder.path, { messages: true, uidNext: true, uidValidity: true, highestModseq: true });
    } catch (_) {
      return true;
    }
    if (status.uidValidity !== undefined && Number(status.uidValidity) !== Number(folder.uidvalidity)) return true;
    if (status.uidNext !== undefined && status.uidNext - 1 > Number(folder.last_uid || 0)) return true;
    if (status.messages !== undefined && status.messages !== Number(folder.total)) return true;
    if (status.highestModseq === undefined || status.highestModseq === null) return true;
    return String(status.highestModseq) !== String(folder.highestmodseq);
  }

  /** Select the IDLE folder again, where imapflow IDLEs once the connection is quiet. */
  async returnToIdle(client = this.client) {
    if (this.current(client) && (!client.mailbox || client.mailbox.path !== this.idleFolder)) {
      await client.mailboxOpen(this.idleFolder);
    }
  }

  /** Mirror mode: one backfill batch of the first folder that has older mail left. */
  backfillStep() {
    if (this.backfilling || this.passing || !this.client) return null;
    const folder = this.syncedPaths()
      .map((p) => this.folders.get(p))
      .find((f) => f && backfillPending(f));
    if (!folder) return null;
    this.backfilling = true;
    const client = this.client;
    return this.enqueue(async () => {
      if (!this.current(client) || this.folders.get(folder.path) !== folder) return;
      const lock = await client.getMailboxLock(folder.path);
      try {
        const stored = await backfill(this.env(client), folder, this.opts.backfillBatch);
        if (stored) this.host.log.debug(`${this.label}: ${folder.path}: ${stored} older messages stored`);
      } finally {
        lock.release();
      }
      await this.returnToIdle(client);
    })
      .catch((e) => this.passFailed(e))
      .finally(() => {
        this.backfilling = false;
      });
  }

  /** A FETCH FLAGS the server pushed for one message: written to its row straight away. */
  async flagEvent(event) {
    const folder = this.folders.get(event.path);
    if (!folder) return;
    await applyFlags({ db: this.host.db }, folder, [{ uid: event.uid, flags: event.flags }]);
  }

  // ---------------------------------------------------------------------------- actions

  /** Run `work(client)` on this account's connection, queued, with no folder selected for it
   * (a LIST, a CREATE, an APPEND). Throws when the account is not connected. */
  withClient(work) {
    return this.enqueue(async () => {
      const client = this.client;
      if (!client || !client.usable) throw new Error(`${this.label} is not connected (${this.state})`);
      return work(client);
    });
  }

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

/** A runner's due work: folders with new mail, folders whose flags are due, folders a poll
 * may skip when STATUS shows no change, and whether to LIST again. */
function emptyDue() {
  return { new: new Set(), flags: new Set(), quick: new Set(), discover: false };
}

/** The connections `withConnection` has open or queued, by account id: one at a time each. */
const ownConnections = new Map();

/**
 * Run `work(client)` on a connection of its own, for an account the service is not running:
 * connected, `path` selected if given, logged out afterwards whatever happens. Calls for one
 * saved account wait for each other, so the app calling actions in parallel opens one such
 * connection at a time (M9.6); with the runner's, at most two.
 */
function withConnection(account, settings, path, work) {
  if (account.id === undefined || account.id === null) return connectOnce(account, settings, path, work);
  const before = ownConnections.get(account.id) || Promise.resolve();
  const run = before.then(() => connectOnce(account, settings, path, work));
  const settled = run.catch(() => {});
  ownConnections.set(account.id, settled);
  settled.then(() => {
    if (ownConnections.get(account.id) === settled) ownConnections.delete(account.id);
  });
  return run;
}

async function connectOnce(account, settings, path, work) {
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

module.exports = { AccountRunner, withConnection, folderRole, assignRoles, nextBackoff, roleRank, DEFAULTS };
