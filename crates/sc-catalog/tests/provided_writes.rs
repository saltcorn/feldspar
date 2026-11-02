//! Writing a **provided** table: the narrowing from a `Statement` into v1's
//! `insertRow`/`updateRow`/`deleteRows` (design §8.3).
//!
//! The read path interprets a `Select` over whatever JSON a provider answered,
//! because a provider is allowed to ignore the query. The write path has the
//! opposite problem: v1's three methods are **not a query language**. `insertRow`
//! takes a record, `updateRow` takes a record and one primary key, `deleteRows`
//! takes a v1 `where` object — so an `UPDATE … WHERE votes > 5` has to become a
//! list of keys before it can be sent anywhere, and a `RETURNING *` has to be
//! read back because neither method answers a row.
//!
//! The fake host here is a **writable, in-memory table** rather than a feed: it
//! applies what it is told and answers what it holds, so what is asserted is the
//! catalog's translation and not a mock's script. It records every call in v1's
//! own vocabulary, which is the other half of the claim — a real module receives
//! exactly what `@saltcorn/postgres-tables` receives from v1.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_catalog::{
    Catalog, DataField, ProvidedTableDef, ProvidedWrites, TableMeta, TableProviderHost,
    TableProviderKind, bootstrap_table_meta, save_table_meta,
};
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, Update, Value,
};
use sc_test_harness::TestDb;
use sc_types::Attrs;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value as Json, json};

const MODULE: &str = "@saltcorn/postgres-tables";
const PROVIDER: &str = "PostgreSQL remote table";

/// One call, as the module saw it.
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Rows { filter: Json, options: Json },
    Insert(Json),
    Update { id: Json, record: Json },
    Delete(Json),
}

/// A provider standing in for `@saltcorn/postgres-tables`: it holds rows, it
/// applies writes to them, and — like that plugin — it **honours** the `where`
/// object it is handed rather than ignoring it.
///
/// `read_only` is that plugin's own flag, and it works the way v1 works: the
/// three methods are simply not offered, which is what `writes` reports.
struct RemoteTable {
    rows: Mutex<Vec<Json>>,
    calls: Mutex<Vec<Call>>,
    next_id: Mutex<i64>,
    read_only: bool,
    /// Whether the fixture reports a primary key at all — the "there is no way
    /// to address a row" refusal needs a table without one.
    keyed: bool,
}

impl RemoteTable {
    fn new(read_only: bool) -> Arc<RemoteTable> {
        Arc::new(RemoteTable {
            rows: Mutex::new(vec![
                json!({ "id": 1, "title": "alpha", "votes": 10 }),
                json!({ "id": 2, "title": "beta", "votes": 2 }),
                json!({ "id": 3, "title": "gamma", "votes": 7 }),
            ]),
            calls: Mutex::new(Vec::new()),
            next_id: Mutex::new(4),
            read_only,
            keyed: true,
        })
    }

    fn keyless() -> Arc<RemoteTable> {
        Arc::new(RemoteTable {
            rows: Mutex::new(vec![json!({ "title": "alpha", "votes": 10 })]),
            calls: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
            read_only: false,
            keyed: false,
        })
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn stored(&self) -> Vec<Json> {
        self.rows.lock().unwrap().clone()
    }

    /// v1's `{ col: { in: [...] } }`, which is the only `where` this milestone's
    /// delete sends — honoured here so that a delete really is narrowed to the
    /// rows named.
    fn matches(row: &Json, filter: &Json) -> bool {
        let Some(object) = filter.as_object() else {
            return true;
        };
        object.iter().all(|(column, condition)| {
            let value = row.get(column).unwrap_or(&Json::Null);
            match condition.get("in").and_then(Json::as_array) {
                Some(list) => list.contains(value),
                None => condition == value,
            }
        })
    }
}

#[async_trait]
impl TableProviderHost for RemoteTable {
    fn providers(&self) -> Vec<TableProviderKind> {
        vec![TableProviderKind {
            module: MODULE.to_owned(),
            provider: PROVIDER.to_owned(),
            config_spec: Vec::new(),
        }]
    }

