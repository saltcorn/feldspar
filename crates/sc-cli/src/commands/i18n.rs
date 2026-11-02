//! `feldspar i18n …`: extracting, linting, checking and translating the message catalogues.

use sc_cli::DbConfig;
use sc_cli::connect_catalog;
use sc_error::Result;

/// `feldspar i18n extract|lint|check|translate …` (task 2.4).
///
/// The one extraction pass in `sc_i18n::extract`, with four things to do with
/// it. Only `translate` touches a database — it is the one that calls an LLM,
/// and which provider that is is a stored row — so the other three run in a
/// checkout with nothing else standing up.
pub(crate) async fn i18n_command(args: &[String]) -> Result<()> {
    use sc_cli::i18n::{Command, I18nArgs, find_root};

    // The database flags belong to `translate` alone. Extracting them for the
    // other three as well would mean `feldspar i18n check --root /tmp/x` in a
    // container with no `DATABASE_URL` failing on a connection it never makes.
    let translating = args.first().map(String::as_str) == Some("translate");
    let (db, rest) = match translating {
        true => {
            let (db, rest) = DbConfig::extract(args.to_vec())?;
            (Some(db), rest)
        }
        false => (None, args.to_vec()),
    };
    let parsed = I18nArgs::parse(&rest)?;
    // The shared catalogue is in the database, not in a checkout.
    if parsed.instance {
        let db = db.ok_or_else(|| sc_error::Error::config("no database flags parsed"))?;
        return i18n_translate_instance(&parsed, db).await;
    }
    let root = match &parsed.root {
        Some(root) => root.clone(),
        None => find_root(&std::env::current_dir().map_err(|e| {
            sc_error::Error::config(format!("reading the working directory: {e}"))
        })?)?,
    };
    match parsed.command {
        Command::Extract => i18n_extract(&root, &parsed),
        Command::Lint => i18n_lint(&root, &parsed),
        Command::Check => i18n_check(&root, &parsed),
        Command::Translate => {
            let db = db.ok_or_else(|| sc_error::Error::config("no database flags parsed"))?;
            i18n_translate(&root, &parsed, db).await
        }
    }
}

/// The trees a command reads: the bare paths when there are any, else the
/// domains'.
///
/// A bare path is what makes `feldspar i18n lint ui/admin/src` work (task 3.4)
/// and what a scaffolded application's own tree will be passed as.
pub(crate) fn i18n_trees(
    root: &std::path::Path,
    parsed: &sc_cli::i18n::I18nArgs,
) -> Vec<(String, std::path::PathBuf, sc_cli::i18n::Kind)> {
    if !parsed.paths.is_empty() {
        return parsed
            .paths
            .iter()
            .map(|path| {
                let full = match path.is_absolute() {
                    true => path.clone(),
                    false => root.join(path),
                };
                // A named path is source to read, and which parser to read it
                // with is decided per file — so the kind here only picks the
                // extension filter, and a tree of `.rs` gets the Rust one.
                let kind = match parsed
                    .domains
                    .iter()
                    .find(|d| d.sources.iter().any(|s| path.starts_with(s)))
                {
                    Some(domain) => domain.kind,
                    None => sc_cli::i18n::Kind::Js,
                };
                (path.to_string_lossy().into_owned(), full, kind)
            })
            .collect();
    }
    parsed
        .domains
        .iter()
        .flat_map(|domain| {
            domain
                .sources
                .iter()
                .map(move |source| (domain.name.to_owned(), root.join(source), domain.kind))
        })
        .collect()
}

/// `feldspar i18n extract` — every message, `file:line: key`, one per line.
///
/// Greppable on purpose: this is the command somebody runs to find out where a
/// sentence they saw on a screen is written.
pub(crate) fn i18n_extract(root: &std::path::Path, parsed: &sc_cli::i18n::I18nArgs) -> Result<()> {
    let mut problems = 0usize;
    for domain in &parsed.domains {
        let (found, _) = sc_cli::i18n::scan_domain(root, domain)?;
        for message in &found.messages {
            println!(
                "{}:{}: {}",
                message.file,
                message.line,
                display_key(&message.key)
            );
        }
        for problem in &found.problems {
            eprintln!("error: {problem}");
        }
        problems += found.problems.len();
        eprintln!(
            "feldspar: {} — {} message{} at {} call site{}",
            domain.name,
            found.keys().len(),
            plural(found.keys().len()),
            found.messages.len(),
            plural(found.messages.len())
        );
    }
    match problems {
        0 => Ok(()),
        n => Err(sc_error::Error::invalid(format!(
            "{n} call site{} could not be read",
            plural(n)
        ))),
    }
}

