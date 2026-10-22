//! `plugins/react-native` — the bundled React Native (Expo) module — installed
//! from the directory it ships in, and asked for a project.
//!
//! The same questions `bundled_vue.rs` asks of the Vue module: does the
//! declaration translate, does every relative import in the generated project
//! resolve, and does a rebuild leave the developer's screens alone. More are this
//! framework's own: it declares an Android build target, its screens use no DOM
//! (the same source is the web bundle and the APK), and a phone's requests go to
//! the application's URL rather than to a page it does not have.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{have_npm, temp_root};
use sc_app::{DeclaredFile, FilePhase, FrameworkHost};
use sc_module::{
    BundledModules, Installer, LoadedModule, Module, ModuleFrameworks, ModuleHost,
    ModulePermissions, ModuleSet, ModuleSource,
};
use sc_types::{Attrs, ShowIfCondition};
use serde_json::{Value as Json, json};

/// The framework's name, as an application row stores it.
const FRAMEWORK: &str = "react-native";

/// The files Saltcorn writes into the runtime directory for every framework,
/// which the module's project may import and must not write.
const SALTCORN_WRITES: [&str; 7] = [
    "client.ts",
    "helper.ts",
    "offline-types.ts",
    "offline-backends.ts",
    "offline-copy.ts",
    "schema.sql",
    "SKILL.md",
];

/// The bundled module, installed into a throwaway root and loaded on a worker.
async fn frameworks(tag: &str) -> ModuleFrameworks {
    frameworks_configured(tag, json!({})).await.0
}

/// [`frameworks`] with the module's own settings — what the Modules tab saves —
/// answering the manifest too, for what the module declared about them.
async fn frameworks_configured(
    tag: &str,
    configuration: Json,
) -> (ModuleFrameworks, sc_module::ModuleManifest) {
    let catalog = BundledModules::discover(None);
    let entry = catalog
        .get("react-native")
        .expect("the React Native module ships");
    let root = temp_root(tag);
    let installer = Installer::new(&root);
    let package = installer
        .install(ModuleSource::Local, &entry.directory.display().to_string())
        .await
        .unwrap_or_else(|e| panic!("installing plugins/react-native: {e}"));
    assert_eq!(package.name, entry.name);

    let host = Arc::new(ModuleHost::new(&root));
    let manifest = host
        .load(
            &package.name,
            &installer.package_dir(&package.name),
            &configuration,
            &ModulePermissions::closed(),
        )
        .await
        .expect("the bundled React Native module loads");
    assert!(
        manifest.issues.is_empty(),
        "the module this server ships should declare nothing it has to apologise for: {:?}",
        manifest.issues
    );

    let module = Module::new(
        &package.name,
        ModuleSource::Bundled,
        entry.directory.display().to_string(),
    );
    let set = ModuleSet::empty().merged(vec![LoadedModule {
        module,
        manifest: Some(manifest.clone()),
        config_spec: Vec::new(),
        issues: Vec::new(),
    }]);
    let frameworks = ModuleFrameworks::new(&host, &set);
    assert!(
        frameworks.issues().is_empty(),
        "the declaration translates cleanly: {:?}",
        frameworks.issues()
    );
    (frameworks, manifest)
}

/// A writable table with a numeric key and all three control kinds.
fn tasks_table() -> Json {
    json!({
        "name": "tasks",
        "pascal": "Tasks",
        "title": "Tasks",
        "client": "tasks",
        "pk": "id",
        "pk_ts_type": "number",
        "storable": true,
        "ops": { "list": true, "get": true, "create": true, "update": true, "delete": true },
        "fields": [
            { "name": "id", "label": "Id", "control": "number", "ts_type": "number",
              "empty": "0", "required": true, "primary_key": true },
            { "name": "title", "label": "Title", "control": "text", "ts_type": "string",
              "empty": "\"\"", "required": true, "primary_key": false },
            { "name": "estimate", "label": "Estimate", "control": "number", "ts_type": "number",
              "empty": "0", "required": false, "primary_key": false },
            { "name": "done", "label": "Done", "control": "checkbox", "ts_type": "boolean",
              "empty": "false", "required": false, "primary_key": false }
        ],
        "form": {
            "inputs": [
                { "name": "title", "label": "Title", "control": "text", "ts_type": "string",
                  "empty": "\"\"", "required": true, "primary_key": false },
                { "name": "estimate", "label": "Estimate", "control": "number", "ts_type": "number",
                  "empty": "0", "required": false, "primary_key": false },
                { "name": "done", "label": "Done", "control": "checkbox", "ts_type": "boolean",
                  "empty": "false", "required": false, "primary_key": false }
            ],
            "minted": []
        }
    })
}

/// A read-only table with no addressable row.
fn readings_table() -> Json {
    json!({
        "name": "readings",
        "pascal": "Readings",
        "title": "Readings",
        "client": "readings",
        "pk": null,
        "pk_ts_type": "string",
        "storable": false,
        "ops": { "list": true, "get": false, "create": false, "update": false, "delete": false },
        "fields": [
            { "name": "at", "label": "At", "control": "text", "ts_type": "string",
              "empty": "\"\"", "required": true, "primary_key": false }
        ],
        "form": { "inputs": [], "minted": [] }
    })
}

fn context(auth: bool, tables: Vec<Json>) -> Json {
    json!({
        "project": "todo",
        "name": "todo",
        "runtime": "src/feldspar",
        "client": "src/feldspar/client.ts",
        "auth": auth,
        "app": { "name": "My Todo", "subdomain": "todo", "description": "",
                 "url": "https://todo.example.com" },
        "tables": tables,
        "graphql": null,
        "schema_sql": "CREATE TABLE tasks ();",
        "roles": [{ "name": "Admin", "role": 1, "description": "" }],
        "skill": "# SKILL\n"
    })
}

async fn scaffold(tag: &str, auth: bool, tables: Vec<Json>) -> Vec<DeclaredFile> {
    frameworks(tag)
        .await
        .framework_files(FRAMEWORK, FilePhase::Scaffold, context(auth, tables))
        .await
        .expect("the scaffold runs")
}

fn file(files: &[DeclaredFile], path: &str) -> String {
    files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path}"))
        .contents
        .clone()
}

/// Every relative import in every generated file, resolved against what the
/// project will actually contain.
fn assert_imports_resolve(files: &[DeclaredFile]) {
    let mut present: BTreeSet<String> = files.iter().map(|f| f.path.clone()).collect();
    for name in SALTCORN_WRITES {
        present.insert(format!("src/feldspar/{name}"));
    }
    for file in files {
        let dir = file.path.rsplit_once('/').map_or("", |(dir, _)| dir);
        for spec in relative_imports(&file.contents) {
            let resolved = resolve(dir, &spec);
            let found = ["", ".ts", ".tsx"]
                .iter()
                .any(|ext| present.contains(&format!("{resolved}{ext}")));
            assert!(
                found,
                "{} imports `{spec}`, which resolves to `{resolved}` and nothing writes it. \
                 The project has: {present:?}",
                file.path
            );
        }
    }
}

/// The `./x` and `../x` specifiers of every `import`/`export … from` in a file.
fn relative_imports(contents: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, _) in contents.match_indices("from \"") {
        let rest = &contents[index + 6..];
        let Some(end) = rest.find('"') else { continue };
        let spec = &rest[..end];
        if spec.starts_with("./") || spec.starts_with("../") {
            out.push(spec.to_owned());
        }
    }
    out
}

