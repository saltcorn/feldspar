//! `feldspar llm …`: the LLM providers and their models, from a terminal.
//!
//! The same rows the admin UI's LLM providers screen and the `createLlmProvider`
//! MCP tool write, saved through the same checks (`sc_llm::save_llm_provider`,
//! `save_llm_model`), so a provider set up here is one an agent, a code body's
//! `llm.generate()` and an `embed_from` field can all use. A running server
//! reads providers from the database on each call, so it sees the change.
//!
//! The API key never goes on the command line, where `ps` and the shell history
//! would keep it: `--api-key-env VAR` reads it from the environment and
//! `--api-key-file PATH` from a file. Nothing here prints it back.

use sc_catalog::Catalog;
use sc_cli::{DbConfig, connect_catalog};
use sc_error::{Error, Result};
use sc_llm::{LlmModelDef, LlmProviderDef, ModelKind};
use serde_json::{Value as Json, json};

const USAGE: &str = "usage:
  feldspar llm list [--json]
  feldspar llm set-provider NAME --backend BACKEND [--api-key-env VAR | --api-key-file PATH]
                    [--base-url URL] [--set KEY=VALUE]... [--description TEXT] [--default]
  feldspar llm set-model PROVIDER MODEL [--kind chat|embedding|image|speech] [--default]
                    [--set KEY=VALUE]... [--description TEXT]
  feldspar llm set-default PROVIDER
  feldspar llm test PROVIDER [MODEL]
(each takes the database flags)";

/// `feldspar llm SUBCOMMAND …`.
pub(crate) async fn llm_command(args: &[String]) -> Result<()> {
    let (db, rest) = DbConfig::extract(args)?;
    let Some((verb, rest)) = rest.split_first() else {
        return Err(Error::config(USAGE));
    };
    if !["list", "set-provider", "set-model", "set-default", "test"].contains(&verb.as_str()) {
        return Err(Error::config(format!(
            "unknown llm subcommand `{verb}`\n{USAGE}"
        )));
    }
    if let Some(source) = db.source() {
        eprintln!("feldspar: database configured from {source}");
    }
    let catalog = connect_catalog(&db).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_llm::bootstrap_llm_models(&catalog).await?;
    match verb.as_str() {
        "list" => list(&catalog, rest).await,
        "set-provider" => set_provider(&catalog, rest).await,
        "set-model" => set_model(&catalog, rest).await,
        "set-default" => set_default(&catalog, rest).await,
        _ => test(&catalog, rest).await,
    }
}

/// The flags of one subcommand: its positional words, and each `--flag value`
/// (or bare `--flag`) in order.
struct Parsed {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Parsed {
    /// Split `args`, where `valued` are the flags that take a value and `bare`
    /// the ones that do not. Anything else is refused by name.
    fn of(args: &[String], valued: &[&str], bare: &[&str]) -> Result<Parsed> {
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut words = args.iter();
        while let Some(word) = words.next() {
            let Some(flag) = word.strip_prefix("--") else {
                positional.push(word.clone());
                continue;
            };
            if bare.contains(&flag) {
                flags.push((flag.to_owned(), None));
            } else if valued.contains(&flag) {
                let value = words
                    .next()
                    .ok_or_else(|| Error::config(format!("`--{flag}` needs a value")))?;
                flags.push((flag.to_owned(), Some(value.clone())));
            } else {
                return Err(Error::config(format!("unknown flag `--{flag}`\n{USAGE}")));
            }
        }
        Ok(Parsed { positional, flags })
    }

    fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|(f, _)| f == flag)
    }

    /// The last value given for `flag`.
    fn value(&self, flag: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| f == flag)
            .and_then(|(_, v)| v.as_deref())
    }

    /// Every `--set KEY=VALUE`, as a setting: `true`, `false` and numbers are
    /// read as JSON, anything else as text.
    fn settings(&self) -> Result<Vec<(String, Json)>> {
        self.flags
            .iter()
            .filter(|(f, _)| f == "set")
            .filter_map(|(_, v)| v.as_deref())
            .map(|pair| {
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| Error::config(format!("`--set {pair}` is not KEY=VALUE")))?;
                let value = match serde_json::from_str::<Json>(value) {
                    Ok(v @ (Json::Bool(_) | Json::Number(_))) => v,
                    _ => Json::String(value.to_owned()),
                };
                Ok((key.trim().to_owned(), value))
            })
            .collect()
    }

    /// Exactly `n` positional words, or the usage.
    fn words(&self, n: std::ops::RangeInclusive<usize>) -> Result<&[String]> {
        if n.contains(&self.positional.len()) {
            Ok(&self.positional)
        } else {
            Err(Error::config(USAGE))
        }
    }
}

