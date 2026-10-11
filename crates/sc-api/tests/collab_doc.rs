//! A `collab_doc` field (TODO.md "Live updates" L5.1) on SQLite: the column is
//! left out of every read — the row has no key for it, not a null — and out of
//! the generated row type; every API write naming it is refused with a sentence
//! saying why; a filter, an ordering and a formula cannot name it; and the one
//! writer that may, the document persister (`rows::write_document_in`), writes
//! it and its text mirror as a system update.

use std::sync::Arc;

use sc_api::{ApiProvider, RestProvider, rows};
use sc_catalog::{
    CallerContext, Catalog, DataField, FieldMeta, TableMeta, bootstrap_field_meta,
    bootstrap_table_meta, save_field_meta, save_table_meta,
};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_query::Value;
use sc_types::{BasicType, COLLAB_DOC, TypeRef};
use serde_json::json;

/// `notes(id, title, body collab_doc, body_text)` on an in-memory SQLite.
async fn notes() -> Result<Arc<Catalog>> {
    let driver: Arc<dyn DatabaseDriver> = Arc::new(SqliteDriver::open_in_memory()?);
    let catalog = Arc::new(Catalog::init(driver).await?);
    bootstrap_table_meta(&catalog).await?;
    bootstrap_field_meta(&catalog).await?;
    catalog
        .create_table(
            "notes",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
                DataField::plain("body", TypeRef::Basic(BasicType::Bytes)),
                DataField::plain("body_text", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    catalog.reload().await?;
    save_field_meta(
        &catalog,
        &FieldMeta::new("notes", "body").rich_type(COLLAB_DOC),
    )
    .await?;
    Ok(catalog)
}

fn refused_as_a_document(err: sc_error::Error) {
    let text = err.to_string();
    assert!(text.contains("collaborative document"), "{text}");
}

#[tokio::test]
async fn a_document_field_is_never_read_and_never_written_through_an_api() -> Result<()> {
    let catalog = notes().await?;
    let table = catalog.require("notes")?;
    let body = table.field("body").unwrap();
    assert!(body.is_collab_doc(), "the overlay made it a document");

    // Written: every API path refuses it, whoever asks.
    let admin = CallerContext::new(1, None);
    refused_as_a_document(
        rows::create_row_ctx(
            &catalog,
            &table,
            &json!({"title": "x", "body": [1, 2]}),
            Some(&admin),
        )
        .await
        .unwrap_err(),
    );
    refused_as_a_document(
        rows::create_row(&catalog, &table, &json!({"body": null}))
            .await
            .unwrap_err(),
    );
    let row = rows::create_row(&catalog, &table, &json!({"title": "Plans"})).await?;
    // The insert's own answer has no `body` key at all.
    assert!(row.get("body").is_none(), "{row}");
    assert_eq!(row["title"], "Plans");
    let id = row["id"].to_string();
    refused_as_a_document(
        rows::update_row_ctx(&catalog, &table, &id, &json!({"body": [0]}), Some(&admin))
            .await
            .unwrap_err(),
    );

    // The persister writes it, and the mirror beside it.
    let written = rows::write_document_in(
        &catalog,
        &table,
        &id,
        "body",
        &[1, 2, 3],
        Some(("body_text", "hello")),
        &rows::Executor::Pooled,
    )
    .await?;
    assert!(written.get("body").is_none(), "{written}");
    assert_eq!(written["body_text"], "hello");
    assert_eq!(
        rows::read_field(&catalog, &table, "body", &id).await?,
        Value::Bytes(vec![1, 2, 3])
    );
    // …and only a document column.
    assert!(
        rows::write_document_in(
            &catalog,
            &table,
            &id,
            "title",
            &[1],
            None,
            &rows::Executor::Pooled
        )
        .await
        .is_err()
    );

    // Read: absent from every row, not null.
    let listed = rows::list_rows(&catalog, &table).await?;
    let first = &listed.as_array().unwrap()[0];
    assert!(first.get("body").is_none(), "{first}");
    assert_eq!(first["body_text"], "hello");

    // Filtered or ordered on: refused, saying what it is.
    let fields: Vec<String> = table.fields.iter().map(|f| f.base.name.clone()).collect();
    refused_as_a_document(
        sc_api::filter::queryable_field(&table, &fields, "body", "where").unwrap_err(),
    );
    Ok(())
}

#[tokio::test]
async fn the_generated_row_type_has_no_document_field() -> Result<()> {
    let catalog = notes().await?;
    let table = catalog.require("notes")?;
    let provider = RestProvider::project("/api", &[table]);
    let client = sc_api::generate_client(ApiProvider::endpoints(&provider));
    let start = client
        .find("export interface NotesRow {")
        .expect("a row type");
    let row_type = &client[start..start + client[start..].find('}').unwrap()];
    assert!(
        row_type.contains("body_text"),
        "the mirror is a column like any other"
    );
    assert!(
        !row_type
            .lines()
            .map(str::trim_start)
            .any(|l| l.starts_with("body:") || l.starts_with("body?:")),
        "no `body` in the row type:\n{row_type}"
    );
    Ok(())
}

#[tokio::test]
async fn a_formula_cannot_read_a_document() -> Result<()> {
    let catalog = notes().await?;
    // `body` is not a field a formula knows, so this formula reads nothing
    // real, and is refused or left inactive with the reason.
    let mut meta = TableMeta::new("notes");
    meta.set_ownership_formula(Some("body !== null"));
    let saved = save_table_meta(&catalog, &meta).await;
    let table = catalog.require("notes")?;
    match saved {
        Err(e) => assert!(e.to_string().contains("body"), "{e}"),
        Ok(_) => {
            assert!(table.ownership.is_none(), "the formula must not be live");
            let why = table.ownership_error.clone().unwrap_or_default();
            assert!(why.contains("body"), "{why}");
        }
    }
    // While a formula over the other fields is fine.
    meta.set_ownership_formula(Some("title !== null"));
    save_table_meta(&catalog, &meta).await?;
    assert!(catalog.require("notes")?.ownership.is_some());
    Ok(())
}