/// `src/screens` + `../feldspar/hooks` → `src/feldspar/hooks`.
fn resolve(dir: &str, spec: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for segment in spec.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[tokio::test]
async fn the_bundled_react_native_module_declares_a_web_framework() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let frameworks = frameworks("bundled-rn-decl").await;
    let declared = frameworks.frameworks();
    assert_eq!(declared.len(), 1);
    let rn = &declared[0];
    assert_eq!(rn.name, FRAMEWORK);
    assert_eq!(rn.info().label, "React Native");
    assert!(rn.scaffolds);
    assert_eq!(rn.checks, ["typecheck"]);
    assert_eq!(
        rn.config_spec.iter().map(|f| f.name()).collect::<Vec<_>>(),
        [
            "store",
            "project",
            "mobile_url",
            "app_id",
            "app_version",
            "app_icon",
            "build_type",
            "package_format",
            "own_keystore",
            "keystore_file",
            "keystore_alias",
            "keystore_password",
            "ios_profile_source",
            "ios_profile",
            "simulator_configuration"
        ]
    );

    let config: Attrs = [
        ("store".to_owned(), json!("apps")),
        ("project".to_owned(), json!("todo")),
    ]
    .into_iter()
    .collect();
    let spec = rn.build_spec(&config).unwrap();
    assert_eq!(spec.command, "npm");
    assert_eq!(spec.args, ["run", "build"]);
    assert_eq!(spec.output_dir, "todo/dist");
    assert_eq!(
        rn.client_path(&config).unwrap().unwrap(),
        "todo/src/feldspar/client.ts"
    );

    // The APK is a build target beside the web bundle, run in the same project,
    // and so are the two iOS builds.
    assert_eq!(
        rn.targets
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["android", "ios", "ios_simulator"]
    );
    let apk = rn.target_spec("android", &config).unwrap();
    assert_eq!(apk.label, "Android app");
    assert_eq!(apk.command, "npm");
    // A release APK unless the application says otherwise, left where
    // `prebuild --clean` does not delete it.
    assert_eq!(apk.args, ["run", "build:android:release-apk"]);
    assert_eq!(apk.source_dir, "todo");
    assert_eq!(apk.artifact, "todo/android-output/app-release.apk");
    // An AAB for the Play Store, offered for a release build only.
    let mut aab = config.clone();
    aab.insert("package_format".to_owned(), json!("aab"));
    let spec = rn.target_spec("android", &aab).unwrap();
    assert_eq!(spec.args, ["run", "build:android:release-aab"]);
    assert_eq!(spec.artifact, "todo/android-output/app-release.aab");
    // Its own settings, which the application form shows under the target. The
    // App ID, version and icon are not among them: they are the application's,
    // shared by Android and iOS, and shown with the framework's settings.
    assert_eq!(
        rn.targets[0].options,
        [
            "build_type",
            "package_format",
            "own_keystore",
            "keystore_file",
            "keystore_alias",
            "keystore_password"
        ]
    );
    let icon = rn
        .config_spec
        .iter()
        .find(|f| f.name() == "app_icon")
        .unwrap();
    // The native apps' settings are a group of their own under a heading,
    // starting at the server URL; the project's above it have none.
    let sections: Vec<(&str, Option<&str>)> = rn
        .config_spec
        .iter()
        .take(6)
        .map(|f| (f.name(), f.section.as_deref()))
        .collect();
    assert_eq!(
        sections,
        [
            ("store", None),
            ("project", None),
            ("mobile_url", Some("Native apps (Android and iOS)")),
            ("app_id", None),
            ("app_version", None),
            ("app_icon", None)
        ]
    );
    // Picked from the application's store, and a PNG: iOS takes nothing else.
    assert_eq!(icon.query(), Some("store_files:png"));
    for shared in ["app_id", "app_version", "app_icon"] {
        assert!(
            rn.targets
                .iter()
                .all(|t| !t.options.iter().any(|o| o == shared)),
            "{shared} belongs to no target"
        );
    }
    // The keystore settings: a release build's, once its own keystore is
    // chosen, and the password a secret that reaches the build as its
    // environment.
    let field = |name: &str| rn.config_spec.iter().find(|f| f.name() == name).unwrap();
    assert_eq!(
        field("package_format").show_if,
        [ShowIfCondition::new("build_type", vec![json!("release")])]
    );
    assert_eq!(
        field("own_keystore").show_if,
        [ShowIfCondition::new("build_type", vec![json!("release")])]
    );
    assert_eq!(
        field("keystore_alias").show_if,
        [
            ShowIfCondition::new("build_type", vec![json!("release")]),
            ShowIfCondition::new("own_keystore", vec![json!(true)])
        ]
    );
    assert!(field("keystore_password").secret);
    assert_eq!(
        field("keystore_file").query(),
        Some("store_files:jks,keystore,p12")
    );
    let mut signed = config.clone();
    signed.insert("keystore_password".to_owned(), json!("s3cret"));
    let apk = rn.target_spec("android", &signed).unwrap();
    assert_eq!(apk.env["FELDSPAR_KEYSTORE_PASSWORD"], "s3cret");
    // Unsigned, or switched off with nothing filled in: the build needs nothing.
    sc_types::validate_attrs(&rn.config_spec, &config).unwrap();
    let mut debug = config.clone();
    debug.insert("build_type".to_owned(), json!("debug"));
    let apk = rn.target_spec("android", &debug).unwrap();
    assert_eq!(apk.args, ["run", "build:android:debug-apk"]);
    assert_eq!(apk.artifact, "todo/android-output/app-debug.apk");

    // iOS: an IPA for App Store Connect, signed from an App Store profile
    // that is either the admin's own or generated, and an unsigned simulator
    // app, which needs neither.
    let ipa = rn.target_spec("ios", &config).unwrap();
    assert_eq!(ipa.label, "iOS app");
    assert_eq!(ipa.args, ["run", "build:ios:device"]);
    assert_eq!(ipa.source_dir, "todo");
    assert_eq!(ipa.artifact, "todo/ios-output/app.ipa");
    assert_eq!(rn.targets[1].options, ["ios_profile_source", "ios_profile"]);
    assert_eq!(field("ios_profile_source").default, Some(json!("own")));
    let mut generated = config.clone();
    generated.insert("ios_profile_source".to_owned(), json!("generate"));
    sc_types::validate_attrs(&rn.config_spec, &generated).unwrap();
    generated.insert("ios_profile_source".to_owned(), json!("ad-hoc"));
    assert!(sc_types::validate_attrs(&rn.config_spec, &generated).is_err());
    // An uploaded profile is asked for only when it is the source, and never
    // required: an application need not be built for iOS at all.
    assert_eq!(
        field("ios_profile").query(),
        Some("store_files:mobileprovision")
    );
    assert_eq!(
        field("ios_profile").show_if,
        [ShowIfCondition::new(
            "ios_profile_source",
            vec![json!("own")]
        )]
    );
    assert!(!field("ios_profile").required);
    let sim = rn.target_spec("ios_simulator", &config).unwrap();
    assert_eq!(sim.label, "iOS simulator app");
    assert_eq!(sim.args, ["run", "build:ios:simulator"]);
    assert_eq!(sim.artifact, "todo/ios-output/app-simulator.zip");
    // Release unless asked: Debug is for the dev menu and warnings.
    assert_eq!(rn.targets[2].options, ["simulator_configuration"]);
    assert_eq!(
        field("simulator_configuration").default,
        Some(json!("release"))
    );
    let mut debug_sim = config.clone();
    debug_sim.insert("simulator_configuration".to_owned(), json!("debug"));
    sc_types::validate_attrs(&rn.config_spec, &debug_sim).unwrap();
    debug_sim.insert("simulator_configuration".to_owned(), json!("profile"));
    assert!(sc_types::validate_attrs(&rn.config_spec, &debug_sim).is_err());

    // One widening beyond React's policy: react-native-web injects its styles at
    // run time. Scripts stay strict.
    let csp = rn.default_csp();
    assert_eq!(csp.directives["style-src"], ["'self'", "'unsafe-inline'"]);
    assert!(!csp.directives.contains_key("script-src"));
    assert!(!csp.header_value().contains("unsafe-eval"));
    assert_eq!(csp.directives["default-src"], ["'self'"]);
    assert_eq!(csp.directives["frame-ancestors"], ["'none'"]);
}

