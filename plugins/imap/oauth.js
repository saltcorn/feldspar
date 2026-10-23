// **OAuth2 / XOAUTH2 accounts** (TODO-imap.md M10.3): Outlook.com, Microsoft 365 and Gmail.
//
// Outlook.com answers `LOGINDISABLED` and offers only `AUTH=XOAUTH2`, so such a mailbox cannot
// be used with a password at all; Gmail can, with an app password, but needs none this way. An
// account with `auth = "oauth2"` logs in to IMAP and SMTP with an access token instead:
//
// 1. **Consent.** `mail_oauth_start` (the account's owner) asks the server for a consent flow
//    (`oauth.begin`, `sc_module::consent`) and answers `{ url }`; the application sends the
//    browser there. The server runs the provider's consent page, exchanges the code and writes
//    `oauth_access_token`, `oauth_refresh_token` (Secret fields) and `oauth_expires_at` into the
//    account's row, with `auth = "oauth2"` and `oauth_provider`, then sends the browser back to
//    the application's `return_to` (with `oauth_error=…` when it failed). The service hears the
//    change and connects.
// 2. **Refresh.** Before each connection and each send, `fresh(account)` renews an access token
//    that expires within 5 minutes with the refresh token, at the provider's token endpoint, and
//    writes the new tokens back. One refresh at a time per account. A refresh the provider
//    refuses (`invalid_grant`: the consent was withdrawn, the password changed) is a login
//    failure with `reconnect = true`: the runner sets `auth_failed` at once, saying to connect
//    the account again.
// 3. **A login refused with a token that should be good** refreshes once and tries again
//    (`account.js`).
//
// The provider's app is the admin's: the module's settings hold its client id and secret (and,
// for Microsoft, the tenant), and its redirect address is the server's one OAuth callback,
// `https://<admin host>/_feldspar/oauth/callback`, as for signing in with a provider.

"use strict";

/** The providers, by `oauth_provider`. `tenant` is Microsoft's: `common` (work and personal
 * accounts), `consumers` (Outlook.com, Hotmail, Live only), `organizations` or a tenant id. */
const PROVIDERS = {
  microsoft: {
    label: "Microsoft (Outlook.com, Microsoft 365)",
    authorization_url: (s) => `https://login.microsoftonline.com/${encodeURIComponent(s.tenant)}/oauth2/v2.0/authorize`,
    token_url: (s) => `https://login.microsoftonline.com/${encodeURIComponent(s.tenant)}/oauth2/v2.0/token`,
    scope: "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send",
    params: { prompt: "select_account" },
    imap: { imap_host: "outlook.office365.com", imap_port: 993, imap_security: "tls" },
    smtp: { smtp_host: "smtp.office365.com", smtp_port: 587, smtp_security: "starttls" },
  },
  google: {
    label: "Google (Gmail)",
    authorization_url: () => "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: () => "https://oauth2.googleapis.com/token",
    scope: "https://mail.google.com/",
    // A refresh token only comes with `access_type=offline`, and again only with `consent`.
    params: { access_type: "offline", prompt: "consent" },
    imap: { imap_host: "imap.gmail.com", imap_port: 993, imap_security: "tls" },
    smtp: { smtp_host: "smtp.gmail.com", smtp_port: 465, smtp_security: "tls" },
  },
};

/** Which provider an address's domain belongs to, when nobody said. */
const DOMAINS = [
  [/(^|\.)(outlook\.[a-z.]+|hotmail\.[a-z.]+|live\.[a-z.]+|msn\.com|office365\.com)$/i, "microsoft"],
  [/(^|\.)(gmail\.com|googlemail\.com)$/i, "google"],
];

/** The fields the tokens live in. */
const TOKEN_FIELDS = { access_token: "oauth_access_token", refresh_token: "oauth_refresh_token", expires_at: "oauth_expires_at" };

/** An access token expiring sooner than this is renewed before it is used. */
const REFRESH_MARGIN_MS = 5 * 60_000;

