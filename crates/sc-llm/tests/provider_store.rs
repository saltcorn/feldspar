//! `_fd_llm_providers` and `_fd_llm_models` against a **real Postgres**
//! (principle 4): the rows are the definitions, so what a save writes and a load
//! reads back is the whole of whether a configured provider and its models
//! survive a restart.
//!
//! The secret round trip is here rather than in a unit test for the same reason.
//! The claim `FormField::secret` makes is that a *stored* key survives a
//! read-edit-save cycle in which the reader never saw it, and that claim spans
//! the redaction, the merge and the storage. Asserting it against the real table
//! is what makes it a claim about the system rather than about three functions.

use std::sync::Arc;

use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_llm::{
    ANTHROPIC_BACKEND, CFG_API_KEY, CFG_BASE_URL, CFG_CONTEXT_WINDOW, CFG_EDIT_FORMAT,
    CFG_PRICE_INPUT, CFG_PRICE_OUTPUT, LLM_MODELS_TABLE, LlmModelDef, LlmProviderDef,
    LlmProviderDefId, ModelKind, OPENAI_CHAT_BACKEND, OPENAI_RESPONSES_BACKEND,
    bootstrap_llm_providers, check_provider_saveable, connect_model, delete_llm_model,
    delete_llm_provider, list_llm_models, list_llm_providers, load_llm_model, load_llm_provider,
    load_llm_provider_by_name, provider_config_spec, require_llm_model, require_llm_provider,
    require_model_of_kind, save_llm_model, save_llm_provider,
};
use sc_test_harness::TestDb;
use sc_types::{SECRET_SENTINEL, merge_secrets, redact_attrs};

/// A catalog over a per-test database, with both tables bootstrapped.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_llm_providers(&catalog).await?;
    Ok(catalog)
}

fn anthropic(name: &str) -> LlmProviderDef {
    LlmProviderDef::new(name, ANTHROPIC_BACKEND).with(CFG_API_KEY, "sk-ant-secret")
}

fn openai(name: &str) -> LlmProviderDef {
    LlmProviderDef::new(name, OPENAI_RESPONSES_BACKEND).with(CFG_API_KEY, "sk-1")
}

#[tokio::test]
async fn a_provider_and_its_model_round_trip_through_their_rows() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;

    let def = anthropic("main").description("The house Anthropic key");
    save_llm_provider(&catalog, &def).await?;
    let model = LlmModelDef::new(def.id, "claude-sonnet-5")
        .description("The everyday model")
        .default_model()
        .with(CFG_PRICE_INPUT, 3.0)
        .with(CFG_PRICE_OUTPUT, 15.0)
        .with(CFG_CONTEXT_WINDOW, 200_000);
    save_llm_model(&catalog, &model).await?;

    let loaded = load_llm_provider(&catalog, def.id)
        .await?
        .expect("the row that was just written");
    assert_eq!(loaded, def);
    assert_eq!(
        load_llm_provider_by_name(&catalog, "main").await?.as_ref(),
        Some(&def)
    );
    assert_eq!(require_llm_provider(&catalog, "main").await?, def);

    let loaded_model = load_llm_model(&catalog, model.id)
        .await?
        .expect("the model row");
    assert_eq!(loaded_model, model);
    assert_eq!(
        list_llm_models(&catalog, &loaded).await?,
        std::slice::from_ref(&model)
    );
    // By name, and as the default an agent that names no model gets.
    assert_eq!(
        require_llm_model(&catalog, &loaded, Some("claude-sonnet-5")).await?,
        model
    );
    assert_eq!(require_llm_model(&catalog, &loaded, None).await?, model);

    // A definition that survived the round trip is one that still connects, with
    // its prices and capabilities resolved.
    let connected = connect_model(&loaded, &loaded_model)?;
    assert_eq!(connected.provider.model(), "claude-sonnet-5");
    assert_eq!(connected.prices.output, Some(15.0));
    Ok(())
}