/// The Android toolchain is the module's own setting, edited on the Modules
/// tab, and it reaches the APK build as `ANDROID_HOME` and `JAVA_HOME` — so the
/// server does not depend on the shell that started it.
#[tokio::test]
async fn the_modules_toolchain_settings_are_the_apk_builds_environment() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let (configured, manifest) = frameworks_configured(
        "bundled-rn-toolchain",
        json!({
            "android_home": "/opt/android-sdk",
            "java_home": "",
            "use_pod_dir": true,
            "pod_dir": "/opt/homebrew/bin",
            "use_asc": true,
            "asc_issuer_id": "69a6de70-0000-47e3-e053-5b8c7c11a4d1",
            "asc_key_id": "2X9R4HXF34",
            "asc_key": "-----BEGIN PRIVATE KEY-----\nMIGT\n-----END PRIVATE KEY-----",
            "ios_distribution_identity": ""
        }),
    )
    .await;
    // The settings the Modules tab shows for this module.
    let names: Vec<&str> = manifest
        .config_fields
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(
        names,
        [
            "android_home",
            "java_home",
            "use_pod_dir",
            "pod_dir",
            "use_asc",
            "asc_issuer_id",
            "asc_key_id",
            "asc_key",
            "ios_distribution_identity"
        ]
    );

    let config: Attrs = [("store".to_owned(), json!("apps"))].into_iter().collect();
    let apk = configured.frameworks()[0]
        .target_spec("android", &config)
        .unwrap();
    assert_eq!(
        apk.env.get("ANDROID_HOME").map(String::as_str),
        Some("/opt/android-sdk")
    );
    // A setting left blank leaves the variable to the server's environment.
    assert!(!apk.env.contains_key("JAVA_HOME"), "{:?}", apk.env);

    // And the target says what it needs, so a server without the toolchain is
    // told before Gradle runs: both directories, met by these settings or by the
    // server's own environment.
    let names: Vec<String> = apk
        .requires
        .iter()
        .map(|r| match &r.kind {
            sc_app::TargetRequirementKind::Env { name, directory } => {
                assert!(directory, "{name} must be a directory");
                name.clone()
            }
            other => panic!("an Android build needs only its directories: {other:?}"),
        })
        .collect();
    assert_eq!(names, ["ANDROID_HOME", "JAVA_HOME"]);
    assert!(
        apk.requires
            .iter()
            .all(|r| r.hint.contains("Settings → Modules"))
    );

    // Both iOS builds need a Mac with Xcode (the one xcode-select points to)
    // and CocoaPods, which may also be in the module's CocoaPods directory:
    // the builds get it as `FELDSPAR_POD_DIR`, and `ios/build.cjs` puts it on the
    // PATH. What signing needs depends on the profile's source, so `ios/build.cjs`
    // checks it rather than a requirement.
    let rn = &configured.frameworks()[0];
    for target in ["ios", "ios_simulator"] {
        let spec = rn.target_spec(target, &config).unwrap();
        assert!(!spec.env.contains_key("DEVELOPER_DIR"), "{target}");
        assert_eq!(
            spec.env["FELDSPAR_POD_DIR"], "/opt/homebrew/bin",
            "{target}"
        );
        let needs: Vec<String> = spec
            .requires
            .iter()
            .map(|r| match &r.kind {
                sc_app::TargetRequirementKind::Os { name } => format!("os {name}"),
                sc_app::TargetRequirementKind::Command { name, dir_env } => match dir_env {
                    Some(dir) => format!("command {name} or in {dir}"),
                    None => format!("command {name}"),
                },
                sc_app::TargetRequirementKind::Env { name, directory } => {
                    assert!(!directory, "{name}");
                    format!("env {name}")
                }
            })
            .collect();
        assert_eq!(
            needs,
            [
                "os macos",
                "command xcodebuild",
                "command pod or in FELDSPAR_POD_DIR"
            ],
            "{target}"
        );
        assert!(spec.requires.iter().all(|r| !r.hint.is_empty()), "{target}");
    }

    // With the API switched on, the App Store Connect key is the server's, so
    // it reaches the App Store build from the module's settings; the blank
    // identity is left out. The profile operation is offered.
    let ipa = rn.target_spec("ios", &config).unwrap();
    assert_eq!(
        ipa.env.get("FELDSPAR_ASC_KEY").map(String::as_str),
        Some("-----BEGIN PRIVATE KEY-----\nMIGT\n-----END PRIVATE KEY-----")
    );
    assert_eq!(ipa.env["FELDSPAR_ASC_KEY_ID"], "2X9R4HXF34");
    assert!(ipa.env.contains_key("FELDSPAR_ASC_ISSUER_ID"));
    assert!(!ipa.env.contains_key("FELDSPAR_IOS_IDENTITY"));
    let sim = rn.target_spec("ios_simulator", &config).unwrap();
    assert!(!sim.env.contains_key("FELDSPAR_ASC_KEY"));
    let ios = rn.targets.iter().find(|t| t.name == "ios").unwrap();
    assert_eq!(ios.operations[0].name, "generate_profile");

    // Switched off, the key stays with the module: no API settings reach the
    // build. The operation is still offered, and refuses (see
    // `the_profile_operation_runs_in_the_modules_worker`).
    let (off, _) = frameworks_configured(
        "bundled-rn-toolchain-asc-off",
        json!({
            "use_pod_dir": false,
            "pod_dir": "/opt/homebrew/bin",
            "use_asc": false,
            "asc_key_id": "2X9R4HXF34",
            "asc_key": "secret"
        }),
    )
    .await;
    let rn_off = &off.frameworks()[0];
    let ipa = rn_off.target_spec("ios", &config).unwrap();
    assert!(
        ipa.env.keys().all(|k| !k.starts_with("FELDSPAR_ASC")),
        "{:?}",
        ipa.env
    );
    let ios = rn_off.targets.iter().find(|t| t.name == "ios").unwrap();
    assert_eq!(ios.operations[0].name, "generate_profile");
    // Likewise the CocoaPods directory, with its checkbox off.
    assert!(!ipa.env.contains_key("FELDSPAR_POD_DIR"), "{:?}", ipa.env);

    // Grouped under a heading per platform on the Modules tab, the API's
    // settings shown only while it is switched on, and each explained under
    // its input.
    let (spec, issues) = sc_module::config_fields_to_form_fields(&manifest.config_fields, "test");
    assert!(issues.is_empty(), "{issues:?}");
    let headed: Vec<(&str, &str)> = spec
        .iter()
        .filter_map(|f| f.section.as_deref().map(|s| (f.name(), s)))
        .collect();
    assert_eq!(
        headed,
        [("android_home", "Android"), ("use_pod_dir", "iOS")]
    );
    let pod = spec.iter().find(|f| f.name() == "pod_dir").unwrap();
    assert_eq!(
        pod.show_if,
        [ShowIfCondition::new("use_pod_dir", vec![json!(true)])]
    );
    for name in [
        "asc_issuer_id",
        "asc_key_id",
        "asc_key",
        "ios_distribution_identity",
    ] {
        let field = spec.iter().find(|f| f.name() == name).unwrap();
        assert_eq!(
            field.show_if,
            [ShowIfCondition::new("use_asc", vec![json!(true)])],
            "{name}"
        );
    }
    assert!(
        spec.iter().all(|f| f.sublabel.is_some()),
        "every setting explained"
    );
    assert!(
        pod.sublabel.as_deref().unwrap().contains("which pod"),
        "{:?}",
        pod.sublabel
    );
    // The API key's three parts are refused on save unless they look like
    // what App Store Connect hands out: a pasted key keeps its PEM lines, and
    // whitespace around a value is fine (the generator trims it).
    let api = |issuer: &str, key_id: &str, key: &str| {
        let mut attrs = sc_types::Attrs::new();
        attrs.insert("use_asc".to_owned(), json!(true));
        attrs.insert("asc_issuer_id".to_owned(), json!(issuer));
        attrs.insert("asc_key_id".to_owned(), json!(key_id));
        attrs.insert("asc_key".to_owned(), json!(key));
        sc_types::validate_patterns(&spec, &attrs)
    };
    let pem = "-----BEGIN PRIVATE KEY-----\nMIGTAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBHkw\nGJ+1/q==\n-----END PRIVATE KEY-----\n";
    api("57246542-96fe-1a63-e053-0824d011072a ", "2X9R4HXF34", pem).unwrap();
    // A key pasted into the one-line field loses its line breaks.
    api(
        "57246542-96fe-1a63-e053-0824d011072a",
        " 2X9R4HXF34",
        &pem.replace('\n', ""),
    )
    .unwrap();
    for (issuer, key_id, key, wanted) in [
        ("issuer-1", "2X9R4HXF34", pem, "UUID"),
        (
            "57246542-96fe-1a63-e053-0824d011072a",
            "2x9r4hxf34",
            pem,
            "ten capital",
        ),
        (
            "57246542-96fe-1a63-e053-0824d011072a",
            "2X9R4HXF34",
            "MIGTAgEAMBMG",
            "whole .p8",
        ),
    ] {
        let err = api(issuer, key_id, key).unwrap_err().to_string();
        assert!(err.contains(wanted), "{err}");
    }

    // The key is a secret: a password input, masked by the admin API.
    let key = manifest
        .config_fields
        .iter()
        .find(|f| f["name"] == "asc_key")
        .unwrap();
    assert_eq!(key["input_type"], "password");

    // And unconfigured, the build carries no environment of its own at all.
    let unconfigured = frameworks("bundled-rn-toolchain-none").await;
    for target in ["android", "ios", "ios_simulator"] {
        let spec = unconfigured.frameworks()[0]
            .target_spec(target, &config)
            .unwrap();
        assert!(spec.env.is_empty(), "{target}: {:?}", spec.env);
    }
}

