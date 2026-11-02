//! The LLM provider a host supplies (`feldspar.toml`), and the installation's
//! default provider — against a real Postgres, because the claim is about what
//! the readers merge and the writers refuse *beside* the stored rows.

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_llm::{
    ANTHROPIC_BACKEND, CFG_API_KEY, CFG_PRICE_INPUT, HostLlmProvider, LlmModelDef, LlmProviderDef,
    OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND, bootstrap_llm_providers, default_llm_provider,
    delete_llm_model, delete_llm_provider, host_provider_id, is_host_llm_provider, list_llm_models,
    list_llm_providers, load_llm_model, load_llm_provider, load_llm_provider_by_name,
    require_llm_model, save_llm_model, save_llm_provider, set_default_llm_provider,
    set_host_llm_provider,
};
use sc_test_harness::TestDb;

/// A catalog over a per-test database, with the provider tables and the
/// configuration table (where the default is stored) bootstrapped.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_llm_providers(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    Ok(catalog)
}

/// The host's provider: `hosted`, two models, the first the default.
fn hosted() -> Result<HostLlmProvider> {
    let def = LlmProviderDef::new("hosted", ANTHROPIC_BACKEND).with(CFG_API_KEY, "sk-operator");
    let models = vec![
        LlmModelDef::new(def.id, "claude-sonnet-5").with(CFG_PRICE_INPUT, 3.0),
        LlmModelDef::new(def.id, "claude-haiku-4-5"),
    ];
    HostLlmProvider::new(def, models, "claude-sonnet-5")
}

#[tokio::test]
async fn the_hosts_provider_is_found_like_a_stored_one_and_never_stored() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let admins = LlmProviderDef::new("mine", OPENAI_RESPONSES_BACKEND).with(CFG_API_KEY, "sk-1");
    save_llm_provider(&catalog, &admins).await?;
    set_host_llm_provider(&catalog, Some(hosted()?)).await?;

    // Listed beside the admin's, ordered by name, with an id derived from its
    // name — the same on every boot.
    let listed = list_llm_providers(&catalog).await?;
    let names: Vec<&str> = listed.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["hosted", "mine"]);
    let host = &listed[0];
    assert_eq!(host.id, host_provider_id("hosted"));
    assert!(is_host_llm_provider(&catalog, host.id));
    assert!(!is_host_llm_provider(&catalog, admins.id));

    // Found by id and by name, with its key: an agent calls through it.
    assert_eq!(
        load_llm_provider(&catalog, host.id).await?.as_ref(),
        Some(host)
    );
    let by_name = load_llm_provider_by_name(&catalog, "hosted")
        .await?
        .expect("by name");
    assert_eq!(by_name.setting(CFG_API_KEY), Some("sk-operator"));

    // Its models, the default among them, with their settings.
    let models = list_llm_models(&catalog, host).await?;
    let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["claude-haiku-4-5", "claude-sonnet-5"]);
    let default = require_llm_model(&catalog, host, None).await?;
    assert_eq!(default.name, "claude-sonnet-5");
    assert_eq!(default.prices().input, Some(3.0));
    assert_eq!(load_llm_model(&catalog, default.id).await?, Some(default));

    // And no row was written for it: the key is in no table.
    let rows = catalog
        .primary(sc_catalog::Unchecked::system("test"))
        .query(&sc_query::Statement::from(sc_query::Select::from(
            sc_query::Source::table(sc_llm::LLM_PROVIDERS_TABLE),
        )))
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 1, "only the admin's provider is a row");
    Ok(())
}

#[tokio::test]
async fn every_write_to_the_hosts_provider_is_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    set_host_llm_provider(&catalog, Some(hosted()?)).await?;
    let host = load_llm_provider_by_name(&catalog, "hosted")
        .await?
        .expect("host");
    let read_only = |e: sc_error::Error| {
        let text = e.to_string();
        assert!(text.contains("configuration file"), "{text}");
    };

    // Changing it, deleting it.
    let changed = host.clone().with(CFG_API_KEY, "sk-admin");
    read_only(
        save_llm_provider(&catalog, &changed)
            .await
            .expect_err("save"),
    );
    read_only(
        delete_llm_provider(&catalog, host.id, &[])
            .await
            .expect_err("delete"),
    );
    // Adding a model, changing one, deleting one.
    let added = LlmModelDef::new(host.id, "claude-opus-5-5");
    read_only(
        save_llm_model(&catalog, &added)
            .await
            .expect_err("add model"),
    );
    let existing = require_llm_model(&catalog, &host, None).await?;
    let mut edited = existing.clone();
    edited.description = "edited".to_owned();
    read_only(
        save_llm_model(&catalog, &edited)
            .await
            .expect_err("edit model"),
    );
    read_only(
        delete_llm_model(&catalog, existing.id, &[])
            .await
            .expect_err("delete model"),
    );
    // And a provider the admin adds may not take its name.
    let squatter = LlmProviderDef::new("hosted", OPENAI_RESPONSES_BACKEND).with(CFG_API_KEY, "k");
    read_only(
        save_llm_provider(&catalog, &squatter)
            .await
            .expect_err("name"),
    );

    // All of it still as the file said.
    assert_eq!(list_llm_models(&catalog, &host).await?.len(), 2);
    assert_eq!(
        load_llm_provider(&catalog, host.id)
            .await?
            .and_then(|d| d.setting(CFG_API_KEY).map(str::to_owned))
            .as_deref(),
        Some("sk-operator")
    );
    Ok(())
}

