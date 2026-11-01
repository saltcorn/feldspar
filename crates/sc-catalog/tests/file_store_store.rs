//! Phase 1.1 integration test: a file-store definition round-tripping through
//! its `_fd_file_stores` row, against a real database (design §9, §14.1).
//!
//! A file store, like an application, exists *only* as stored configuration —
//! there is nothing to introspect it from. So the things asserted here are the
//! things that make that claim true: what is saved comes back identical, the
//! name everything resolves through cannot be claimed twice, a database that has
//! never seen Saltcorn grows the table without a migration step, and a delete
//! removes the definition without touching a byte of the data.

use sc_types::Role;
use std::sync::Arc;

use sc_catalog::{
    Catalog, DataField, DataFieldKind, FieldMeta, FileStoreId, bootstrap_field_meta,
    bootstrap_file_stores, delete_field_meta, delete_file_store, file_store_field_references,
    list_file_stores, load_file_store, load_file_store_by_name, save_field_meta, save_file_store,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Repr, Result};
use sc_files::{CFG_PATH, FileStoreDef, LOCAL_BACKEND};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};

/// A catalog over a per-test database that has never seen Saltcorn.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

/// A definition exercising every column: both §9 required extras (description,
/// attributes), a backend with settings, and a role floor.
fn docs_store() -> FileStoreDef {
    let mut def = FileStoreDef::local("docs", "/srv/docs")
        .description("Shared documents")
        .min_role(Role::of(40));
    def.attributes.insert("owner".into(), "facilities".into());
    def
}

#[tokio::test]
async fn a_definition_round_trips_through_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // A database with no Saltcorn tables grows this one with no migration step.
    assert!(cat.get(sc_catalog::FILE_STORES_TABLE)?.is_none());
    let table = bootstrap_file_stores(&cat).await?;
    assert_eq!(table.name, sc_catalog::FILE_STORES_TABLE);
    // Idempotent: a second call returns the existing table.
    assert_eq!(bootstrap_file_stores(&cat).await?.name, table.name);

    let def = docs_store();
    save_file_store(&cat, &def).await?;

    // Identical in every field, including the ones a lazy mapping would drop:
    // the description, the sparse attributes, the role floor and the settings.
    let loaded = load_file_store(&cat, def.id)
        .await?
        .expect("the definition should load by id");
    assert_eq!(loaded, def);
    assert_eq!(loaded.setting(CFG_PATH), Some("/srv/docs"));
    assert_eq!(loaded.min_role, Some(Role::of(40)));
    assert_eq!(loaded.backend, LOCAL_BACKEND);
    assert_eq!(
        loaded.attributes.get("owner").and_then(|v| v.as_str()),
        Some("facilities")
    );

    // And by name, which is the lookup everything referencing a store uses.
    assert_eq!(load_file_store_by_name(&cat, "docs").await?, Some(def));
    assert_eq!(load_file_store_by_name(&cat, "nope").await?, None);
    Ok(())
}

#[tokio::test]
async fn saving_again_updates_in_place_rather_than_inserting() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;

    let mut def = docs_store();
    save_file_store(&cat, &def).await?;

    // Re-point the store at a new path — the edit the MVP could not express
    // without changing the command line and restarting.
    def.config.insert(CFG_PATH.into(), "/mnt/docs".into());
    def.min_role = None;
    save_file_store(&cat, &def).await?;

    let all = list_file_stores(&cat).await?;
    assert_eq!(
        all.len(),
        1,
        "the id already existed, so this was an update"
    );
    assert_eq!(all[0].setting(CFG_PATH), Some("/mnt/docs"));
    // Clearing the floor round-trips as NULL, not as a stale 40.
    assert_eq!(all[0].min_role, None);
    Ok(())
}

#[tokio::test]
async fn the_name_cannot_be_claimed_twice() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;

    save_file_store(&cat, &FileStoreDef::local("apps", "/srv/apps")).await?;

    // A *different* definition (different id) claiming the same name is
    // refused: everything resolves a store by name, so two rows holding one
    // name is not a state the system can serve.
    let clash = FileStoreDef::local("apps", "/somewhere/else");
    let err = save_file_store(&cat, &clash).await.unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(err.to_string().contains("apps"), "should name it: {err}");

    // The original is untouched.
    let stored = load_file_store_by_name(&cat, "apps").await?.unwrap();
    assert_eq!(stored.setting(CFG_PATH), Some("/srv/apps"));
    Ok(())
}

#[tokio::test]
async fn a_store_needs_a_name_and_a_backend() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;

    let err = save_file_store(&cat, &FileStoreDef::local("   ", "/srv/x"))
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");

    let err = save_file_store(&cat, &FileStoreDef::new("apps", ""))
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    Ok(())
}

#[tokio::test]
async fn listing_is_ordered_by_name() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;

    for name in ["uploads", "apps", "docs"] {
        save_file_store(&cat, &FileStoreDef::local(name, format!("/srv/{name}"))).await?;
    }
    let names: Vec<String> = list_file_stores(&cat)
        .await?
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, ["apps", "docs", "uploads"]);
    Ok(())
}