#[tokio::test]
async fn the_scaffold_is_an_expo_project_whose_every_import_resolves() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let files = scaffold(
        "bundled-rn-scaffold",
        true,
        vec![tasks_table(), readings_table()],
    )
    .await;

    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    for expected in [
        "package.json",
        "app.config.js",
        "tsconfig.json",
        "AGENTS.md",
        "index.ts",
        "src/Root.tsx",
        "src/App.tsx",
        "src/routes.tsx",
        "src/navigation.tsx",
        "src/theme.ts",
        "src/auth.tsx",
        "src/Login.tsx",
        "src/screens/Home.tsx",
        "src/screens/Tasks.tsx",
        "src/screens/Readings.tsx",
        "src/feldspar/hooks.ts",
        "src/feldspar/config.ts",
        "src/feldspar/native.json",
        "src/feldspar/ios/build.cjs",
        "src/feldspar/ios/prebuild.cjs",
        "src/feldspar/ios/signing.cjs",
        "src/feldspar/ios/profile-generator.cjs",
        "src/feldspar/README.md",
    ] {
        assert!(paths.contains(&expected), "missing {expected}: {paths:?}");
    }
    for name in SALTCORN_WRITES {
        let path = format!("src/feldspar/{name}");
        assert!(!paths.contains(&path.as_str()), "the module wrote {path}");
    }
    assert_imports_resolve(&files);

    // One Expo project: the web bundle is `expo export`, the APK is prebuild plus
    // Gradle, both over the same dependencies.
    let package = file(&files, "package.json");
    for dependency in ["\"expo\":", "\"react-native\":", "\"react-native-web\":"] {
        assert!(package.contains(dependency), "{dependency}: {package}");
    }
    assert!(
        package.contains("tsc --noEmit && expo export --platform web --output-dir dist"),
        "{package}"
    );
    assert!(
        package.contains("\"typecheck\": \"tsc --noEmit\""),
        "{package}"
    );
    // One script per build type and package, each copying its result to
    // `android-output/`.
    for (script, task, output) in [
        (
            "release-apk",
            "assembleRelease",
            "apk/release/app-release.apk",
        ),
        (
            "release-aab",
            "bundleRelease",
            "bundle/release/app-release.aab",
        ),
        ("debug-apk", "assembleDebug", "apk/debug/app-debug.apk"),
        ("debug-aab", "bundleDebug", "bundle/debug/app-debug.aab"),
    ] {
        let line = package
            .lines()
            .find(|l| l.contains(&format!("\"build:android:{script}\"")))
            .unwrap_or_else(|| panic!("no {script} script: {package}"));
        assert!(
            line.contains("expo prebuild --platform android --clean"),
            "{line}"
        );
        assert!(line.contains(&format!("./gradlew {task}")), "{line}");
        assert!(
            line.contains(&format!("cp app/build/outputs/{output} ../android-output/")),
            "{line}"
        );
    }
    assert!(file(&files, ".gitignore").contains("android-output"));
    // The iOS builds are the generated `ios/build.cjs`, so a fix to them reaches every
    // project at its next build; their results go to `ios-output/`.
    for (script, kind) in [("simulator", "simulator"), ("device", "device")] {
        assert!(
            package.contains(&format!(
                "\"build:ios:{script}\": \"node src/feldspar/ios/build.cjs {kind}\""
            )),
            "{package}"
        );
    }
    assert!(file(&files, ".gitignore").contains("ios-output"));
    // Only the project's own native directories are ignored, not the runtime's
    // `src/feldspar/ios/`.
    let gitignore = file(&files, ".gitignore");
    let ignored: Vec<&str> = gitignore.lines().collect();
    assert!(
        ignored.contains(&"/ios") && ignored.contains(&"/android"),
        "{ignored:?}"
    );
    assert!(!ignored.contains(&"ios"), "{ignored:?}");
    assert!(file(&files, "index.ts").contains("registerRootComponent(Root)"));
    let app_config = file(&files, "app.config.js");
    assert!(
        app_config.contains("src/feldspar/native.json"),
        "{app_config}"
    );
    assert!(app_config.contains("package: native.appId"), "{app_config}");
    // The bundle ID is the profile's on a device build, `native.json`'s
    // otherwise, and the profile is read only for a device build.
    assert!(
        app_config.contains("src/feldspar/ios/prebuild.cjs"),
        "{app_config}"
    );
    assert!(app_config.contains("bundleIdentifier,"), "{app_config}");
    assert!(
        app_config.contains("ios.prebuildSigning(__dirname, native)"),
        "{app_config}"
    );
    assert!(
        file(&files, "src/feldspar/ios/prebuild.cjs")
            .contains("process.env.FELDSPAR_IOS_BUILD === \"device\""),
        "the profile is read only for a device build"
    );
    assert!(
        !app_config.contains("NSAllowsArbitraryLoads"),
        "{app_config}"
    );
    assert!(app_config.contains("output: \"single\""), "{app_config}");
    // The checks' regular expressions survive the template they are written
    // from: an unescaped `.` or `d` would refuse every version.
    assert!(
        app_config.contains(r"/^(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/"),
        "{app_config}"
    );
    assert!(
        app_config.contains(r"(\.[a-zA-Z][a-zA-Z0-9_]*)+$/"),
        "{app_config}"
    );
    // Served over HTTPS, so the APK asks for no cleartext permission.
    assert!(!app_config.contains("usesCleartextTraffic"), "{app_config}");
    // Nothing set: the default application id, version and build type, and
    // Expo's own icon.
    let native: Json = serde_json::from_str(&file(&files, "src/feldspar/native.json")).unwrap();
    assert_eq!(
        native,
        json!({ "appId": "com.feldspar.todo", "version": "1.0.0", "icon": null,
                "buildType": "release", "signing": null,
                "ios": { "profileSource": "own", "profile": null, "bundleId": "com.feldspar.todo",
                         "simulatorConfiguration": "release" } })
    );
    assert!(!package.contains("expo-build-properties"), "{package}");

    // A phone has no page to be relative to: its requests go to the app's URL.
    let hooks = file(&files, "src/feldspar/hooks.ts");
    assert!(
        hooks.contains("baseUrl: Platform.OS === \"web\" ? \"\" : APP_URL"),
        "{hooks}"
    );
    assert!(
        file(&files, "src/feldspar/config.ts")
            .contains("export const APP_URL = \"https://todo.example.com\";")
    );

    // The screens are React Native, not DOM: that is what a native target needs.
    for path in paths.iter().filter(|p| p.starts_with("src/screens/")) {
        let screen = file(&files, path);
        assert!(screen.contains("from \"react-native\""), "{path}");
        for dom in ["<div", "<input", "<button", "className"] {
            assert!(!screen.contains(dom), "{path} uses `{dom}`:\n{screen}");
        }
    }

    // The hooks are typed per table; a keyless read-only table gets the read only.
    assert!(hooks.starts_with("// Code generated by Saltcorn. DO NOT EDIT."));
    assert!(hooks.contains("export function useTasks(): Query<TasksRow[]>"));
    assert!(hooks.contains("export function useDeleteTasks(): Mutation<number>"));
    assert!(hooks.contains("export function useReadings()"));
    assert!(!hooks.contains("useDeleteReadings") && !hooks.contains("useCreateReadings"));

    // Each control kind is the React Native component for it.
    let tasks = file(&files, "src/screens/Tasks.tsx");
    assert!(tasks.contains("useCreateTasks") && tasks.contains("useDeleteTasks"));
    assert!(tasks.contains("<Switch value={form.done}"), "{tasks}");
    assert!(tasks.contains("value={form.title}"), "{tasks}");
    assert!(tasks.contains("value={String(form.estimate)}"), "{tasks}");
    // …and a screen with no form imports none of what a form needs.
    let readings = file(&files, "src/screens/Readings.tsx");
    for unused in [
        "useState",
        "TextInput",
        "Switch",
        "Pressable",
        "useCreateReadings",
    ] {
        assert!(!readings.contains(unused), "{unused} in:\n{readings}");
    }

    // The shell gates on the signed-in user.
    let app = file(&files, "src/App.tsx");
    assert!(
        app.contains("<Login />") && app.contains("signOut"),
        "{app}"
    );
    assert!(file(&files, "src/routes.tsx").contains("path: \"/tasks\""));
}

#[tokio::test]
async fn an_app_that_cannot_sign_anyone_in_gets_no_auth_layer() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let files = scaffold("bundled-rn-anon", false, vec![tasks_table()]).await;
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(!paths.contains(&"src/auth.tsx"));
    assert!(!paths.contains(&"src/Login.tsx"));
    assert_imports_resolve(&files);
    for path in ["src/App.tsx", "src/Root.tsx"] {
        let contents = file(&files, path);
        assert!(
            !contents.contains("Login") && !contents.contains("Auth"),
            "{path}:\n{contents}"
        );
    }
}

#[tokio::test]
async fn an_app_with_no_tables_still_gets_a_project_that_imports_cleanly() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let files = scaffold("bundled-rn-empty", true, vec![]).await;
    assert_imports_resolve(&files);
    let home = file(&files, "src/screens/Home.tsx");
    assert!(
        !home.contains("useNavigation"),
        "a home screen with nothing to link to must not import the navigator: {home}"
    );
}

#[tokio::test]
async fn an_app_served_over_http_lets_its_apk_reach_it() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let mut ctx = context(true, vec![tasks_table()]);
    ctx["app"]["url"] = json!("http://todo.192.168.1.50.nip.io:3032");
    let files = frameworks("bundled-rn-cleartext")
        .await
        .framework_files(FRAMEWORK, FilePhase::Scaffold, ctx)
        .await
        .expect("the scaffold runs");
    // A release APK refuses plain HTTP unless it says otherwise, and a
    // development server is plain HTTP.
    let app_config = file(&files, "app.config.js");
    assert!(
        app_config.contains("usesCleartextTraffic: true"),
        "{app_config}"
    );
    assert!(file(&files, "package.json").contains("\"expo-build-properties\":"));
    // iOS's App Transport Security refuses plain HTTP the same way.
    assert!(
        app_config.contains("NSAppTransportSecurity: { NSAllowsArbitraryLoads: true }"),
        "{app_config}"
    );
    assert!(
        file(&files, "src/feldspar/config.ts").contains("http://todo.192.168.1.50.nip.io:3032")
    );
}

#[tokio::test]
async fn the_mobile_url_setting_is_where_the_apk_sends_its_requests() {
    skip_without!(have_npm(), "npm is not on the PATH");
    // An emulator cannot reach the application's own URL, so the admin names one
    // it can: the setting wins over the URL, and a trailing slash is dropped.
    let mut ctx = context(true, vec![tasks_table()]);
    ctx["settings"] = json!({ "mobile_url": " http://todo.10.0.2.2.nip.io:3032/ " });
    let files = frameworks("bundled-rn-mobile-url")
        .await
        .framework_files(FRAMEWORK, FilePhase::Scaffold, ctx)
        .await
        .expect("the scaffold runs");
    assert!(
        file(&files, "src/feldspar/config.ts")
            .contains("export const APP_URL = \"http://todo.10.0.2.2.nip.io:3032\";")
    );
    // That URL is plain HTTP, so the APK may speak it — although the
    // application itself is served over HTTPS.
    assert!(file(&files, "app.config.js").contains("usesCleartextTraffic: true"));
}

