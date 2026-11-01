//! The milestone's **definition of done**: a writable table provider over a
//! real PostgreSQL table, driven through the catalog (design §8.3).
//!
//! What runs, with nothing mocked:
//!
//! - a remote table created in a throwaway Postgres database, which stands in
//!   for the database `@saltcorn/postgres-tables` points at;
//! - the `pg-module` fixture installed by `npm` (which pulls **npm's `pg`**) and
//!   loaded on a `deno_runtime` worker in this process, granted exactly the one
//!   socket the database is on;
//! - a `_fd_tables` row in *Saltcorn's own* database making a provided table out
//!   of it;
//! - `INSERT`, `UPDATE` and `DELETE` statements — the ones `sc-api`'s `rows.rs`
//!   builds, `RETURNING *` and all — going through `Catalog::provider`, and the
//!   remote database checked afterwards to see that they landed.
//!
//! ## Why the fixture and not `@saltcorn/postgres-tables` itself
//!
//! `pg` runs under Deno; that plugin does not, and the reason is not `pg`. Its
//! first line is `require("@saltcorn/data/db")` — it is a client of v1's own
//! internals rather than a thin wrapper over an npm library, and v1's package
//! does not survive being required on a worker with no v1 server around it (it
//! fails inside its own module graph with `isNode is not a function`). The
//! fixture keeps everything about that plugin this milestone is *about*: the
//! four methods, the `read_only` flag that withholds three of them, and the
//! `where`/`options` pair turned into SQL rather than ignored.
//!
//! `#[ignore]` on the test that reaches the npm registry, which is this
//! workspace's rule:
//! `cargo test -p sc-module --features deno-host --test pg_provider -- --ignored`.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::sync::Arc;

use common::{fixture, have_npm, surfaces, temp_root};
use sc_action::ActionRegistry;
use sc_catalog::{
    Attrs, Catalog, ProvidedTableDef, ProvidedWrites, TableMeta, TableProviderHost,
    bootstrap_table_meta, save_table_meta,
};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_module::{
    Installer, Module, ModuleHost, ModulePermissions, ModuleSet, ModuleSource,
    ModuleTableProviders, bootstrap_modules, save_module,
};
use sc_query::{Assignment, BinOp, Delete, Expr, Insert, Projection, Statement, Update, Value};
use sc_test_harness::TestDb;
use serde_json::json;

const PROVIDER: &str = "PostgreSQL table";

/// The permission one connection to `parts` needs, and nothing else.
///
/// A Postgres connection is one of two things depending on how the machine is
/// set up, and `pg` chooses by itself from the host it was given:
///
/// - a **TCP** socket (CI's container, and any genuinely remote database): one
///   `host:port` entry;
/// - a **Unix** socket (a local server, where libpq spells the host as the
///   socket *directory*): the `unix:` net entry **and** read/write on the socket
///   file, because that is what opening one costs in Deno's permission model.
///
/// Both are one connection's worth, and neither is a directory.
fn one_database(parts: &sc_test_harness::ConnectionParts) -> ModulePermissions {
    if !parts.host.starts_with('/') {
        return ModulePermissions {
            net: vec![format!("{}:{}", parts.host, parts.port)],
            ..ModulePermissions::closed()
        };
    }
    let socket = format!("{}/.s.PGSQL.{}", parts.host, parts.port);
    ModulePermissions {
        net: vec![format!("unix:{socket}")],
        read: vec![socket.clone()],
        write: vec![socket],
        ..ModulePermissions::closed()
    }
}

/// The provider configuration pointing at `remote`'s `people` table.
fn config(parts: &sc_test_harness::ConnectionParts, read_only: bool) -> Attrs {
    let mut attrs = Attrs::new();
    attrs.insert("host".into(), json!(parts.host));
    attrs.insert("port".into(), json!(parts.port));
    attrs.insert("user".into(), json!(parts.user));
    attrs.insert("password".into(), json!(parts.password));
    attrs.insert("database".into(), json!(parts.database));
    attrs.insert("table_name".into(), json!("people"));
    attrs.insert("read_only".into(), json!(read_only));
    attrs
}

/// `RETURNING *`, which every write `rows.rs` issues asks for.
fn all() -> Vec<Projection> {
    vec![Projection::all()]
}