    async fn fields(
        &self,
        _module: &str,
        _provider: &str,
        _config: &Json,
    ) -> Result<Vec<DataField>> {
        let id = DataField::plain("id", TypeRef::Basic(BasicType::Int));
        Ok(vec![
            if self.keyed { id.primary_key() } else { id },
            DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            DataField::plain("votes", TypeRef::Basic(BasicType::Int)),
        ])
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
        self.calls.lock().unwrap().push(Call::Rows {
            filter: filter.clone(),
            options: options.clone(),
        });
        Ok(self.stored())
    }

    async fn writes(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
    ) -> Result<ProvidedWrites> {
        Ok(match self.read_only {
            true => ProvidedWrites::NONE,
            false => ProvidedWrites::ALL,
        })
    }

    async fn insert(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
        record: &Json,
    ) -> Result<Json> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Insert(record.clone()));
        let mut row = record.clone();
        let mut next = self.next_id.lock().unwrap();
        let id = *next;
        *next += 1;
        // A remote database fills in the key, and `insertRow` answers it — which
        // is the only way the caller can read the row back.
        if let Some(object) = row.as_object_mut() {
            object.insert("id".into(), json!(id));
            // …and a default the caller never sent, so that "the row as the
            // provider now has it" is visibly different from "the record as
            // written".
            object.entry("votes").or_insert(json!(0));
        }
        self.rows.lock().unwrap().push(row);
        Ok(json!(id))
    }

    async fn update(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
        id: &Json,
        record: &Json,
    ) -> Result<()> {
        self.calls.lock().unwrap().push(Call::Update {
            id: id.clone(),
            record: record.clone(),
        });
        let mut rows = self.rows.lock().unwrap();
        for row in rows.iter_mut() {
            if row.get("id") == Some(id)
                && let (Some(target), Some(changes)) = (row.as_object_mut(), record.as_object())
            {
                for (column, value) in changes {
                    target.insert(column.clone(), value.clone());
                }
            }
        }
        Ok(())
    }

    async fn delete(
        &self,
        _module: &str,
        _provider: &str,
        _table: &str,
        _config: &Json,
        filter: &Json,
    ) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Delete(filter.clone()));
        self.rows
            .lock()
            .unwrap()
            .retain(|row| !RemoteTable::matches(row, filter));
        Ok(())
    }
}

async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Catalog::init(driver).await?;
    bootstrap_table_meta(&catalog).await?;
    Ok(catalog)
}

/// A catalog with `remote` defined as a provided table over `host`.
async fn with_table(db: &TestDb, host: Arc<RemoteTable>) -> Result<Catalog> {
    let catalog = catalog(db).await?;
    catalog.set_table_providers(host as Arc<dyn TableProviderHost>)?;
    let mut meta = TableMeta::new("remote");
    meta.set_provider(Some(
        &ProvidedTableDef::new(MODULE, PROVIDER).configuration(Attrs::new()),
    ));
    save_table_meta(&catalog, &meta).await?;
    Ok(catalog)
}

/// `RETURNING *`, which is what every write `rows.rs` issues asks for.
fn all() -> Vec<Projection> {
    vec![Projection::all()]
}

#[tokio::test]
async fn writability_is_a_property_of_the_configuration_and_is_carried_on_the_table() -> Result<()>
{
    let db = TestDb::new().await?;
    let catalog = with_table(&db, RemoteTable::new(false)).await?;
    assert_eq!(
        catalog.require("remote")?.provided_writes(),
        ProvidedWrites::ALL
    );

    // The same provider, configured read-only: `get_table` offers none of the
    // three, and the table says so before anybody presses a button. A second
    // database, because a table has at most one definition row.
    let other = TestDb::new().await?;
    let catalog = with_table(&other, RemoteTable::new(true)).await?;
    let table = catalog.require("remote")?;
    assert_eq!(table.provided_writes(), ProvidedWrites::NONE);
    assert!(!table.provided_writes().any());
    // A table in a database is not "read-only" in this sense at all — its writes
    // are the driver's.
    assert!(catalog.provided_table_issues().is_empty());
    Ok(())
}

