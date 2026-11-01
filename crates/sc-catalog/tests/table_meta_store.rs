//! Phase 1.1 integration test: a table's overlay row round-tripping through
//! `_fd_tables`, against a real database (design §9).
//!
//! An overlay is not like the stored objects that came before it. A file store
//! or an application *is* its row; a table exists whether or not it has one. So
//! what is asserted here is what makes that distinction true in the storage
//! layer: what is saved comes back identical, a table cannot have two overlays,
//! a role that is not on the 1–100 scale is refused rather than clamped, an
//! overlay outlives its table instead of vanishing with it, and deleting an
//! overlay removes the configuration and nothing else.

use sc_types::Role;
use std::sync::Arc;

use sc_catalog::{
    AccessRules, Catalog, DataField, TABLE_META_TABLE, TableMeta, TableMetaId,
    bootstrap_table_meta, delete_table_meta, list_table_meta, load_table_meta,
    load_table_meta_by_name, orphan_table_meta, save_table_meta,
};
use sc_db::{DatabaseDriver, SchemaChange};
use sc_db_postgres::PgDriver;
use sc_error::{Repr, Result};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

/// A catalog over a per-test database that has never seen Saltcorn.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

/// Create a plain table for an overlay to overlay.
async fn create_books(cat: &Catalog) -> Result<()> {
    cat.create_table(
        "books",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
                .required()
                .primary_key(),
            DataField::plain("title", TypeRef::Basic(BasicType::Text)),
        ],
    )
    .await?;
    Ok(())
}

/// An overlay exercising every column: both §9 required extras (description,
/// attributes), a label, and a read/write role pair that is not the default.
fn books_meta() -> TableMeta {
    let mut meta = TableMeta::new("books")
        .label("Books")
        .description("The library catalogue")
        .access(Role::of(80), Role::of(40));
    meta.attributes.insert("icon".into(), "book".into());
    meta
}

#[tokio::test]
async fn an_overlay_round_trips_through_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // A database with no Saltcorn tables grows this one with no migration step.
    assert!(cat.get(TABLE_META_TABLE)?.is_none());
    let table = bootstrap_table_meta(&cat).await?;
    assert_eq!(table.name, TABLE_META_TABLE);
    // Idempotent: a second call returns the existing table.
    assert_eq!(bootstrap_table_meta(&cat).await?.name, table.name);

    create_books(&cat).await?;
    let meta = books_meta();
    save_table_meta(&cat, &meta).await?;

    // Identical in every field, including the ones a lazy mapping would drop:
    // the label, the description, the sparse attributes, and — the point of the
    // whole phase — both roles, separately.
    let loaded = load_table_meta(&cat, meta.id)
        .await?
        .expect("the overlay should load by id");
    assert_eq!(loaded, meta);
    assert_eq!(
        loaded.access,
        AccessRules {
            min_role_read: Role::of(80),
            min_role_write: Role::of(40),
        }
    );
    assert_eq!(
        loaded.attributes.get("icon").and_then(|v| v.as_str()),
        Some("book")
    );

    // And by table name, which is the lookup the merge (§1.2) will perform.
    assert_eq!(load_table_meta_by_name(&cat, "books").await?, Some(meta));
    assert_eq!(load_table_meta_by_name(&cat, "nope").await?, None);
    Ok(())
}

#[tokio::test]
async fn saving_again_updates_in_place_rather_than_inserting() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;
    create_books(&cat).await?;

    let mut meta = books_meta();
    save_table_meta(&cat, &meta).await?;

    // Narrow the table back to admin-only writes — the edit the whole phase
    // exists to make possible.
    meta.access.min_role_write = Role::ADMIN;
    meta.label = "Library".to_owned();
    save_table_meta(&cat, &meta).await?;

    let all = list_table_meta(&cat).await?;
    assert_eq!(
        all.len(),
        1,
        "the id already existed, so this was an update"
    );
    assert_eq!(all[0].access.min_role_write, Role::ADMIN);
    assert_eq!(
        all[0].access.min_role_read,
        Role::of(80),
        "unchanged by the edit"
    );
    assert_eq!(all[0].label, "Library");
    Ok(())
}

