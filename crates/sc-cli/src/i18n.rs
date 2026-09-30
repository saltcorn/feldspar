//! `feldspar i18n extract | lint | check | translate` (task 2.4).
//!
//! The four things a person does to a catalogue from a terminal, over the one
//! extraction pass in `sc_i18n::extract` (decision D6):
//!
//! | Command | What it answers | Exit |
//! |---|---|---|
//! | `extract` | what does this domain say, and where | non-zero on an unreadable call site |
//! | `lint` | what does it say that nobody wrapped | non-zero on a finding |
//! | `check` | where does each locale stand | non-zero on a **placeholder or plural mismatch** |
//! | `translate` | fill one locale through the configured LLM | non-zero on a failed call |
//!
//! **Coverage is a number, not a gate.** `check` prints it and does not fail on
//! it: a new English string is a normal thing to add and must not break the
//! build of everyone who adds one. What it *does* fail on is a translation that
//! renames a placeholder or is missing a plural form, because that is a string
//! that renders wrongly in front of a person, and an unreadable call site,
//! because that is a message that will never reach a catalogue at all (task
//! 2.1's rule).
//!
//! # The four domains
//!
//! [`DOMAINS`] is the whole of the mapping from a name to a source tree and a
//! catalogue directory. There is no configuration file behind it, on purpose:
//! these are *our* four domains, they are in this repository, and an
//! application's catalogue is not reached from here at all — it lives wherever
//! that application's definition lives, and the Translations screen is its
//! surface (task 4.4).

use std::path::{Path, PathBuf};

use sc_error::{Context, Error, Result};
use sc_i18n::extract::js::Language;
use sc_i18n::extract::{Extraction, Finding};
use sc_i18n::{Catalog, Locale, Message, source_text};

/// How a domain's sources are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `t!(` / `tc!(` in `.rs`.
    Rust,
    /// `t(` / `tc(` / `<T text="…">` in `.ts`, `.tsx`, `.js`, `.jsx`.
    Js,
}

/// One domain: a name, the trees its messages are written in, and where its
/// catalogues live.
#[derive(Debug, Clone, Copy)]
pub struct Domain {
    pub name: &'static str,
    /// Relative to the repository root.
    pub sources: &'static [&'static str],
    /// Trees whose messages count but whose call sites are **not ours**.
    ///
    /// The `builder` domain's phrases are `t("Delete")` call sites inside
    /// `ui/builder/vendor/saltcorn-builder` — Saltcorn 1's builder, vendored
    /// whole and never edited here. Those keys have to reach the catalogue,
    /// because filling v1's `translations` map is the entire job (task 3.5),
    /// but the *lint* must not report a vendored file: there is nothing to do
    /// about what it says, and a lint whose findings cannot be acted on is a
    /// lint that gets turned off. A call site whose message is not a literal is
    /// skipped here for the same reason — it is a v1 decision, made in 2021, in
    /// a package this repository copies rather than writes.
    pub vendored: &'static [&'static str],
    /// Relative to the repository root.
    pub locales: &'static str,
    pub kind: Kind,
    /// One line, for the usage text.
    pub about: &'static str,
}

/// The four domains of population **A** — the product's own strings.
pub const DOMAINS: &[Domain] = &[
    Domain {
        name: "core",
        sources: &["crates"],
        vendored: &[],
        locales: "crates/sc-i18n/locales",
        kind: Kind::Rust,
        about: "everything the server says: authentication, validation, the admin API's refusals",
    },
    Domain {
        name: "admin",
        sources: &["ui/admin/src"],
        vendored: &[],
        locales: "ui/admin/src/locales",
        kind: Kind::Js,
        about: "the admin SPA",
    },
    Domain {
        name: "builder",
        sources: &["ui/builder/src"],
        // Where the phrases actually are: v1's builder, vendored (see
        // `vendored` above).
        vendored: &["ui/builder/vendor/saltcorn-builder"],
        locales: "ui/builder/src/locales",
        kind: Kind::Js,
        about: "the drag-and-drop layout builder",
    },
    Domain {
        name: "analytics",
        sources: &["ui/analytics/src"],
        vendored: &[],
        locales: "ui/analytics/src/locales",
        kind: Kind::Js,
        about: "the Analytics UI",
    },
];