#[tokio::test]
async fn the_apk_settings_reach_native_json_on_every_build() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let mut ctx = context(true, vec![tasks_table()]);
    // The icon is picked from the store; the project is `todo` in that store,
    // and Expo resolves the icon against the project.
    ctx["settings"] = json!({
        "app_id": " com.example.todo ",
        "app_version": "2.3.4",
        "app_icon": "todo/assets/icon.png",
        "build_type": "debug"
    });
    let files = frameworks("bundled-rn-native")
        .await
        .framework_files(FRAMEWORK, FilePhase::Runtime, ctx.clone())
        .await
        .expect("the runtime runs");
    let native: Json = serde_json::from_str(&file(&files, "src/feldspar/native.json")).unwrap();
    assert_eq!(
        native,
        json!({ "appId": "com.example.todo", "version": "2.3.4",
                "icon": "assets/icon.png", "buildType": "debug", "signing": null,
                "ios": { "profileSource": "own", "profile": null, "bundleId": "com.example.todo",
                         "simulatorConfiguration": "release" } })
    );
    // An icon outside the project is reached from it.
    ctx["settings"]["app_icon"] = json!("branding/logo.png");
    let files = frameworks("bundled-rn-native-outside")
        .await
        .framework_files(FRAMEWORK, FilePhase::Runtime, ctx.clone())
        .await
        .expect("the runtime runs");
    let native: Json = serde_json::from_str(&file(&files, "src/feldspar/native.json")).unwrap();
    assert_eq!(native["icon"], json!("../branding/logo.png"));

    // A release build signed with the application's own keystore: its path
    // and alias are in `native.json`, its password is not.
    ctx["settings"] = json!({
        "build_type": "release",
        "own_keystore": true,
        "keystore_file": "keys/release.jks",
        "keystore_alias": "upload",
        "keystore_password": "s3cret"
    });
    let files = frameworks("bundled-rn-native-signed")
        .await
        .framework_files(FRAMEWORK, FilePhase::Runtime, ctx.clone())
        .await
        .expect("the runtime runs");
    let native_text = file(&files, "src/feldspar/native.json");
    let native: Json = serde_json::from_str(&native_text).unwrap();
    assert_eq!(
        native["signing"],
        json!({ "keystore": "../keys/release.jks", "alias": "upload" })
    );
    assert!(!native_text.contains("s3cret"), "{native_text}");
    // A debug build is signed with the debug key, whatever the settings say.
    ctx["settings"]["build_type"] = json!("debug");
    let files = frameworks("bundled-rn-native-debug")
        .await
        .framework_files(FRAMEWORK, FilePhase::Runtime, ctx.clone())
        .await
        .expect("the runtime runs");
    let native: Json = serde_json::from_str(&file(&files, "src/feldspar/native.json")).unwrap();
    assert_eq!(native["signing"], Json::Null);

    // iOS: the profile is reached from the project like the icon is. The
    // bundle ID is the shared App ID, with `_` (which iOS refuses) as `-`.
    ctx["settings"] = json!({
        "app_id": "com.example.my_todo",
        "ios_profile": "todo/signing/adhoc.mobileprovision"
    });
    let files = frameworks("bundled-rn-native-ios")
        .await
        .framework_files(FRAMEWORK, FilePhase::Runtime, ctx.clone())
        .await
        .expect("the runtime runs");
    let native: Json = serde_json::from_str(&file(&files, "src/feldspar/native.json")).unwrap();
    assert_eq!(
        native["ios"],
        json!({
            "profileSource": "own",
            "profile": "signing/adhoc.mobileprovision",
            "bundleId": "com.example.my-todo",
            "simulatorConfiguration": "release"
        })
    );
    // And it is the same App ID Android packages the app under.
    assert_eq!(native["appId"], "com.example.my_todo");

    // A generated profile is made at build time: an uploaded one still
    // chosen (but hidden) is not passed on.
    ctx["settings"]["ios_profile_source"] = json!("generate");
    ctx["settings"]["simulator_configuration"] = json!("debug");
    let files = frameworks("bundled-rn-native-ios-generate")
        .await
        .framework_files(FRAMEWORK, FilePhase::Runtime, ctx)
        .await
        .expect("the runtime runs");
    let native: Json = serde_json::from_str(&file(&files, "src/feldspar/native.json")).unwrap();
    assert_eq!(
        native["ios"],
        json!({ "profileSource": "generate", "profile": null, "bundleId": "com.example.my-todo",
                "simulatorConfiguration": "debug" })
    );
}

