//! The **real** bundled catalog: `plugins/`, as this repository has it.
//!
//! `bundled.rs`'s unit tests read manifests written by the test, which proves
//! the parser and nothing about what is shipped. This file reads the directory
//! the release tarball copies, so the things that actually go wrong are caught
//! here: a manifest that will not parse, a `name` that has drifted from the
//! package's own, a language whose package is not the shape that language
//! installs, a permission entry that is not a permission.
//!
//! It needs no npm, no pip and no network — everything asserted is a file in
//! this repository — so it runs in every `cargo test`, which is the point of
//! writing it rather than leaving the catalog to the two `#[ignore]`d install
//! tests that reach a registry.

use std::path::Path;

use sc_module::{BUNDLED_IN_CHECKOUT, BundledModules, ModuleLanguage};

/// The catalog as it ships: read from the checkout, with nothing else on the
/// path that could be standing in for it.
fn catalog() -> BundledModules {
    let catalog = BundledModules::discover(None);
    assert!(
        catalog.root().is_some(),
        "the checkout's bundled catalog was not found at {BUNDLED_IN_CHECKOUT}"
    );
    catalog
}

#[test]
fn every_bundled_manifest_reads() {
    let catalog = catalog();
    assert!(
        catalog.issues().is_empty(),
        "the bundled catalog has unreadable entries: {:?}",
        catalog.issues()
    );
    assert!(
        !catalog.modules().is_empty(),
        "the bundled catalog is empty; plugins/ should hold at least the RSS module"
    );
    // Every entry says what it is for and what it gives, because a card with a
    // heading and nothing under it is a card nobody can decide from.
    for module in catalog.modules() {
        assert!(!module.title.is_empty(), "{}", module.id);
        assert!(!module.description.is_empty(), "{}", module.id);
        assert!(
            !module.supplies.is_empty(),
            "{} declares nothing it supplies",
            module.id
        );
    }
}

/// TODO-imap M4.6: a module that creates tables lists them, and its card's
/// `supplies` names each one — the reader refuses a manifest that does not, so
/// here it is enough that every entry read, and that each listed table is a
/// whole word of some `supplies` line.
#[test]
fn a_manifests_tables_are_listed_in_what_it_supplies() {
    let catalog = catalog();
    assert!(catalog.issues().is_empty(), "{:?}", catalog.issues());
    for module in catalog.modules() {
        for table in &module.tables {
            assert!(
                module.supplies.iter().any(|line| line
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .any(|w| w == table)),
                "{}: the table `{table}` is not named in `supplies`",
                module.id
            );
        }
    }
}

#[test]
fn a_manifests_name_is_the_package_s_own() {
    // The one that silently breaks everything: the row, the loaded set and the
    // "already installed" check all key on the package's own name, so a
    // manifest naming something else produces a module the tab offers to
    // install over and over.
    for module in catalog().modules() {
        let declared = match module.language {
            ModuleLanguage::JavaScript => package_json_name(&module.directory),
            ModuleLanguage::Python => pyproject_name(&module.directory),
        };
        assert_eq!(
            declared, module.name,
            "{}: feldspar-module.json says `{}`, the package says `{declared}`",
            module.id, module.name
        );
    }
}

#[test]
fn a_bundled_module_is_a_package_its_manager_can_install() {
    for module in catalog().modules() {
        match module.language {
            // npm reads `main` (or an `index.js`) — a directory with neither is
            // installed happily and loads as nothing.
            ModuleLanguage::JavaScript => {
                assert!(
                    module.directory.join("package.json").is_file(),
                    "{}: no package.json",
                    module.id
                );
                assert!(
                    module.directory.join("index.js").is_file(),
                    "{}: no index.js",
                    module.id
                );
            }
            ModuleLanguage::Python => {
                assert!(
                    module.directory.join("pyproject.toml").is_file(),
                    "{}: no pyproject.toml",
                    module.id
                );
            }
        }
    }
}

#[test]
fn what_a_bundled_module_installs_is_what_its_package_depends_on() {
    // `installs` is what the tab promises will be downloaded, and the promise is
    // only worth making if it is the truth. Checked against the package's own
    // dependency declaration, so adding a dependency without saying so on the
    // card fails here.
    for module in catalog().modules() {
        let declared = match module.language {
            ModuleLanguage::JavaScript => package_json_dependencies(&module.directory),
            ModuleLanguage::Python => pyproject_dependencies(&module.directory),
        };
        for package in &module.installs {
            assert!(
                declared.iter().any(|d| d == package),
                "{}: the card promises `{package}` is installed, and the package depends on \
                 {declared:?}",
                module.id
            );
        }
        // And the other direction, which is the one that rots: a dependency
        // added later without a line on the card is a download nobody was told
        // about.
        for dependency in &declared {
            assert!(
                module.installs.contains(dependency),
                "{}: the package depends on `{dependency}` and the card does not say it is \
                 downloaded",
                module.id
            );
        }
    }
}

