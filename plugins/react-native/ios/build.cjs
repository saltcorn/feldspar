"use strict";
// Entry point for the iOS build scripts (Saltcorn's "iOS app" and "iOS
// simulator app" buttons):
//
//   node src/feldspar/ios/build.cjs simulator   unsigned, ios-output/app-simulator.zip
//   node src/feldspar/ios/build.cjs device      signed,   ios-output/app.ipa
//
// A device build is an .ipa for App Store Connect, signed from an App Store
// provisioning profile: uploaded, or generated (profile-generator.cjs).
// Reading the profile is in signing.cjs.

const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");
const {
  deviceSigning,
  exportOptions,
  keychainIdentities,
} = require("./signing.cjs");
const { IosProfileGenerator } = require("./profile-generator.cjs");

/** Copy the profile to where Xcode finds it by UUID (Xcode 16+ and older). */
function installProfile(signing) {
  const home = os.homedir();
  for (const dir of [
    path.join(
      home,
      "Library",
      "Developer",
      "Xcode",
      "UserData",
      "Provisioning Profiles"
    ),
    path.join(home, "Library", "MobileDevice", "Provisioning Profiles"),
  ]) {
    fs.mkdirSync(dir, { recursive: true });
    fs.copyFileSync(
      signing.file,
      path.join(dir, signing.info.uuid + ".mobileprovision")
    );
  }
}

/** Expo's Xcode build scripts that break on a space in the project path,
 * with quoted replacements. Matched exactly, so a changed script is left alone. */
const SPACE_FIXES = [
  {
    project: "pods",
    from: 'shellScript = "bash -l -c \\"$PODS_TARGET_SRCROOT/../scripts/get-app-config-ios.sh\\"";',
    to:
      'shellScript = "PROJECT_ROOT=\\"$PODS_ROOT/../..\\" PROJECT_DIR=Pods bash -l ' +
      '\\"$PODS_TARGET_SRCROOT/../scripts/get-app-config-ios.sh\\"";',
  },
  {
    project: "app",
    from:
      "`\\\"$NODE_BINARY\\\" --print \\\"require('path').dirname(require.resolve('react-native/package.json'))" +
      " + '/scripts/react-native-xcode.sh'\\\"`",
    to:
      "\\\"$(\\\"$NODE_BINARY\\\" --print \\\"require('path').dirname(require.resolve('react-native/package.json'))" +
      " + '/scripts/react-native-xcode.sh'\\\")\\\"",
  },
];

/** Paths of the two project.pbxproj files to patch: CocoaPods' (Pods/) and
 * the app's (<name>.xcodeproj, null if not there yet). */
function xcodeProjects(iosDir) {
  const app = fs.existsSync(iosDir)
    ? fs.readdirSync(iosDir).find((f) => f.endsWith(".xcodeproj"))
    : null;
  return {
    pods: path.join(iosDir, "Pods", "Pods.xcodeproj", "project.pbxproj"),
    app: app ? path.join(iosDir, app, "project.pbxproj") : null,
  };
}

/** Apply SPACE_FIXES; returns how many applied. Warns if one is missing. */
function quoteScriptPaths(iosDir) {
  const files = xcodeProjects(iosDir);
  let fixed = 0;
  for (const { project, from, to } of SPACE_FIXES) {
    const file = files[project];
    const text =
      file && fs.existsSync(file) ? fs.readFileSync(file, "utf8") : "";
    if (text.includes(from)) {
      fs.writeFileSync(file, text.split(from).join(to));
      fixed++;
    } else if (/\s/.test(iosDir) && !text.includes(to)) {
      console.warn(
        "warning: the project path has a space, and a build phase known to break on it was " +
          "not found in the " +
          project +
          " project to fix; if the archive fails with " +
          "'No such file or directory', move the file store to a path without spaces."
      );
    }
  }
  return fixed;
}

/** Generate the App Store profile into outDir and return its path. The
 * certificate must be in this Mac's keychain. */
async function generateProfile(native, env, outDir) {
  const generator = new IosProfileGenerator(env);
  const profile = await generator.generate(
    (native.ios || {}).bundleId,
    keychainIdentities()
  );
  const file = path.join(outDir, "app-store.mobileprovision");
  fs.mkdirSync(outDir, { recursive: true });
  fs.writeFileSync(file, Buffer.from(profile.content, "base64"));
  return file;
}

function run(command, args, options) {
  console.log("$ " + [command].concat(args).join(" "));
  execFileSync(command, args, Object.assign({ stdio: "inherit" }, options));
}

/** The simulator build's Xcode configuration. A Debug app has no JavaScript
 * inside: it loads it from Metro (`npx expo start`), as Expo's dev runtime
 * requires. Embedding it does not help: that runtime refuses to run from an
 * embedded bundle. */
function simulatorConfiguration(native) {
  return {
    configuration:
      (native.ios || {}).simulatorConfiguration === "debug"
        ? "Debug"
        : "Release",
  };
}

