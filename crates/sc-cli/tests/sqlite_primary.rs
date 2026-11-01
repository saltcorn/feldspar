//! A whole Saltcorn installation whose **primary database is a SQLite file**.
//!
//! The same boot path the binary takes — [`DbConfig`] → [`connect_catalog`]
//! (connect, introspect, bootstrap every `_fd_*` table) — with a file where the
//! Postgres URL usually is. That is the claim `--sqlite` makes, and it is not a
//! claim about the driver alone: the bootstrap creates tables with uuid primary
//! keys that fill themselves in, json columns and unique constraints, and every
//! one of those is a place where a backend that is only *nearly* supported
//! stops working.
//!
//! No harness and no server here: the point is that there is nothing to set up
//! but a path.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use sc_catalog::DataField;
use sc_cli::{DbConfig, connect_catalog};
use sc_query::{Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};

/// A temporary directory, removed when the test ends.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> TempDir {
        let path = std::env::temp_dir().join(format!("sc-sqlite-primary-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

#[tokio::test]
async fn a_sqlite_file_is_a_whole_installation() -> sc_error::Result<()> {
    let dir = TempDir::new();
    let path = dir.0.join("feldspar.sqlite");

    // --- boot ----------------------------------------------------------------
    // The file does not exist yet: naming it is the installation.
    assert!(!path.exists());
    let cfg = DbConfig::from_sqlite(path.display().to_string());
    assert!(cfg.target().contains("feldspar.sqlite"));
    let catalog = connect_catalog(&cfg).await?;
    assert!(path.is_file(), "serving a SQLite file creates it");

    // Every bootstrap ran: the users table, the applications table, the file
    // stores and the connections. Each is a `CREATE TABLE` with a uuid or an
    // identity key and a json column, so this is the real test of the DDL.
    assert!(catalog.get(sc_auth::USERS_TABLE)?.is_some());
    assert!(catalog.get(sc_app::APPLICATIONS_TABLE)?.is_some());
    assert!(catalog.get(sc_catalog::FILE_STORES_TABLE)?.is_some());
    assert!(catalog.get(sc_catalog::DB_CONNECTIONS_TABLE)?.is_some());

    // The backend says what it cannot do, and the layers above read this rather
    // than assuming Postgres: no policies, so authorization is enforced above
    // the database.
    assert!(!catalog.primary().capabilities().row_level_security);
    assert!(catalog.primary().capabilities().returning);

    // --- a table an admin would make ----------------------------------------
    catalog
        .create_table(
            "book",
            &[
                // A key that numbers itself, which is what the schema editor
                // gives an integer primary key — and on SQLite means the rowid
                // alias rather than a sequence.
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(sc_db::ColumnGenerator::Identity),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)).required(),
                DataField::plain("tags", TypeRef::Basic(BasicType::Json)),
            ],
        )
        .await?;
    catalog.reload().await?;

    let table = catalog.require("book")?;
    assert_eq!(table.database, sc_catalog::DbId::primary());
    let key = table.field("id").expect("the key field");
    assert!(key.primary_key);

    // …written and read through the ordinary paths, with the key filling itself
    // in — the thing that makes a form able to insert at all.
    let provider = catalog.provider(&table, sc_catalog::Unchecked::system("test"))?;
    let inserted = provider
        .write(&Statement::from(
            Insert::row(
                "book",
                vec!["title".into(), "tags".into()],
                vec![
                    Expr::lit("Orlando"),
                    Expr::Lit(Value::Json(serde_json::json!(["fiction"]))),
                ],
            )
            .returning(vec![Projection::expr(Expr::col("id"))]),
        ))
        .await?
        .try_collect()
        .await?;
    assert_eq!(inserted.len(), 1);
    assert!(matches!(inserted[0].get("id"), Some(Value::Int(_))));

    let read = provider
        .query(&Select::from(Source::table("book")))
        .await?
        .try_collect()
        .await?;
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].get("title"), Some(&Value::Text("Orlando".into())));
    assert_eq!(
        read[0].get("tags"),
        Some(&Value::Json(serde_json::json!(["fiction"])))
    );

    // --- and it is still there when the process restarts ---------------------
    drop(catalog);
    let catalog = connect_catalog(&cfg).await?;
    let read = catalog
        .provider(
            &catalog.require("book")?,
            sc_catalog::Unchecked::system("test"),
        )?
        .query(&Select::from(Source::table("book")))
        .await?
        .try_collect()
        .await?;
    assert_eq!(read.len(), 1, "the file is the database");
    Ok(())
}