#[tokio::test]
async fn an_insert_becomes_insert_row_and_returns_the_row_the_provider_now_has() -> Result<()> {
    let db = TestDb::new().await?;
    let host = RemoteTable::new(false);
    let catalog = with_table(&db, Arc::clone(&host)).await?;
    let table = catalog.require("remote")?;

    let insert =
        Insert::row("remote", vec!["title".to_owned()], vec![Expr::lit("delta")]).returning(all());
    let rows = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .write(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;

    // One row back, and it is the row **as the provider now has it**: the key it
    // generated and the default it filled in, neither of which the caller sent.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("id"), Some(&Value::Int(4)));
    assert_eq!(rows[0].get("title"), Some(&Value::Text("delta".into())));
    assert_eq!(rows[0].get("votes"), Some(&Value::Int(0)));

    // What the module was handed is v1's own vocabulary: a record of exactly the
    // columns the caller wrote, and then a read to fetch the row back.
    let calls = host.calls();
    assert_eq!(calls[0], Call::Insert(json!({ "title": "delta" })));
    assert!(
        matches!(&calls[1], Call::Rows { filter, .. } if filter == &json!({ "id": 4 })),
        "{calls:?}"
    );
    Ok(())
}

#[tokio::test]
async fn an_update_is_resolved_to_keys_and_applied_one_row_at_a_time() -> Result<()> {
    let db = TestDb::new().await?;
    let host = RemoteTable::new(false);
    let catalog = with_table(&db, Arc::clone(&host)).await?;
    let table = catalog.require("remote")?;

    // A filter over a *non-key* column, which v1's `updateRow` cannot take: it
    // has to be run as a read first, and the two rows it matches updated by their
    // own keys.
    let update = Update {
        table: "remote".to_owned(),
        assignments: vec![Assignment::new("title", Expr::lit("edited"))],
        filter: Some(Expr::binary(
            BinOp::Ge,
            Expr::col("votes"),
            Expr::lit(7_i64),
        )),
        returning: all(),
    };
    let rows = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .write(&Statement::from(update))
        .await?
        .try_collect()
        .await?;

    // `RETURNING` is the row **after** the change, which `updateRow` never
    // answered: it was read back.
    assert_eq!(rows.len(), 2);
    let titles: Vec<Option<&Value>> = rows.iter().map(|r| r.get("title")).collect();
    assert!(
        titles
            .iter()
            .all(|t| *t == Some(&Value::Text("edited".into()))),
        "{titles:?}"
    );

    let calls = host.calls();
    let updates: Vec<&Call> = calls
        .iter()
        .filter(|c| matches!(c, Call::Update { .. }))
        .collect();
    assert_eq!(updates.len(), 2, "{calls:?}");
    assert_eq!(
        updates[0],
        &Call::Update {
            id: json!(1),
            record: json!({ "title": "edited" })
        }
    );
    assert_eq!(
        updates[1],
        &Call::Update {
            id: json!(3),
            record: json!({ "title": "edited" })
        }
    );
    // And the row the filter did not match is untouched.
    let stored = host.stored();
    assert_eq!(stored[1]["title"], json!("beta"));
    Ok(())
}

#[tokio::test]
async fn a_delete_reads_first_and_names_the_rows_it_means() -> Result<()> {
    let db = TestDb::new().await?;
    let host = RemoteTable::new(false);
    let catalog = with_table(&db, Arc::clone(&host)).await?;
    let table = catalog.require("remote")?;

    let delete = Delete {
        table: "remote".to_owned(),
        filter: Some(Expr::binary(
            BinOp::Lt,
            Expr::col("votes"),
            Expr::lit(8_i64),
        )),
        returning: all(),
    };
    let rows = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .write(&Statement::from(delete))
        .await?
        .try_collect()
        .await?;

    // The rows **as they were**: the only copy of them anyone will ever get, and
    // unobtainable after the delete.
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get("title"), Some(&Value::Text("beta".into())));
    assert_eq!(rows[1].get("title"), Some(&Value::Text("gamma".into())));

    // The `where` the module was handed names those two rows and nothing else —
    // never `{}`, which a provider would read as "the whole table".
    let calls = host.calls();
    let deletes: Vec<&Call> = calls
        .iter()
        .filter(|c| matches!(c, Call::Delete(_)))
        .collect();
    assert_eq!(deletes.len(), 1, "{calls:?}");
    assert_eq!(deletes[0], &Call::Delete(json!({ "id": { "in": [2, 3] } })));

    let stored = host.stored();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0]["id"], json!(1));
    Ok(())
}

