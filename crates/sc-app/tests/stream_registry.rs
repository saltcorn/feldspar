//! An app's streams are typed against the provider registry of the server its
//! catalog belongs to, not the last one installed in the process.
//!
//! A test binary runs many servers at once, each installing its own registry;
//! when the process-wide one was all `app_streams` read, a test's scripted
//! provider could vanish from under its scaffold because a neighbour booted.

use std::sync::Arc;

use sc_app::{
    ApiConfig, Application, FrameworkRef, StreamRef, app_streams, install_stream_registry,
};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_stream::testing::ScriptedProvider;
use sc_stream::{
    ElementField, ElementType, Internal, Stream, StreamProvider, StreamRegistry, bootstrap_streams,
    save_stream,
};
use sc_test_harness::TestDb;
use sc_types::BasicType;

async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_streams(&catalog).await?;
    Ok(catalog)
}

fn registry(providers: Vec<Arc<dyn StreamProvider>>) -> Arc<StreamRegistry> {
    let mut registry = StreamRegistry::new();
    for provider in providers {
        registry.register(provider).unwrap();
    }
    Arc::new(registry)
}

#[tokio::test]
async fn an_app_is_typed_against_its_own_servers_registry() -> Result<()> {
    let (ours_db, theirs_db) = (TestDb::new().await?, TestDb::new().await?);
    let (ours, theirs) = (catalog(&ours_db).await?, catalog(&theirs_db).await?);

    let scripted = ScriptedProvider::new(
        "scripted",
        ElementType::json([ElementField::new("temperature", BasicType::Float).required()]),
    );
    let our_registry = registry(vec![Arc::new(scripted), Arc::new(Internal)]);
    install_stream_registry(&ours, our_registry.clone());
    save_stream(&ours, &our_registry, &Stream::new("boiler", "scripted")).await?;

    // Another server in the same process boots after ours, with no `scripted`.
    install_stream_registry(&theirs, registry(vec![Arc::new(Internal)]));

    let app = Application::new("Todo", "todo", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_stream(StreamRef::new("boiler"));
    let exposed = app_streams(&app, &ours).await?;
    assert_eq!(exposed.len(), 1);
    assert_eq!(exposed[0].stream.provider, "scripted");
    assert_eq!(exposed[0].element_type.keys().len(), 1);
    Ok(())
}