/// The domain called `name`, or an error listing them.
pub fn domain(name: &str) -> Result<&'static Domain> {
    DOMAINS.iter().find(|d| d.name == name).ok_or_else(|| {
        Error::config(format!(
            "unknown domain `{name}`; the domains are {}",
            DOMAINS
                .iter()
                .map(|d| d.name)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

/// Which of the four commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Extract,
    Lint,
    Check,
    Translate,
}

/// The parsed command line, after the database flags have been taken out.
#[derive(Debug, Clone)]
pub struct I18nArgs {
    pub command: Command,
    /// The domains named with `--domain`; every domain when none was.
    pub domains: Vec<&'static Domain>,
    /// The locales named with `--locale`; for `check`, every catalogue on disk
    /// when none was.
    pub locales: Vec<String>,
    /// Bare paths — `feldspar i18n lint ui/admin/src`. When there are any, they
    /// replace the domains' source trees.
    pub paths: Vec<PathBuf>,
    /// The repository root. Found by searching upwards when not given.
    pub root: Option<PathBuf>,
    /// `translate`: which configured LLM provider, and which of its models.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// `lint --json`: one JSON object per finding, with its byte span.
    ///
    /// For a tool rather than for a person. The sweep of task 3.4 was done with
    /// it, and the Translations screen (4.4) shows what it reports; the plain
    /// output stays what somebody reading a terminal wants, which is a
    /// `file:line:` they can click.
    pub json: bool,
}

impl I18nArgs {
    /// Parse `extract|lint|check|translate` and its flags.
    pub fn parse(args: &[String]) -> Result<I18nArgs> {
        let command = match args.first().map(String::as_str) {
            Some("extract") => Command::Extract,
            Some("lint") => Command::Lint,
            Some("check") => Command::Check,
            Some("translate") => Command::Translate,
            Some(other) => {
                return Err(Error::config(format!(
                    "unknown i18n subcommand `{other}`; they are extract, lint, check and translate"
                )));
            }
            None => {
                return Err(Error::config(
                    "usage: feldspar i18n extract|lint|check|translate [--domain NAME] \
                     [--locale TAG]",
                ));
            }
        };
        let mut parsed = I18nArgs {
            command,
            domains: Vec::new(),
            locales: Vec::new(),
            paths: Vec::new(),
            root: None,
            provider: None,
            model: None,
            json: false,
        };
        let mut rest = args[1..].iter();
        while let Some(arg) = rest.next() {
            let mut value = |flag: &str| -> Result<String> {
                rest.next()
                    .cloned()
                    .ok_or_else(|| Error::config(format!("{flag} needs a value")))
            };
            match arg.as_str() {
                "--domain" => parsed.domains.push(domain(&value("--domain")?)?),
                "--locale" => parsed.locales.push(value("--locale")?),
                "--root" => parsed.root = Some(PathBuf::from(value("--root")?)),
                "--provider" => parsed.provider = Some(value("--provider")?),
                "--model" => parsed.model = Some(value("--model")?),
                "--json" => parsed.json = true,
                other if other.starts_with('-') => {
                    return Err(Error::config(format!("unknown i18n argument `{other}`")));
                }
                other => parsed.paths.push(PathBuf::from(other)),
            }
        }
        if parsed.domains.is_empty() {
            parsed.domains = DOMAINS.iter().collect();
        }
        Ok(parsed)
    }
}

/// The repository root: the nearest ancestor of `start` holding
/// `crates/sc-i18n`.
///
/// Searched for rather than baked in with `CARGO_MANIFEST_DIR`, because a
/// released binary is not run from its own checkout and a test is not run from
/// the repository root.
pub fn find_root(start: &Path) -> Result<PathBuf> {
    let mut here =
        std::fs::canonicalize(start).with_context(|| format!("reading {}", start.display()))?;
    loop {
        if here.join("crates/sc-i18n/Cargo.toml").is_file() {
            return Ok(here);
        }
        if !here.pop() {
            return Err(Error::config(format!(
                "no Saltcorn checkout at or above {} — `feldspar i18n` reads this \
                 repository's own sources, so run it inside one or name it with --root",
                start.display()
            )));
        }
    }
}

/// Directory names never walked.
///
/// `locales` is on the list because a catalogue is JSON and nothing here parses
/// one as source; the rest are output, dependencies and history.
const SKIPPED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".git",
    "locales",
    "vendor",
    "coverage",
];

/// Whether a file's messages belong in a shipped catalogue.
///
/// A test's strings are not product strings: `t("Add a task")` in
/// `appNav.test.ts` is a fixture, and translating it would put a sentence
/// nobody reads in front of a translator who has to decide what it means.
/// Likewise a `crates/*/tests` tree, and the `fixtures` the extractor's own
/// tests read.
/// The files that **define** the runtime rather than call it.
///
/// `i18n.tsx` is where `t`, `tc` and `<T>` are written, so it holds the one
/// `t(text, args)` in the tree whose message is a variable — and it has to.
/// Reported, it would be a permanent error in every run of `check`, which is a
/// rule that teaches people to ignore the command. `messages.ts` is the same
/// file in an application's generated runtime (task 4.3).
const RUNTIME_FILES: &[&str] = &["i18n.ts", "i18n.tsx", "messages.ts"];

fn scannable(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    !name.contains(".test.") && !name.contains(".spec.") && !RUNTIME_FILES.contains(&name)
}

fn scannable_directory(name: &str) -> bool {
    !SKIPPED_DIRECTORIES.contains(&name) && name != "tests" && name != "fixtures"
}

/// Every source file under `dir`, sorted, so two runs print the same lines in
/// the same order.
fn walk(dir: &Path, kind: Kind, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        if path.is_dir() {
            if scannable_directory(&name) {
                walk(&path, kind, out)?;
            }
            continue;
        }
        if !scannable(&path) {
            continue;
        }
        let wanted = match kind {
            Kind::Rust => name.ends_with(".rs"),
            Kind::Js => Language::for_path(&name).is_some(),
        };
        if wanted {
            out.push(path);
        }
    }
    Ok(())
}

/// Extract and lint one tree in one pass.
///
/// Paths in the result are relative to `root`, so the output of a run in a
/// container and a run on a laptop are the same text.
pub fn scan_tree(root: &Path, dir: &Path, kind: Kind) -> Result<(Extraction, Vec<Finding>)> {
    let mut files = Vec::new();
    walk(dir, kind, &mut files)?;
    let mut extraction = Extraction::default();
    let mut findings = Vec::new();
    for path in files {
        let name = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        match kind {
            Kind::Rust => {
                let source = String::from_utf8_lossy(&bytes);
                extraction.merge(sc_i18n::extract_rust(&name, &source));
            }
            Kind::Js => {
                let Some(language) = Language::for_path(&name) else {
                    continue;
                };
                let (found, lint) = sc_i18n::extract::js::scan(language, &name, &bytes);
                extraction.merge(found);
                findings.extend(lint);
            }
        }
    }
    Ok((extraction, findings))
}

/// Everything one domain says, over all of its source trees.
pub fn scan_domain(root: &Path, domain: &Domain) -> Result<(Extraction, Vec<Finding>)> {
    let mut extraction = Extraction::default();
    let mut findings = Vec::new();
    for source in domain.sources {
        let (found, lint) = scan_tree(root, &root.join(source), domain.kind)?;
        extraction.merge(found);
        findings.extend(lint);
    }
    for source in domain.vendored {
        let (mut found, _lint) = scan_tree(root, &root.join(source), domain.kind)?;
        // Its messages, and neither its lint nor its problems: see `vendored`.
        found.problems.clear();
        extraction.merge(found);
    }
    Ok((extraction, findings))
}

/// The locales a domain has a catalogue for, sorted.
pub fn catalogue_locales(root: &Path, domain: &Domain) -> Result<Vec<String>> {
    let dir = root.join(domain.locales);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut tags: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            (path.extension().and_then(|x| x.to_str()) == Some("json"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect();
    tags.sort();
    Ok(tags)
}

/// Read one catalogue, or an empty one when the file is not there yet.
pub fn load_catalogue(root: &Path, domain: &Domain, locale: &Locale) -> Result<Catalog> {
    let path = catalogue_path(root, domain, locale);
    if !path.is_file() {
        return Ok(Catalog::new(locale.clone()));
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Catalog::parse(locale.clone(), &text).with_context(|| format!("in {}", path.display()))
}

/// Write one catalogue back: sorted, indented, with a trailing newline, so a
/// diff of two runs is the messages that changed and nothing else.
pub fn save_catalogue(root: &Path, domain: &Domain, catalog: &Catalog) -> Result<PathBuf> {
    let path = catalogue_path(root, domain, catalog.locale());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&catalog.to_json())
        .map_err(|e| Error::invalid(format!("serialising the catalogue: {e}")))?;
    text.push('\n');
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn catalogue_path(root: &Path, domain: &Domain, locale: &Locale) -> PathBuf {
    root.join(domain.locales)
        .join(format!("{}.json", locale.as_str()))
}

/// Where one locale of one domain stands.
#[derive(Debug, Clone)]
pub struct Coverage {
    pub locale: Locale,
    /// Keys the source uses that this catalogue translates.
    pub translated: usize,
    /// Keys the source uses.
    pub total: usize,
    /// Keys in the catalogue the source no longer uses. Reported, never
    /// deleted: a message removed on a branch is a message somebody will want
    /// back, and a translation is work.
    pub orphans: Vec<String>,
    /// The one thing that fails a check: a translation whose placeholders or
    /// plural forms do not match its key.
    pub mismatches: Vec<(String, String)>,
}

impl Coverage {
    /// Rounded down, so 99% never means "done".
    pub fn percent(&self) -> u32 {
        match self.total {
            0 => 100,
            total => u32::try_from(self.translated * 100 / total).unwrap_or(100),
        }
    }

    /// The line `check` prints.
    pub fn line(&self) -> String {
        format!(
            "  {:<8} {:>5}/{:<5} {:>3}%{}",
            self.locale.as_str(),
            self.translated,
            self.total,
            self.percent(),
            match self.orphans.len() {
                0 => String::new(),
                n => format!("   ({n} unused)"),
            }
        )
    }
}

/// Measure one catalogue against the keys the source uses, and check every
/// entry it has.
pub fn coverage(catalog: &Catalog, keys: &[String]) -> Coverage {
    let translated = keys.iter().filter(|k| catalog.get(k).is_some()).count();
    let mismatches = catalog
        .messages()
        .iter()
        .filter_map(|(key, message)| {
            sc_i18n::check(key, message, catalog.locale())
                .err()
                .map(|reason| (key.clone(), reason))
        })
        .collect();
    Coverage {
        locale: catalog.locale().clone(),
        translated,
        total: keys.len(),
        orphans: catalog.orphans(keys.iter().map(String::as_str)),
        mismatches,
    }
}

// The translator over a configured LLM provider (decision D9) lives in
// `sc-server`, because both callers of it are above that crate: this command and
// the Translations screen's **Translate missing** button must fill a catalogue
// the same way, and two implementations of one prompt would drift on the first
// fix to either.
pub use sc_server::{LlmTranslator, parse_answer};

/// A one-line summary of what a catalogue holds for a key, for `extract --domain`
/// output and for the Translations screen's future use.
pub fn preview(message: &Message) -> String {
    message.forms().join(" / ")
}

/// The English behind a key, for a printed line.
pub fn english(key: &str) -> &str {
    source_text(key)
}
