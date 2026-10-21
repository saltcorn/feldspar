// **The connection rules** (TODO-imap.md §5.1, M8.1): one function that decides where a
// connection may go, used for IMAP, for SMTP and by `mail_test_account`.
//
//   const target = await resolveTarget({ kind: "imap", host, port, security }, settings);
//   new ImapFlow(imapOptions(account, target, settings));
//
// - **Ports.** IMAP is 993 with TLS or 143 with STARTTLS required; SMTP is 465 with TLS or 587
//   with STARTTLS required. A port that does not match its security is refused, and so is no
//   security at all.
// - **Public addresses only.** The host is resolved once, every address it resolves to must be
//   globally routable, and the connection goes to the address that was checked: `host` is the IP,
//   and the name is kept for SNI and the certificate check (`servername`). This is
//   `sc_email::relay_mailer` (M3.3), case for case; `isPublic` is `sc_email::is_public`.
// - **Certificates are always verified.** `ca_certificates` in the module's settings are the CAs
//   of the trusted servers (below), used for them in place of the public roots; every other server
//   is checked against the public roots alone. They never turn verification off. (Reading Node's
//   own roots to add to them, `tls.rootCertificates`, needs `sys` access in feldspar's worker,
//   which a module is not granted.)
// - **Trusted servers.** The module's `trusted_servers` setting lists `host` or `host:port`, one
//   per line: servers inside the admin's own network, such as an Exchange on the LAN. Such a host
//   may resolve to private addresses, and with a port it may use that port as well as the
//   standard ones. It is the admin's setting, not an account's: a user who types `10.0.0.5` into
//   their own account is still refused.
// - **Passwords never appear** in a log, an error or `last_error`: see `redact`.

"use strict";

const dns = require("node:dns");
const net = require("node:net");

/** The standard ports, by protocol and security (§5.1). */
const PORTS = {
  imap: { tls: 993, starttls: 143 },
  smtp: { tls: 465, starttls: 587 },
};

/** imapflow's and nodemailer's timeouts, in ms. */
const TIMEOUTS = {
  connection: 30_000,
  greeting: 16_000,
  socket: 5 * 60_000,
};

// ------------------------------------------------------------------------------ addresses

/** The four bytes of an IPv4 address, or null. */
function v4Bytes(ip) {
  if (net.isIPv4(ip)) return ip.split(".").map(Number);
  return null;
}

/** The eight 16-bit groups of an IPv6 address, or null. An embedded dotted IPv4 tail
 * (`::ffff:1.2.3.4`) is folded into the last two groups. */
function v6Groups(ip) {
  if (!net.isIPv6(ip)) return null;
  let text = ip.toLowerCase().replace(/%.*$/, "");
  const tail = text.match(/(\d+\.\d+\.\d+\.\d+)$/);
  if (tail) {
    const [a, b, c, d] = tail[1].split(".").map(Number);
    text = text.slice(0, -tail[1].length) + `${((a << 8) | b).toString(16)}:${((c << 8) | d).toString(16)}`;
  }
  const [head, rest] = text.includes("::") ? text.split("::") : [text, null];
  const left = head ? head.split(":") : [];
  const right = rest === null ? [] : rest ? rest.split(":") : [];
  const fill = rest === null ? [] : new Array(8 - left.length - right.length).fill("0");
  const groups = [...left, ...fill, ...right].map((g) => parseInt(g, 16));
  return groups.length === 8 && groups.every((g) => g >= 0 && g <= 0xffff) ? groups : null;
}

function isPublicV4([a, b, c, d]) {
  return !(
    // 0.0.0.0/8 ("this network", and unspecified)
    a === 0 ||
    // loopback
    a === 127 ||
    // private
    a === 10 ||
    (a === 172 && b >= 16 && b < 32) ||
    (a === 192 && b === 168) ||
    // link-local
    (a === 169 && b === 254) ||
    // carrier-grade NAT
    (a === 100 && b >= 64 && b < 128) ||
    // protocol assignments
    (a === 192 && b === 0 && c === 0) ||
    // documentation
    (a === 192 && b === 0 && c === 2) ||
    (a === 198 && b === 51 && c === 100) ||
    (a === 203 && b === 0 && c === 113) ||
    // benchmarking
    (a === 198 && (b === 18 || b === 19)) ||
    // multicast, reserved and broadcast
    a >= 224 ||
    (a === 255 && b === 255 && c === 255 && d === 255)
  );
}