#[tokio::test]
async fn blank_model_settings_are_not_stored() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let def = openai("oai");
    save_llm_provider(&catalog, &def).await?;

    // What a form sends for untouched inputs.
    let model = LlmModelDef::new(def.id, "gpt-5.1")
        .with(CFG_PRICE_INPUT, "")
        .with(CFG_EDIT_FORMAT, serde_json::Value::Null)
        .with(CFG_PRICE_OUTPUT, 10.0);
    let saved = save_llm_model(&catalog, &model).await?;
    let loaded = load_llm_model(&catalog, model.id).await?.expect("the row");
    assert_eq!(loaded, saved);
    assert_eq!(loaded.config.len(), 1, "{:?}", loaded.config);
    // Blank means the built-in rule, which is what applies.
    assert_eq!(
        loaded.capabilities(OPENAI_RESPONSES_BACKEND).edit_format,
        sc_llm::EditFormat::ApplyPatch
    );
    Ok(())
}

#[tokio::test]
async fn bootstrap_is_idempotent_and_saving_twice_updates_in_place() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    // Called again exactly as a second boot would.
    bootstrap_llm_providers(&catalog).await?;
    assert!(catalog.get(LLM_MODELS_TABLE)?.is_some());

    let mut def = openai("gateway");
    save_llm_provider(&catalog, &def).await?;
    def.config.insert(
        CFG_BASE_URL.to_owned(),
        "https://gateway.internal/v1".into(),
    );
    def.description = "Through the gateway".to_owned();
    save_llm_provider(&catalog, &def).await?;

    let all = list_llm_providers(&catalog).await?;
    assert_eq!(all.len(), 1, "a second save must update, not insert");
    assert_eq!(
        all[0].setting(CFG_BASE_URL),
        Some("https://gateway.internal/v1")
    );
    assert_eq!(all[0].description, "Through the gateway");

    let mut model = LlmModelDef::new(def.id, "gpt-5.1");
    save_llm_model(&catalog, &model).await?;
    model.description = "edited".to_owned();
    save_llm_model(&catalog, &model).await?;
    let models = list_llm_models(&catalog, &def).await?;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].description, "edited");
    Ok(())
}

#[tokio::test]
async fn a_stored_key_survives_a_read_edit_save_cycle_that_never_saw_it() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;

    let def = anthropic("main");
    save_llm_provider(&catalog, &def).await?;

    // The admin opens the form. What the API serialises is redacted, so this is
    // everything the browser could possibly know.
    let stored = load_llm_provider(&catalog, def.id).await?.expect("the row");
    let spec = provider_config_spec(&stored.backend)?;
    let shown = redact_attrs(&spec, &stored.config);
    assert_eq!(
        shown.get(CFG_API_KEY),
        Some(&serde_json::json!(SECRET_SENTINEL))
    );
    assert!(
        !serde_json::Value::Object(shown.clone())
            .to_string()
            .contains("sk-ant"),
        "no part of the key may leave, not even its prefix"
    );

    // They change the base URL and save, sending the sentinel back untouched.
    let mut submitted = shown;
    submitted.insert(CFG_BASE_URL.to_owned(), "https://proxy.internal".into());
    let saved = LlmProviderDef {
        config: merge_secrets(&spec, &stored.config, &submitted),
        ..stored.clone()
    };
    save_llm_provider(&catalog, &saved).await?;

    // The stored key is untouched and the URL changed.
    let after = load_llm_provider(&catalog, def.id).await?.expect("the row");
    assert_eq!(after.setting(CFG_API_KEY), Some("sk-ant-secret"));
    assert_eq!(after.setting(CFG_BASE_URL), Some("https://proxy.internal"));
    assert_ne!(after.setting(CFG_API_KEY), Some(SECRET_SENTINEL));
    Ok(())
}

