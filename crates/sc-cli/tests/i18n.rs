//! `feldspar i18n` over this repository (tasks 2.3 and 2.4).
//!
//! The interesting subject here is **this checkout**: the commands read our own
//! sources and our own catalogues, so the test that matters is the one that
//! runs them against the real tree and asserts what CI will assert. Nothing
//! here spends a token — the `translate` path is driven by a scripted
//! [`Translator`], which is also how the two failure modes D9 names are made to
//! happen on purpose.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use sc_cli::i18n::{
    Coverage, DOMAINS, Domain, I18nArgs, Kind, catalogue_locales, coverage, domain, find_root,
    load_catalogue, parse_answer, save_catalogue, scan_domain,
};
use sc_i18n::{Catalog, Locale, Message, Translator, translate_missing};
use serde_json::{Value as Json, json};

fn root() -> PathBuf {
    find_root(Path::new(env!("CARGO_MANIFEST_DIR"))).expect("this test runs inside the checkout")
}

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-cli-i18n-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("mkdir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Task 2.3's test: every key the Rust scanner finds in `crates/**/*.rs` is one
/// the shipped `core` catalogues can be checked against.
///
/// Two halves, and both matter. Every call site has to be *readable* — a
/// `t!(loc, MESSAGE)` anywhere in the workspace is a message that will never
/// reach a catalogue, which is the failure task 2.1 exists to prevent. And
/// every entry a shipped catalogue holds has to pass the placeholder and plural
/// check, because that is the one thing `feldspar i18n check` fails CI on.
#[test]
fn every_core_call_site_is_readable_and_every_shipped_catalogue_checks() {
    let root = root();
    let core = domain("core").expect("the core domain");
    let (found, _) = scan_domain(&root, core).expect("scanning crates/");
    assert!(
        found.problems.is_empty(),
        "unreadable t!/tc! call sites:\n{}",
        found
            .problems
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let keys = found.keys();
    for tag in catalogue_locales(&root, core).expect("listing crates/sc-i18n/locales") {
        let locale = Locale::parse(&tag).expect("a shipped catalogue is named by its tag");
        let catalog = load_catalogue(&root, core, &locale).expect("a shipped catalogue parses");
        let coverage = coverage(&catalog, &keys);
        assert!(
            coverage.mismatches.is_empty(),
            "{tag}.json: {:?}",
            coverage.mismatches
        );
    }
}

/// The admin SPA is swept in task 3.4, so its lint is expected to be noisy
/// today. What must hold *now* is that its call sites are readable: an
/// unreadable one in `ui/admin/src` would fail CI the moment 2.5 lands.
#[test]
fn every_declared_domain_scans_without_an_unreadable_call_site() {
    let root = root();
    for domain in DOMAINS {
        let (found, _) = scan_domain(&root, domain).expect("scanning a domain");
        assert!(
            found.problems.is_empty(),
            "{}: {:?}",
            domain.name,
            found.problems
        );
    }
}

/// Every domain's source trees are in the tree, so a renamed directory is a
/// domain that silently extracts nothing.
#[test]
fn every_domain_points_at_a_directory_that_exists() {
    let root = root();
    for domain in DOMAINS {
        for source in domain.sources {
            assert!(
                root.join(source).is_dir(),
                "{}: {source} is not a directory",
                domain.name
            );
        }
        assert!(
            root.join(domain.locales)
                .parent()
                .is_some_and(|p| p.is_dir()),
            "{}: {} has nowhere to live",
            domain.name,
            domain.locales
        );
    }
}

/// Coverage is a number and a mismatch is a failure — the two halves of what
/// `check` prints and what it exits on.
#[test]
fn coverage_counts_and_the_check_catches_a_renamed_placeholder() {
    let fr = Locale::parse("fr").expect("fr");
    let mut catalog = Catalog::new(fr.clone());
    catalog.insert("Save", Message::Simple("Enregistrer".to_owned()));
    catalog.insert(
        "Delete {name}?",
        Message::Simple("Supprimer {nom} ?".to_owned()),
    );
    catalog.insert("Gone", Message::Simple("Parti".to_owned()));
    let keys = vec![
        "Save".to_owned(),
        "Delete {name}?".to_owned(),
        "Cancel".to_owned(),
    ];

    let measured: Coverage = coverage(&catalog, &keys);
    assert_eq!(measured.translated, 2);
    assert_eq!(measured.total, 3);
    assert_eq!(measured.percent(), 66);
    // A key the source no longer uses is shown and never deleted.
    assert_eq!(measured.orphans, vec!["Gone".to_owned()]);
    assert_eq!(measured.mismatches.len(), 1, "{:?}", measured.mismatches);
    assert_eq!(measured.mismatches[0].0, "Delete {name}?");
    assert!(
        measured.mismatches[0].1.contains("{nom}"),
        "{}",
        measured.mismatches[0].1
    );
    assert!(measured.line().contains("66%"), "{}", measured.line());
}

/// A translator that answers from a table, and gets one of them wrong on
/// purpose (D9): the placeholder is renamed, so the machine must refuse it and
/// leave correct English behind.
struct Scripted(BTreeMap<String, Json>);

#[async_trait]
impl Translator for Scripted {
    async fn translate_batch(
        &self,
        _source: &Locale,
        _target: &Locale,
        keys: &[String],
    ) -> sc_error::Result<BTreeMap<String, Json>> {
        Ok(keys
            .iter()
            .filter_map(|key| self.0.get(key).map(|v| (key.clone(), v.clone())))
            .collect())
    }
}

/// The whole `translate` path bar the vendor: extract-shaped keys in, a
/// catalogue file on disk out, read back by the same loader `check` uses.
#[tokio::test]
async fn translate_fills_a_catalogue_file_and_refuses_a_mangled_message() {
    let temp = TempDir::new("translate");
    // A throwaway repository root: `save_catalogue` writes under the domain's
    // own relative path, so this is the whole of the fixture.
    let core: &Domain = domain("core").expect("the core domain");
    std::fs::create_dir_all(temp.0.join(core.locales)).expect("mkdir");

    let fr = Locale::parse("fr").expect("fr");
    let mut catalog = load_catalogue(&temp.0, core, &fr).expect("an absent catalogue is empty");
    assert!(catalog.is_empty());

    let keys = vec![
        "Incorrect password".to_owned(),
        "Delete {name}?".to_owned(),
        "{count} rows".to_owned(),
        "Never answered".to_owned(),
    ];
    let translator = Scripted(BTreeMap::from([
        (
            "Incorrect password".to_owned(),
            json!("Mot de passe incorrect"),
        ),
        // The placeholder is renamed: refused, and left in English.
        ("Delete {name}?".to_owned(), json!("Supprimer {nom} ?")),
        (
            "{count} rows".to_owned(),
            json!({"one": "{count} ligne", "many": "{count} de lignes", "other": "{count} lignes"}),
        ),
    ]));

    let report = translate_missing(&mut catalog, &Locale::source(), &translator, &keys)
        .await
        .expect("the scripted translator answers");
    assert_eq!(
        report.filled,
        vec!["Incorrect password".to_owned(), "{count} rows".to_owned()]
    );
    assert_eq!(report.rejected.len(), 1, "{:?}", report.rejected);
    assert_eq!(report.rejected[0].key, "Delete {name}?");
    assert_eq!(report.unanswered, vec!["Never answered".to_owned()]);

    let path = save_catalogue(&temp.0, core, &catalog).expect("writing fr.json");
    assert!(path.ends_with("crates/sc-i18n/locales/fr.json"), "{path:?}");
    let written = std::fs::read_to_string(&path).expect("reading it back");
    assert!(written.ends_with('\n'), "a file ends with a newline");
    assert!(!written.contains("{nom}"), "{written}");

    // The same loader `check` uses, so the round trip is the real one.
    let reread = load_catalogue(&temp.0, core, &fr).expect("re-reading fr.json");
    assert_eq!(reread.len(), 2);
    let measured = coverage(&reread, &keys);
    assert_eq!(measured.translated, 2);
    assert_eq!(measured.total, 4);
    assert!(measured.mismatches.is_empty(), "{:?}", measured.mismatches);
    assert_eq!(
        catalogue_locales(&temp.0, core).expect("listing"),
        vec!["fr".to_owned()]
    );
}

/// Providers wrap a JSON answer in a fenced block often enough that failing a
/// batch of fifty over a pair of backticks would be a bug of ours.
#[test]
fn a_fenced_answer_is_still_an_answer() {
    let parsed = parse_answer("Here you go:\n```json\n{\"Save\": \"Enregistrer\"}\n```\n")
        .expect("the object inside the fence");
    assert_eq!(parsed["Save"], json!("Enregistrer"));
    assert!(parse_answer("I cannot do that.").is_err());
    assert!(parse_answer("[\"Enregistrer\"]").is_err());
}

#[test]
fn the_arguments_parse_and_a_typo_is_refused() {
    let args = |words: &[&str]| -> Vec<String> { words.iter().map(|w| (*w).to_owned()).collect() };

    let parsed = I18nArgs::parse(&args(&["check"])).expect("check");
    // No `--domain` means every domain, which is what CI runs.
    assert_eq!(parsed.domains.len(), DOMAINS.len());

    let parsed =
        I18nArgs::parse(&args(&["translate", "--domain", "core", "--locale", "fr"])).expect("ok");
    assert_eq!(parsed.domains.len(), 1);
    assert_eq!(parsed.domains[0].kind, Kind::Rust);
    assert_eq!(parsed.locales, vec!["fr".to_owned()]);

    let parsed = I18nArgs::parse(&args(&["lint", "ui/admin/src"])).expect("a bare path");
    assert_eq!(parsed.paths, vec![PathBuf::from("ui/admin/src")]);

    let refused = I18nArgs::parse(&args(&["check", "--domain", "nope"])).expect_err("unknown");
    assert!(refused.to_string().contains("builder"), "{refused}");
    assert!(I18nArgs::parse(&args(&["extractt"])).is_err());
    assert!(I18nArgs::parse(&args(&["check", "--locale"])).is_err());

    // `--instance` (TODO F6.4): the shared catalogue, no domains.
    let parsed =
        I18nArgs::parse(&args(&["translate", "--instance", "--locale", "de"])).expect("ok");
    assert!(parsed.instance);
    assert!(parsed.domains.is_empty());
    let refused = I18nArgs::parse(&args(&["check", "--instance"])).expect_err("not translate");
    assert!(refused.to_string().contains("translate"), "{refused}");
    let refused = I18nArgs::parse(&args(&["translate", "--instance", "--domain", "core"]))
        .expect_err("a domain too");
    assert!(refused.to_string().contains("--domain"), "{refused}");
}

/// `feldspar i18n translate --instance` reads the database, not a checkout: run
/// outside one, it gets as far as asking for an LLM provider (TODO F6.4).
#[test]
fn translating_the_shared_catalogue_needs_no_checkout() {
    let temp = TempDir::new("instance");
    let db = temp.0.join("f6.db");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .args([
            "i18n",
            "translate",
            "--instance",
            "--locale",
            "de",
            "--sqlite",
        ])
        .arg(&db)
        .current_dir(&temp.0)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the feldspar binary");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(!stderr.contains("no Saltcorn checkout"), "{stderr}");
    assert!(stderr.contains("LLM provider"), "{stderr}");

    // And without a locale it says which flag is missing.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_feldspar"))
        .args(["i18n", "translate", "--instance", "--sqlite"])
        .arg(&db)
        .current_dir(&temp.0)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the feldspar binary");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--locale"), "{stderr}");
}

/// Task 2.5: the pipeline runs this command. A guard rather than a nicety —
/// the check is only worth having if it is run, and a step quietly dropped
/// from the YAML looks exactly like a repository with nothing to check.
#[test]
fn the_pipeline_runs_the_check() {
    let yaml = std::fs::read_to_string(root().join("whale-ci.yml")).expect("whale-ci.yml");
    assert!(
        yaml.contains("i18n check"),
        "whale-ci.yml no longer runs `feldspar i18n check`"
    );
}