/// The generated iOS files into `dir`, side by side as in a project, so Node
/// can `require` them; each carries the generated-code header.
fn write_ios_files(files: &[DeclaredFile], dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    for name in [
        "ios/build.cjs",
        "ios/prebuild.cjs",
        "ios/signing.cjs",
        "ios/profile-generator.cjs",
    ] {
        let contents = file(files, &format!("src/feldspar/{name}"));
        assert!(contents.starts_with("// Code generated by Saltcorn. DO NOT EDIT."));
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

/// Signing an iOS build from nothing but a provisioning profile, as Saltcorn 1
/// does: the generated iOS files read the team, bundle ID, export method,
/// expiry and certificates out of the profile **by key**, and refuses one no
/// build could sign with. Run under Node with the profile as `@expo/plist`
/// parses it, so it needs neither a Mac's `security` nor Xcode.
#[tokio::test]
async fn an_ios_build_is_signed_from_what_its_provisioning_profile_says() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let files = frameworks("bundled-rn-ios-profile")
        .await
        .framework_files(
            FRAMEWORK,
            FilePhase::Runtime,
            context(true, vec![tasks_table()]),
        )
        .await
        .expect("the runtime runs");
    let dir = temp_root("bundled-rn-ios-profile-node");
    write_ios_files(&files, &dir);
    let script = r#"
const ios = { ...require("./ios/signing.cjs"), ...require("./ios/prebuild.cjs") };
// An ad hoc profile (it lists devices, and no debugger may attach) whose one
// certificate is the three bytes 00 01 02, as @expo/plist parses it: <data>
// is a Buffer and <date> a Date.
const plist = {
  DeveloperCertificates: [Buffer.from([0, 1, 2])],
  Entitlements: {
    "application-identifier": "ABCDE12345.com.example.todo",
    "get-task-allow": false,
  },
  ExpirationDate: new Date("2030-01-01T00:00:00Z"),
  Name: "Todo Ad Hoc",
  ProvisionedDevices: ["00008101-000A1234"],
  TeamIdentifier: ["ABCDE12345"],
  UUID: "1b2c3d4e-0000-4000-8000-123456789abc",
};
const info = ios.profileInfo(plist);
const refused = (f) => { try { f(); return null; } catch (e) { return e.message; } };
const identity = { sha1: info.certificates[0], name: "Apple Distribution: Test (ABCDE12345)" };
const signing = { info, bundleId: ios.bundleIdFor(info, "com.feldspar.todo"), identity };
// The app target is the one with a bundle ID; the other configuration is left.
const configs = {
  A: { buildSettings: { PRODUCT_BUNDLE_IDENTIFIER: "com.example.todo",
                        '"CODE_SIGN_IDENTITY[sdk=iphoneos*]"': '"iPhone Developer"' } },
  B: { buildSettings: { SDKROOT: "iphoneos" } },
};
ios.applySigning({ pbxXCBuildConfigurationSection: () => configs }, signing);
console.log(JSON.stringify({
  info: { ...info, expires: info.expires.toISOString() },
  bundleId: signing.bundleId,
  exportOptions: ios.exportOptions(signing),
  app: configs.A.buildSettings,
  other: configs.B.buildSettings,
  methods: [
    ios.profileInfo({ ...plist, Entitlements: { ...plist.Entitlements, "get-task-allow": true } }).method,
    ios.profileInfo({ ...plist, ProvisionedDevices: undefined }).method,
    ios.profileInfo({ ...plist, ProvisionsAllDevices: true }).method,
  ],
  wildcard: ios.bundleIdFor({ ...info, appId: "com.feldspar.*" }, "com.feldspar.todo"),
  refusals: {
    expired: refused(() => ios.checkProfile(info, new Date("2031-01-01"))),
    wildcard: refused(() => ios.bundleIdFor({ ...info, appId: "com.other.*" }, "com.feldspar.todo")),
    keychain: refused(() => ios.signingIdentity(info, [{ sha1: "0".repeat(40), name: "Apple Development: X" }])),
    bundle: refused(() => ios.checkConfig(".", { version: "1.0.0" }, "com.my_todo")),
    jpeg: refused(() => ios.checkConfig(".", { version: "1.0.0", icon: "icon.jpg" }, "com.example.todo")),
    adHoc: refused(() => ios.checkAppStore(info)),
    appStore: refused(() => ios.checkAppStore({ ...info, method: "app-store-connect" })),
  },
}));
"#;
    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(script)
        .current_dir(&dir)
        .output()
        .expect("node runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got: Json = serde_json::from_slice(&out.stdout).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        got["info"],
        json!({
            "uuid": "1b2c3d4e-0000-4000-8000-123456789abc",
            "name": "Todo Ad Hoc",
            "team": "ABCDE12345",
            "appId": "com.example.todo",
            "method": "release-testing",
            "expires": "2030-01-01T00:00:00.000Z",
            // SHA-1 of the certificate's DER, as `security find-identity` prints it.
            "certificates": ["0C7A623FD2BBC05B06423BE359E4021D36E721AD"]
        })
    );
    // The profile's own bundle ID wins over the application's.
    assert_eq!(got["bundleId"], "com.example.todo");
    assert_eq!(
        got["methods"],
        json!(["debugging", "app-store-connect", "enterprise"])
    );
    assert_eq!(got["wildcard"], "com.feldspar.todo");

    // Manual signing on the app target only, with the profile and the identity
    // pinned by UUID and SHA-1; the SDK-specific identity Expo writes is gone.
    assert_eq!(
        got["app"],
        json!({
            "PRODUCT_BUNDLE_IDENTIFIER": "com.example.todo",
            "CODE_SIGN_STYLE": "Manual",
            "DEVELOPMENT_TEAM": "ABCDE12345",
            "PROVISIONING_PROFILE_SPECIFIER": "\"1b2c3d4e-0000-4000-8000-123456789abc\"",
            "CODE_SIGN_IDENTITY": "\"0C7A623FD2BBC05B06423BE359E4021D36E721AD\""
        })
    );
    assert_eq!(got["other"], json!({ "SDKROOT": "iphoneos" }));
    let export = got["exportOptions"].as_str().unwrap();
    for line in [
        "<key>method</key><string>release-testing</string>",
        "<key>teamID</key><string>ABCDE12345</string>",
        "<key>signingCertificate</key><string>0C7A623FD2BBC05B06423BE359E4021D36E721AD</string>",
        "<key>com.example.todo</key><string>1b2c3d4e-0000-4000-8000-123456789abc</string>",
        "<key>manageAppVersionAndBuildNumber</key><false/>",
    ] {
        assert!(export.contains(line), "{line}:\n{export}");
    }

    // Each refusal is a sentence naming what to do, before Xcode is started.
    let refusal = |name: &str| got["refusals"][name].as_str().unwrap_or("").to_owned();
    assert!(
        refusal("expired").contains("expired on 2030-01-01"),
        "{}",
        refusal("expired")
    );
    assert!(refusal("wildcard").contains("does not cover the bundle ID com.feldspar.todo"));
    assert!(refusal("keychain").contains("Import the certificate's .p12"));
    assert!(refusal("bundle").contains("com.my_todo"));
    assert!(refusal("jpeg").contains("not a PNG"));
    // Only an App Store profile signs: the app is built for App Store Connect.
    assert!(
        refusal("adHoc").contains("\"Todo Ad Hoc\" is an ad hoc profile"),
        "{}",
        refusal("adHoc")
    );
    assert_eq!(got["refusals"]["appStore"], Json::Null);
}

/// A generated provisioning profile: `ios/build.cjs` signs into the App Store
/// Connect API with the module's key, picks the one distribution certificate
/// both Apple and the keychain have, registers the bundle ID if Apple lacks
/// it, and reuses the profile an earlier build made until it goes stale. Run
/// under Node against a fake API, so it needs neither Apple nor a Mac.
#[tokio::test]
async fn an_ios_build_can_generate_its_app_store_profile() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let files = frameworks("bundled-rn-ios-generate")
        .await
        .framework_files(
            FRAMEWORK,
            FilePhase::Runtime,
            context(true, vec![tasks_table()]),
        )
        .await
        .expect("the runtime runs");
    let dir = temp_root("bundled-rn-ios-generate-node");
    write_ios_files(&files, &dir);
    let script = r#"
const crypto = require("crypto");
const fs = require("fs");
const { IosProfileGenerator } = require("./ios/profile-generator.cjs");
const refused = (f) => { try { f(); return null; } catch (e) { return e.message; } };
const now = new Date("2027-01-01T00:00:00Z");

// The key as the module setting holds it: the .p8's contents, here pasted
// into a one-line field, so its line breaks became spaces.
const { privateKey, publicKey } = crypto.generateKeyPairSync("ec", { namedCurve: "P-256" });
const pem = privateKey.export({ type: "pkcs8", format: "pem" }).replace(/\n/g, " ");
const env = { FELDSPAR_ASC_ISSUER_ID: "issuer-1", FELDSPAR_ASC_KEY_ID: "KEY123", FELDSPAR_ASC_KEY: pem };
// A generator on a fake API and clock, optionally naming a certificate.
const quiet = () => {};
const generator = (api, wanted) =>
  new IosProfileGenerator({ ...env, FELDSPAR_IOS_IDENTITY: wanted || "" }, { api, now, log: quiet });
const decode = (part) => JSON.parse(Buffer.from(part, "base64url").toString());

// Two certificates Apple knows, one of them in the keychain, one expired.
const der = (text) => Buffer.from(text).toString("base64");
const sha1 = (text) => crypto.createHash("sha1").update(Buffer.from(text)).digest("hex").toUpperCase();
const certs = [
  { id: "C1", attributes: { certificateContent: der("one"), expirationDate: "2028-01-01T00:00:00Z" } },
  { id: "C2", attributes: { certificateContent: der("two"), expirationDate: "2028-01-01T00:00:00Z" } },
  { id: "C3", attributes: { certificateContent: der("old"), expirationDate: "2026-01-01T00:00:00Z" } },
];
const keychain = [
  { sha1: sha1("one"), name: "Apple Distribution: One" },
  { sha1: sha1("old"), name: "Apple Distribution: Old" },
];
const both = keychain.concat([{ sha1: sha1("two"), name: "Apple Distribution: Two" }]);
const cert = generator().pickCertificate(certs, keychain);

// A fake API: what it was asked, answered from a little state.
function fakeApi(state) {
  const calls = [];
  const api = async (method, path, body) => {
    calls.push(method + " " + path.split("?")[0]);
    if (method === "GET" && path.startsWith("/v1/bundleIds")) return { data: state.bundles };
    if (method === "POST" && path === "/v1/bundleIds") {
      const made = { id: "B9", attributes: body.data.attributes };
      state.bundles.push(made);
      return { data: made };
    }
    if (method === "GET" && path.startsWith("/v1/profiles")) {
      if (!path.includes("filter[profileType]=IOS_APP_STORE")) throw new Error("not App Store: " + path);
      return { data: state.profiles };
    }
    if (method === "DELETE") return null;
    if (method === "POST" && path === "/v1/profiles") {
      state.posted = body;
      return { data: { attributes: { profileContent: "TkVX" } } };
    }
    throw new Error("unexpected " + method + " " + path);
  };
  return { api, calls };
}
const profile = (id, certId, extra) => ({
  id,
  attributes: { profileState: "ACTIVE", expirationDate: "2027-06-01T00:00:00Z",
                profileContent: "T0xE", ...extra },
  relationships: { bundleId: { data: { id: "B1" } }, certificates: { data: [{ id: certId }] } },
});
const bundle = { id: "B1", attributes: { identifier: "com.example.todo" } };

(async () => {
  const token = generator().token(1800000000);
  const [h, c, sig] = token.split(".");
  const verified = crypto.verify("sha256", Buffer.from(h + "." + c),
    { key: publicKey, dsaEncoding: "ieee-p1363" }, Buffer.from(sig, "base64url"));
  const fresh = fakeApi({ bundles: [], profiles: [] });
  const freshContent = (await generator(fresh.api).profileFor("com.example.todo", cert)).content;
  const state = { bundles: [bundle], profiles: [profile("P1", "C1")] };
  const reuse = fakeApi(state);
  const reused = await generator(reuse.api).profileFor("com.example.todo", cert);
  const staleState = { bundles: [bundle],
    profiles: [profile("P2", "C2"), profile("P3", "C1", { expirationDate: "2027-01-03T00:00:00Z" })] };
  const stale = fakeApi(staleState);
  const remade = (await generator(stale.api).profileFor("com.example.todo", cert)).content;
  // The operation's way: no keychain, so any unexpired certificate Apple
  // knows, and the whole job in one call.
  const whole = fakeApi({ bundles: [bundle], profiles: [profile("P1", "C1")] });
  const certApi = async (method, path, body) =>
    path.startsWith("/v1/certificates") ? { data: certs.slice(0, 1).concat(certs.slice(2)) } : whole.api(method, path, body);
  const operation = await generator(certApi).generate("com.example.todo", null);
  console.log(JSON.stringify({
    claims: decode(c), header: decode(h), verified,
    cert,
    named: generator(null, "Apple Distribution: Two").pickCertificate(certs, both).id,
    bySha1: generator(null, sha1("two").toLowerCase()).pickCertificate(certs, both).id,
    fresh: { content: freshContent, calls: fresh.calls },
    reuse: { content: reused.content, name: reused.name, expires: reused.expires, calls: reuse.calls },
    stale: { content: remade, calls: stale.calls, posted: staleState.posted },
    operation: { content: operation.content, certificate: operation.certificate.id },
    refusals: {
      none: refused(() => generator().pickCertificate(certs, [])),
      many: refused(() => generator().pickCertificate(certs, both)),
      manyWithoutKeychain: refused(() => generator().pickCertificate(certs, null)),
      config: refused(() => new IosProfileGenerator({ FELDSPAR_ASC_KEY_ID: "KEY123" })),
      badKey: refused(() => new IosProfileGenerator({ ...env, FELDSPAR_ASC_KEY: "bm90IGEga2V5" }).token(1)),
    },
  }));
})().catch((e) => { console.error(e.stack); process.exit(1); });
"#;
    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(script)
        .current_dir(&dir)
        .output()
        .expect("node runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The last line: the lines before it are what the build would log.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let got: Json = serde_json::from_str(stdout.lines().last().unwrap_or("")).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    // The bearer token: the key's ID in the header, the issuer and Apple's
    // audience in the claims, valid for 15 minutes, and signed by the key.
    assert_eq!(
        got["header"],
        json!({ "alg": "ES256", "kid": "KEY123", "typ": "JWT" })
    );
    assert_eq!(
        got["claims"],
        json!({ "iss": "issuer-1", "iat": 1800000000u64, "exp": 1800000900u64, "aud": "appstoreconnect-v1" })
    );
    assert_eq!(got["verified"], true);

    // The certificate: the one Apple and the keychain share and that has not
    // expired; with two, the module's setting picks by name or SHA-1.
    assert_eq!(got["cert"]["id"], "C1");
    assert_eq!(got["cert"]["name"], "Apple Distribution: One");
    assert_eq!(got["named"], "C2");
    assert_eq!(got["bySha1"], "C2");

    // A first build registers the bundle ID and makes the profile.
    assert_eq!(got["fresh"]["content"], "TkVX");
    assert_eq!(
        got["fresh"]["calls"],
        json!([
            "GET /v1/bundleIds",
            "POST /v1/bundleIds",
            "GET /v1/profiles",
            "POST /v1/profiles"
        ])
    );
    // A later one reuses it while it is good.
    assert_eq!(got["reuse"]["content"], "T0xE");
    assert_eq!(got["reuse"]["name"], "Feldspar com.example.todo App Store");
    assert_eq!(got["reuse"]["expires"], "2027-06-01T00:00:00Z");
    // Without a keychain, the one unexpired certificate Apple knows.
    assert_eq!(
        got["operation"],
        json!({ "content": "T0xE", "certificate": "C1" })
    );
    assert_eq!(
        got["reuse"]["calls"],
        json!(["GET /v1/bundleIds", "GET /v1/profiles"])
    );
    // One for another certificate, or days from expiring, is replaced.
    assert_eq!(got["stale"]["content"], "TkVX");
    assert_eq!(
        got["stale"]["calls"],
        json!([
            "GET /v1/bundleIds",
            "GET /v1/profiles",
            "DELETE /v1/profiles/P2",
            "DELETE /v1/profiles/P3",
            "POST /v1/profiles"
        ])
    );
    let posted = &got["stale"]["posted"]["data"];
    assert_eq!(
        posted["attributes"],
        json!({ "name": "Feldspar com.example.todo App Store", "profileType": "IOS_APP_STORE" })
    );
    assert_eq!(posted["relationships"]["bundleId"]["data"]["id"], "B1");
    assert_eq!(
        posted["relationships"]["certificates"]["data"][0]["id"],
        "C1"
    );

    // Each refusal says what to do.
    let refusal = |name: &str| got["refusals"][name].as_str().unwrap_or("").to_owned();
    assert!(
        refusal("none").contains("Manage Certificates"),
        "{}",
        refusal("none")
    );
    assert!(
        refusal("many").contains("name one under Settings → Modules"),
        "{}",
        refusal("many")
    );
    assert!(
        refusal("manyWithoutKeychain").contains("name one under Settings → Modules"),
        "{}",
        refusal("manyWithoutKeychain")
    );
    assert!(
        refusal("config").contains("issuer ID, private key"),
        "{}",
        refusal("config")
    );
    assert!(
        refusal("badKey").contains("not a valid .p8 key"),
        "{}",
        refusal("badKey")
    );
}

/// "Generate a provisioning profile" runs in the module's worker: it is
/// declared on the iOS target, says what is missing without the module's App
/// Store Connect settings, and with them signs its token there (Node's `crypto.sign`
/// in the sandbox) before calling Apple. The test grants the worker no network,
/// so that call is refused and nothing reaches Apple; the API itself is
/// covered against a fake by `an_ios_build_can_generate_its_app_store_profile`.
#[tokio::test]
async fn the_profile_operation_runs_in_the_modules_worker() {
    skip_without!(have_npm(), "npm is not on the PATH");
    // The API switched on, but none of its settings filled in.
    let (frameworks, _) =
        frameworks_configured("bundled-rn-profile-op", json!({ "use_asc": true })).await;
    let declared = frameworks.frameworks();
    let ios = declared[0]
        .targets
        .iter()
        .find(|t| t.name == "ios")
        .unwrap();
    assert_eq!(ios.operations.len(), 1);
    let op = &ios.operations[0];
    assert_eq!(op.name, "generate_profile");
    assert_eq!(
        op.show_if,
        [ShowIfCondition::new(
            "ios_profile_source",
            vec![json!("own")]
        )]
    );
    let context = json!({ "app": { "name": "My Todo", "subdomain": "todo" }, "project": "todo",
                          "settings": { "app_id": "com.example.my_todo" } });
    let err = frameworks
        .call_target_operation(FRAMEWORK, "ios", "generate_profile", context.clone())
        .await
        .expect_err("no API key is set")
        .to_string();
    assert!(err.contains("issuer ID, key ID, private key"), "{err}");
    assert!(err.contains("Use the App Store Connect API"), "{err}");

    // Switched off, the button is still there and says where to switch it on.
    let (off, _) =
        frameworks_configured("bundled-rn-profile-op-off", json!({ "use_asc": false })).await;
    let err = off
        .call_target_operation(FRAMEWORK, "ios", "generate_profile", context.clone())
        .await
        .expect_err("the API is switched off")
        .to_string();
    assert!(err.contains("switched off"), "{err}");
    assert!(err.contains("Settings → Modules → React Native"), "{err}");

    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(
            "const { privateKey } = require('crypto').generateKeyPairSync('ec', { namedCurve: 'P-256' });\n\
             process.stdout.write(privateKey.export({ type: 'pkcs8', format: 'pem' }));",
        )
        .output()
        .expect("node runs");
    let pem = String::from_utf8(out.stdout).unwrap();
    assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"), "{pem}");
    let (configured, _) = frameworks_configured(
        "bundled-rn-profile-op-key",
        json!({ "use_asc": true, "asc_issuer_id": "issuer-1", "asc_key_id": "KEY123", "asc_key": pem }),
    )
    .await;
    let err = configured
        .call_target_operation(FRAMEWORK, "ios", "generate_profile", context)
        .await
        .expect_err("the test grants no network")
        .to_string();
    assert!(!err.contains("not a valid .p8 key"), "{err}");
    assert!(err.contains("api.appstoreconnect.apple.com"), "{err}");
}

/// A project whose path has a space, as every local file store on macOS has
/// (`~/Library/Application Support/…`). Two build phases Expo writes break on
/// it: expo-constants' in the Pods project, and the app's own "Bundle React
/// Native code and images". `ios/build.cjs` quotes both after `pod install`, matched
/// exactly, and leaves every other phase alone.
#[tokio::test]
async fn an_ios_build_quotes_the_project_path_in_the_phases_that_split_it() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let files = frameworks("bundled-rn-ios-space")
        .await
        .framework_files(
            FRAMEWORK,
            FilePhase::Runtime,
            context(true, vec![tasks_table()]),
        )
        .await
        .expect("the runtime runs");
    let dir = temp_root("bundled-rn-ios-space-node");
    write_ios_files(&files, &dir);
    // The two phases as Expo writes them, each beside one that is not theirs.
    let ios = dir.join("Application Support/app/ios");
    let pods = ios.join("Pods/Pods.xcodeproj/project.pbxproj");
    let app = ios.join("app.xcodeproj/project.pbxproj");
    std::fs::create_dir_all(pods.parent().unwrap()).unwrap();
    std::fs::create_dir_all(app.parent().unwrap()).unwrap();
    let constants =
        r#"shellScript = "bash -l -c \"$PODS_TARGET_SRCROOT/../scripts/get-app-config-ios.sh\"";"#;
    let bundle = r#"`\"$NODE_BINARY\" --print \"require('path').dirname(require.resolve('react-native/package.json')) + '/scripts/react-native-xcode.sh'\"`"#;
    let other = r#"shellScript = "bash -l -c \"$PODS_TARGET_SRCROOT/other.sh\"";"#;
    std::fs::write(&pods, format!("\t\t\t{constants}\n\t\t\t{other}\n")).unwrap();
    std::fs::write(
        &app,
        format!("\t\t\tshellScript = \"fi\\n\\n{bundle}\\n\\n\";\n\t\t\t{other}\n"),
    )
    .unwrap();
    let script = r#"
const ios = require("./ios/build.cjs");
const dir = require("path").join(process.cwd(), "Application Support/app/ios");
const first = ios.quoteScriptPaths(dir);
const again = ios.quoteScriptPaths(dir);
// A Debug simulator build is plain Debug: its JavaScript comes from Metro.
const sim = [ios.simulatorConfiguration({ ios: { simulatorConfiguration: "debug" } }),
             ios.simulatorConfiguration({})];
console.log(JSON.stringify({ first, again, sim }));
"#;
    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(script)
        .current_dir(&dir)
        .output()
        .expect("node runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got: Json = serde_json::from_slice(&out.stdout).unwrap();
    let pods = std::fs::read_to_string(&pods).unwrap();
    let app = std::fs::read_to_string(&app).unwrap();
    std::fs::remove_dir_all(&dir).ok();

    // Both fixed once; a second build finds nothing left to fix.
    assert_eq!(got["first"], 2);
    assert_eq!(got["again"], 0);
    assert_eq!(
        got["sim"],
        json!([
            { "configuration": "Debug" },
            { "configuration": "Release" }
        ])
    );
    assert!(!pods.contains(constants), "{pods}");
    assert!(
        pods.contains(
            r#"shellScript = "PROJECT_ROOT=\"$PODS_ROOT/../..\" PROJECT_DIR=Pods bash -l \"$PODS_TARGET_SRCROOT/../scripts/get-app-config-ios.sh\"";"#
        ),
        "{pods}"
    );
    assert!(!app.contains(bundle), "{app}");
    assert!(
        app.contains(
            r#"\"$(\"$NODE_BINARY\" --print \"require('path').dirname(require.resolve('react-native/package.json')) + '/scripts/react-native-xcode.sh'\")\""#
        ),
        "{app}"
    );
    // Any other phase is left as it is.
    assert!(pods.contains(other) && app.contains(other), "{pods}\n{app}");
}

#[tokio::test]
async fn a_rebuild_rewrites_the_generated_directory_and_nothing_else() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let files = frameworks("bundled-rn-runtime")
        .await
        .framework_files(
            FRAMEWORK,
            FilePhase::Runtime,
            context(true, vec![tasks_table()]),
        )
        .await
        .expect("the runtime runs");
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "src/feldspar/hooks.ts",
            "src/feldspar/config.ts",
            "src/feldspar/native.json",
            "src/feldspar/ios/build.cjs",
            "src/feldspar/ios/prebuild.cjs",
            "src/feldspar/ios/signing.cjs",
            "src/feldspar/ios/profile-generator.cjs",
            "src/feldspar/README.md"
        ],
        "a build must not rewrite a screen the developer owns"
    );
}