/** Whether an address is globally routable: `sc_email::is_public`, case for case. */
function isPublic(ip) {
  const v4 = v4Bytes(ip);
  if (v4) return isPublicV4(v4);
  const g = v6Groups(ip);
  if (!g) return false;
  const v4From = (hi, lo) => [hi >> 8, hi & 0xff, lo >> 8, lo & 0xff];
  // An IPv4 address carried in IPv6 is judged as the IPv4 address it is.
  if (g.slice(0, 5).every((x) => x === 0) && g[5] === 0xffff) return isPublicV4(v4From(g[6], g[7]));
  // 64:ff9b::/96, NAT64.
  if (g[0] === 0x64 && g[1] === 0xff9b && g.slice(2, 6).every((x) => x === 0)) return isPublicV4(v4From(g[6], g[7]));
  return !(
    // ::/96: unspecified, loopback (::1) and the deprecated IPv4-compatible form
    g.slice(0, 6).every((x) => x === 0) ||
    // multicast
    (g[0] & 0xff00) === 0xff00 ||
    // unique local
    (g[0] & 0xfe00) === 0xfc00 ||
    // link-local
    (g[0] & 0xffc0) === 0xfe80 ||
    // documentation
    (g[0] === 0x2001 && g[1] === 0x0db8)
  );
}

// ------------------------------------------------------------------------------ settings

/** The module's settings that bear on connections, from its configuration:
 * `{ trusted: [{ host, port }], ca: [pem] }`. */
function connectionSettings(configuration) {
  const cfg = configuration || {};
  const lines = (value) =>
    (Array.isArray(value) ? value : String(value || "").split(/[\n,]/))
      .map((l) => String(l).trim())
      .filter(Boolean);
  const trusted = lines(cfg.trusted_servers).map((entry) => {
    // `host`, `host:port`, `[v6]:port` or a bare IPv6 address.
    const bracket = entry.match(/^\[([^\]]+)\](?::(\d+))?$/);
    if (bracket) return { host: bracket[1].toLowerCase(), port: bracket[2] ? Number(bracket[2]) : null };
    const m = entry.match(/^([^:]+):(\d+)$/);
    if (m) return { host: m[1].toLowerCase(), port: Number(m[2]) };
    return { host: entry.toLowerCase(), port: null };
  });
  const pem = String(cfg.ca_certificates || "");
  const ca = pem.match(/-----BEGIN CERTIFICATE-----[\s\S]+?-----END CERTIFICATE-----/g) || [];
  return { trusted, ca };
}

// ------------------------------------------------------------------------------ resolve

/** A refusal: an Error with `refused = true`, so a caller can tell it from a network failure
 * (and `account.js` does not retry it). */
function refusal(message) {
  const e = new Error(message);
  e.refused = true;
  return e;
}

/** Every address `host` resolves to. */
async function lookupAll(host) {
  const found = await dns.promises.lookup(host, { all: true, verbatim: true });
  return found.map((f) => f.address);
}

/**
 * Where a connection may go (§5.1), or a refusal saying why not.
 *
 * @param {{ kind: "imap"|"smtp", host: string, port?: number, security?: string }} server
 * @param {{ trusted?: {host,port}[], lookup?: (host) => Promise<string[]> }} [settings]
 * @returns {Promise<{ kind, host, address, port, security, secure: boolean, servername: string|undefined }>}
 */
async function resolveTarget(server, settings = {}) {
  const kind = server.kind;
  const ports = PORTS[kind];
  if (!ports) throw refusal(`unknown connection kind ${kind}`);
  const label = kind.toUpperCase();
  const host = String(server.host || "").trim().replace(/^\[(.*)\]$/, "$1").replace(/\.$/, "").toLowerCase();
  if (!host) throw refusal(`no ${label} host is set`);
  const security = String(server.security || "").trim().toLowerCase() || (Number(server.port) === ports.starttls ? "starttls" : "tls");
  if (security !== "tls" && security !== "starttls") {
    throw refusal(`${label} security must be tls or starttls, not ${JSON.stringify(server.security)}`);
  }
  const port = server.port === undefined || server.port === null || server.port === "" ? ports[security] : Number(server.port);
  const trust = (settings.trusted || []).filter((t) => t.host === host);
  const trustedPort = trust.some((t) => t.port === port);
  if (!Number.isInteger(port) || (port !== ports[security] && !trustedPort)) {
    const other = Object.entries(ports).find(([, p]) => p === port);
    throw refusal(
      other
        ? `${label} port ${port} is for ${other[0]}, not ${security}`
        : `${label} must use port ${ports.tls} (tls) or ${ports.starttls} (starttls), not ${server.port}`,
    );
  }
  const addresses = net.isIP(host) ? [host] : await (settings.lookup || lookupAll)(host).catch((e) => {
    throw new Error(`${label} host ${host} could not be resolved: ${e.code || e.message}`);
  });
  if (!addresses.length) throw new Error(`${label} host ${host} could not be resolved`);
  if (!trust.length) {
    const inside = addresses.filter((a) => !isPublic(a));
    if (inside.length) {
      throw refusal(
        `${label} host ${host} resolves to ${inside.join(", ")}, which is not a public address. ` +
          "An admin can list servers inside the network under the module's trusted servers",
      );
    }
  }
  return {
    kind,
    host,
    trusted: trust.length > 0,
    address: addresses[0],
    port,
    security,
    secure: security === "tls",
    // An IP needs no SNI, and the certificate is checked against the IP itself.
    servername: net.isIP(host) ? undefined : host,
  };
}

