//! The **bundled catalog**: the modules this server ships with, and installs
//! from itself.
//!
//! A module is somebody else's package, fetched from a registry an admin has to
//! know the name of. That is right for the long tail and wrong for the short
//! one: an RSS table, a Markdown renderer, a thing every third application wants
//! and no server should carry unasked. Those live in this repository, in
//! `plugins/`, and travel **inside the release tarball** — so a host with no
//! registry access, or an admin with no package name in mind, still has a
//! catalog with an Install button beside each entry.
//!
//! What is shipped is the module and **not its dependencies**. `plugins/rss` is
//! an `index.js`, a `package.json` naming `rss-parser`, and the manifest below;
//! `rss-parser` is downloaded by npm at the moment somebody installs it, and
//! never on a server that installs nothing. That is the whole trade: the code
//! that makes it *this server's* module travels with the server, and the tree
//! underneath it stays where package managers keep trees.
//!
//! # One click, and what the click is
//!
//! Installing a bundled module is an ordinary install from a **local
//! directory** — `npm install <dir>` or `pip install <dir>`, the same command
//! the Modules tab's "local directory" source runs — with the directory filled
//! in by the server instead of typed by the admin. What is stored on the row is
//! the entry's **id** ([`ModuleSource::Bundled`]), never the path: the path is
//! `<install prefix>/plugins/<id>`, which differs between a developer's
//! checkout, a host running the tarball, and the same host after an upgrade
//! moved the prefix. An id survives all three, so the row stays reinstallable.
//!
//! # The manifest
//!
//! One `feldspar-module.json` per directory, and the directory's name is the id:
//!
//! ```json
//! {
//!   "name": "@feldspar/rss",
//!   "language": "javascript",
//!   "title": "RSS feeds",
//!   "description": "Read an RSS or Atom feed as a table.",
//!   "supplies": ["A table provider, \"RSS feed\": …"],
//!   "installs": ["rss-parser"],
//!   "permissions": { "net": ["*"] }
//! }
//! ```
//!
//! `name` is what the package calls itself once installed, because that is the
//! key `_fd_modules` and the loaded set resolve through — it is how this catalog
//! knows an entry is already installed, and `tests/bundled_catalog.rs` asserts it
//! agrees with the package's own `package.json` or `pyproject.toml`.
//!
//! `permissions` is a **request**, and the distinction is the one
//! [`crate::permissions`] draws: what a package declares is not what it gets.
//! A bundled module's request is granted by the *install*, because the Modules
//! tab prints the set beside the button and the admin who clicked it read the
//! sentence. That is a person granting a permission, which is the rule; it is
//! not a package granting itself one.
//!
//! # Nothing here fails
//!
//! A missing directory is an empty catalog, and a manifest that will not parse
//! is an [`issue`](BundledModules::issues) naming the file — never an error out
//! of a listing, and never a server that will not start. The catalog is a
//! convenience: a server whose copy of it is broken must still install modules
//! from npm, list the ones it has, and say what is wrong.

use std::path::{Path, PathBuf};

use sc_error::{Error, Result};
use serde_json::Value as Json;

use crate::module::ModuleLanguage;
use crate::permissions::ModulePermissions;

/// The file that puts a directory in the catalog.
pub const MANIFEST_FILE: &str = "feldspar-module.json";

/// Where the bundled modules are in the checkout this crate was built in.
///
/// The fallback for every binary that was not told otherwise: `cargo run`, a
/// test, and a `sc-cli` built without a bundle prefix all find `plugins/` beside
/// the source they were compiled from. A packaged binary carries the installed
/// path instead (`crates/sc-cli/build.rs`, `SC_PLUGINS_DIR`).
pub const BUNDLED_IN_CHECKOUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../plugins");