/** How long a token request may take. */
const TOKEN_TIMEOUT_MS = 15_000;

/** The provider settings from the module's configuration, by provider; one without a client
 * id is not configured. `oauth_endpoints` (tests only, not on the settings form) replaces a
 * provider's addresses: `{ microsoft: { authorization_url, token_url } }`. */
function providerSettings(configuration) {
  const cfg = configuration || {};
  const trim = (v) => String(v || "").trim();
  const endpoints = cfg.oauth_endpoints || {};
  const out = {};
  for (const name of Object.keys(PROVIDERS)) {
    const client_id = trim(cfg[`${name}_client_id`]);
    if (!client_id) continue;
    const settings = {
      client_id,
      client_secret: trim(cfg[`${name}_client_secret`]),
      tenant: trim(cfg[`${name}_tenant`]) || "common",
    };
    const provider = PROVIDERS[name];
    out[name] = {
      ...settings,
      authorization_url: (endpoints[name] && endpoints[name].authorization_url) || provider.authorization_url(settings),
      token_url: (endpoints[name] && endpoints[name].token_url) || provider.token_url(settings),
    };
  }
  return out;
}

/** The provider an account uses: its `oauth_provider`, or the one its address or IMAP host
 * belongs to. */
function providerOf(account) {
  const named = String(account.oauth_provider || "").trim().toLowerCase();
  if (named) return named;
  for (const value of [account.email, account.imap_user, account.imap_host]) {
    const domain = String(value || "").split("@").pop().trim().toLowerCase();
    for (const [pattern, name] of DOMAINS) if (domain && pattern.test(domain)) return name;
  }
  return null;
}

/** Whether an account logs in with a token. */
const isOAuth = (account) => String((account && account.auth) || "").toLowerCase() === "oauth2";

/** A login failure that a password (or a new consent) has to fix: not retried. */
function loginFailure(message, { reconnect = false } = {}) {
  const e = new Error(message);
  e.authenticationFailed = true;
  e.reconnect = reconnect;
  return e;
}

/**
 * What `oauth.begin` is asked for an account (`mail_oauth_start`): the provider's addresses and
 * client, the scopes, the account's address as the login hint, and where the tokens go. An
 * account with no IMAP or SMTP server yet is given the provider's.
 */
function consentRequest(account, provider, configuration, returnTo) {
  const name = provider || providerOf(account);
  const known = PROVIDERS[name];
  if (!known) throw new Error(`there is no OAuth provider ${JSON.stringify(name)}; the mail module knows ${Object.keys(PROVIDERS).join(" and ")}`);
  const settings = providerSettings(configuration)[name];
  if (!settings) throw new Error(`${known.label} is not set up: an admin enters its client id and secret in the mail module's settings`);
  const hint = account.imap_user || account.email;
  const set = { auth: "oauth2", oauth_provider: name };
  for (const group of [known.imap, known.smtp]) {
    const [hostField] = Object.keys(group);
    if (!account[hostField]) Object.assign(set, group);
  }
  return {
    authorization_url: settings.authorization_url,
    token_url: settings.token_url,
    client_id: settings.client_id,
    client_secret: settings.client_secret,
    scope: known.scope,
    params: { ...known.params, ...(hint ? { login_hint: hint } : {}) },
    return_to: returnTo || "/",
    store: { table: "mail_accounts", id: account.id, fields: TOKEN_FIELDS, set },
  };
}

/** The refreshes running, by account id: one at a time each. */
const refreshing = new Map();

/** Whether an account's access token is missing or expires within the margin. */
function expiresSoon(account, now = Date.now()) {
  if (!account.oauth_access_token) return true;
  if (!account.oauth_expires_at) return false;
  return new Date(account.oauth_expires_at).getTime() - now < REFRESH_MARGIN_MS;
}