#[tokio::test]
#[ignore = "reaches the npm registry"]
async fn a_remote_postgres_table_is_read_and_written_through_a_module() {
    skip_without!(have_npm(), "npm is not on the PATH");

    // The *remote* database — somebody else's, as far as Saltcorn is concerned.
    let remote = TestDb::new().await.unwrap();
    let client = remote.client().await.unwrap();
    client
        .batch_execute(
            "create table people (id serial primary key, name text not null, \
             votes integer not null default 0)",
        )
        .await
        .unwrap();
    client
        .batch_execute("insert into people (name, votes) values ('alpha', 10), ('beta', 2)")
        .await
        .unwrap();
    let parts = remote.parts();

    // Saltcorn's own database, and the module set over it.
    let own = TestDb::new().await.unwrap();
    let catalog = catalog(&own).await.unwrap();
    bootstrap_table_meta(&catalog).await.unwrap();
    bootstrap_modules(&catalog).await.unwrap();

    let root = temp_root("pg-provider");
    let installer = Installer::new(&root);
    let package = installer
        .install(
            ModuleSource::Local,
            &fixture("pg-module").display().to_string(),
        )
        .await
        .unwrap();
    let mut module = Module::new(
        &package.name,
        ModuleSource::Local,
        fixture("pg-module").display().to_string(),
    );
    module.version = Some(package.version);
    module.permissions = one_database(&parts);
    save_module(&catalog, &module).await.unwrap();

    let host = Arc::new(ModuleHost::new(&root));
    let mut registry = ActionRegistry::new();
    let set = ModuleSet::load(&catalog, &host, &installer, &surfaces(), &mut registry)
        .await
        .unwrap();
    let loaded = set.get(&package.name).unwrap();
    assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
    let providers = Arc::new(ModuleTableProviders::new(&host, &set));
    catalog
        .set_table_providers(Arc::clone(&providers) as Arc<dyn TableProviderHost>)
        .unwrap();

    // The `_fd_tables` row that *is* the table.
    let mut meta = TableMeta::new("people").label("People");
    meta.set_provider(Some(
        &ProvidedTableDef::new(&package.name, PROVIDER).configuration(config(&parts, false)),
    ));
    save_table_meta(&catalog, &meta).await.unwrap();

    let table = catalog.require("people").unwrap();
    assert!(
        catalog.provided_table_issues().is_empty(),
        "{:?}",
        catalog.provided_table_issues()
    );
    // Its columns are the remote table's, read out of `information_schema` by
    // the module — including which of them is the key, which is what makes an
    // update addressable at all.
    let columns: Vec<&str> = table.fields.iter().map(|f| f.base.name.as_str()).collect();
    assert_eq!(columns, ["id", "name", "votes"]);
    assert_eq!(table.primary_key, ["id"]);
    // And it is writable, because this configuration is not read-only.
    assert_eq!(table.provided_writes(), ProvidedWrites::ALL);

    let provider = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))
        .unwrap();

    // --- INSERT -------------------------------------------------------------
    let insert =
        Insert::row("people", vec!["name".to_owned()], vec![Expr::lit("gamma")]).returning(all());
    let rows = provider
        .write(&Statement::from(insert))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    // The row as the *remote database* now has it: the serial key it assigned
    // and the default it filled in, neither of which was sent.
    assert_eq!(rows[0].get("id"), Some(&Value::Int(3)));
    assert_eq!(rows[0].get("name"), Some(&Value::Text("gamma".into())));
    assert_eq!(rows[0].get("votes"), Some(&Value::Int(0)));

    // --- UPDATE -------------------------------------------------------------
    // Filtered on a non-key column, which v1's `updateRow` cannot take: it is
    // resolved to keys first.
    let update = Update {
        table: "people".to_owned(),
        assignments: vec![Assignment::new("name", Expr::lit("edited"))],
        filter: Some(Expr::binary(
            BinOp::Ge,
            Expr::col("votes"),
            Expr::lit(10_i64),
        )),
        returning: all(),
    };
    let rows = provider
        .write(&Statement::from(update))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("name"), Some(&Value::Text("edited".into())));

    // --- DELETE -------------------------------------------------------------
    let delete = Delete {
        table: "people".to_owned(),
        filter: Some(Expr::col("name").eq(Expr::lit("beta"))),
        returning: all(),
    };
    let rows = provider
        .write(&Statement::from(delete))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    // The row **as it was** — the only copy of it anyone will ever get.
    assert_eq!(rows[0].get("name"), Some(&Value::Text("beta".into())));

    // --- and the remote database agrees ------------------------------------
    let after = client
        .query("select id, name, votes from people order by id", &[])
        .await
        .unwrap();
    let seen: Vec<(i32, String, i32)> = after
        .iter()
        .map(|r| (r.get(0), r.get::<_, String>(1), r.get(2)))
        .collect();
    assert_eq!(
        seen,
        vec![(1, "edited".to_owned(), 10), (3, "gamma".to_owned(), 0)]
    );

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "reaches the npm registry"]
async fn a_read_only_configuration_refuses_all_three_and_still_reads() {
    skip_without!(have_npm(), "npm is not on the PATH");

    let remote = TestDb::new().await.unwrap();
    let client = remote.client().await.unwrap();
    client
        .batch_execute(
            "create table people (id serial primary key, name text not null, \
             votes integer not null default 0)",
        )
        .await
        .unwrap();
    client
        .batch_execute("insert into people (name, votes) values ('alpha', 10)")
        .await
        .unwrap();
    let parts = remote.parts();

    let own = TestDb::new().await.unwrap();
    let catalog = catalog(&own).await.unwrap();
    bootstrap_table_meta(&catalog).await.unwrap();
    bootstrap_modules(&catalog).await.unwrap();

    let root = temp_root("pg-provider-ro");
    let installer = Installer::new(&root);
    let package = installer
        .install(
            ModuleSource::Local,
            &fixture("pg-module").display().to_string(),
        )
        .await
        .unwrap();
    let mut module = Module::new(
        &package.name,
        ModuleSource::Local,
        fixture("pg-module").display().to_string(),
    );
    module.permissions = one_database(&parts);
    save_module(&catalog, &module).await.unwrap();

    let host = Arc::new(ModuleHost::new(&root));
    let mut registry = ActionRegistry::new();
    let set = ModuleSet::load(&catalog, &host, &installer, &surfaces(), &mut registry)
        .await
        .unwrap();
    catalog
        .set_table_providers(
            Arc::new(ModuleTableProviders::new(&host, &set)) as Arc<dyn TableProviderHost>
        )
        .unwrap();

    // The same provider, the same remote table, `read_only` ticked.
    let mut meta = TableMeta::new("people");
    meta.set_provider(Some(
        &ProvidedTableDef::new(&package.name, PROVIDER).configuration(config(&parts, true)),
    ));
    save_table_meta(&catalog, &meta).await.unwrap();

    let table = catalog.require("people").unwrap();
    assert_eq!(table.provided_writes(), ProvidedWrites::NONE);
    let provider = catalog
        .provider(&table, sc_catalog::Unchecked::system("test"))
        .unwrap();

    // Reading is unaffected.
    let rows = provider
        .query(&sc_query::Select::from(sc_query::Source::table("people")))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("name"), Some(&Value::Text("alpha".into())));

    // Each of the three refuses by name, and nothing reaches the database.
    let insert =
        Insert::row("people", vec!["name".to_owned()], vec![Expr::lit("x")]).returning(all());
    let said = refusal(&provider, Statement::from(insert)).await;
    assert!(said.contains("insertRow"), "{said}");
    assert!(said.contains("read-only"), "{said}");

    let update = Update {
        table: "people".to_owned(),
        assignments: vec![Assignment::new("name", Expr::lit("x"))],
        filter: None,
        returning: all(),
    };
    let said = refusal(&provider, Statement::from(update)).await;
    assert!(said.contains("updateRow"), "{said}");

    let delete = Delete {
        table: "people".to_owned(),
        filter: None,
        returning: all(),
    };
    let said = refusal(&provider, Statement::from(delete)).await;
    assert!(said.contains("deleteRows"), "{said}");

    let after = client
        .query("select count(*) from people", &[])
        .await
        .unwrap();
    assert_eq!(after[0].get::<_, i64>(0), 1);

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// The sentence one refused write answered with.
async fn refusal(provider: &Arc<dyn sc_catalog::TableProvider>, statement: Statement) -> String {
    match provider.write(&statement).await {
        Ok(_) => panic!("a read-only provided table must refuse this write"),
        Err(e) => e.to_string(),
    }
}

async fn catalog(db: &TestDb) -> Result<Catalog> {
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    Catalog::init(driver as Arc<dyn DatabaseDriver>).await
}
