//! A **provided** table, end to end inside the catalog (design §8.3).
//!
//! A `_fd_tables` row carrying a provider is not an overlay — it is the table's
//! only definition — so what this asserts is the whole of that claim against a
//! real database: the row makes a table appear, its columns are the host's
//! answer, `Catalog::provider` serves its rows, and nothing above the trait knows
//! the rows never came from the database it is connected to.
//!
//! A **fake** host, for the reason `module_functions.rs` gives: what is worth
//! asserting here is the catalog's half — that the definition round-trips, that
//! the query reaches the provider in v1's own vocabulary, and that a host which
//! is not there leaves a table an admin can still see and fix. The module
//! worker's half is `sc-module`'s `deno_host` suite, and the two halves together
//! are the `rss` test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_catalog::{
    Attrs, Catalog, DataField, ProvidedTableDef, ProvidedWrites, TableMeta, TableProviderHost,
    TableProviderKind, TableSource, bootstrap_table_meta, save_table_meta,
};
use sc_db_postgres::PgDriver;
use sc_error::{Error, Result};
use sc_query::{BinOp, Expr, OrderBy, Select, Source, Value};
use sc_test_harness::TestDb;
use sc_types::{BasicType, FormField, TypeRef};
use serde_json::{Value as Json, json};

/// A host supplying one provider, `RSS feed` of `@saltcorn/rss`, which answers a
/// fixed three-item feed and records what it was asked for.
struct FakeFeed {
    asked: Mutex<Vec<(Json, Json)>>,
    /// Whether `fields(cfg)` works, so the "the module is down" path has a way
    /// in.
    healthy: bool,
}

impl FakeFeed {
    fn new(healthy: bool) -> Arc<FakeFeed> {
        Arc::new(FakeFeed {
            asked: Mutex::new(Vec::new()),
            healthy,
        })
    }
}

#[async_trait]
impl TableProviderHost for FakeFeed {
    fn providers(&self) -> Vec<TableProviderKind> {
        vec![TableProviderKind {
            module: "@saltcorn/rss".to_owned(),
            provider: "RSS feed".to_owned(),
            config_spec: vec![
                FormField::new("url", TypeRef::Basic(BasicType::Text)).label("Feed URL"),
            ],
        }]
    }

    async fn fields(
        &self,
        _module: &str,
        _provider: &str,
        config: &Json,
    ) -> Result<Vec<DataField>> {
        if !self.healthy {
            return Err(Error::invalid("the feed could not be read"));
        }
        // The columns depend on the configuration, which is v1's second shape
        // and the one that makes "ask on every reload" worth doing.
        let mut fields = vec![
            DataField::plain("title", TypeRef::Basic(BasicType::Text)).label("Title"),
            DataField::plain("link", TypeRef::Basic(BasicType::Text)).label("Link"),
        ];
        if config.get("with_votes").and_then(Json::as_bool) == Some(true) {
            fields.push(DataField::plain("votes", TypeRef::Basic(BasicType::Int)));
        }
        Ok(fields)
    }

    async fn rows(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
        filter: &Json,
        options: &Json,
    ) -> Result<Vec<Json>> {
        self.asked
            .lock()
            .unwrap()
            .push((filter.clone(), options.clone()));
        // Deliberately ignoring both, exactly as `@saltcorn/rss` does: a
        // provider is allowed to answer the whole feed whatever it was asked,
        // and the catalog has to give the right answer anyway.
        Ok(vec![
            json!({ "title": "beta", "link": "/b", "votes": 2, "author": "not a column" }),
            json!({ "title": "alpha", "link": "/a", "votes": 10 }),
            json!({ "title": "gamma", "link": "/c" }),
        ])
    }

    /// A feed is read-only, which is what `@saltcorn/rss` is: `get_table`
    /// answers `getRows` and nothing else.
    async fn writes(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
    ) -> Result<ProvidedWrites> {
        Ok(ProvidedWrites::NONE)
    }

    async fn insert(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
        _record: &Json,
    ) -> Result<Json> {
        panic!("a read-only provider must be refused before the host is reached")
    }

    async fn update(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
        _id: &Json,
        _record: &Json,
    ) -> Result<()> {
        panic!("a read-only provider must be refused before the host is reached")
    }

    async fn delete(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
        _filter: &Json,
    ) -> Result<()> {
        panic!("a read-only provider must be refused before the host is reached")
    }
}

/// A catalog over a throwaway database with `_fd_tables` bootstrapped.
async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver).await?;
    bootstrap_table_meta(&catalog).await?;
    Ok(catalog)
}

/// Write the definition row for a provided table called `headlines`.
async fn define_headlines(catalog: &Catalog, configuration: Attrs) -> Result<()> {
    let mut meta = TableMeta::new("headlines").label("Headlines");
    meta.set_provider(Some(
        &ProvidedTableDef::new("@saltcorn/rss", "RSS feed").configuration(configuration),
    ));
    save_table_meta(catalog, &meta).await
}

#[tokio::test]
async fn a_row_with_a_provider_is_a_table_the_database_never_had() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let host = FakeFeed::new(true);
    catalog.set_table_providers(Arc::clone(&host) as Arc<dyn TableProviderHost>)?;

    let mut configuration = Attrs::new();
    configuration.insert("url".into(), Json::String("https://example.org/f".into()));
    configuration.insert("with_votes".into(), Json::Bool(true));
    define_headlines(&catalog, configuration).await?;

    // The table exists because the row does, and it is not in any database's
    // `information_schema`.
    let table = catalog.require("headlines")?;
    assert_eq!(
        table.source,
        TableSource::Provider {
            module: "@saltcorn/rss".into(),
            provider: "RSS feed".into(),
            writes: ProvidedWrites::NONE,
        }
    );
    assert_eq!(table.provider(), Some(("@saltcorn/rss", "RSS feed")));
    // Its columns are the host's answer to *this* configuration — three, because
    // `with_votes` is on.
    let columns: Vec<&str> = table.fields.iter().map(|f| f.base.name.as_str()).collect();
    assert_eq!(columns, ["title", "link", "votes"]);
    assert_eq!(table.fields[0].base.label, "Title");
    // And the rest of the row is an overlay like any other table's.
    assert_eq!(table.label, "Headlines");
    assert!(table.overlay.is_some());
    assert!(catalog.provided_table_issues().is_empty());
    Ok(())
}

