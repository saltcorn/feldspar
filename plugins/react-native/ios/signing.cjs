"use strict";
// Signing an iOS device build from an App Store provisioning profile. Shared by
// prebuild.cjs (signs the Xcode project) and build.cjs (exports the .ipa).
// The certificate's private key must be in the server user's keychain.

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

/** The fields a build needs from a decoded profile.
 *
 * The export method (Xcode 15.3+ names) depends on the devices:
 * - all devices: enterprise
 * - a device list: development with get-task-allow, otherwise ad hoc
 * - no list: App Store */
function profileInfo(plist) {
  const entitlements = plist.Entitlements || {};
  const full = String(entitlements["application-identifier"] || "");
  const dot = full.indexOf(".");
  let method = "app-store-connect";
  if (plist.ProvisionsAllDevices) method = "enterprise";
  else if (Array.isArray(plist.ProvisionedDevices)) {
    method = entitlements["get-task-allow"] ? "debugging" : "release-testing";
  }
  return {
    uuid: String(plist.UUID || ""),
    name: String(plist.Name || ""),
    team: String((plist.TeamIdentifier || [])[0] || ""),
    appId: dot >= 0 ? full.slice(dot + 1) : full,
    method,
    expires: plist.ExpirationDate ? new Date(plist.ExpirationDate) : null,
    certificates: (plist.DeveloperCertificates || []).map((der) =>
      crypto.createHash("sha1").update(der).digest("hex").toUpperCase()
    ),
  };
}

/** Refuse an unusable profile before Xcode starts. */
function checkProfile(info, now) {
  const missing = ["uuid", "team", "appId"].filter((k) => !info[k]);
  if (missing.length) {
    throw new Error(
      "The provisioning profile has no " +
        missing.join(", ") +
        "; is it a .mobileprovision file?"
    );
  }
  if (!info.certificates.length) {
    throw new Error(
      'The provisioning profile "' +
        info.name +
        '" names no certificate to sign with.'
    );
  }
  if (info.expires && info.expires.getTime() <= now.getTime()) {
    throw new Error(
      'The provisioning profile "' +
        info.name +
        '" expired on ' +
        info.expires.toISOString().slice(0, 10) +
        "; renew it in the Apple Developer portal and upload the new one."
    );
  }
}

/** The profile's bundle ID, or for a wildcard profile (com.example.*) the
 * app's own, which the wildcard must cover. */
function bundleIdFor(info, fallback) {
  if (info.appId !== "*" && !info.appId.endsWith(".*")) return info.appId;
  const prefix = info.appId.slice(0, -1);
  if (!fallback || !fallback.startsWith(prefix)) {
    throw new Error(
      'The provisioning profile "' +
        info.name +
        '" is a wildcard profile for ' +
        info.appId +
        ", which does not cover the bundle ID " +
        (fallback || "(none)") +
        ". Set an App ID it covers in the application's settings."
    );
  }
  return fallback;
}

/** The keychain's signing identities (with private key): SHA-1 and name. */
function keychainIdentities() {
  const out = execFileSync(
    "security",
    ["find-identity", "-v", "-p", "codesigning"],
    { encoding: "utf8" }
  );
  const found = [];
  for (const line of out.split("\n")) {
    const m = /^\s*\d+\)\s+([0-9A-F]{40})\s+"(.*)"/.exec(line);
    if (m) found.push({ sha1: m[1], name: m[2] });
  }
  return found;
}

/** The first of the profile's certificates found in the keychain. Matched by
 * SHA-1, so a renewed certificate is never confused with an expired one of
 * the same name. */
function signingIdentity(info, identities) {
  const match = identities.find(
    (id) => info.certificates.indexOf(id.sha1) >= 0
  );
  if (match) return match;
  throw new Error(
    'None of the certificates the provisioning profile "' +
      info.name +
      '" was issued for is in ' +
      "this Mac's keychain with its private key (it has " +
      (identities.length
        ? identities.map((id) => id.name).join(", ")
        : "no signing identities") +
      "). Import the certificate's .p12 into the login keychain of the user the server runs as."
  );
}

/** Refuse anything but an App Store profile: for now only App Store builds
 * are supported. */
function checkAppStore(info) {
  if (info.method === "app-store-connect") return;
  const kind = {
    "release-testing": "an ad hoc",
    debugging: "a development",
    enterprise: "an enterprise",
  }[info.method];
  throw new Error(
    'The provisioning profile "' +
      info.name +
      '" is ' +
      (kind || "not an App Store") +
      " profile. " +
      "An iOS app is built for App Store Connect, so it needs an App Store profile " +
      '(Distribution → App Store Connect in the Apple Developer portal), or choose "generate".'
  );
}

/** Decode a .mobileprovision (an XML plist inside a signed CMS message). */
function readProfile(file) {
  let xml;
  try {
    xml = execFileSync("security", ["cms", "-D", "-i", file], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
  } catch (e) {
    const why = e.stderr ? String(e.stderr).trim() : e.message;
    throw new Error(
      "Could not decode the provisioning profile " + file + ": " + why
    );
  }
  // Expo's plist parser: <data> becomes a Buffer, <date> a Date.
  return require("@expo/plist").default.parse(xml);
}

/** What a device build signs with. The profile is the generated one
 * (FELDSPAR_IOS_PROFILE_FILE, also seen by expo prebuild) or else the
 * uploaded one from native.json. */
function deviceSigning(projectDir, native) {
  const ios = native.ios || {};
  let file = process.env.FELDSPAR_IOS_PROFILE_FILE;
  if (!file) {
    if (!ios.profile) {
      throw new Error(
        "An iOS app needs an App Store provisioning profile: choose one in the application's " +
          'iOS app settings, or choose "generate".'
      );
    }
    file = path.resolve(projectDir, ios.profile);
    if (!fs.existsSync(file)) {
      throw new Error(
        'The provisioning profile "' +
          ios.profile +
          '" does not exist (relative to the project).'
      );
    }
  }
  const info = profileInfo(readProfile(file));
  checkProfile(info, new Date());
  checkAppStore(info);
  return {
    file,
    info,
    bundleId: bundleIdFor(info, ios.bundleId),
    identity: signingIdentity(info, keychainIdentities()),
  };
}

/** Options for xcodebuild -exportArchive. Xcode must keep the version and
 * build number; they come from the app settings. */
function exportOptions(signing) {
  const esc = (s) =>
    String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;");
  return [
    '<?xml version="1.0" encoding="UTF-8"?>',
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">',
    '<plist version="1.0">',
    "<dict>",
    "  <key>method</key><string>" + esc(signing.info.method) + "</string>",
    "  <key>teamID</key><string>" + esc(signing.info.team) + "</string>",
    "  <key>signingStyle</key><string>manual</string>",
    "  <key>signingCertificate</key><string>" +
      esc(signing.identity.sha1) +
      "</string>",
    "  <key>provisioningProfiles</key>",
    "  <dict>",
    "    <key>" +
      esc(signing.bundleId) +
      "</key><string>" +
      esc(signing.info.uuid) +
      "</string>",
    "  </dict>",
    "  <key>manageAppVersionAndBuildNumber</key><false/>",
    "</dict>",
    "</plist>",
    "",
  ].join("\n");
}

module.exports = {
  exportOptions,
  profileInfo,
  checkProfile,
  checkAppStore,
  bundleIdFor,
  keychainIdentities,
  signingIdentity,
  deviceSigning,
};
