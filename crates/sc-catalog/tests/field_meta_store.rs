//! Phase 3.1 integration test: a field's overlay row round-tripping through
//! `_fd_fields`, against a real database (design §9).
//!
//! The field-level twin of `table_meta_store.rs`, and it asserts the two things
//! that are new here: a `File` field's kind parameters (store, folder, MIME list)
//! survive the trip out through the sparse `attributes` bag and back into the
//! structured kind, and a `kind` the model does not know is refused **by name**
//! rather than loaded as some default. Plus the overlay invariants shared with
//! `_fd_tables`: one row per field (enforced by the composite key), an overlay
//! outliving its column, and a delete that removes the row and nothing else.

use std::sync::Arc;

use sc_catalog::{
    Catalog, DataField, DataFieldKind, FIELD_META_TABLE, FieldMeta, FieldMetaId, FileStoreId,
    bootstrap_field_meta, delete_field_meta, list_field_meta, list_field_meta_for_table,
    load_field_meta, load_field_meta_by_field, save_field_meta,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::{Repr, Result};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::json;

/// A catalog over a per-test database that has never seen Saltcorn.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}

/// Create a plain `books` table for the field overlays to overlay.
async fn create_books(cat: &Catalog) -> Result<()> {
    cat.create_table(
        "books",
        &[
            DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
                .required()
                .primary_key(),
            DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            DataField::plain("cover", TypeRef::Basic(BasicType::Text)),
        ],
    )
    .await?;
    Ok(())
}

/// A `File` overlay for `books.cover` exercising every kind parameter.
fn cover_meta() -> FieldMeta {
    FieldMeta::new("books", "cover")
        .label("Cover image")
        .description("The book's cover")
        .kind(DataFieldKind::File {
            store: FileStoreId("uploads".into()),
            folder: Some("covers".into()),
            mime_allow: vec!["image/png".into(), "image/jpeg".into()],
            on_delete: Default::default(),
            on_conflict: Default::default(),
        })
}

#[tokio::test]
async fn a_file_field_overlay_round_trips_through_its_row() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // A database with no Saltcorn tables grows this one with no migration step.
    assert!(cat.get(FIELD_META_TABLE)?.is_none());
    let table = bootstrap_field_meta(&cat).await?;
    assert_eq!(table.name, FIELD_META_TABLE);
    // Idempotent.
    assert_eq!(bootstrap_field_meta(&cat).await?.name, table.name);

    create_books(&cat).await?;
    let meta = cover_meta();
    save_field_meta(&cat, &meta).await?;

    // Identical in every field — the label, the description, and (the point of
    // this phase) the File kind's store, folder and MIME list, reconstructed out
    // of the attributes bag.
    let loaded = load_field_meta(&cat, meta.id)
        .await?
        .expect("the overlay should load by id");
    assert_eq!(loaded, meta);
    assert_eq!(
        loaded.kind,
        DataFieldKind::File {
            store: FileStoreId("uploads".into()),
            folder: Some("covers".into()),
            mime_allow: vec!["image/png".into(), "image/jpeg".into()],
            on_delete: Default::default(),
            on_conflict: Default::default(),
        }
    );

    // And by (table, field), which is the lookup the merge (§3.2) will perform.
    assert_eq!(
        load_field_meta_by_field(&cat, "books", "cover").await?,
        Some(meta)
    );
    assert_eq!(load_field_meta_by_field(&cat, "books", "nope").await?, None);
    Ok(())
}

#[tokio::test]
async fn a_rich_type_and_its_attributes_round_trip() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;
    create_books(&cat).await?;

    // A plain field given a rich type by name plus its own attributes — kept
    // whole, since a plain field reserves no attribute keys.
    let mut meta = FieldMeta::new("books", "title").rich_type("string");
    meta.attributes.insert("max_length".into(), json!(120));
    meta.attributes
        .insert("regex".into(), json!("^[A-Za-z ]+$"));
    save_field_meta(&cat, &meta).await?;

    let loaded = load_field_meta_by_field(&cat, "books", "title")
        .await?
        .expect("loads");
    assert_eq!(loaded, meta);
    assert_eq!(loaded.type_name.as_deref(), Some("string"));
    assert_eq!(loaded.attributes.get("max_length"), Some(&json!(120)));
    assert_eq!(loaded.kind, DataFieldKind::Plain);
    Ok(())
}