#[tokio::test]
async fn the_catalogs_provider_serves_its_rows_through_the_ordinary_read_path() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    let host = FakeFeed::new(true);
    catalog.set_table_providers(Arc::clone(&host) as Arc<dyn TableProviderHost>)?;
    let mut configuration = Attrs::new();
    configuration.insert("with_votes".into(), Json::Bool(true));
    define_headlines(&catalog, configuration).await?;
    let table = catalog.require("headlines")?;

    // A plain `SELECT *`, exactly as `rows.rs` builds one.
    let rows = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .query(&Select::from(Source::table("headlines")))
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 3);
    // Only declared columns: the provider answered an `author` and there is no
    // column for it.
    // …in the order the table declares them, which is what a database answers a
    // `SELECT *` with.
    assert_eq!(rows[0].columns(), ["title", "link", "votes"]);
    assert_eq!(rows[0].get("title"), Some(&Value::Text("beta".into())));

    // Now a filtered, ordered, bounded read — which the provider ignores
    // entirely, so this is the catalog's own answer.
    let mut select = Select::from(Source::table("headlines"));
    select.filter = Some(Expr::binary(
        BinOp::Ge,
        Expr::col("votes"),
        Expr::lit(2_i64),
    ));
    select.order = vec![OrderBy::desc(Expr::col("votes"))];
    select.limit = Some(1);
    let rows = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .query(&select)
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("title"), Some(&Value::Text("alpha".into())));

    // And the provider was *offered* the query in v1's own vocabulary, so one
    // that can do the work in its own backend can.
    let asked = host.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0], (json!({}), json!({})));
    assert_eq!(
        asked[1],
        (
            json!({ "votes": { "gt": 2, "equal": true } }),
            json!({ "orderBy": "votes", "orderDesc": true, "limit": 1 })
        )
    );
    Ok(())
}

/// A provider that answers no write method is read-only, and the refusal names
/// the method a module author would have to add rather than talking about
/// drivers or about this version of Saltcorn.
#[tokio::test]
async fn a_read_only_provider_refuses_a_write_and_says_which_method_is_missing() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    catalog.set_table_providers(FakeFeed::new(true) as Arc<dyn TableProviderHost>)?;
    define_headlines(&catalog, Attrs::new()).await?;
    let table = catalog.require("headlines")?;

    let insert = sc_query::Statement::Insert(Box::new(sc_query::Insert::row(
        "headlines",
        vec!["title".to_owned()],
        vec![Expr::lit("new")],
    )));
    let Err(err) = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .write(&insert)
        .await
    else {
        panic!("a provided table must refuse a write");
    };
    let err = err.to_string();
    assert!(err.contains("RSS feed"), "{err}");
    assert!(err.contains("@saltcorn/rss"), "{err}");
    assert!(err.contains("read-only"), "{err}");
    assert!(err.contains("insertRow"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_module_that_cannot_answer_leaves_a_table_with_no_columns_and_a_reason() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    catalog.set_table_providers(FakeFeed::new(false) as Arc<dyn TableProviderHost>)?;
    define_headlines(&catalog, Attrs::new()).await?;

    // Still in the catalog, still in the list, still editable — which is the
    // point, because the admin UI is the only place it can be fixed from.
    let table = catalog.require("headlines")?;
    assert!(table.fields.is_empty());
    let issues = catalog.provided_table_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(issues[0].table, "headlines");
    assert!(issues[0].problem.contains("RSS feed"), "{issues:?}");
    assert!(
        issues[0].problem.contains("could not be read"),
        "{issues:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_process_with_no_module_host_still_lists_the_table_and_says_why() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    // No host installed at all — a build tool, a test, a server started without
    // modules.
    define_headlines(&catalog, Attrs::new()).await?;

    let table = catalog.require("headlines")?;
    assert!(table.fields.is_empty());
    assert!(
        catalog.provided_table_issues()[0]
            .problem
            .contains("no module host"),
        "{:?}",
        catalog.provided_table_issues()
    );
    // And a read of it says the same thing rather than reaching for a driver.
    let Err(err) = catalog.provider(&table, sc_catalog::Unchecked::system("test")) else {
        panic!("a provided table needs a module host to be served at all");
    };
    let err = err.to_string();
    assert!(err.contains("no module host"), "{err}");
    Ok(())
}

#[tokio::test]
async fn the_database_wins_a_name_a_provided_table_also_claims() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = catalog(&db).await?;
    catalog.set_table_providers(FakeFeed::new(true) as Arc<dyn TableProviderHost>)?;
    catalog
        .create_table(
            "headlines",
            &[DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key()],
        )
        .await?;
    define_headlines(&catalog, Attrs::new()).await?;

    // The rows that are already there are somebody's data: a definition row must
    // not be able to repoint every read of that name at a feed.
    let table = catalog.require("headlines")?;
    assert_eq!(table.source, TableSource::Database);
    let issues = catalog.provided_table_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(issues[0].problem.contains("already exists"), "{issues:?}");
    Ok(())
}