// ------------------------------------------------------------------------------ options

/** The TLS options both libraries take: verification on, the name we resolved, and for a
 * trusted server the admin's CAs. */
function tlsOptions(target, settings) {
  const tls = { rejectUnauthorized: true, minVersion: "TLSv1.2" };
  if (target.servername) tls.servername = target.servername;
  if (target.trusted && settings && settings.ca && settings.ca.length) tls.ca = [...settings.ca];
  return tls;
}

/** `new ImapFlow(…)`'s options for an account and its resolved IMAP target. */
function imapOptions(account, target, settings, extra = {}) {
  return {
    host: target.address,
    port: target.port,
    secure: target.secure,
    // STARTTLS required: imapflow refuses to go on without it.
    doSTARTTLS: target.secure ? undefined : true,
    servername: target.servername,
    tls: tlsOptions(target, settings),
    auth: { user: account.imap_user || account.email || "", pass: account.imap_password || "" },
    logger: false,
    connectionTimeout: TIMEOUTS.connection,
    greetingTimeout: TIMEOUTS.greeting,
    socketTimeout: TIMEOUTS.socket,
    clientInfo: { name: "feldspar" },
    ...extra,
  };
}

/** The domain of an address, for the EHLO name (§5.7: the sending domain, not the machine). */
function domainOf(address) {
  const at = String(address || "").lastIndexOf("@");
  return at > 0 ? String(address).slice(at + 1).trim().toLowerCase() || undefined : undefined;
}

/** `nodemailer.createTransport(…)`'s options for an account and its resolved SMTP target. */
function smtpOptions(account, target, settings) {
  return {
    host: target.address,
    port: target.port,
    secure: target.secure,
    requireTLS: !target.secure,
    name: domainOf(account.email) || domainOf(account.smtp_user) || target.host,
    tls: tlsOptions(target, settings),
    auth: { user: account.smtp_user || account.email || "", pass: account.smtp_password || "" },
    logger: false,
    debug: false,
    connectionTimeout: TIMEOUTS.connection,
    greetingTimeout: TIMEOUTS.greeting,
    socketTimeout: TIMEOUTS.socket,
  };
}

/** The IMAP target of an account. */
function imapTarget(account, settings) {
  return resolveTarget({ kind: "imap", host: account.imap_host, port: account.imap_port, security: account.imap_security }, settings);
}

/** The SMTP target of an account. */
function smtpTarget(account, settings) {
  return resolveTarget({ kind: "smtp", host: account.smtp_host, port: account.smtp_port, security: account.smtp_security }, settings);
}

// ------------------------------------------------------------------------------ errors

/** An error's message with the account's passwords taken out, cut to a length a status column
 * holds. Includes the server's own response text when imapflow has one, which is what says
 * *why* a login failed. */
function redact(error, account) {
  let text = error && typeof error === "object" ? error.message || String(error) : String(error || "");
  if (error && typeof error === "object" && error.responseText && !text.includes(error.responseText)) {
    text += `: ${error.responseText}`;
  }
  for (const secret of [account && account.imap_password, account && account.smtp_password]) {
    if (typeof secret === "string" && secret.length >= 1) text = text.split(secret).join("•••");
  }
  return text.replace(/\0/g, "").slice(0, 1000);
}

module.exports = {
  PORTS,
  TIMEOUTS,
  isPublic,
  connectionSettings,
  resolveTarget,
  imapTarget,
  smtpTarget,
  imapOptions,
  smtpOptions,
  domainOf,
  redact,
  refusal,
};