/// `feldspar i18n lint [PATH…]` — the literals nobody wrapped (task 2.2).
pub(crate) fn i18n_lint(root: &std::path::Path, parsed: &sc_cli::i18n::I18nArgs) -> Result<()> {
    let mut total = 0usize;
    for (name, dir, kind) in i18n_trees(root, parsed) {
        let (_, findings) = sc_cli::i18n::scan_tree(root, &dir, kind)?;
        for finding in &findings {
            if parsed.json {
                // One object per line, so the output streams and `jq` reads it
                // without holding the whole run.
                println!(
                    "{}",
                    serde_json::json!({
                        "file": finding.file,
                        "line": finding.line,
                        "what": match &finding.what {
                            sc_i18n::Unwrapped::JsxText => "text".to_owned(),
                            sc_i18n::Unwrapped::Attribute(name) => name.clone(),
                        },
                        "start": finding.span.0,
                        "end": finding.span.1,
                    })
                );
            } else {
                println!("{finding}");
            }
        }
        eprintln!(
            "feldspar: {name} — {} unwrapped literal{}",
            findings.len(),
            plural(findings.len())
        );
        total += findings.len();
    }
    match total {
        0 => Ok(()),
        n => Err(sc_error::Error::invalid(format!(
            "{n} unwrapped literal{}",
            plural(n)
        ))),
    }
}

/// `feldspar i18n check` — coverage per locale, and the one failure.
///
/// **Coverage is a number, not a gate.** What fails here is a translation whose
/// placeholders or plural forms do not match its key, and a call site that
/// could not be read: the first renders wrongly in front of a person, the
/// second never reaches a catalogue at all. A locale that is 40% translated is
/// a fact about a work in progress and not a broken build.
pub(crate) fn i18n_check(root: &std::path::Path, parsed: &sc_cli::i18n::I18nArgs) -> Result<()> {
    let mut failures = 0usize;
    for domain in &parsed.domains {
        let (found, findings) = sc_cli::i18n::scan_domain(root, domain)?;
        let keys = found.keys();
        let locales = match parsed.locales.is_empty() {
            true => sc_cli::i18n::catalogue_locales(root, domain)?,
            false => parsed.locales.clone(),
        };
        println!(
            "{}: {} message{}, {} locale{}, {} unwrapped literal{}",
            domain.name,
            keys.len(),
            plural(keys.len()),
            locales.len(),
            plural(locales.len()),
            findings.len(),
            plural(findings.len())
        );
        for problem in &found.problems {
            eprintln!("error: {problem}");
        }
        failures += found.problems.len();
        for tag in &locales {
            let locale = sc_i18n::Locale::parse(tag)?;
            let catalog = sc_cli::i18n::load_catalogue(root, domain, &locale)?;
            let coverage = sc_cli::i18n::coverage(&catalog, &keys);
            println!("{}", coverage.line());
            for (key, reason) in &coverage.mismatches {
                eprintln!(
                    "error: {}/{}.json: `{}`: {reason}",
                    domain.locales,
                    locale.as_str(),
                    display_key(key)
                );
            }
            failures += coverage.mismatches.len();
        }
    }
    match failures {
        0 => Ok(()),
        n => Err(sc_error::Error::invalid(format!(
            "{n} translation{} or call site{} must be fixed",
            plural(n),
            plural(n)
        ))),
    }
}