/**
 * The account with an access token good for a while: as it is, or refreshed and written back.
 * Changes `account` in place and answers it. A password account is answered as it is.
 *
 * @param {object} account a `mail_accounts` row, secrets revealed
 * @param {object} configuration the module's settings
 * @param {{ mail_accounts }} db where the new tokens are written (a service's `ctx.db`, or the
 *   actions' module chain)
 * @param {{ force?: boolean, fetch?: typeof fetch, now?: () => number }} [options] `force`
 *   refreshes even a token that looks good (a login refused it)
 */
async function fresh(account, configuration, db, options = {}) {
  if (!isOAuth(account)) return account;
  if (!options.force && !expiresSoon(account, (options.now || Date.now)())) return account;
  const key = account.id === undefined || account.id === null ? account : account.id;
  if (!refreshing.has(key)) {
    const run = refresh(account, configuration, db, options).finally(() => refreshing.delete(key));
    refreshing.set(key, run);
  }
  const tokens = await refreshing.get(key);
  Object.assign(account, tokens);
  return account;
}

/** One refresh at the provider's token endpoint: the new token fields. */
async function refresh(account, configuration, db, options) {
  const name = providerOf(account);
  const settings = providerSettings(configuration)[name];
  if (!settings) throw new Error(`the account uses ${name || "an unknown provider"}, which is not set up in the mail module's settings`);
  if (!account.oauth_refresh_token) {
    throw loginFailure("the account has no OAuth refresh token; connect it to its provider again", { reconnect: true });
  }
  const body = new URLSearchParams({
    grant_type: "refresh_token",
    refresh_token: account.oauth_refresh_token,
    client_id: settings.client_id,
  });
  if (settings.client_secret) body.set("client_secret", settings.client_secret);
  if (name === "microsoft") body.set("scope", PROVIDERS.microsoft.scope);
  const doFetch = options.fetch || fetch;
  let response;
  let answer;
  try {
    response = await doFetch(settings.token_url, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded", accept: "application/json" },
      body: body.toString(),
      signal: AbortSignal.timeout(TOKEN_TIMEOUT_MS),
    });
    answer = await response.json().catch(() => ({}));
  } catch (e) {
    throw new Error(`could not reach ${PROVIDERS[name].label} to renew the account's access: ${e.message}`);
  }
  if (!response.ok || !answer.access_token) {
    const why = answer.error_description || answer.error || `status ${response.status}`;
    // `invalid_grant`: the refresh token is spent, withdrawn or expired; nothing but a new
    // consent mends that. Anything else (a 5xx, a rate limit) is worth another try later.
    if (answer.error === "invalid_grant" || answer.error === "unauthorized_client" || answer.error === "invalid_client") {
      throw loginFailure(`${PROVIDERS[name].label} refused to renew the account's access (${String(why).split("\n")[0]}); connect the account again`, {
        reconnect: true,
      });
    }
    throw new Error(`${PROVIDERS[name].label} did not renew the account's access: ${why}`);
  }
  const seconds = Number(answer.expires_in);
  const tokens = {
    oauth_access_token: answer.access_token,
    oauth_expires_at: Number.isFinite(seconds) ? new Date((options.now || Date.now)() + seconds * 1000).toISOString() : null,
  };
  // Microsoft hands out a new refresh token with each refresh; Google keeps the one it gave.
  if (answer.refresh_token) tokens.oauth_refresh_token = answer.refresh_token;
  if (account.id !== undefined && account.id !== null) await db.mail_accounts.where({ id: account.id }).update(tokens);
  return tokens;
}

/** The `auth` imapflow and nodemailer are given: a password, or a token. */
function authOf(account, user, kind) {
  if (!isOAuth(account)) return { user, pass: (kind === "smtp" ? account.smtp_password : account.imap_password) || "" };
  if (kind === "smtp") return { type: "OAuth2", user, accessToken: account.oauth_access_token || "" };
  return { user, accessToken: account.oauth_access_token || "" };
}

module.exports = {
  PROVIDERS,
  TOKEN_FIELDS,
  REFRESH_MARGIN_MS,
  providerSettings,
  providerOf,
  isOAuth,
  consentRequest,
  expiresSoon,
  fresh,
  authOf,
  loginFailure,
};