#[tokio::test]
async fn a_name_clash_is_refused_where_the_admin_can_fix_it() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;

    save_llm_provider(&catalog, &anthropic("main")).await?;

    // A *different* definition claiming the same name.
    let clash = openai("main");
    let err = save_llm_provider(&catalog, &clash)
        .await
        .expect_err("two providers cannot share a name");
    assert!(err.to_string().contains("main"), "{err}");
    assert_eq!(list_llm_providers(&catalog).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_model_name_is_unique_per_provider_and_shared_across_providers() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let direct = openai("direct");
    let gateway = LlmProviderDef::new("gateway", OPENAI_CHAT_BACKEND)
        .with(CFG_BASE_URL, "http://gateway.internal/v1");
    save_llm_provider(&catalog, &direct).await?;
    save_llm_provider(&catalog, &gateway).await?;

    // The same model bought two ways is two rows, with different prices.
    save_llm_model(
        &catalog,
        &LlmModelDef::new(direct.id, "gpt-5.1").with(CFG_PRICE_INPUT, 1.25),
    )
    .await?;
    save_llm_model(
        &catalog,
        &LlmModelDef::new(gateway.id, "gpt-5.1").with(CFG_PRICE_INPUT, 1.5),
    )
    .await?;
    assert_eq!(list_llm_models(&catalog, &direct).await?.len(), 1);
    assert_eq!(list_llm_models(&catalog, &gateway).await?.len(), 1);

    // A second row of that name under one provider is refused, naming both.
    let err = save_llm_model(&catalog, &LlmModelDef::new(direct.id, "gpt-5.1"))
        .await
        .expect_err("one row per name per provider");
    let text = err.to_string();
    assert!(
        text.contains("direct") && text.contains("gpt-5.1"),
        "{text}"
    );

    // And the database is the authority behind the check.
    let table = catalog.require(LLM_MODELS_TABLE)?;
    assert!(
        table
            .constraints
            .iter()
            .any(|c| matches!(&c.kind, sc_catalog::ConstraintKind::Unique { fields } if fields.len() == 2)),
        "the jointly-unique key exists"
    );

    // A model for a provider that does not exist is refused.
    assert!(
        save_llm_model(&catalog, &LlmModelDef::new(LlmProviderDefId::new(), "x"))
            .await
            .is_err()
    );
    // So is a setting the provider's backend does not declare.
    assert!(
        save_llm_model(
            &catalog,
            &LlmModelDef::new(gateway.id, "y").with("native_apply_patch", "yes")
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn a_provider_has_at_most_one_default_model() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let def = anthropic("main");
    save_llm_provider(&catalog, &def).await?;

    // No default yet: an agent naming no model is told so.
    let err = require_llm_model(&catalog, &def, None)
        .await
        .expect_err("no default");
    assert!(err.to_string().contains("no default model"), "{err}");
    let err = require_llm_model(&catalog, &def, Some("claude-opus-5"))
        .await
        .expect_err("no such model");
    assert!(
        err.to_string().contains("no model named `claude-opus-5`"),
        "{err}"
    );

    let sonnet = LlmModelDef::new(def.id, "claude-sonnet-5").default_model();
    let opus = LlmModelDef::new(def.id, "claude-opus-5");
    save_llm_model(&catalog, &sonnet).await?;
    save_llm_model(&catalog, &opus).await?;
    assert_eq!(
        require_llm_model(&catalog, &def, None).await?.name,
        "claude-sonnet-5"
    );

    // Making another the default takes the flag from the first.
    save_llm_model(&catalog, &opus.clone().default_model()).await?;
    let defaults: Vec<String> = list_llm_models(&catalog, &def)
        .await?
        .into_iter()
        .filter(|m| m.is_default)
        .map(|m| m.name)
        .collect();
    assert_eq!(defaults, ["claude-opus-5"]);
    Ok(())
}

#[tokio::test]
async fn a_structurally_wrong_config_is_refused_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;

    // No API key: knowable without a network, so the save is refused rather
    // than deferred to the first chat.
    let no_key = LlmProviderDef::new("main", ANTHROPIC_BACKEND);
    let err = save_llm_provider(&catalog, &no_key)
        .await
        .expect_err("a provider with no key cannot be saved");
    assert!(err.to_string().contains(CFG_API_KEY), "{err}");

    // A backend nothing implements, likewise.
    let unknown = LlmProviderDef::new("main", "bedrock").with(CFG_API_KEY, "x");
    assert!(save_llm_provider(&catalog, &unknown).await.is_err());

    // And a nameless one.
    let nameless = anthropic("   ");
    assert!(check_provider_saveable(&catalog, &nameless).await.is_err());

    assert!(list_llm_providers(&catalog).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn deleting_a_provider_deletes_its_models_in_the_same_transaction() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;

    let alpha = anthropic("alpha");
    let zeta = anthropic("zeta");
    save_llm_provider(&catalog, &alpha).await?;
    save_llm_provider(&catalog, &zeta).await?;
    for (provider, name) in [
        (&alpha, "claude-sonnet-5"),
        (&alpha, "claude-opus-5"),
        (&zeta, "claude-sonnet-5"),
    ] {
        save_llm_model(&catalog, &LlmModelDef::new(provider.id, name)).await?;
    }
    let names: Vec<String> = list_llm_providers(&catalog)
        .await?
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, ["alpha", "zeta"]);

    assert!(delete_llm_provider(&catalog, alpha.id, &[]).await?);
    // Its models went with it; the other provider's did not.
    assert!(list_llm_models(&catalog, &alpha).await?.is_empty());
    assert_eq!(list_llm_models(&catalog, &zeta).await?.len(), 1);
    // Deleting again reports "there was nothing there" rather than failing.
    assert!(!delete_llm_provider(&catalog, alpha.id, &[]).await?);
    assert!(!delete_llm_provider(&catalog, LlmProviderDefId::new(), &[]).await?);

    let names: Vec<String> = list_llm_providers(&catalog)
        .await?
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, ["zeta"]);
    Ok(())
}