#[test]
fn the_rss_module_is_in_the_catalog_and_asks_for_the_network() {
    let catalog = catalog();
    let rss = catalog.get("rss").expect("the RSS module ships");
    assert_eq!(rss.name, "@feldspar/rss");
    assert_eq!(rss.language, ModuleLanguage::JavaScript);
    assert_eq!(rss.installs, ["rss-parser"]);
    // A feed's host is typed into the *table's* settings, so it cannot be on an
    // allow-list granted when the module is installed. This is the case the
    // wildcard exists for, and the card prints it as "may connect to any host"
    // beside the button that grants it.
    assert!(rss.permissions.any_host(), "{:?}", rss.permissions);
    assert_eq!(rss.permissions.sentences(), ["may connect to any host"]);
}

#[test]
fn the_python_half_of_the_catalog_ships_too() {
    // Both hosts, from one catalog and one Install button (§8): the tab does not
    // know which language it is installing until the manifest says.
    let catalog = catalog();
    let python: Vec<&str> = catalog
        .modules()
        .iter()
        .filter(|m| m.language == ModuleLanguage::Python)
        .map(|m| m.id.as_str())
        .collect();
    assert!(
        python.contains(&"markdown") && python.contains(&"sklearn"),
        "the Python bundled modules are {python:?}"
    );
    let markdown = catalog.get("markdown").expect("the Markdown module ships");
    assert_eq!(markdown.name, "feldspar-markdown");
    // Nothing to grant, and the manifest reader refuses a Python module that
    // asks (there is no sandbox to enforce it).
    assert!(markdown.permissions.is_closed());
}

#[test]
fn the_sklearn_module_ships_and_promises_the_download_it_makes() {
    // The third bundled module, and the one that proves a **model provider** is
    // an extension point: five scikit-learn estimators appear on the model form
    // beside the built-in regressions, and nothing above the seam knows they are
    // Python.
    let catalog = catalog();
    let sklearn = catalog
        .get("sklearn")
        .expect("the scikit-learn module ships");
    assert_eq!(sklearn.name, "feldspar-sklearn");
    assert_eq!(sklearn.language, ModuleLanguage::Python);
    // The whole point of a bundled module: the code is in the tarball and the
    // stack it wraps is not, so a server that never installs it downloads
    // neither scikit-learn nor numpy.
    assert!(sklearn.installs.contains(&"scikit-learn".to_owned()));
    assert!(sklearn.installs.contains(&"numpy".to_owned()));
    assert!(sklearn.permissions.is_closed());
    // Five cards' worth of "what you get", one per provider, so the Install
    // button is pressed by somebody who knows what appears afterwards.
    assert_eq!(sklearn.supplies.len(), 5, "{:?}", sklearn.supplies);
    assert!(
        sklearn
            .supplies
            .iter()
            .all(|line| line.contains("model provider")),
        "{:?}",
        sklearn.supplies
    );
}

#[test]
fn the_mail_module_ships_and_declares_the_tables_its_card_lists() {
    // TODO-imap M7.1. The card promises four tables; the plugin object is what
    // the server creates them from, so it must declare exactly those, and in a
    // shape the table facility reads without an issue.
    let catalog = catalog();
    let imap = catalog.get("imap").expect("the mail module ships");
    assert_eq!(imap.name, "@feldspar/imap");
    assert_eq!(imap.language, ModuleLanguage::JavaScript);
    assert!(imap.permissions.any_host(), "{:?}", imap.permissions);
    for package in ["imapflow", "nodemailer", "@zone-eu/mailsplit"] {
        assert!(imap.installs.iter().any(|p| p == package), "{:?}", imap.installs);
    }
    assert_eq!(
        imap.tables,
        ["mail_accounts", "mail_folders", "mail_messages", "mail_attachments"]
    );

    // `index.js` is JavaScript; node evaluates it (it requires nothing that
    // `npm install` would have to fetch first).
    let script = "console.log(JSON.stringify(require(process.argv[1]).tables))";
    let output = match std::process::Command::new("node")
        .args(["-e", script])
        .arg(imap.directory.join("index.js"))
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("the mail module's tables: skipped, there is no `node` to evaluate index.js");
            return;
        }
        Err(e) => panic!("could not run node: {e}"),
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let declared: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("`tables` is JSON");
    let (decls, issues) = sc_module::tables::parse_declarations(&declared);
    assert!(issues.is_empty(), "{issues:?}");
    let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, imap.tables);
}

