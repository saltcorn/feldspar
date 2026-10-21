// The connection rules (TODO-imap.md §5.1, M8.1): ports, public addresses only, the checked
// address is the one connected to, trusted servers, and no password in an error.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { isPublic, resolveTarget, connectionSettings, imapOptions, smtpOptions, redact } = require("../connect.js");

/** A resolver that answers from a table, so no test touches DNS. */
const resolver = (table) => async (host) => {
  if (!(host in table)) throw Object.assign(new Error("not found"), { code: "ENOTFOUND" });
  return table[host];
};
const dns = resolver({
  "mail.example.com": ["93.184.216.34"],
  "v6.example.com": ["2606:2800:220:1:248:1893:25c8:1946"],
  "inside.example.com": ["10.1.2.3"],
  "split.example.com": ["93.184.216.34", "192.168.1.10"],
  "loop.example.com": ["127.0.0.1"],
  "link.example.com": ["169.254.169.254"],
  "mapped.example.com": ["::ffff:127.0.0.1"],
});

test("public and non-public addresses, as sc_email::is_public", () => {
  for (const ip of ["93.184.216.34", "8.8.8.8", "2606:2800:220:1:248:1893:25c8:1946", "2a00:1450:4001:80b::200e", "::ffff:8.8.8.8"]) {
    assert.equal(isPublic(ip), true, ip);
  }
  for (const ip of [
    "0.0.0.0", "0.1.2.3", "127.0.0.1", "127.255.0.1", "10.0.0.1", "172.16.0.1", "172.31.255.255",
    "192.168.0.1", "169.254.169.254", "100.64.0.1", "100.127.255.255", "192.0.0.8", "192.0.2.1",
    "198.51.100.1", "203.0.113.9", "198.18.0.1", "224.0.0.1", "240.0.0.1", "255.255.255.255",
    "::", "::1", "fe80::1", "fc00::1", "fd12:3456::1", "ff02::1", "2001:db8::1", "::ffff:127.0.0.1",
    "::ffff:10.0.0.1", "64:ff9b::a00:1", "::127.0.0.1", "not an ip",
  ]) {
    assert.equal(isPublic(ip), false, ip);
  }
  assert.equal(isPublic("172.32.0.1"), true);
  assert.equal(isPublic("100.128.0.1"), true);
  assert.equal(isPublic("64:ff9b::808:808"), true);
});

test("allowed ports and security", async () => {
  const ok = async (kind, port, security) => resolveTarget({ kind, host: "mail.example.com", port, security }, { lookup: dns });
  assert.deepEqual(
    { ...(await ok("imap", 993, "tls")) },
    { kind: "imap", host: "mail.example.com", trusted: false, address: "93.184.216.34", port: 993, security: "tls", secure: true, servername: "mail.example.com" },
  );
  assert.equal((await ok("imap", 143, "starttls")).secure, false);
  assert.equal((await ok("smtp", 465, "tls")).secure, true);
  assert.equal((await ok("smtp", 587, "starttls")).secure, false);
  // Security inferred from a standard port, and a port from the security.
  assert.equal((await ok("imap", 143, null)).security, "starttls");
  assert.equal((await ok("smtp", null, "tls")).port, 465);

  const refused = async (kind, port, security, pattern) => {
    await assert.rejects(ok(kind, port, security), (e) => e.refused === true && pattern.test(e.message));
  };
  await refused("imap", 143, "tls", /port 143 is for starttls/);
  await refused("imap", 993, "starttls", /port 993 is for tls/);
  await refused("imap", 110, "tls", /must use port 993 \(tls\) or 143 \(starttls\)/);
  await refused("smtp", 25, "starttls", /must use port 465 .* or 587/);
  await refused("smtp", 2525, "tls", /not 2525/);
  await refused("imap", 993, "none", /must be tls or starttls/);
  await refused("imap", 143, "plain", /must be tls or starttls/);
});

test("private, loopback, link-local and mixed answers are refused", async () => {
  const at = (host) => resolveTarget({ kind: "imap", host, port: 993, security: "tls" }, { lookup: dns });
  for (const [host, address] of [
    ["inside.example.com", "10.1.2.3"],
    ["loop.example.com", "127.0.0.1"],
    ["link.example.com", "169.254.169.254"],
    ["mapped.example.com", "::ffff:127.0.0.1"],
    ["127.0.0.1", "127.0.0.1"],
    ["[::1]", "::1"],
    ["192.168.0.10", "192.168.0.10"],
  ]) {
    await assert.rejects(at(host), (e) => e.refused && e.message.includes(address) && /not a public address/.test(e.message), host);
  }
  // One public and one private address: refused, naming the private one.
  await assert.rejects(at("split.example.com"), (e) => e.refused && /192\.168\.1\.10/.test(e.message) && !/93\.184/.test(e.message));
  // Unresolvable: an error, not a refusal (it may resolve later).
  await assert.rejects(at("nowhere.example.com"), (e) => !e.refused && /could not be resolved: ENOTFOUND/.test(e.message));
  await assert.rejects(resolveTarget({ kind: "imap", host: "", port: 993 }, { lookup: dns }), /no IMAP host/);
});