#[tokio::test]
async fn a_provider_or_model_something_still_references_is_not_deleted() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;

    let def = openai("main");
    save_llm_provider(&catalog, &def).await?;
    let model = LlmModelDef::new(def.id, "gpt-5.1");
    save_llm_model(&catalog, &model).await?;

    // An agent lives a layer above, so its reference is passed in — the same
    // arrangement `delete_file_store` uses for applications.
    let err = delete_llm_provider(&catalog, def.id, &["agent `librarian`".to_owned()])
        .await
        .expect_err("a referenced provider must not be deleted out from under it");
    assert!(err.to_string().contains("librarian"), "{err}");
    assert!(load_llm_provider(&catalog, def.id).await?.is_some());
    assert!(
        load_llm_model(&catalog, model.id).await?.is_some(),
        "the models stay too"
    );

    let err = delete_llm_model(&catalog, model.id, &["agent `librarian`".to_owned()])
        .await
        .expect_err("a referenced model must not be deleted");
    assert!(err.to_string().contains("librarian"), "{err}");

    assert!(delete_llm_model(&catalog, model.id, &[]).await?);
    assert!(!delete_llm_model(&catalog, model.id, &[]).await?);
    assert!(load_llm_model(&catalog, model.id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn a_renamed_provider_keeps_its_row_identity_and_its_models() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;

    let mut def = openai("old-name");
    save_llm_provider(&catalog, &def).await?;
    save_llm_model(&catalog, &LlmModelDef::new(def.id, "gpt-5.1")).await?;
    let id = def.id;

    def.name = "new-name".to_owned();
    save_llm_provider(&catalog, &def).await?;

    assert_eq!(
        load_llm_provider(&catalog, id)
            .await?
            .map(|d| d.name)
            .as_deref(),
        Some("new-name")
    );
    assert!(
        load_llm_provider_by_name(&catalog, "old-name")
            .await?
            .is_none()
    );
    assert_eq!(list_llm_models(&catalog, &def).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn every_backend_stores_and_reconnects() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;

    let providers = [
        openai("oai").with(CFG_BASE_URL, "http://127.0.0.1:9/v1"),
        anthropic("ant"),
        LlmProviderDef::new("local", OPENAI_CHAT_BACKEND)
            .with(CFG_BASE_URL, "http://127.0.0.1:9/v1"),
    ];
    for (provider, model) in providers
        .iter()
        .zip(["gpt-5.1", "claude-sonnet-5", "llama3.2"])
    {
        save_llm_provider(&catalog, provider).await?;
        save_llm_model(
            &catalog,
            &LlmModelDef::new(provider.id, model).default_model(),
        )
        .await?;
    }

    // What boot does: read every row and turn it back into something callable.
    for def in list_llm_providers(&catalog).await? {
        let model = require_llm_model(&catalog, &def, None).await?;
        let connected = connect_model(&def, &model)?;
        assert_eq!(connected.provider.model(), model.name);
        assert_eq!(connected.backend, def.backend);
    }
    Ok(())
}

#[tokio::test]
async fn each_kind_of_model_has_its_own_default_and_keeps_its_kind() -> Result<()> {
    // TODO-agent-skills AS3.1.
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let def = openai("main");
    save_llm_provider(&catalog, &def).await?;

    let chat = LlmModelDef::new(def.id, "gpt-5.1").default_model();
    let small = LlmModelDef::new(def.id, "text-embedding-3-small")
        .kind(ModelKind::Embedding)
        .default_model()
        .with(CFG_PRICE_INPUT, 0.02);
    let large = LlmModelDef::new(def.id, "text-embedding-3-large").kind(ModelKind::Embedding);
    for model in [&chat, &small, &large] {
        save_llm_model(&catalog, model).await?;
    }
    // The kind is stored and read back.
    let loaded = load_llm_model(&catalog, small.id).await?.expect("saved");
    assert_eq!(loaded.kind, ModelKind::Embedding);
    assert_eq!(loaded.prices().input, Some(0.02));

    // Making another embedding model the default leaves the chat default be.
    save_llm_model(&catalog, &large.clone().default_model()).await?;
    let defaults: Vec<(String, ModelKind)> = list_llm_models(&catalog, &def)
        .await?
        .into_iter()
        .filter(|m| m.is_default)
        .map(|m| (m.name, m.kind))
        .collect();
    assert_eq!(
        defaults,
        [
            ("gpt-5.1".to_owned(), ModelKind::Chat),
            ("text-embedding-3-large".to_owned(), ModelKind::Embedding),
        ]
    );

    // Each kind resolves to its own default, and a row of another kind is
    // refused by name.
    assert_eq!(
        require_llm_model(&catalog, &def, None).await?.name,
        "gpt-5.1"
    );
    assert_eq!(
        require_model_of_kind(&catalog, &def, None, ModelKind::Embedding)
            .await?
            .name,
        "text-embedding-3-large"
    );
    let err = require_llm_model(&catalog, &def, Some("text-embedding-3-small"))
        .await
        .expect_err("not chat");
    assert!(
        err.to_string()
            .contains("is an embedding model, not a chat model"),
        "{err}"
    );
    let err = require_model_of_kind(&catalog, &def, None, ModelKind::Image)
        .await
        .expect_err("no image default");
    assert!(err.to_string().contains("no default image model"), "{err}");
    // A non-chat model named with no row is used by its name, unpriced.
    let unlisted =
        require_model_of_kind(&catalog, &def, Some("dall-e-3"), ModelKind::Image).await?;
    assert_eq!(
        (unlisted.name.as_str(), unlisted.kind),
        ("dall-e-3", ModelKind::Image)
    );
    assert_eq!(unlisted.prices().per_image, None);

    // A chat model cannot connect as anything else, nor a row change kind.
    let err = connect_model(&def, &small).expect_err("not chat");
    assert!(err.to_string().contains("not a chat model"), "{err}");
    let err = save_llm_model(&catalog, &small.clone().kind(ModelKind::Speech))
        .await
        .expect_err("kind is fixed");
    assert!(err.to_string().contains("cannot become a speech"), "{err}");

    // Anthropic serves chat only.
    let claude = anthropic("claude");
    save_llm_provider(&catalog, &claude).await?;
    let err = save_llm_model(
        &catalog,
        &LlmModelDef::new(claude.id, "voyage").kind(ModelKind::Embedding),
    )
    .await
    .expect_err("anthropic has no embeddings");
    assert!(err.to_string().contains("chat models only"), "{err}");
    Ok(())
}