#[tokio::test]
async fn deleting_removes_the_definition_and_reports_whether_one_existed() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;

    let def = FileStoreDef::local("scratch", "/tmp/scratch");
    save_file_store(&cat, &def).await?;

    assert!(delete_file_store(&cat, def.id, &[]).await?);
    assert_eq!(load_file_store(&cat, def.id).await?, None);
    assert!(list_file_stores(&cat).await?.is_empty());

    // Deleting again is not an error — it reports that nothing was there.
    assert!(!delete_file_store(&cat, def.id, &[]).await?);
    Ok(())
}

/// The successor to the milestone's tripwire. Its ancestor asserted that a
/// `File` field's store reference was *inert* — modelled but stored nowhere, so
/// the catalog-level delete check found nothing. The `_fd_fields` overlay (§3.1)
/// and its merge (§3.2) landed, so the reference is now real, and this asserts
/// the opposite: the catalog-level check **finds** it and the delete is refused,
/// naming the field.
#[tokio::test]
async fn a_file_field_reference_is_caught_by_the_catalog_delete_check() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;
    bootstrap_field_meta(&cat).await?;

    let def = FileStoreDef::local("uploads", "/srv/uploads");
    save_file_store(&cat, &def).await?;

    // A plain `attachment` text column…
    cat.create_table(
        "documents",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
                .required()
                .primary_key(),
            DataField::plain("attachment", TypeRef::Basic(BasicType::Text)),
        ],
    )
    .await?;

    // …made a `File` field pointing at `uploads` by an overlay row. This is the
    // only way a column becomes a File field (§3.2), and it is what the ancestor
    // test could not do.
    let meta = FieldMeta::new("documents", "attachment").kind(DataFieldKind::File {
        store: FileStoreId("uploads".to_owned()),
        folder: None,
        mime_allow: Vec::new(),
        on_delete: Default::default(),
        on_conflict: Default::default(),
    });
    save_field_meta(&cat, &meta).await?;

    // The merged column is now a File field…
    let documents = cat.require("documents")?;
    let attachment = documents
        .fields
        .iter()
        .find(|f| f.base.name == "attachment")
        .expect("the column exists");
    assert!(matches!(attachment.kind, DataFieldKind::File { .. }));

    // …so the catalog-level check finds the reference, and the delete is refused,
    // naming the field. The tripwire has flipped.
    assert_eq!(
        file_store_field_references(&cat, "uploads")?,
        ["documents.attachment"]
    );
    let err = delete_file_store(&cat, def.id, &[]).await.unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(
        err.to_string().contains("documents.attachment"),
        "the refusal names the field: {err}"
    );

    // Forgetting the overlay frees the store to be deleted — the reference was
    // the overlay's, and nothing else held it.
    delete_field_meta(&cat, meta.id).await?;
    assert!(file_store_field_references(&cat, "uploads")?.is_empty());
    assert!(delete_file_store(&cat, def.id, &[]).await?);
    Ok(())
}

/// The catalog-level check, exercised where it *is* live: it reports references
/// from whatever fields the catalog actually holds, and does not confuse one
/// store's name for another's.
#[tokio::test]
async fn field_references_are_scoped_to_the_named_store() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;

    cat.create_table(
        "documents",
        &[DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key()],
    )
    .await?;

    // No File fields anywhere, so no store is referenced — including the
    // `_fd_file_stores` table's own columns, which must not be mistaken for one.
    assert!(file_store_field_references(&cat, "uploads")?.is_empty());
    assert!(file_store_field_references(&cat, "docs")?.is_empty());
    Ok(())
}

#[tokio::test]
async fn references_this_crate_cannot_see_are_passed_in() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;

    let def = FileStoreDef::local("apps", "/srv/apps");
    save_file_store(&cat, &def).await?;

    // `sc-catalog` cannot see applications — they live a crate above it — so a
    // caller that knows about them supplies those referents. Without them the
    // delete would wrongly succeed, which is exactly why the admin API composes
    // both halves.
    let err = delete_file_store(&cat, def.id, &["application `My Blog`".to_owned()])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("My Blog"), "{err}");
    assert!(load_file_store(&cat, def.id).await?.is_some());
    Ok(())
}

/// TODO §1.2: the backend's settings are checked against its declared spec **on
/// save**, so a mis-configured store is rejected where the admin can fix it
/// rather than becoming a store that silently never connects.
#[tokio::test]
async fn a_misconfigured_backend_is_rejected_on_save() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_file_stores(&cat).await?;

    // A `local` store with no `path` at all.
    let err = save_file_store(&cat, &FileStoreDef::new("docs", LOCAL_BACKEND))
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(err.to_string().contains(CFG_PATH), "{err}");

    // A backend nothing implements.
    let err = save_file_store(
        &cat,
        &FileStoreDef::new("docs", "s3").with("bucket", "things"),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("s3"), "{err}");

    // Neither reached the table.
    assert!(list_file_stores(&cat).await?.is_empty());

    // But a well-formed definition whose directory does not exist *is* saved:
    // reachability is not a save-time question, and a store whose disk was
    // unmounted has to stay editable — that is how the admin repoints it.
    let unreachable = FileStoreDef::local("docs", "/definitely/not/here");
    save_file_store(&cat, &unreachable).await?;
    assert_eq!(list_file_stores(&cat).await?.len(), 1);

    Ok(())
}