/// Every mail action declares what an application sends it and gets back, and
/// each declaration lowers — so `api.runMailSend()` and the rest are typed, and
/// no declaration is dropped as "not a type" when the module loads.
#[test]
fn the_mail_actions_declare_types_that_lower() {
    let imap = catalog()
        .get("imap")
        .expect("the mail module ships")
        .clone();
    let script = "const a = require(process.argv[1]).actions({}); \
                  console.log(JSON.stringify(Object.fromEntries(Object.entries(a) \
                  .map(([n, x]) => [n, { payload: x.payload, returns: x.returns }]))))";
    let output = match std::process::Command::new("node")
        .args(["-e", script])
        .arg(imap.directory.join("index.js"))
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("the mail actions' types: skipped, there is no `node` to evaluate index.js");
            return;
        }
        Err(e) => panic!("could not run node: {e}"),
    };
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actions: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("the actions' types are JSON");
    assert_eq!(actions.len(), 10, "{actions:?}");
    for (name, types) in &actions {
        match sc_api::parse_returns(&types["payload"]) {
            Ok(sc_api::TypeSchema::Struct(_)) => {}
            other => panic!("{name}'s payload: {other:?}"),
        }
        if let Err(e) = sc_api::parse_returns(&types["returns"]) {
            panic!("{name}'s returns: {e}");
        }
    }
}

/// TODO-imap M10.1: the catalog the Modules tab shows is the one
/// `plugins/README.md` documents — each bundled module has its row there, with
/// its directory and its package's name, so a module added to `plugins/` is
/// not one the docs have never heard of.
#[test]
fn every_bundled_module_has_its_row_in_the_plugins_readme() {
    let catalog = catalog();
    let readme = read(
        &catalog
            .root()
            .expect("the catalog has a root")
            .join("README.md"),
    );
    for module in catalog.modules() {
        let row = format!("| `{}/` | `{}` |", module.id, module.name);
        assert!(
            readme.lines().any(|line| line.starts_with(&row)),
            "plugins/README.md has no row starting `{row}`"
        );
    }
    assert!(
        catalog.get("imap").is_some(),
        "the mail module is in the catalog"
    );
}

/// The names in a `package.json`'s `dependencies`.
fn package_json_dependencies(dir: &Path) -> Vec<String> {
    let text = read(&dir.join("package.json"));
    let value: serde_json::Value = serde_json::from_str(&text).expect("package.json is JSON");
    match value.get("dependencies") {
        Some(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// The names in a `pyproject.toml`'s `[project] dependencies`, with the version
/// specifier taken off — `markdown>=3.5` is a dependency on `markdown`.
///
/// A line scan for the same reason [`pyproject_name`] is one: no TOML dependency
/// in this crate, and the file is one we write.
fn pyproject_dependencies(dir: &Path) -> Vec<String> {
    let text = read(&dir.join("pyproject.toml"));
    let Some(start) = text.find("dependencies = [") else {
        return Vec::new();
    };
    let rest = &text[start..];
    let end = rest.find(']').expect("the dependency list is closed");
    rest[..end]
        .split('"')
        .filter(|part| !part.trim().is_empty() && !part.contains('['))
        .map(|spec| {
            let end = spec.find(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.');
            spec[..end.unwrap_or(spec.len())].to_owned()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `"name": "…"` out of a `package.json`, without a JSON dependency in a test.
fn package_json_name(dir: &Path) -> String {
    let text = read(&dir.join("package.json"));
    let value: serde_json::Value = serde_json::from_str(&text).expect("package.json is JSON");
    value["name"]
        .as_str()
        .expect("package.json has a name")
        .to_owned()
}

/// `name = "…"` out of a `pyproject.toml`'s `[project]` table.
///
/// A line scan rather than a TOML parser: this crate has no TOML dependency,
/// the file is one we write, and what is being checked is that two strings in
/// this repository agree.
fn pyproject_name(dir: &Path) -> String {
    let text = read(&dir.join("pyproject.toml"));
    let mut in_project = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_project = line == "[project]";
            continue;
        }
        if in_project && let Some(rest) = line.strip_prefix("name") {
            let value = rest.trim_start_matches([' ', '=']).trim();
            return value.trim_matches('"').to_owned();
        }
    }
    panic!("{}/pyproject.toml has no [project] name", dir.display());
}