/// Install → scaffold → `npm install` → `tsc` → `expo export`, with a real
/// database, a real file store and a real Node toolchain.
///
/// Opt-in (`SC_TEST_NPM=1`), like the Vue module's: it needs npm and a network.
/// It is the only test that type-checks the generated screens and hooks against
/// the generated client and React Native's own types. The APK target is not
/// built here: that needs an Android SDK and is minutes long.
#[tokio::test]
async fn the_bundled_react_native_module_scaffolds_an_application_that_builds() {
    if std::env::var("SC_TEST_NPM").as_deref() != Ok("1") {
        eprintln!("skipping: set SC_TEST_NPM=1 to run the real npm build");
        return;
    }
    skip_without!(have_npm(), "npm is not on the PATH");

    let frameworks = frameworks("bundled-rn-e2e").await;
    sc_app::install_frameworks(sc_app::FrameworkSet::new(Arc::new(frameworks)))
        .expect("the registry is not poisoned");

    let db = sc_test_harness::TestDb::new()
        .await
        .expect("a test database");
    db.client()
        .await
        .expect("a client")
        .batch_execute(
            "CREATE TABLE tasks (\
               id bigint generated by default as identity primary key, \
               title text not null, \
               estimate integer, \
               done boolean)",
        )
        .await
        .expect("the app's table");
    let driver = Arc::new(sc_db_postgres::PgDriver::from_pool(db.pool().clone()));
    let cat = sc_catalog::Catalog::init(driver as Arc<dyn sc_db::DatabaseDriver>)
        .await
        .expect("a catalog");

    let dir = temp_root("bundled-rn-project");
    std::fs::create_dir_all(&dir).expect("the store's directory");
    cat.connect_file_store(Arc::new(
        sc_files::LocalFileStore::new("apps", &dir).expect("a store"),
    ))
    .expect("the store connects");
    sc_app::bootstrap(&cat)
        .await
        .expect("the applications table");

    let app = sc_app::Application::new(
        "Todo",
        "todo",
        sc_app::FrameworkRef::new(FRAMEWORK)
            .with("store", "apps")
            .with("project", "todo"),
    )
    .with_table(sc_catalog::TableId("tasks".to_owned()))
    .with_file_store(sc_catalog::FileStoreId("apps".to_owned()))
    .with_api(sc_app::ApiConfig::new("rest", "/api"));
    let app = sc_app::save_application(&cat, &app)
        .await
        .expect("a `react-native` application saves like any other");

    let report = sc_app::scaffold_app(&cat, &app, None)
        .await
        .expect("the module's scaffold runs");
    assert!(
        report
            .files
            .iter()
            .any(|f| f == "todo/src/screens/Tasks.tsx")
    );

    let source = sc_app::app_source_from_config(&app.framework).expect("a source");
    let built = sc_app::build_application(&cat, &app, &source, None)
        .await
        .expect("the scaffolded project type-checks and builds");
    assert!(built.installed, "the first build installs dependencies");

    let fw = sc_app::CodeFramework::new(FRAMEWORK, built.bundle);
    let index = fw.serve(&sc_app::AppRequest::get("/"));
    assert_eq!(index.status, 200);
    assert!(String::from_utf8_lossy(&index.body).contains("<div id=\"root\">"));
    assert_eq!(fw.serve(&sc_app::AppRequest::get("/tasks")).status, 200);

    std::fs::remove_dir_all(&dir).ok();
}