#[tokio::test]
async fn a_table_cannot_have_two_overlays() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;
    create_books(&cat).await?;

    save_table_meta(&cat, &books_meta()).await?;

    // A *different* row (different id) claiming the same table is refused:
    // the merge joins on the name, and two rows would leave no rule saying
    // whose access applies.
    let clash = TableMeta::new("books").access(Role::PUBLIC, Role::PUBLIC);
    let err = save_table_meta(&cat, &clash).await.unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(err.to_string().contains("books"), "should name it: {err}");

    // The original is untouched — in particular it did not become public.
    let stored = load_table_meta_by_name(&cat, "books").await?.unwrap();
    assert_eq!(stored.access.min_role_read, Role::of(80));
    Ok(())
}

#[test]
fn a_role_off_the_scale_cannot_be_constructed() {
    // 0 is not "more admin than admin" and 101 is not "more public than
    // public": the scale is fixed, so an out-of-range role is a mistake to
    // report, not a value to round to the nearest legal one — rounding would
    // silently decide who can reach the data. `Role` makes such an
    // `AccessRules` unrepresentable; reading one back from a corrupt row is
    // refused in `table_meta`'s unit tests.
    assert!(Role::new(0).is_none());
    assert!(Role::new(101).is_none());
}

#[tokio::test]
async fn a_system_table_is_not_configurable_and_a_nameless_overlay_is_not_savable() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;

    // `_fd_*` tables are hidden from users (§9); their access is not the
    // admin's to widen, so the row is refused rather than merged-and-ignored.
    let err = save_table_meta(&cat, &TableMeta::new(TABLE_META_TABLE))
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(err.to_string().contains(TABLE_META_TABLE), "{err}");

    let err = save_table_meta(&cat, &TableMeta::new("   "))
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");

    assert!(list_table_meta(&cat).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn an_overlay_outlives_the_table_it_overlays_and_says_so() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;
    create_books(&cat).await?;
    save_table_meta(&cat, &books_meta()).await?;
    assert!(orphan_table_meta(&cat).await?.is_empty());

    // Drop the table underneath the row — a restore, or a migration run outside
    // Saltcorn, does exactly this.
    cat.primary(sc_catalog::Unchecked::system("test"))
        .apply_schema(&SchemaChange::DropTable {
            name: "books".to_owned(),
            if_exists: false,
        })
        .await?;
    cat.reload().await?;

    // The row survives: recreating the table must restore its access rules
    // rather than silently leaving it admin-only with nothing saying why.
    let orphans = orphan_table_meta(&cat).await?;
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].table_name, "books");
    assert_eq!(orphans[0].access.min_role_read, Role::of(80));

    // Recreating the table clears the orphan without a save.
    create_books(&cat).await?;
    assert!(orphan_table_meta(&cat).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn listing_is_ordered_by_table_name() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;

    for name in ["orders", "authors", "books"] {
        save_table_meta(&cat, &TableMeta::new(name)).await?;
    }
    let names: Vec<String> = list_table_meta(&cat)
        .await?
        .into_iter()
        .map(|m| m.table_name)
        .collect();
    assert_eq!(names, ["authors", "books", "orders"]);
    Ok(())
}

#[tokio::test]
async fn deleting_an_overlay_removes_the_configuration_and_not_the_table() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;
    create_books(&cat).await?;

    let meta = books_meta();
    save_table_meta(&cat, &meta).await?;

    assert!(delete_table_meta(&cat, meta.id).await?);
    assert_eq!(load_table_meta(&cat, meta.id).await?, None);
    assert!(list_table_meta(&cat).await?.is_empty());

    // The table itself — and its rows — are exactly where they were. "Forget
    // what I configured" is not "drop the table".
    cat.reload().await?;
    let books = cat.require("books")?;
    assert_eq!(books.fields.len(), 2);

    // Deleting again is not an error — it reports that nothing was there.
    assert!(!delete_table_meta(&cat, TableMetaId::new()).await?);
    Ok(())
}