/// One entry in the catalog: a module this server can install from itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledModule {
    /// The directory's name, and what a row's `location` holds — `rss`.
    pub id: String,
    /// The package's own name once installed — `@feldspar/rss`. The key the
    /// module row, the loaded set and this catalog's "already installed?" all
    /// resolve through.
    pub name: String,
    /// Which host loads it, and which package manager installs it.
    pub language: ModuleLanguage,
    /// The catalog card's heading.
    pub title: String,
    /// One sentence about what it is for, in the admin's words.
    pub description: String,
    /// What it supplies, one line each: an action, a function, a provider.
    pub supplies: Vec<String>,
    /// The packages installing it downloads — the dependencies that are
    /// deliberately *not* in the tarball.
    pub installs: Vec<String>,
    /// The tables it declares (TODO-imap M4), which installing creates and
    /// uninstalling keeps — said on the card before the click, because a module
    /// that adds tables to the schema is one an admin should know about first.
    /// Each must also be named in [`supplies`](Self::supplies).
    pub tables: Vec<String>,
    /// The services it runs in the background (TODO-imap M5) — said on the card
    /// before the click, because a module that is always running is one an
    /// admin should know about first.
    pub services: Vec<String>,
    /// What it asks to be allowed to reach. Granted by the install, printed by
    /// the tab beside the button that does it.
    pub permissions: ModulePermissions,
    /// The directory the package is installed from.
    pub directory: PathBuf,
}

/// The catalog: every readable entry under one root, and what was unreadable.
#[derive(Debug, Clone, Default)]
pub struct BundledModules {
    root: Option<PathBuf>,
    modules: Vec<BundledModule>,
    issues: Vec<String>,
}

impl BundledModules {
    /// The catalog under `root`, or under the checkout's `plugins/` when no root
    /// was given.
    ///
    /// Never fails. A root that is not there is an empty catalog with no issue
    /// recorded — a binary built without the bundled modules is a supported
    /// build, not a broken one — and everything that *is* there but wrong is an
    /// issue naming the file.
    pub fn discover(root: Option<PathBuf>) -> BundledModules {
        let root = root.unwrap_or_else(|| PathBuf::from(BUNDLED_IN_CHECKOUT));
        if !root.is_dir() {
            return BundledModules::default();
        }
        let mut catalog = BundledModules {
            root: Some(root.clone()),
            ..BundledModules::default()
        };
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) => {
                catalog
                    .issues
                    .push(format!("{} could not be read: {e}", root.display()));
                return catalog;
            }
        };
        // Sorted by id, so the tab's order is the same on every machine and does
        // not depend on what order a filesystem happens to hand directories
        // back in.
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.join(MANIFEST_FILE).is_file())
            .collect();
        dirs.sort();
        for dir in dirs {
            match read_manifest(&dir) {
                Ok(module) => catalog.modules.push(module),
                Err(e) => catalog.issues.push(sc_error::format_chain(&e)),
            }
        }
        catalog
    }

    /// An empty catalog — a server that ships none, and the starting point for a
    /// test that does not want one.
    pub fn empty() -> BundledModules {
        BundledModules::default()
    }

    /// Where the catalog was read from, when there was anywhere to read.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Every entry, by id.
    pub fn modules(&self) -> &[BundledModule] {
        &self.modules
    }

    /// What could not be read, one sentence each.
    pub fn issues(&self) -> &[String] {
        &self.issues
    }

    /// The entry with this id, if there is one.
    pub fn get(&self, id: &str) -> Option<&BundledModule> {
        self.modules.iter().find(|module| module.id == id)
    }

    /// The entry with this id, or an error naming the ids there are.
    ///
    /// The message lists them because the id comes from a form the *server*
    /// filled in: a request naming one that is not here is a client out of step
    /// with the server it is talking to, and the repair is to reload the tab.
    pub fn require(&self, id: &str) -> Result<&BundledModule> {
        self.get(id).ok_or_else(|| {
            let known: Vec<&str> = self.modules.iter().map(|m| m.id.as_str()).collect();
            Error::not_found(if known.is_empty() {
                format!("`{id}` is not a module that ships with this server, which ships none")
            } else {
                format!(
                    "`{id}` is not a module that ships with this server; it ships {}",
                    known.join(", ")
                )
            })
        })
    }
}

/// Whether `line` names `word` as a whole word — `mail_messages` in "the
/// tables mail_accounts, mail_messages", not in "mail_messages_archive".
fn mentions(line: &str, word: &str) -> bool {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| w == word)
}