/// `feldspar i18n translate --domain D --locale L` — fill one catalogue through
/// the configured LLM, checking every answer (decision D9).
pub(crate) async fn i18n_translate(
    root: &std::path::Path,
    parsed: &sc_cli::i18n::I18nArgs,
    db: DbConfig,
) -> Result<()> {
    if parsed.locales.is_empty() {
        return Err(sc_error::Error::config(
            "feldspar i18n translate needs a target: --locale fr",
        ));
    }
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog_db = connect_catalog(&db).await?;
    let translator = cli_translator(&catalog_db, parsed).await?;
    let source = sc_i18n::Locale::source();

    for domain in &parsed.domains {
        let (found, _) = sc_cli::i18n::scan_domain(root, domain)?;
        let keys = found.keys();
        for tag in &parsed.locales {
            let locale = sc_i18n::Locale::parse(tag)?;
            let mut catalogue = sc_cli::i18n::load_catalogue(root, domain, &locale)?;
            let missing = catalogue.missing(keys.iter().map(String::as_str)).len();
            eprintln!(
                "feldspar: {} → {}: {missing} message{} to translate with {}",
                domain.name,
                locale.as_str(),
                plural(missing),
                translator.describes()
            );
            if missing == 0 {
                continue;
            }
            let report =
                sc_i18n::translate_missing(&mut catalogue, &source, &translator, &keys).await?;
            for warning in report.warnings() {
                eprintln!("warning: {warning}");
            }
            let path = sc_cli::i18n::save_catalogue(root, domain, &catalogue)?;
            eprintln!(
                "feldspar: filled {}, refused {}, unanswered {} — wrote {}",
                report.filled.len(),
                report.rejected.len(),
                report.unanswered.len(),
                path.display()
            );
        }
    }
    Ok(())
}

/// The translator `--provider`/`--model` name: a named provider, or the
/// installation's default provider, with its default model. The error says what
/// to configure rather than what failed.
pub(crate) async fn cli_translator(
    catalog_db: &sc_catalog::Catalog,
    parsed: &sc_cli::i18n::I18nArgs,
) -> Result<sc_server::LlmTranslator> {
    let provider = match &parsed.provider {
        Some(name) => sc_llm::load_llm_provider_by_name(catalog_db, name)
            .await?
            .ok_or_else(|| sc_error::Error::not_found(format!("no LLM provider named `{name}`")))?,
        None => sc_llm::default_llm_provider(catalog_db)
            .await?
            .ok_or_else(|| {
                sc_error::Error::config(
                    "no LLM provider is configured — add one in Settings → LLM providers, \
                     or name one with --provider",
                )
            })?,
    };
    let model = sc_llm::require_llm_model(catalog_db, &provider, parsed.model.as_deref()).await?;
    Ok(sc_server::LlmTranslator::new(sc_llm::connect_model(
        &provider, &model,
    )?))
}

/// `feldspar i18n translate --instance --locale L` — fill the instance's shared
/// catalogue (TODO F6) through the configured LLM: every key another of its
/// locales has, checked as the screen's **Translate missing** is.
pub(crate) async fn i18n_translate_instance(
    parsed: &sc_cli::i18n::I18nArgs,
    db: DbConfig,
) -> Result<()> {
    if parsed.locales.is_empty() {
        return Err(sc_error::Error::config(
            "feldspar i18n translate --instance needs a target: --locale de",
        ));
    }
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog_db = connect_catalog(&db).await?;
    let translator = cli_translator(&catalog_db, parsed).await?;
    for tag in &parsed.locales {
        let locale = sc_i18n::Locale::parse(tag)?;
        eprintln!(
            "feldspar: shared catalogue → {}: translating with {}",
            locale.as_str(),
            translator.describes()
        );
        let report =
            sc_server::translations::fill_instance_missing(&catalog_db, None, &locale, &translator)
                .await?;
        for warning in report["warnings"].as_array().into_iter().flatten() {
            eprintln!("warning: {}", warning.as_str().unwrap_or_default());
        }
        eprintln!(
            "feldspar: filled {}, refused {}, unanswered {}",
            report["filled"],
            report["rejected"].as_array().map_or(0, Vec::len),
            report["unanswered"].as_array().map_or(0, Vec::len),
        );
    }
    eprintln!(
        "feldspar: a running server serves the change after `pkill -HUP feldspar` (or a restart)"
    );
    Ok(())
}

/// A catalogue key as a person reads it: the context, where there is one, in
/// front of the English rather than a control character between them.
pub(crate) fn display_key(key: &str) -> String {
    match sc_i18n::key_context(key) {
        Some(context) => format!("[{context}] {}", sc_i18n::source_text(key)),
        None => key.to_owned(),
    }
}

pub(crate) fn plural(n: usize) -> &'static str {
    match n {
        1 => "",
        _ => "s",
    }
}