/// A stored provider already holding the name is the operator's to resolve:
/// shadowing it would quietly swap which key the admin's agents are billed to.
#[tokio::test]
async fn a_host_provider_clashing_with_a_stored_one_is_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let stored = LlmProviderDef::new("hosted", OPENAI_RESPONSES_BACKEND).with(CFG_API_KEY, "sk-1");
    save_llm_provider(&catalog, &stored).await?;
    let err = set_host_llm_provider(&catalog, Some(hosted()?))
        .await
        .expect_err("two providers named `hosted`");
    assert!(err.to_string().contains("rename"), "{err}");
    assert!(sc_llm::host_llm_provider(&catalog).is_none());
    Ok(())
}

/// What the file says is checked as a save would check it, so a typo stops
/// the server rather than an agent.
#[test]
fn a_host_provider_is_checked_like_a_saved_one() {
    // No key for a backend that needs one.
    let keyless = LlmProviderDef::new("hosted", ANTHROPIC_BACKEND);
    assert!(HostLlmProvider::new(keyless, vec![], "m").is_err());
    // An unknown backend.
    let unknown = LlmProviderDef::new("hosted", "bard").with(CFG_API_KEY, "k");
    assert!(HostLlmProvider::new(unknown, vec![], "m").is_err());
    // A model setting the backend does not declare.
    let def = LlmProviderDef::new("hosted", OPENAI_CHAT_BACKEND)
        .with(sc_llm::CFG_BASE_URL, "http://localhost:8080/v1");
    let typo = LlmModelDef::new(def.id, "m").with("price_inptu", 1.0);
    let err = HostLlmProvider::new(def.clone(), vec![typo], "m").expect_err("typo");
    assert!(err.to_string().contains("price_inptu"), "{err}");
    // A model listed twice.
    let twice = vec![LlmModelDef::new(def.id, "m"), LlmModelDef::new(def.id, "m")];
    assert!(HostLlmProvider::new(def.clone(), twice, "m").is_err());
    // The default left out of the list is added, with the built-in settings.
    let host = HostLlmProvider::new(
        def,
        vec![LlmModelDef::new(sc_llm::LlmProviderDefId::new(), "a")],
        "b",
    )
    .expect("valid");
    let models: Vec<(&str, bool)> = host
        .models()
        .iter()
        .map(|m| (m.name.as_str(), m.is_default))
        .collect();
    assert_eq!(models, [("a", false), ("b", true)]);
    assert!(host.models().iter().all(|m| m.provider_id == host.def().id));
}

/// The default: the admin's choice, else the host's provider, else the first
/// by name — and a choice whose provider is gone falls back.
#[tokio::test]
async fn the_default_provider_is_the_admins_choice_then_the_hosts_then_the_first() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    assert_eq!(default_llm_provider(&catalog).await?, None);

    let zed = LlmProviderDef::new("zed", OPENAI_RESPONSES_BACKEND).with(CFG_API_KEY, "k");
    let alpha = LlmProviderDef::new("alpha", OPENAI_RESPONSES_BACKEND).with(CFG_API_KEY, "k");
    save_llm_provider(&catalog, &zed).await?;
    save_llm_provider(&catalog, &alpha).await?;
    let name = |d: Option<LlmProviderDef>| d.map(|d| d.name);
    assert_eq!(
        name(default_llm_provider(&catalog).await?).as_deref(),
        Some("alpha")
    );

    // The host's provider outranks the first by name…
    set_host_llm_provider(&catalog, Some(hosted()?)).await?;
    assert_eq!(
        name(default_llm_provider(&catalog).await?).as_deref(),
        Some("hosted")
    );

    // …and the admin's choice outranks the host's.
    set_default_llm_provider(&catalog, Some(zed.id)).await?;
    assert_eq!(
        name(default_llm_provider(&catalog).await?).as_deref(),
        Some("zed")
    );
    // The host's own provider may be chosen too.
    set_default_llm_provider(&catalog, Some(host_provider_id("hosted"))).await?;
    assert_eq!(
        name(default_llm_provider(&catalog).await?).as_deref(),
        Some("hosted")
    );

    // A chosen provider that is deleted falls back rather than failing.
    set_default_llm_provider(&catalog, Some(zed.id)).await?;
    delete_llm_provider(&catalog, zed.id, &[]).await?;
    assert_eq!(
        name(default_llm_provider(&catalog).await?).as_deref(),
        Some("hosted")
    );

    // A provider that does not exist cannot be chosen.
    let missing = sc_llm::LlmProviderDefId::new();
    assert!(
        set_default_llm_provider(&catalog, Some(missing))
            .await
            .is_err()
    );
    Ok(())
}