test("the connection goes to the checked address and verifies the name", async () => {
  const settings = connectionSettings({});
  const target = await resolveTarget({ kind: "imap", host: "V6.Example.com.", port: 143, security: "starttls" }, { ...settings, lookup: dns });
  const options = imapOptions({ imap_user: "u", imap_password: "p" }, target, settings);
  assert.equal(options.host, "2606:2800:220:1:248:1893:25c8:1946");
  assert.equal(options.servername, "v6.example.com");
  assert.equal(options.tls.servername, "v6.example.com");
  assert.equal(options.tls.rejectUnauthorized, true);
  assert.equal(options.doSTARTTLS, true);
  assert.equal(options.secure, false);
  assert.equal(options.logger, false);
  assert.ok(options.connectionTimeout && options.greetingTimeout && options.socketTimeout);

  const smtp = smtpOptions(
    { email: "orders@shop.example", smtp_user: "orders", smtp_password: "p" },
    await resolveTarget({ kind: "smtp", host: "mail.example.com", port: 587 }, { lookup: dns }),
    settings,
  );
  assert.equal(smtp.host, "93.184.216.34");
  assert.equal(smtp.requireTLS, true);
  assert.equal(smtp.name, "shop.example", "the EHLO name is the sending domain");
  assert.equal(smtp.tls.servername, "mail.example.com");
});

test("trusted servers may be private and use their own port; others may not", async () => {
  const settings = connectionSettings({ trusted_servers: "inside.example.com\n127.0.0.1:10993\n[::1]:10143" });
  const lookup = dns;
  const ok = await resolveTarget({ kind: "imap", host: "inside.example.com", port: 993, security: "tls" }, { ...settings, lookup });
  assert.equal(ok.address, "10.1.2.3");
  // A trusted host without a port gets the standard ports only.
  await assert.rejects(resolveTarget({ kind: "imap", host: "inside.example.com", port: 10993, security: "tls" }, { ...settings, lookup }), /must use port/);
  const local = await resolveTarget({ kind: "imap", host: "127.0.0.1", port: 10993, security: "tls" }, { ...settings, lookup });
  assert.equal(local.address, "127.0.0.1");
  assert.equal(local.servername, undefined);
  assert.equal((await resolveTarget({ kind: "imap", host: "::1", port: 10143, security: "starttls" }, { ...settings, lookup })).port, 10143);
  // The trusted port is for that host only.
  await assert.rejects(resolveTarget({ kind: "imap", host: "loop.example.com", port: 10993, security: "tls" }, { ...settings, lookup }), /must use port/);
  await assert.rejects(resolveTarget({ kind: "imap", host: "loop.example.com", port: 993, security: "tls" }, { ...settings, lookup }), /not a public address/);
});

test("extra CAs are for trusted servers only, and never turn verification off", async () => {
  const pem = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
  const settings = connectionSettings({ ca_certificates: `junk\n${pem}\n${pem}`, trusted_servers: "inside.example.com" });
  assert.equal(settings.ca.length, 2);
  const inside = await resolveTarget({ kind: "imap", host: "inside.example.com", port: 993 }, { ...settings, lookup: dns });
  const options = imapOptions({}, inside, settings);
  assert.equal(options.tls.rejectUnauthorized, true);
  assert.deepEqual(options.tls.ca, [pem, pem]);
  // A public server is checked against the public roots alone.
  const outside = await resolveTarget({ kind: "imap", host: "mail.example.com", port: 993 }, { ...settings, lookup: dns });
  const public_ = imapOptions({}, outside, settings);
  assert.equal(public_.tls.ca, undefined);
  assert.equal(public_.tls.rejectUnauthorized, true);
});

test("passwords never appear in an error", () => {
  const account = { imap_password: "Grüße-€-pässwort", smtp_password: "s3cret" };
  const e = Object.assign(new Error("LOGIN Grüße-€-pässwort failed"), { responseText: "bad s3cret" });
  const text = redact(e, account);
  assert.ok(!text.includes("Grüße-€-pässwort") && !text.includes("s3cret"), text);
  assert.match(text, /LOGIN ••• failed: bad •••/);
  assert.equal(redact("x\0y", {}), "xy");
});