async fn list(catalog: &Catalog, args: &[String]) -> Result<()> {
    let parsed = Parsed::of(args, &[], &["json"])?;
    parsed.words(0..=0)?;
    let default = sc_llm::default_llm_provider(catalog).await?.map(|d| d.id);
    let mut out = Vec::new();
    for def in sc_llm::list_llm_providers(catalog).await? {
        let models = sc_llm::list_llm_models(catalog, &def).await?;
        out.push(json!({
            "name": def.name,
            "backend": def.backend,
            "description": def.description,
            "base_url": def.setting(sc_llm::CFG_BASE_URL),
            "has_api_key": !def.api_key().expose().is_empty(),
            "is_default": Some(def.id) == default,
            "from_config_file": sc_llm::is_host_llm_provider(catalog, def.id),
            "models": models.iter().map(|m| json!({
                "name": m.name,
                "kind": m.kind.as_str(),
                "is_default": m.is_default,
            })).collect::<Vec<_>>(),
        }));
    }
    if parsed.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&out).map_err(|e| Error::msg(e.to_string()))?
        );
        return Ok(());
    }
    if out.is_empty() {
        eprintln!("feldspar: no LLM providers; add one with `feldspar llm set-provider`");
    }
    for p in &out {
        let mut notes = Vec::new();
        if p["is_default"] == json!(true) {
            notes.push("default");
        }
        if p["from_config_file"] == json!(true) {
            notes.push("from the configuration file");
        }
        if p["has_api_key"] == json!(false) {
            notes.push("no API key");
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join(", "))
        };
        println!(
            "{} [{}]{notes}",
            p["name"].as_str().unwrap_or_default(),
            p["backend"].as_str().unwrap_or_default()
        );
        for m in p["models"].as_array().into_iter().flatten() {
            println!(
                "  {} — {}{}",
                m["name"].as_str().unwrap_or_default(),
                m["kind"].as_str().unwrap_or_default(),
                if m["is_default"] == json!(true) {
                    ", default"
                } else {
                    ""
                }
            );
        }
    }
    Ok(())
}

/// The key `--api-key-env` or `--api-key-file` names, if either does.
fn api_key(parsed: &Parsed) -> Result<Option<String>> {
    match (parsed.value("api-key-env"), parsed.value("api-key-file")) {
        (Some(_), Some(_)) => Err(Error::config(
            "give the API key with `--api-key-env` or `--api-key-file`, not both",
        )),
        (Some(var), None) => std::env::var(var)
            .map(|k| Some(k.trim().to_owned()))
            .map_err(|_| Error::config(format!("the environment variable `{var}` is not set"))),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map(|k| Some(k.trim().to_owned()))
            .map_err(|e| Error::config(format!("reading the API key from `{path}`: {e}"))),
        (None, None) => Ok(None),
    }
}

async fn set_provider(catalog: &Catalog, args: &[String]) -> Result<()> {
    let parsed = Parsed::of(
        args,
        &[
            "backend",
            "api-key",
            "api-key-env",
            "api-key-file",
            "base-url",
            "set",
            "description",
        ],
        &["default"],
    )?;
    let name = parsed.words(1..=1)?[0].trim().to_owned();
    if parsed.has("api-key") {
        return Err(Error::config(
            "the API key does not go on the command line; use `--api-key-env VAR` or \
             `--api-key-file PATH`",
        ));
    }
    let stored = sc_llm::load_llm_provider_by_name(catalog, &name).await?;
    // An update starts from what is stored, so a setting left out (the key,
    // above all) is kept.
    let mut def = match (&stored, parsed.value("backend")) {
        (Some(stored), Some(backend)) if backend != stored.backend => {
            return Err(Error::config(format!(
                "LLM provider `{name}` is served by `{}`; a backend is fixed once saved",
                stored.backend
            )));
        }
        (Some(stored), _) => stored.clone(),
        (None, Some(backend)) => LlmProviderDef::new(&name, backend),
        (None, None) => {
            return Err(Error::config(format!(
                "a new LLM provider needs `--backend`: one of {}",
                sc_llm::registered_backends().join(", ")
            )));
        }
    };
    if let Some(key) = api_key(&parsed)? {
        def.config
            .insert(sc_llm::CFG_API_KEY.to_owned(), Json::String(key));
    }
    if let Some(url) = parsed.value("base-url") {
        def.config.insert(
            sc_llm::CFG_BASE_URL.to_owned(),
            Json::String(url.to_owned()),
        );
    }
    for (key, value) in parsed.settings()? {
        def.config.insert(key, value);
    }
    if let Some(description) = parsed.value("description") {
        def.description = description.to_owned();
    }
    sc_llm::save_llm_provider(catalog, &def).await?;
    eprintln!(
        "feldspar: {} LLM provider `{name}` ({})",
        if stored.is_some() { "updated" } else { "added" },
        def.backend
    );
    if parsed.has("default") {
        sc_llm::set_default_llm_provider(catalog, Some(def.id)).await?;
        eprintln!("feldspar: `{name}` is now the default LLM provider");
    }
    if sc_llm::list_llm_models(catalog, &def).await?.is_empty() {
        eprintln!(
            "feldspar: it has no models yet; add one with `feldspar llm set-model {name} MODEL \
             --default`"
        );
    }
    Ok(())
}