#[tokio::test]
async fn string_options_given_as_text_are_stored_as_an_array() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;
    create_books(&cat).await?;

    let mut meta = FieldMeta::new("books", "title").rich_type("string");
    meta.attributes
        .insert("options".into(), json!("Paperback, Hard cover"));
    save_field_meta(&cat, &meta).await?;
    let loaded = load_field_meta_by_field(&cat, "books", "title")
        .await?
        .expect("the overlay should load");
    assert_eq!(
        loaded.attributes.get("options"),
        Some(&json!(["Paperback", "Hard cover"]))
    );

    // The quoted form, too; and text that cannot be read refuses the save.
    meta.attributes
        .insert("options".into(), json!(r#""Paperback", "Hard, cover""#));
    save_field_meta(&cat, &meta).await?;
    let loaded = load_field_meta_by_field(&cat, "books", "title")
        .await?
        .expect("the overlay should load");
    assert_eq!(
        loaded.attributes.get("options"),
        Some(&json!(["Paperback", "Hard, cover"]))
    );
    meta.attributes
        .insert("options".into(), json!(r#""Paperback"#));
    let err = save_field_meta(&cat, &meta).await.unwrap_err().to_string();
    assert!(
        err.contains("books.title") && err.contains("options"),
        "{err}"
    );
    Ok(())
}

#[tokio::test]
async fn saving_again_updates_in_place_rather_than_inserting() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;
    create_books(&cat).await?;

    let mut meta = cover_meta();
    save_field_meta(&cat, &meta).await?;

    // Tighten the MIME list and relabel — an edit, not a second row.
    meta.label = "Jacket".to_owned();
    meta.kind = DataFieldKind::File {
        store: FileStoreId("uploads".into()),
        folder: Some("covers".into()),
        mime_allow: vec!["image/png".into()],
        on_delete: Default::default(),
        on_conflict: Default::default(),
    };
    save_field_meta(&cat, &meta).await?;

    let all = list_field_meta(&cat).await?;
    assert_eq!(
        all.len(),
        1,
        "the id already existed, so this was an update"
    );
    assert_eq!(all[0].label, "Jacket");
    assert_eq!(
        all[0].kind,
        DataFieldKind::File {
            store: FileStoreId("uploads".into()),
            folder: Some("covers".into()),
            mime_allow: vec!["image/png".into()],
            on_delete: Default::default(),
            on_conflict: Default::default(),
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_field_cannot_have_two_overlays() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;
    create_books(&cat).await?;

    save_field_meta(&cat, &cover_meta()).await?;

    // A different row (different id) claiming the same (table, field): refused,
    // naming the field. The composite primary key on the pair is what makes this
    // a real database-level guarantee, not just an application check.
    let clash = FieldMeta::new("books", "cover").rich_type("string");
    let err = save_field_meta(&cat, &clash).await.unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(
        err.to_string().contains("books.cover"),
        "should name it: {err}"
    );

    // The original is untouched — still a File field.
    let stored = load_field_meta_by_field(&cat, "books", "cover")
        .await?
        .unwrap();
    assert!(matches!(stored.kind, DataFieldKind::File { .. }));
    Ok(())
}

#[tokio::test]
async fn an_ill_typed_kind_is_rejected_by_name_on_read() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;

    // A row inserted behind the API with a kind the model does not know — what a
    // hand-edited database or a newer Saltcorn's dump can contain. The strict
    // read refuses it by name rather than loading it as some default.
    db.client()
        .await?
        .execute(
            "INSERT INTO _fd_fields \
               (id, table_name, name, label, description, \"type\", kind, attributes) \
             VALUES (gen_random_uuid(), 'books', 'cover', '', '', NULL, 'blob', '{}'::jsonb)",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let err = list_field_meta(&cat).await.unwrap_err();
    let message = err.to_string();
    assert!(message.contains("blob"), "names the bad kind: {message}");
    assert!(
        message.contains("books.cover"),
        "names the field: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn a_system_table_field_is_not_configurable_and_a_nameless_overlay_is_refused() -> Result<()>
{
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;

    let err = save_field_meta(&cat, &FieldMeta::new(FIELD_META_TABLE, "kind"))
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");
    assert!(err.to_string().contains(FIELD_META_TABLE), "{err}");

    let err = save_field_meta(&cat, &FieldMeta::new("books", "  "))
        .await
        .unwrap_err();
    assert!(matches!(err.repr(), Repr::Invalid(_)), "{err}");

    assert!(list_field_meta(&cat).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn an_overlay_outlives_the_column_it_overlays() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;
    create_books(&cat).await?;
    save_field_meta(&cat, &cover_meta()).await?;

    // Drop the column underneath the row. The row is kept (a restore or an
    // out-of-band migration does exactly this); §3.2 is where it becomes a
    // reported inconsistency.
    db.client()
        .await?
        .execute("ALTER TABLE books DROP COLUMN cover", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    cat.reload().await?;

    let stored = load_field_meta_by_field(&cat, "books", "cover").await?;
    assert!(
        stored.is_some(),
        "the overlay survives its column being dropped"
    );
    Ok(())
}

#[tokio::test]
async fn listing_is_ordered_and_scoped_by_table() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;
    create_books(&cat).await?;
    cat.create_table(
        "authors",
        &[DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key()],
    )
    .await?;

    save_field_meta(&cat, &FieldMeta::new("books", "title")).await?;
    save_field_meta(&cat, &FieldMeta::new("books", "cover")).await?;
    save_field_meta(&cat, &FieldMeta::new("authors", "id")).await?;

    // Global listing: by table then field.
    let all: Vec<(String, String)> = list_field_meta(&cat)
        .await?
        .into_iter()
        .map(|m| (m.table_name, m.field_name))
        .collect();
    assert_eq!(
        all,
        [
            ("authors".to_owned(), "id".to_owned()),
            ("books".to_owned(), "cover".to_owned()),
            ("books".to_owned(), "title".to_owned()),
        ]
    );

    // Per-table listing: only that table's fields, by field name.
    let books: Vec<String> = list_field_meta_for_table(&cat, "books")
        .await?
        .into_iter()
        .map(|m| m.field_name)
        .collect();
    assert_eq!(books, ["cover", "title"]);
    Ok(())
}

#[tokio::test]
async fn deleting_an_overlay_removes_the_row_and_not_the_column() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_field_meta(&cat).await?;
    create_books(&cat).await?;

    let meta = cover_meta();
    save_field_meta(&cat, &meta).await?;

    assert!(delete_field_meta(&cat, meta.id).await?);
    assert_eq!(load_field_meta(&cat, meta.id).await?, None);
    assert!(list_field_meta(&cat).await?.is_empty());

    // The column itself is exactly where it was.
    cat.reload().await?;
    let books = cat.require("books")?;
    assert!(books.fields.iter().any(|f| f.base.name == "cover"));

    // Deleting again reports that nothing was there.
    assert!(!delete_field_meta(&cat, FieldMetaId::new()).await?);
    Ok(())
}
