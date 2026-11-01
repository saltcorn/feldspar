//! The services a layer above installs on a catalog are one value
//! (TODO-code-quality Q5.1), and the schema a reload builds is another
//! (Q5.2):
//!
//! - teardown empties every service, the typed extensions included, so a
//!   service that holds the catalog no longer keeps it alive;
//! - an extension is read back as the type it was stored as;
//! - a schema handed out is immutable: a reload swaps in a new one with the
//!   next generation, and the one already handed out still describes the
//!   reload that built it.

use std::sync::Arc;

use async_trait::async_trait;
use sc_catalog::{Catalog, DataField, TableEvents, TableWrite, WriteOp};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

/// A service that holds the catalog it is installed on, as the trigger
/// dispatcher and the mount registry do.
struct HoldsCatalog(#[allow(dead_code)] Arc<Catalog>);

#[async_trait]
impl TableEvents for HoldsCatalog {
    fn observes(&self, _table: &str, _op: WriteOp) -> bool {
        true
    }

    async fn emit(&self, _catalog: &Catalog, _write: TableWrite<'_>) -> Result<()> {
        Ok(())
    }
}

/// An extension that holds the catalog too.
struct ExtensionHoldingCatalog(#[allow(dead_code)] Arc<Catalog>);

#[derive(Debug, PartialEq)]
struct Marker(&'static str);

async fn catalog(db: &TestDb) -> Result<Arc<Catalog>> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver).await?);
    db.on_teardown(catalog.teardown_hook());
    Ok(catalog)
}

#[tokio::test]
async fn teardown_empties_every_service_and_frees_a_catalog_they_hold() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    // Ours, and the teardown hook the harness holds.
    let held_by_the_test = Arc::strong_count(&catalog);

    let listener = Arc::new(HoldsCatalog(catalog.clone()));
    catalog.add_table_events("holder", listener)?;
    catalog.set_extension(Some(Arc::new(ExtensionHoldingCatalog(catalog.clone()))));
    catalog.set_extension(Some(Arc::new(Marker("kept until teardown"))));
    assert_eq!(Arc::strong_count(&catalog), held_by_the_test + 2);
    assert!(catalog.observes_writes("anything", WriteOp::Insert));
    assert_eq!(
        catalog.extension::<Marker>().as_deref(),
        Some(&Marker("kept until teardown"))
    );

    catalog.teardown();
    assert!(!catalog.observes_writes("anything", WriteOp::Insert));
    assert!(catalog.extension::<Marker>().is_none());
    assert!(catalog.extension::<ExtensionHoldingCatalog>().is_none());
    assert!(catalog.module_functions().is_none());
    assert!(catalog.model_host().is_none());
    assert!(catalog.admin_host().is_none());
    assert_eq!(
        Arc::strong_count(&catalog),
        held_by_the_test,
        "a torn-down catalog is held by nothing it had installed"
    );
    Ok(())
}

#[tokio::test]
async fn an_extension_is_replaced_and_removed_by_its_type() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    catalog.set_extension(Some(Arc::new(Marker("first"))));
    catalog.set_extension(Some(Arc::new(Marker("second"))));
    assert_eq!(
        catalog.extension::<Marker>().as_deref(),
        Some(&Marker("second"))
    );
    catalog.set_extension::<Marker>(None);
    assert!(catalog.extension::<Marker>().is_none());
    Ok(())
}

#[tokio::test]
async fn a_schema_handed_out_is_not_changed_by_a_later_reload() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let before = catalog.schema();
    assert_eq!(before.generation(), catalog.generation());
    assert!(before.get("cards").is_none());

    catalog
        .create_table(
            "cards",
            &[DataField::plain("title", TypeRef::Basic(BasicType::Text))],
        )
        .await?;

    let after = catalog.schema();
    assert!(after.generation() > before.generation());
    assert_eq!(after.generation(), catalog.generation());
    assert!(after.get("cards").is_some());
    assert!(after.tables().iter().any(|t| t.name == "cards"));
    // The value handed out before is the reload that built it, whole.
    assert!(before.get("cards").is_none());
    assert!(!before.tables().iter().any(|t| t.name == "cards"));
    Ok(())
}