async fn set_model(catalog: &Catalog, args: &[String]) -> Result<()> {
    let parsed = Parsed::of(args, &["kind", "set", "description"], &["default"])?;
    let words = parsed.words(2..=2)?;
    let provider = sc_llm::require_llm_provider(catalog, words[0].trim()).await?;
    let name = words[1].trim();
    let kind = parsed.value("kind").map(ModelKind::parse).transpose()?;
    let stored = sc_llm::load_llm_model_by_name(catalog, &provider, name).await?;
    let mut model = match &stored {
        Some(stored) => {
            if let Some(kind) = kind
                && kind != stored.kind
            {
                return Err(Error::config(format!(
                    "model `{name}` of `{}` is {} model; a kind is fixed once saved",
                    provider.name,
                    stored.kind.with_article()
                )));
            }
            stored.clone()
        }
        None => LlmModelDef::new(provider.id, name).kind(kind.unwrap_or_default()),
    };
    if parsed.has("default") {
        model.is_default = true;
    }
    for (key, value) in parsed.settings()? {
        model.config.insert(key, value);
    }
    if let Some(description) = parsed.value("description") {
        model.description = description.to_owned();
    }
    let saved = sc_llm::save_llm_model(catalog, &model).await?;
    eprintln!(
        "feldspar: {} {} model `{name}` of `{}`{}",
        if stored.is_some() { "updated" } else { "added" },
        saved.kind.as_str(),
        provider.name,
        if saved.is_default {
            ", its default"
        } else {
            ""
        }
    );
    Ok(())
}

async fn set_default(catalog: &Catalog, args: &[String]) -> Result<()> {
    let parsed = Parsed::of(args, &[], &[])?;
    let provider = sc_llm::require_llm_provider(catalog, parsed.words(1..=1)?[0].trim()).await?;
    sc_llm::set_default_llm_provider(catalog, Some(provider.id)).await?;
    eprintln!(
        "feldspar: `{}` is now the default LLM provider",
        provider.name
    );
    Ok(())
}

/// Send one trivial request to a model — its provider's default chat model
/// when none is named — and say what came back. This makes a model call.
async fn test(catalog: &Catalog, args: &[String]) -> Result<()> {
    let parsed = Parsed::of(args, &[], &[])?;
    let words = parsed.words(1..=2)?;
    let provider = sc_llm::require_llm_provider(catalog, words[0].trim()).await?;
    let named = words.get(1).map(|m| m.trim());
    let model = match named {
        Some(name) => match sc_llm::load_llm_model_by_name(catalog, &provider, name).await? {
            Some(model) => model,
            None => sc_llm::require_llm_model(catalog, &provider, Some(name)).await?,
        },
        None => sc_llm::require_llm_model(catalog, &provider, None).await?,
    };
    let said = match model.kind {
        ModelKind::Chat => {
            let connected = sc_llm::connect_model(&provider, &model)?;
            let answer = connected
                .provider
                .stream(sc_llm::LlmRequest::prompt("Reply with the single word: ok"))
                .await?
                .collect()
                .await?;
            format!("answered {:?}", answer.content.trim())
        }
        ModelKind::Embedding => {
            let embedded = sc_llm::embed(&provider, &model, &["ok".to_owned()]).await?;
            format!(
                "answered a vector of {} dimensions",
                embedded.vectors.first().map_or(0, Vec::len)
            )
        }
        other => {
            return Err(Error::config(format!(
                "`feldspar llm test` tests chat and embedding models; `{}` is {} model \
                 (test it from the LLM providers screen)",
                model.name,
                other.with_article()
            )));
        }
    };
    println!("{}/{}: {said}", provider.name, model.name);
    Ok(())
}