#[tokio::test]
async fn a_delete_matching_nothing_touches_nothing() -> Result<()> {
    let db = TestDb::new().await?;
    let host = RemoteTable::new(false);
    let catalog = with_table(&db, Arc::clone(&host)).await?;
    let table = catalog.require("remote")?;

    let delete = Delete {
        table: "remote".to_owned(),
        filter: Some(Expr::col("id").eq(Expr::lit(99_i64))),
        returning: all(),
    };
    let rows = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .write(&Statement::from(delete))
        .await?
        .try_collect()
        .await?;
    assert!(rows.is_empty());
    // Nothing was sent: an empty key list would otherwise become a `where` the
    // provider could read as "everything".
    assert!(
        !host.calls().iter().any(|c| matches!(c, Call::Delete(_))),
        "{:?}",
        host.calls()
    );
    assert_eq!(host.stored().len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_provider_with_no_primary_key_cannot_be_addressed_and_says_so() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = with_table(&db, RemoteTable::keyless()).await?;
    let table = catalog.require("remote")?;
    let provider = catalog.provider(&table, sc_catalog::Unchecked::system("test"))?;

    let update = Update {
        table: "remote".to_owned(),
        assignments: vec![Assignment::new("title", Expr::lit("x"))],
        filter: None,
        returning: all(),
    };
    let Err(err) = provider.write(&Statement::from(update)).await else {
        panic!("a table with no key cannot be updated");
    };
    let err = err.to_string();
    assert!(err.contains("no primary key"), "{err}");
    assert!(err.contains(PROVIDER), "{err}");

    // An insert, by contrast, needs no address: it is refused nothing, and comes
    // back with the record as written because there is no key to read it by.
    let insert =
        Insert::row("remote", vec!["title".to_owned()], vec![Expr::lit("new")]).returning(all());
    let rows = provider
        .write(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("title"), Some(&Value::Text("new".into())));
    Ok(())
}

#[tokio::test]
async fn an_expression_with_no_database_to_evaluate_it_in_is_refused() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = with_table(&db, RemoteTable::new(false)).await?;
    let table = catalog.require("remote")?;

    let update = Update {
        table: "remote".to_owned(),
        // `votes = votes + 1`: perfectly good SQL, and there is no SQL engine
        // behind this table to run it.
        assignments: vec![Assignment::new(
            "votes",
            Expr::binary(BinOp::Add, Expr::col("votes"), Expr::lit(1_i64)),
        )],
        filter: None,
        returning: all(),
    };
    let Err(err) = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .write(&Statement::from(update))
        .await
    else {
        panic!("an expression with nothing to evaluate it must be refused");
    };
    let err = err.to_string();
    assert!(err.contains("`votes`"), "{err}");
    assert!(err.contains("has to be a value"), "{err}");
    Ok(())
}

#[tokio::test]
async fn a_select_is_not_a_write() -> Result<()> {
    let db = TestDb::new().await?;
    let catalog = with_table(&db, RemoteTable::new(false)).await?;
    let table = catalog.require("remote")?;
    let select = Statement::Select(Box::new(Select::from(Source::table("remote"))));
    let Err(err) = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))?
        .write(&select)
        .await
    else {
        panic!("a SELECT is not a write");
    };
    let err = err.to_string();
    assert!(err.contains("none of the three"), "{err}");
    Ok(())
}