/// Read one directory's manifest, or say what is wrong with it.
fn read_manifest(dir: &Path) -> Result<BundledModule> {
    let path = dir.join(MANIFEST_FILE);
    let where_ = || format!("{}", path.display());
    let id = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::invalid(format!("{}: the directory has no usable name", where_())))?
        .to_owned();
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Error::invalid(format!("{}: could not be read ({e})", where_())))?;
    let json: Json = serde_json::from_str(&text)
        .map_err(|e| Error::invalid(format!("{}: is not valid JSON ({e})", where_())))?;
    let Json::Object(object) = &json else {
        return Err(Error::invalid(format!("{}: is not an object", where_())));
    };

    let string = |key: &str| -> Result<String> {
        match object.get(key) {
            Some(Json::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
            Some(_) => Err(Error::invalid(format!(
                "{}: `{key}` should be a non-empty string",
                where_()
            ))),
            None => Err(Error::invalid(format!("{}: has no `{key}`", where_()))),
        }
    };
    let lines = |key: &str| -> Result<Vec<String>> {
        match object.get(key) {
            None | Some(Json::Null) => Ok(Vec::new()),
            Some(Json::Array(list)) => list
                .iter()
                .map(|entry| match entry {
                    Json::String(s) => Ok(s.trim().to_owned()),
                    _ => Err(Error::invalid(format!(
                        "{}: `{key}` should be a list of strings",
                        where_()
                    ))),
                })
                .collect(),
            Some(_) => Err(Error::invalid(format!(
                "{}: `{key}` should be a list of strings",
                where_()
            ))),
        }
    };

    let language = ModuleLanguage::parse(&string("language")?)
        .map_err(|e| Error::invalid(format!("{}: {}", where_(), sc_error::format_chain(&e))))?;
    let permissions = match object.get("permissions") {
        None => ModulePermissions::closed(),
        Some(value) => ModulePermissions::from_json(value)
            .map_err(|e| Error::invalid(format!("{}: {}", where_(), sc_error::format_chain(&e))))?,
    };
    // A Python module has no permission model at all (§10 of the Python API), so
    // a manifest that requests one is a manifest whose author expected an
    // enforcement that is not there. Refused rather than ignored: this is a file
    // in this repository, and the failing test is the right place to learn it.
    if language == ModuleLanguage::Python && !permissions.is_closed() {
        return Err(Error::invalid(format!(
            "{}: a Python module cannot request permissions — a Python plugin runs in the \
             server's own interpreter with the server's own privileges, and there is nothing to \
             grant",
            where_()
        )));
    }

    let supplies = lines("supplies")?;
    let tables = lines("tables")?;
    for table in &tables {
        if table.is_empty()
            || table.starts_with("_fd_")
            || !table
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(Error::invalid(format!(
                "{}: `tables` lists `{table}`, which is not a table name a module can declare",
                where_()
            )));
        }
        // The card's sentence is `supplies`; `tables` is the list the server
        // checks. A table missing from the sentence is one the admin is not
        // told about before the click.
        if !supplies.iter().any(|line| mentions(line, table)) {
            return Err(Error::invalid(format!(
                "{}: `tables` lists `{table}`, but no line of `supplies` names it",
                where_()
            )));
        }
    }

    Ok(BundledModule {
        id,
        name: string("name")?,
        language,
        title: string("title")?,
        description: string("description")?,
        supplies,
        installs: lines("installs")?,
        tables,
        services: lines("services")?,
        permissions,
        directory: dir.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory that goes away with the test that made it.
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A catalog over a directory written for the test.
    fn catalog_of(entries: &[(&str, &str)]) -> (TempDir, BundledModules) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "sc-bundled-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (id, manifest) in entries {
            let sub = root.join(id);
            std::fs::create_dir_all(&sub).expect("the entry's directory");
            std::fs::write(sub.join(MANIFEST_FILE), manifest).expect("the manifest");
        }
        let catalog = BundledModules::discover(Some(root.clone()));
        (TempDir(root), catalog)
    }

    const RSS: &str = r#"{
        "name": "@feldspar/rss",
        "language": "javascript",
        "title": "RSS feeds",
        "description": "A feed as a table.",
        "supplies": ["A table provider"],
        "installs": ["rss-parser"],
        "permissions": { "net": ["*"] }
    }"#;

    #[test]
    fn an_entry_is_its_directory_name_and_its_manifest() {
        let (_dir, catalog) = catalog_of(&[("rss", RSS)]);
        assert!(catalog.issues().is_empty(), "{:?}", catalog.issues());
        let module = catalog.get("rss").expect("the entry");
        assert_eq!(module.id, "rss");
        assert_eq!(module.name, "@feldspar/rss");
        assert_eq!(module.language, ModuleLanguage::JavaScript);
        assert_eq!(module.installs, ["rss-parser"]);
        assert!(module.permissions.any_host());
        assert!(module.directory.ends_with("rss"));
    }

    #[test]
    fn a_root_that_is_not_there_is_an_empty_catalog_and_not_an_error() {
        let catalog = BundledModules::discover(Some(PathBuf::from("/nonexistent/plugins")));
        assert!(catalog.modules().is_empty());
        assert!(catalog.issues().is_empty());
        assert!(catalog.root().is_none());
    }

    #[test]
    fn a_broken_manifest_is_an_issue_and_the_rest_of_the_catalog_still_loads() {
        let (_dir, catalog) = catalog_of(&[("rss", RSS), ("broken", "{ not json")]);
        assert_eq!(catalog.modules().len(), 1);
        assert_eq!(catalog.modules()[0].id, "rss");
        assert_eq!(catalog.issues().len(), 1);
        assert!(
            catalog.issues()[0].contains("broken"),
            "{:?}",
            catalog.issues()
        );
    }

    #[test]
    fn a_python_entry_may_not_request_permissions_there_is_nothing_to_enforce_them() {
        let manifest = r#"{
            "name": "feldspar-markdown",
            "language": "python",
            "title": "Markdown",
            "description": "Markdown.",
            "permissions": { "net": ["pypi.org"] }
        }"#;
        let (_dir, catalog) = catalog_of(&[("markdown", manifest)]);
        assert!(catalog.modules().is_empty());
        assert_eq!(catalog.issues().len(), 1);
        assert!(
            catalog.issues()[0].contains("cannot request permissions"),
            "{:?}",
            catalog.issues()
        );
    }

    /// TODO-imap M4.6: the tables a module creates are listed, and the card's
    /// sentence names every one of them.
    #[test]
    fn a_manifests_tables_must_each_be_named_in_what_it_supplies() {
        let mail = |tables: &str| {
            format!(
                r#"{{
                "name": "@feldspar/imap", "language": "javascript", "title": "Mail",
                "description": "Mail.",
                "supplies": ["Tables: mail_accounts, mail_messages", "A service"],
                "tables": {tables}
            }}"#
            )
        };
        let (_dir, catalog) = catalog_of(&[
            ("imap", &mail(r#"["mail_accounts", "mail_messages"]"#)),
            ("imap2", &mail(r#"["mail_accounts", "mail_folders"]"#)),
            ("imap3", &mail(r#"["_fd_tables"]"#)),
            ("imap4", &mail(r#"["mail_account"]"#)),
        ]);
        assert_eq!(catalog.modules().len(), 1, "{:?}", catalog.issues());
        assert_eq!(
            catalog.get("imap").unwrap().tables,
            ["mail_accounts", "mail_messages"]
        );
        let issues = catalog.issues().join("\n");
        assert!(
            issues.contains("lists `mail_folders`, but no line of `supplies` names it"),
            "{issues}"
        );
        assert!(issues.contains("`_fd_tables`, which is not a table name"), "{issues}");
        // A whole word: `mail_account` is not named by `mail_accounts`.
        assert!(issues.contains("`mail_account`, but no line"), "{issues}");
    }

    /// TODO-imap M5: the services a module runs are listed for the card, and a
    /// manifest without them has none.
    #[test]
    fn a_manifest_lists_the_services_it_runs() {
        let mail = r#"{
            "name": "@feldspar/imap", "language": "javascript", "title": "Mail",
            "description": "Mail.", "supplies": ["A service"], "services": ["sync"]
        }"#;
        let (_dir, catalog) = catalog_of(&[("imap", mail), ("rss", RSS)]);
        assert!(catalog.issues().is_empty(), "{:?}", catalog.issues());
        assert_eq!(catalog.get("imap").unwrap().services, ["sync"]);
        assert!(catalog.get("rss").unwrap().services.is_empty());
    }

    #[test]
    fn an_unknown_id_names_what_there_is() {
        let (_dir, catalog) = catalog_of(&[("rss", RSS)]);
        let err = catalog.require("mqtt").unwrap_err().to_string();
        assert!(err.contains("mqtt"), "{err}");
        assert!(err.contains("rss"), "{err}");
        // And the empty catalog says so rather than listing nothing.
        let err = BundledModules::empty()
            .require("rss")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ships none"), "{err}");
    }

    #[test]
    fn the_entries_are_in_a_stable_order_whatever_the_filesystem_says() {
        let (_dir, catalog) = catalog_of(&[
            ("zulu", &RSS.replace("@feldspar/rss", "@feldspar/zulu")),
            ("rss", RSS),
        ]);
        let ids: Vec<&str> = catalog.modules().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["rss", "zulu"]);
    }
}
