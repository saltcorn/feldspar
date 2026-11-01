//! Phase 1.2 integration test: the `_fd_tables` overlay merged onto
//! introspection, against a real database (design §9).
//!
//! The merge has one job and one prohibition. The job: a table an admin has
//! configured comes out of the catalog carrying that configuration, and keeps
//! carrying it across reloads, without a restart. The prohibition: **a table
//! nobody has configured must come out exactly as it did before this feature
//! existed** — that is the zero-setup promise, and it is the thing an overlay
//! can quietly break. So the first test here compares an unconfigured table to
//! `Table::from_physical` in full rather than spot-checking a field, and the
//! rest check the ways an overlay could reach a table it has no business
//! reaching: one whose table is gone, and a system table.

use sc_types::Role;
use std::sync::Arc;

use sc_catalog::{
    AccessRules, Catalog, DataField, TABLE_META_TABLE, Table, TableMeta, bootstrap_table_meta,
    delete_table_meta, save_table_meta,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{Expr, Insert, Statement, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use uuid::Uuid;

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

/// The table as introspection alone would produce it — the "before this feature
/// existed" reference the merge must not disturb.
async fn from_introspection(cat: &Catalog, name: &str) -> Result<Table> {
    let physicals = cat
        .primary(sc_catalog::Unchecked::system("test"))
        .introspect()
        .await?;
    let physical = physicals
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| sc_error::Error::not_found(format!("table `{name}` should exist")))?;
    Ok(Table::from_physical(sc_catalog::DbId::primary(), physical))
}

#[tokio::test]
async fn a_table_with_no_overlay_is_exactly_what_introspection_yields() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;
    create_books(&cat).await?;

    // Field by field, not a spot check: any divergence here is the overlay
    // changing a database nobody has configured, which is the one thing it must
    // never do.
    let books = cat.require("books")?;
    assert_eq!(books, from_introspection(&cat, "books").await?);
    assert_eq!(books.access, AccessRules::default());
    assert_eq!(
        books.label, "books",
        "the name is the label until told otherwise"
    );
    assert_eq!(books.description, "");
    assert_eq!(books.overlay, None, "nobody has configured this table");
    Ok(())
}

#[tokio::test]
async fn an_overlay_reaches_the_cached_table_and_survives_a_reload() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;
    create_books(&cat).await?;

    let meta = TableMeta::new("books")
        .label("Books")
        .description("The library catalogue")
        .access(Role::of(80), Role::of(40));
    save_table_meta(&cat, &meta).await?;

    // No explicit reload: saving does it, because the access rules on the
    // request path are read from this cached value. Without that, an admin
    // would set a role, see it save, and watch the server enforce the old one.
    let books = cat.require("books")?;
    assert_eq!(
        books.access,
        AccessRules {
            min_role_read: Role::of(80),
            min_role_write: Role::of(40),
        }
    );
    assert_eq!(books.label, "Books");
    assert_eq!(books.description, "The library catalogue");
    assert_eq!(books.overlay, Some(meta.id));

    // And it is not an artefact of the save path: a fresh reload — and a fresh
    // catalog, which is what a restart is — reads it back from the row.
    cat.reload().await?;
    assert_eq!(cat.require("books")?.access.min_role_read, Role::of(80));

    let restarted = catalog(&db).await?;
    let books = restarted.require("books")?;
    assert_eq!(books.access.min_role_read, Role::of(80));
    assert_eq!(books.overlay, Some(meta.id));

    // The database still knows what the database knows: the overlay added
    // access and labels and touched nothing about the columns.
    assert_eq!(
        books.fields,
        from_introspection(&cat, "books").await?.fields
    );
    assert_eq!(books.primary_key, vec!["id".to_owned()]);
    Ok(())
}

