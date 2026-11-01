//! Phase 3.5 integration test: a `File` field does real work, against a real
//! Postgres database and a real local file store (design §3.5).
//!
//! This is the milestone's second claim made observable: a column configured as
//! a `File` field validates what is written to it — the path must resolve to a
//! connected store, live under the field's folder, and carry an allowed MIME type
//! — and a store a field points at cannot be deleted out from under it. And the
//! rule files always follow: deleting the *field*, or dropping its table, never
//! touches a byte on disk.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sc_access::rows;
use sc_catalog::{
    Catalog, DataField, DataFieldKind, FieldMeta, FileStoreId, bootstrap_field_meta,
    bootstrap_file_stores, connect_all_file_stores, delete_field_meta, delete_file_store,
    load_file_store_by_name, save_field_meta, save_file_store,
};
use sc_db::{DatabaseDriver, SchemaChange};
use sc_db_postgres::PgDriver;
use sc_error::{Repr, Result};
use sc_files::FileStoreDef;
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::json;

/// A catalog with the file-store and field overlays bootstrapped.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let cat = Catalog::init(driver as Arc<dyn DatabaseDriver>).await?;
    bootstrap_file_stores(&cat).await?;
    bootstrap_field_meta(&cat).await?;
    Ok(cat)
}

/// A fresh directory that exists, with a `covers/` subdir, and its path string.
fn temp_store_dir(tag: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!(
        "sc-filefield-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("covers")).unwrap();
    let path = dir.to_string_lossy().into_owned();
    (dir, path)
}

/// Build a `documents` table with a `cover` column, made a `File` field
/// (store `uploads`, folder `covers`, PNG only) by an overlay. Returns the merged
/// table and the overlay id.
async fn documents_with_cover(
    cat: &Catalog,
) -> Result<(sc_catalog::Table, sc_catalog::FieldMetaId)> {
    cat.create_table(
        "documents",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
                .required()
                .primary_key(),
            DataField::plain("cover", TypeRef::Basic(BasicType::Text)),
        ],
    )
    .await?;

    let meta = FieldMeta::new("documents", "cover").kind(DataFieldKind::File {
        store: FileStoreId("uploads".to_owned()),
        folder: Some("covers".to_owned()),
        mime_allow: vec!["image/png".to_owned()],
        on_delete: Default::default(),
        on_conflict: Default::default(),
    });
    save_field_meta(cat, &meta).await?;
    Ok((cat.require("documents")?, meta.id))
}

#[tokio::test]
async fn a_file_field_validates_store_folder_and_mime_on_write() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let (dir, path) = temp_store_dir("write");
    save_file_store(&cat, &FileStoreDef::local("uploads", &path)).await?;
    connect_all_file_stores(&cat).await?;

    let (table, _) = documents_with_cover(&cat).await?;

    // A valid path: under the folder, a PNG, in a connected store.
    let ok = rows::create_row(
        &cat,
        &table,
        &json!({ "id": "00000000-0000-0000-0000-000000000001", "cover": "covers/a.png" }),
        sc_catalog::Unchecked::system("test"),
    )
    .await?;
    assert_eq!(ok["cover"], json!("covers/a.png"));

    // Outside the field's folder.
    let outside = rows::create_row(
        &cat,
        &table,
        &json!({ "cover": "elsewhere/a.png" }),
        sc_catalog::Unchecked::system("test"),
    )
    .await
    .expect_err("outside the folder");
    assert!(matches!(outside.repr(), Repr::Invalid(_)), "{outside:?}");
    assert!(
        outside.to_string().contains("cover"),
        "names the field: {outside}"
    );
    assert!(
        outside.to_string().contains("covers"),
        "names the folder: {outside}"
    );

    // A disallowed MIME type (a GIF where only PNG is allowed).
    let wrong_mime = rows::create_row(
        &cat,
        &table,
        &json!({ "cover": "covers/a.gif" }),
        sc_catalog::Unchecked::system("test"),
    )
    .await
    .expect_err("disallowed mime");
    assert!(
        matches!(wrong_mime.repr(), Repr::Invalid(_)),
        "{wrong_mime:?}"
    );
    assert!(wrong_mime.to_string().contains("cover"), "{wrong_mime}");
    assert!(
        wrong_mime.to_string().contains("image/gif"),
        "names the offending type: {wrong_mime}"
    );

    // Only the valid row was written.
    let listed = rows::list_rows(&cat, &table, sc_catalog::Unchecked::system("test")).await?;
    assert_eq!(listed.as_array().map_or(0, Vec::len), 1);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn a_reference_into_an_unresolvable_store_is_refused_naming_it() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // The overlay points `cover` at a store that was never saved or connected, so
    // it does not resolve — a reference into nothing.
    cat.create_table(
        "documents",
        &[DataField::plain("cover", TypeRef::Basic(BasicType::Text))],
    )
    .await?;
    save_field_meta(
        &cat,
        &FieldMeta::new("documents", "cover").kind(DataFieldKind::File {
            store: FileStoreId("ghost".to_owned()),
            folder: None,
            mime_allow: Vec::new(),
            on_delete: Default::default(),
            on_conflict: Default::default(),
        }),
    )
    .await?;
    let table = cat.require("documents")?;

    let err = rows::create_row(
        &cat,
        &table,
        &json!({ "cover": "a.png" }),
        sc_catalog::Unchecked::system("test"),
    )
    .await
    .expect_err("the store does not resolve");
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("ghost"), "names the store: {err}");
    assert!(err.to_string().contains("cover"), "names the field: {err}");
    Ok(())
}

#[tokio::test]
async fn a_store_cannot_be_deleted_while_a_field_points_at_it() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let (dir, path) = temp_store_dir("delete");
    save_file_store(&cat, &FileStoreDef::local("uploads", &path)).await?;
    connect_all_file_stores(&cat).await?;
    let (_, meta_id) = documents_with_cover(&cat).await?;

    let def = load_file_store_by_name(&cat, "uploads").await?.unwrap();
    let err = delete_file_store(&cat, def.id, &[])
        .await
        .expect_err("a field still points at the store");
    assert!(
        err.to_string().contains("documents.cover"),
        "names the field: {err}"
    );

    // Forget the field overlay and the store is free to delete.
    delete_field_meta(&cat, meta_id).await?;
    assert!(delete_file_store(&cat, def.id, &[]).await?);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

#[tokio::test]
async fn deleting_a_field_or_dropping_its_table_leaves_the_bytes_alone() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    let (dir, path) = temp_store_dir("bytes");
    // A real file the field could reference.
    let file = dir.join("covers").join("a.png");
    std::fs::write(&file, b"\x89PNG fake").unwrap();

    save_file_store(&cat, &FileStoreDef::local("uploads", &path)).await?;
    connect_all_file_stores(&cat).await?;
    let (_, meta_id) = documents_with_cover(&cat).await?;

    // Forgetting the File field's overlay removes the reference, not the file.
    delete_field_meta(&cat, meta_id).await?;
    assert!(file.exists(), "forgetting the field left the file in place");

    // Dropping the whole table likewise touches no bytes in the store.
    cat.primary(sc_catalog::Unchecked::system("test"))
        .apply_schema(&SchemaChange::DropTable {
            name: "documents".to_owned(),
            if_exists: false,
        })
        .await?;
    cat.reload().await?;
    assert!(file.exists(), "dropping the table left the file in place");

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
