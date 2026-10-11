"use strict";
// Entry point for app.config.js, which expo prebuild loads while it writes the
// Xcode project: check the app settings and, for a device build, set signing.

const fs = require("fs");
const path = require("path");
const { deviceSigning } = require("./signing.cjs");

/** The signing for a device build (build.cjs sets FELDSPAR_IOS_BUILD), or null
 * for the simulator, the web export and Android, which load app.config.js
 * too. */
function prebuildSigning(projectDir, native) {
  return process.env.FELDSPAR_IOS_BUILD === "device"
    ? deviceSigning(projectDir, native)
    : null;
}

/** Check the app settings during expo prebuild, so mistakes fail with a
 * clear message instead of an Xcode error. */
function checkConfig(projectDir, native, bundleId) {
  if (!/^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$/.test(bundleId || "")) {
    throw new Error(
      'The bundle ID "' +
        bundleId +
        '" is not one iOS accepts: two or more dot-separated ' +
        "segments of letters, digits and - (com.example.todo)."
    );
  }
  if (!/^\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(native.version || "")) {
    throw new Error(
      'The version "' +
        native.version +
        '" is not major.minor.patch, e.g. 1.2.3.'
    );
  }
  if (native.icon) {
    if (!/\.png$/i.test(native.icon)) {
      throw new Error(
        'The icon "' +
          native.icon +
          '" is not a PNG, which an iOS app icon has to be.'
      );
    }
    if (!fs.existsSync(path.join(projectDir, native.icon))) {
      throw new Error(
        'The icon "' +
          native.icon +
          '" does not exist (relative to the project).'
      );
    }
  }
}

/** Set manual signing (profile UUID, identity SHA-1) on the app target, the
 * one with a bundle ID. Not via xcodebuild flags: those would also hit the Pods'
 * targets, which reject a profile. */
function applySigning(project, signing) {
  const configs = project.pbxXCBuildConfigurationSection();
  let signed = 0;
  for (const key of Object.keys(configs)) {
    const settings = configs[key] && configs[key].buildSettings;
    if (!settings || !settings.PRODUCT_BUNDLE_IDENTIFIER) continue;
    for (const name of Object.keys(settings)) {
      if (name.replace(/"/g, "").startsWith("CODE_SIGN_IDENTITY["))
        delete settings[name];
    }
    settings.CODE_SIGN_STYLE = "Manual";
    settings.DEVELOPMENT_TEAM = signing.info.team;
    settings.PROVISIONING_PROFILE_SPECIFIER = '"' + signing.info.uuid + '"';
    settings.CODE_SIGN_IDENTITY = '"' + signing.identity.sha1 + '"';
    signed++;
  }
  if (!signed)
    throw new Error("The iOS project has no target with a bundle ID to sign.");
}

module.exports = { prebuildSigning, checkConfig, applySigning };