#[tokio::test]
async fn deleting_the_overlay_returns_the_table_to_admin_only() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;
    create_books(&cat).await?;

    let meta = TableMeta::new("books").access(Role::PUBLIC, Role::PUBLIC);
    save_table_meta(&cat, &meta).await?;
    assert_eq!(cat.require("books")?.access.min_role_read, Role::PUBLIC);

    // "Forget what I configured" takes effect now, not at the next restart —
    // and what it reverts to is the closed default, not the last value.
    delete_table_meta(&cat, meta.id).await?;
    let books = cat.require("books")?;
    assert_eq!(books.access, AccessRules::default());
    assert_eq!(books.overlay, None);
    assert_eq!(books.label, "books");
    Ok(())
}

#[tokio::test]
async fn an_orphan_overlay_merges_onto_nothing_rather_than_inventing_a_table() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;

    // A row for a table that does not exist — a restore or an external
    // migration leaves exactly this. It is kept (§1.1), so the merge has to
    // cope with it on every reload.
    save_table_meta(
        &cat,
        &TableMeta::new("ghosts").access(Role::PUBLIC, Role::PUBLIC),
    )
    .await?;

    assert!(
        cat.get("ghosts")?.is_none(),
        "introspection is what makes a table exist; a row must not conjure one"
    );
    cat.reload().await?;
    assert!(cat.get("ghosts")?.is_none());

    // And when the table does arrive, the configuration is waiting for it.
    cat.create_table(
        "ghosts",
        &[DataField::plain("id", TypeRef::Basic(BasicType::Uuid))
            .required()
            .primary_key()],
    )
    .await?;
    assert_eq!(cat.require("ghosts")?.access.min_role_read, Role::PUBLIC);
    Ok(())
}

#[tokio::test]
async fn a_system_table_ignores_an_overlay_inserted_behind_the_api() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;
    bootstrap_table_meta(&cat).await?;

    // `save_table_meta` refuses to write this row, so the only way to have one
    // is to insert it directly — which is exactly what a hand-edited database
    // or a restored dump can contain. `_fd_*` tables are hidden from users (§9)
    // and their access is nobody's to widen, so the merge ignores it.
    let insert = Insert::row(
        TABLE_META_TABLE,
        vec![
            "id".to_owned(),
            "name".to_owned(),
            "label".to_owned(),
            "description".to_owned(),
            "min_role_read".to_owned(),
            "min_role_write".to_owned(),
            "attributes".to_owned(),
        ],
        vec![
            Expr::lit(Uuid::new_v4()),
            Expr::lit(TABLE_META_TABLE),
            Expr::lit("Everything"),
            Expr::lit(""),
            Expr::Lit(Value::Int(100)),
            Expr::Lit(Value::Int(100)),
            Expr::Lit(Value::Json(serde_json::json!({}))),
        ],
    );
    cat.primary(sc_catalog::Unchecked::system("test"))
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    cat.reload().await?;

    let system = cat.require(TABLE_META_TABLE)?;
    assert_eq!(
        system.access,
        AccessRules::default(),
        "a system table stays admin-only whatever a row claims"
    );
    assert_eq!(system.label, TABLE_META_TABLE);
    assert_eq!(system.overlay, None);
    Ok(())
}

#[tokio::test]
async fn a_database_without_the_overlay_table_still_loads() -> Result<()> {
    let db = TestDb::new().await?;
    let cat = catalog(&db).await?;

    // No bootstrap: this is a legacy database, and the merge must not query a
    // table that is not there. It is also the state `bootstrap_table_meta`
    // itself runs in — it creates the table *through* `create_table`, which
    // reloads — so a reload that assumed the table existed could never
    // bootstrap it.
    create_books(&cat).await?;
    assert!(cat.get(TABLE_META_TABLE)?.is_none());
    assert_eq!(cat.require("books")?.access, AccessRules::default());

    bootstrap_table_meta(&cat).await?;
    assert!(cat.get(TABLE_META_TABLE)?.is_some());
    assert_eq!(cat.require("books")?.access, AccessRules::default());
    Ok(())
}
