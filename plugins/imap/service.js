// **The sync service** (TODO-imap.md §5, M8.5): one `AccountRunner` per enabled account, kept in
// step with `mail_accounts` as it changes.
//
// - **At start** it reads the enabled accounts and starts a runner for each.
// - **When an account changes** (§5.5), through `ctx.onTableChange("mail_accounts")`: a new
//   enabled account is started, a disabled or deleted one stopped, and one whose settings changed
//   restarted. Its own writes (`status`, `last_error`, `last_sync_at`) arrive with `own: true`
//   and are ignored. An account at `auth_failed` or `refused` restarts on any change by somebody
//   else, so saving the row again is how a user says "try again" (§5.1). An OAuth2 account's
//   tokens (`oauth.js`) are not settings: new ones are handed to the running runner, and only an
//   account at `auth_failed` restarts for them — which is what connecting it again does.
// - **At stop** every runner is stopped and its connection logged out.
//
// Ingest and mirror accounts alike: the runner (`account.js`) does what the mode needs.
//
// The runners are kept in `runners`, at module level, so the actions (`actions.js`), which run on
// the same worker, can find an account's connection and queue on it.

"use strict";

const { AccountRunner } = require("./account.js");
const { connectionSettings } = require("./connect.js");
const { headerNames } = require("./sync.js");

/** The running accounts, by id. Shared with the actions. */
const runners = new Map();

/** The fields the service itself writes: a change to only these is not a change to the account. */
const OWN_FIELDS = new Set(["status", "last_error", "last_sync_at"]);

/** An OAuth2 account's tokens, renewed by whoever connects first (`oauth.js`). */
const TOKEN_FIELDS = ["oauth_access_token", "oauth_refresh_token", "oauth_expires_at"];

/** What a runner depends on: every field but the service's own. */
function fingerprint(account) {
  const copy = {};
  for (const key of Object.keys(account).sort()) if (!OWN_FIELDS.has(key) && !TOKEN_FIELDS.includes(key)) copy[key] = account[key];
  return JSON.stringify(copy);
}

/**
 * The service's `start` (tutorial-modules Step 8). `options` are the runner's overrides
 * (`account.js` `DEFAULTS`), from the module's configuration, for tests.
 */
async function start(ctx) {
  const settings = connectionSettings(ctx.configuration);
  const options = (ctx.configuration && ctx.configuration.runner_options) || {};
  const host = { db: ctx.db, files: ctx.files, log: ctx.log, settings, configuration: ctx.configuration, extraHeaders: headerNames(ctx.configuration) };
  const prints = new Map();
  // Changes are applied one at a time, in the order they were committed.
  let chain = Promise.resolve();
  const serial = (work) => (chain = chain.then(work).catch((e) => ctx.log.error(`mail accounts: ${e.stack || e}`)));

  async function stopOne(id) {
    const runner = runners.get(id);
    if (!runner) return;
    runners.delete(id);
    prints.delete(id);
    await runner.stop();
  }

  async function startOne(account) {
    if (account.mode && account.mode !== "ingest" && account.mode !== "mirror") {
      await ctx.db.mail_accounts.where({ id: account.id }).update({ status: "refused", last_error: `the mode must be ingest or mirror, not ${account.mode}` });
      return;
    }
    const runner = new AccountRunner(host, account, options);
    runners.set(account.id, runner);
    prints.set(account.id, fingerprint(account));
    runner.start();
  }

  async function changed({ op, row, old, own }) {
    if (own) return;
    const id = (row && row.id) || (old && old.id);
    if (op === "delete" || !row) return stopOne(id);
    const runner = runners.get(id);
    if (!row.enabled) {
      if (runner) {
        await stopOne(id);
        await ctx.db.mail_accounts.where({ id }).update({ status: "disabled" });
      }
      return;
    }
    if (runner && prints.get(id) === fingerprint(row) && !["auth_failed", "refused"].includes(runner.state)) {
      // Tokens an action renewed: the runner connects with them next time.
      for (const key of TOKEN_FIELDS) if (key in row) runner.account[key] = row[key];
      return;
    }
    await stopOne(id);
    await startOne(row);
  }

  await ctx.onTableChange("mail_accounts", (change) => serial(() => changed(change)));
  for (const account of await ctx.db.mail_accounts.where({ enabled: true }).rows()) {
    await serial(() => startOne(account));
  }
  ctx.log(`mail: ${runners.size} account(s) running`);

  return async () => {
    await chain;
    await Promise.all([...runners.keys()].map((id) => stopOne(id)));
  };
}

module.exports = { start, runners, fingerprint };