/** The one entry in `dir` ending in `suffix`; throws if there are none or several. */
function findSingle(dir, suffix, what) {
  const found = fs.existsSync(dir)
    ? fs.readdirSync(dir).filter((f) => f.endsWith(suffix))
    : [];
  if (found.length !== 1) {
    throw new Error(
      "Expected one " +
        what +
        " in " +
        dir +
        ", found " +
        (found.join(", ") || "none") +
        "."
    );
  }
  return found[0];
}

async function main(kind) {
  if (kind !== "simulator" && kind !== "device") {
    throw new Error("usage: node build.cjs simulator|device");
  }
  const projectDir = process.cwd();
  const native = JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "native.json"), "utf8")
  );
  const env = Object.assign({}, process.env, { FELDSPAR_IOS_BUILD: kind });
  // CocoaPods needs a UTF-8 locale, which a service may not have.
  if (!/UTF-8/i.test(env.LANG || "")) env.LANG = "en_US.UTF-8";
  // The module's CocoaPods directory, for a pod not on the server's PATH.
  if (env.FELDSPAR_POD_DIR)
    env.PATH = env.FELDSPAR_POD_DIR + path.delimiter + (env.PATH || "");
  const outDir = path.join(projectDir, "ios-output");
  const artifact = path.join(
    outDir,
    kind === "device" ? "app.ipa" : "app-simulator.zip"
  );
  fs.rmSync(artifact, { force: true });

  // Check signing first, so problems show up now and not ten minutes into
  // the archive. A generated profile is passed on by path so expo prebuild
  // uses the same one.
  delete env.FELDSPAR_IOS_PROFILE_FILE;
  if (kind === "device" && (native.ios || {}).profileSource === "generate") {
    env.FELDSPAR_IOS_PROFILE_FILE = await generateProfile(native, env, outDir);
  }
  process.env.FELDSPAR_IOS_PROFILE_FILE = env.FELDSPAR_IOS_PROFILE_FILE || "";
  const signing = kind === "device" ? deviceSigning(projectDir, native) : null;
  if (signing) {
    console.log(
      'Signing with the profile "' +
        signing.info.name +
        '" (' +
        signing.info.method +
        ", team " +
        signing.info.team +
        ") as " +
        signing.bundleId +
        ', identity "' +
        signing.identity.name +
        '".'
    );
    installProfile(signing);
  }

  run(
    path.join(projectDir, "node_modules", ".bin", "expo"),
    ["prebuild", "--platform", "ios", "--clean", "--no-install"],
    {
      cwd: projectDir,
      env,
    }
  );
  const iosDir = path.join(projectDir, "ios");
  run("pod", ["install"], { cwd: iosDir, env });
  const quoted = quoteScriptPaths(iosDir);
  if (quoted)
    console.log("Quoted the project path in " + quoted + " build phase(s).");
  const workspace = findSingle(iosDir, ".xcworkspace", "Xcode workspace");
  const scheme = workspace.slice(0, -".xcworkspace".length);
  const build = path.join(iosDir, "build");
  // A device build is always Release (App Store); the simulator's is chosen.
  const sim = simulatorConfiguration(native);
  const configuration = signing ? "Release" : sim.configuration;
  const common = [
    "-workspace",
    workspace,
    "-scheme",
    scheme,
    "-configuration",
    configuration,
    "-derivedDataPath",
    path.join(build, "derived"),
  ];
  fs.mkdirSync(outDir, { recursive: true });

  if (!signing) {
    run(
      "xcodebuild",
      common.concat([
        "-sdk",
        "iphonesimulator",
        "-destination",
        "generic/platform=iOS Simulator",
        "CODE_SIGNING_ALLOWED=NO",
        "build",
      ]),
      { cwd: iosDir, env }
    );
    const products = path.join(
      build,
      "derived",
      "Build",
      "Products",
      configuration + "-iphonesimulator"
    );
    const app = findSingle(products, ".app", "app");
    run("ditto", [
      "-c",
      "-k",
      "--sequesterRsrc",
      "--keepParent",
      path.join(products, app),
      artifact,
    ]);
  } else {
    const archive = path.join(build, scheme + ".xcarchive");
    run(
      "xcodebuild",
      common.concat([
        "-destination",
        "generic/platform=iOS",
        "-archivePath",
        archive,
        "archive",
      ]),
      { cwd: iosDir, env }
    );
    const optionsFile = path.join(build, "ExportOptions.plist");
    fs.writeFileSync(optionsFile, exportOptions(signing));
    const exportDir = path.join(build, "export");
    run(
      "xcodebuild",
      [
        "-exportArchive",
        "-archivePath",
        archive,
        "-exportPath",
        exportDir,
        "-exportOptionsPlist",
        optionsFile,
      ],
      { cwd: iosDir, env }
    );
    fs.copyFileSync(
      path.join(exportDir, findSingle(exportDir, ".ipa", ".ipa")),
      artifact
    );
  }
  console.log("Wrote " + path.relative(projectDir, artifact));
}

module.exports = { quoteScriptPaths, SPACE_FIXES, simulatorConfiguration };

// Run as the build script; required (by a test), only export.
if (require.main === module) {
  main(process.argv[2]).catch((e) => {
    console.error(e.message);
    process.exit(1);
  });
}