/// "Generate a keystore": the module makes a PKCS12 keystore in pure JavaScript
/// and answers it with the settings that name it. Writing and saving are the
/// server's (`sc_app::run_target_operation`); this checks what the module makes.
#[tokio::test]
async fn the_keystore_operation_makes_a_keystore_and_the_settings_naming_it() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let frameworks = frameworks("bundled-rn-keystore").await;
    let declared = frameworks.frameworks();
    let op = &declared[0].targets[0].operations[0];
    assert_eq!(op.name, "generate_keystore");
    assert_eq!(
        op.show_if,
        [
            ShowIfCondition::new("build_type", vec![json!("release")]),
            ShowIfCondition::new("own_keystore", vec![json!(true)])
        ]
    );

    let context = |settings: Json| {
        json!({ "app": { "name": "My Todo", "subdomain": "todo" }, "project": "todo",
                "settings": settings })
    };
    // Nothing filled in: the alias `upload` and a generated password, shown once.
    let answer = frameworks
        .call_target_operation(
            FRAMEWORK,
            "android",
            "generate_keystore",
            context(json!({ "build_type": "release", "own_keystore": true })),
        )
        .await
        .expect("the operation runs");
    assert_eq!(answer.files.len(), 1);
    let (path, bytes) = &answer.files[0];
    assert_eq!(path, "keys/todo-release.p12");
    // A DER SEQUENCE, of a size a 2048-bit key and its certificate make.
    assert_eq!(bytes[0], 0x30);
    assert!(bytes.len() > 2000, "{} bytes", bytes.len());
    assert_eq!(answer.settings["own_keystore"], json!(true));
    assert_eq!(
        answer.settings["keystore_file"],
        json!("keys/todo-release.p12")
    );
    assert_eq!(answer.settings["keystore_alias"], json!("upload"));
    let password = answer.settings["keystore_password"].as_str().unwrap();
    assert_eq!(password.len(), 24);
    assert!(answer.message.contains(password), "{}", answer.message);

    // The alias and password the admin typed are used, and not repeated back.
    let answer = frameworks
        .call_target_operation(
            FRAMEWORK,
            "android",
            "generate_keystore",
            context(json!({ "keystore_alias": "release", "keystore_password": "typed-pass" })),
        )
        .await
        .expect("the operation runs");
    assert_eq!(answer.settings["keystore_alias"], json!("release"));
    assert_eq!(answer.settings["keystore_password"], json!("typed-pass"));
    assert!(!answer.message.contains("typed-pass"), "{}", answer.message);

    // Java refuses a password shorter than six characters, so the module does.
    let err = frameworks
        .call_target_operation(
            FRAMEWORK,
            "android",
            "generate_keystore",
            context(json!({ "keystore_password": "short" })),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("6 characters"), "{err}");
}

/// An app with a synced table keeps it on the device: SQLite on a phone,
/// IndexedDB on the web, chosen by Metro's platform files so `expo-sqlite` is
/// never bundled for the web.
#[tokio::test]
async fn an_app_with_a_synced_table_keeps_it_on_the_device() {
    skip_without!(have_npm(), "npm is not on the PATH");
    let mut synced = tasks_table();
    synced["synced"] = json!(true);
    let mut ctx = context(true, vec![synced]);
    ctx["offline"] = json!(true);
    let files = frameworks("bundled-rn-offline")
        .await
        .framework_files(FRAMEWORK, FilePhase::Scaffold, ctx)
        .await
        .expect("the scaffold runs");
    assert_imports_resolve(&files);

    let hooks = file(&files, "src/feldspar/hooks.ts");
    assert!(
        hooks.contains("import { syncStorage } from \"./storage\";"),
        "{hooks}"
    );
    assert!(
        hooks.contains("offline: { storage: syncStorage() },"),
        "{hooks}"
    );
    assert!(
        hooks.contains("api.sync?.onChange((tables) => tables.forEach(invalidate));"),
        "{hooks}"
    );
    let web = file(&files, "src/feldspar/storage.ts");
    assert!(web.contains("indexedDbStorage(\"feldspar-sync\")"), "{web}");
    assert!(!web.contains("expo-sqlite"), "{web}");
    let native = file(&files, "src/feldspar/storage.native.ts");
    assert!(native.contains("from \"expo-sqlite\""), "{native}");
    assert!(file(&files, "package.json").contains("\"expo-sqlite\": \"~57.0.4\""));

    // A row created offline shows "…" until the server answers its real key.
    let screen = file(&files, "src/screens/Tasks.tsx");
    assert!(
        screen.contains(r#"{api.sync?.isTemporary("tasks", row.id) ? "…" : String(row.id ?? "")}"#),
        "{screen}"
    );
    assert!(screen.contains("import { api, useTasks"), "{screen}");

    // Without a synced table there is no copy, and no storage files.
    let online = scaffold("bundled-rn-online", true, vec![tasks_table()]).await;
    assert!(!online.iter().any(|f| f.path.contains("storage")));
    assert!(!file(&online, "src/feldspar/hooks.ts").contains("offline"));
    assert!(!file(&online, "src/screens/Tasks.tsx").contains("isTemporary"));
}
