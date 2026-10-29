//! Integration test for the **database connections** admin API, driven through
//! the assembled router as a browser-based admin would.
//!
//! `sc-catalog`'s `db_connections.rs` covers the catalog half — that a connected
//! database's tables arrive, are stamped with the connection and are queried
//! against the right server. This covers the half only this layer can get right:
//!
//! 1. the password is **redacted on the way out and restored on the way in**, so
//!    an admin can edit the host without knowing the password, and no response
//!    anywhere carries the stored one;
//! 2. the tables list reports which database each table is in, which is what the
//!    badge beside a table's name is drawn from; and
//! 3. the live registry is kept in step with the definitions — a deleted
//!    connection's tables leave the list, a renamed one does not go on
//!    contributing under its old name.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// What the API sends instead of a stored password (`sc_types::SECRET_SENTINEL`).
const SENTINEL: &str = "••••••••";

/// A cookie-jar-carrying client over the router (CSRF + session), mirroring the
/// helper in `file_store_admin_api.rs`.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router) -> Client {
        Client {
            router,
            cookies: HashMap::new(),
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
        }
        if method != "GET"
            && method != "HEAD"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(ref b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };

        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str() {
                let pair = text.split(';').next().unwrap_or("");
                if let Some((name, value)) = pair.split_once('=') {
                    if value.is_empty() {
                        self.cookies.remove(name);
                    } else {
                        self.cookies.insert(name.to_owned(), value.to_owned());
                    }
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// A router over a real database with the platform tables bootstrapped, and an
/// admin logged in.
async fn setup() -> sc_error::Result<(Client, Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    // Neutralise any `users` table inherited from the template database, for the
    // reason `file_store_admin_api`'s setup does.
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_db_connections(&catalog).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps),
        sessions,
        &ServerConfig::default(),
    )?;

    let mut client = Client::new(router);
    client.send("GET", "/api/auth/status", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    Ok((client, catalog, db))
}

/// The body an admin's form would send to reach `foreign` under `name`.
fn body_for(name: &str, foreign: &TestDb) -> Value {
    let parts = foreign.parts();
    json!({
        "name": name,
        "description": "",
        "host": parts.host,
        "port": parts.port,
        "database": parts.database,
        "username": parts.user,
        "password": parts.password,
        "schema": "public",
    })
}

/// Whether `db` actually holds a table of this name — asked of the server
/// itself, not of the catalog, because the claim under test is *which database
/// the DDL reached*.
async fn foreign_has(db: &TestDb, table: &str) -> sc_error::Result<bool> {
    let rows = db
        .client()
        .await?
        .query(
            "select 1 from information_schema.tables \
             where table_name = $1 and table_type = 'BASE TABLE'",
            &[&table],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    Ok(!rows.is_empty())
}

/// The tables list, as the SPA loads it.
async fn tables(client: &mut Client) -> Vec<Value> {
    let (status, body) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array().unwrap().clone()
}

#[tokio::test]
async fn a_connections_tables_join_the_tables_list_and_say_where_they_are_from()
-> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;
    let foreign = TestDb::new().await?;
    foreign
        .client()
        .await?
        .batch_execute("create table invoice (id bigint primary key, total numeric)")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    // Nothing configured yet, and every table so far is Saltcorn's own.
    let (status, body) = client.send("GET", "/api/db-connections", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_array().unwrap().is_empty());
    assert!(
        tables(&mut client)
            .await
            .iter()
            .all(|t| t["database"] == json!("primary")),
        "with no connections, every table is in the primary database"
    );

    // --- created and connected ----------------------------------------------
    let (status, created) = client
        .send(
            "POST",
            "/api/db-connections",
            Some(body_for("reporting", &foreign)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["connected"], json!(true), "{created}");
    assert_eq!(created["error"], json!(null));
    assert_eq!(created["tables"], json!(1));
    assert_eq!(created["shadowed"], json!([]));

    // The foreign table is in the list, and says which database it is in — the
    // badge beside its name is drawn from exactly this.
    let listed = tables(&mut client).await;
    let invoice = listed
        .iter()
        .find(|t| t["name"] == json!("invoice"))
        .expect("the foreign table is in the tables list");
    assert_eq!(invoice["database"], json!("reporting"));
    // RLS is DDL against the primary driver, so it is never offered on a table a
    // connection contributed: a toggle that can only ever be refused is a trap.
    assert_eq!(invoice["rls_available"], json!(false));
    // Saltcorn's own tables are unmarked and unaffected.
    let users = listed
        .iter()
        .find(|t| t["name"] == json!("users"))
        .expect("the users table");
    assert_eq!(users["database"], json!("primary"));

    // --- the password is not something this API gives back -------------------
    let (_, listing) = client.send("GET", "/api/db-connections", None).await;
    let row = &listing.as_array().unwrap()[0];
    let stored_password = foreign.parts().password;
    if stored_password.is_empty() {
        // "No password" stays empty: dots would claim a password exists.
        assert_eq!(row["password"], json!(""));
    } else {
        assert_eq!(row["password"], json!(SENTINEL));
        // "Anywhere" means anywhere the password has no business being — the
        // non-secret parts are *meant* to come back, and a development database
        // whose password happens to equal one of them (CI's role is `sc_test`
        // with password `sc_test`) would otherwise fail this for saying its own
        // username. Drop those, then look for the password in what is left,
        // `error` included: a connection failure that echoes the DSN is exactly
        // the leak this guards.
        let mut scrubbed = listing.clone();
        for row in scrubbed.as_array_mut().unwrap() {
            let row = row.as_object_mut().unwrap();
            for part in [
                "name",
                "description",
                "host",
                "database",
                "username",
                "schema",
                "file_store",
                "file_path",
            ] {
                row.remove(part);
            }
        }
        assert!(
            !serde_json::to_string(&scrubbed)
                .unwrap()
                .contains(&stored_password),
            "the stored password must not appear anywhere in the listing: {scrubbed}"
        );
    }

    // --- editing without retyping the password -------------------------------
    // The form sends back what it was shown. The server sees its own sentinel
    // and restores what is stored, so the connection keeps working.
    let id = created["id"].as_str().unwrap().to_owned();
    let mut edit = body_for("reporting", &foreign);
    edit["description"] = json!("the analytics replica");
    edit["password"] = row["password"].clone();
    let (status, updated) = client
        .send("PUT", &format!("/api/db-connections/{id}"), Some(edit))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["connected"], json!(true), "{updated}");
    assert_eq!(updated["description"], json!("the analytics replica"));
    assert_eq!(catalog.require("invoice")?.database.0, "reporting");

    // --- a rename does not leave the old name contributing --------------------
    let mut renamed = body_for("analytics", &foreign);
    renamed["password"] = row["password"].clone();
    let (status, updated) = client
        .send("PUT", &format!("/api/db-connections/{id}"), Some(renamed))
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(catalog.database_names()?, vec!["analytics".to_string()]);
    assert_eq!(catalog.require("invoice")?.database.0, "analytics");

    // --- deleted -------------------------------------------------------------
    let (status, deleted) = client
        .send("DELETE", &format!("/api/db-connections/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], json!(true));
    assert!(
        !tables(&mut client)
            .await
            .iter()
            .any(|t| t["name"] == json!("invoice")),
        "a removed connection's tables leave the tables list"
    );
    // And nothing was done to the database it pointed at.
    let (rows, _) = (
        foreign
            .client()
            .await?
            .query("select count(*) from invoice", &[])
            .await
            .map_err(|e| sc_error::Error::database(e.to_string()))?,
        (),
    );
    assert_eq!(rows.len(), 1, "the foreign table is still there");

    Ok(())
}

#[tokio::test]
async fn a_table_is_created_on_a_chosen_connection() -> sc_error::Result<()> {
    let (mut client, catalog, db) = setup().await?;
    let foreign = TestDb::new().await?;

    let (status, created) = client
        .send(
            "POST",
            "/api/db-connections",
            Some(body_for("reporting", &foreign)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    // --- a blank table on the connection -------------------------------------
    let (status, table) = client
        .send(
            "POST",
            "/api/tables",
            Some(json!({ "name": "shipment", "database": "reporting" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{table}");
    assert_eq!(table["database"], json!("reporting"));

    // It is in the foreign database, and **not** in Saltcorn's own — the whole
    // claim of the dropdown.
    assert!(foreign_has(&foreign, "shipment").await?);
    assert!(!foreign_has(&db, "shipment").await?);

    // And it stays editable: a table an admin created on a connection is theirs
    // to add columns to and to drop, or the dropdown would make tables nobody
    // could finish.
    let (status, field) = client
        .send(
            "POST",
            "/api/tables/shipment/fields",
            Some(json!({ "name": "tracking", "type": "text" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{field}");
    assert!(catalog.require("shipment")?.field("tracking").is_some());

    // --- a table from a CSV, on the connection --------------------------------
    let (status, made) = client
        .send(
            "POST",
            "/api/tables/csv",
            Some(json!({
                "name": "parcel",
                "database": "reporting",
                "csv": "id,weight\n1,3\n2,5\n",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    assert_eq!(made["table"]["database"], json!("reporting"));
    assert_eq!(made["inserted"], json!(2));
    // The rows went in beside the table, in the same database.
    assert!(foreign_has(&foreign, "parcel").await?);
    assert!(!foreign_has(&db, "parcel").await?);
    let rows = foreign
        .client()
        .await?
        .query("select count(*)::int8 from parcel", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    assert_eq!(rows[0].get::<_, i64>(0), 2);

    // --- one batch, one database ---------------------------------------------
    // A batch is one transaction and a transaction cannot span two Postgres
    // servers, so a batch reaching into both is refused rather than quietly
    // split into two that can half-fail. Checked through the schema editor,
    // because the admin API only ever sends single-operation batches — an agent
    // and the copilot are what send longer ones.
    let spanning = sc_api::schema_edit::apply(
        &catalog,
        &[
            sc_api::schema_edit::Operation::AddField {
                table: "shipment".into(),
                field: sc_api::schema_edit::FieldSpec {
                    name: "carrier".into(),
                    type_name: "text".into(),
                    ..sc_api::schema_edit::FieldSpec::default()
                },
            },
            sc_api::schema_edit::Operation::AddField {
                table: "users".into(),
                field: sc_api::schema_edit::FieldSpec {
                    name: "nickname".into(),
                    type_name: "text".into(),
                    ..sc_api::schema_edit::FieldSpec::default()
                },
            },
        ],
        &sc_api::schema_edit::ApplyOptions::default(),
    )
    .await
    .expect_err("a batch cannot span two databases");
    assert!(spanning.to_string().contains("two databases"), "{spanning}");
    // Nothing was applied — not even the first operation, which is the point.
    assert!(catalog.require("shipment")?.field("carrier").is_none());
    assert!(catalog.require("users")?.field("nickname").is_none());

    // --- dropping ------------------------------------------------------------
    let (status, dropped) = client.send("DELETE", "/api/tables/shipment", None).await;
    assert_eq!(status, StatusCode::OK, "{dropped}");
    assert!(!foreign_has(&foreign, "shipment").await?);

    // --- a database that is not connected is refused at the keyboard ---------
    let (status, body) = client
        .send(
            "POST",
            "/api/tables",
            Some(json!({ "name": "nowhere", "database": "no-such-connection" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!foreign_has(&db, "nowhere").await?);

    // --- and an omitted database still means Saltcorn's own ------------------
    let (status, table) = client
        .send("POST", "/api/tables", Some(json!({ "name": "memo" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{table}");
    assert_eq!(table["database"], json!("primary"));
    assert!(foreign_has(&db, "memo").await?);
    assert!(!foreign_has(&foreign, "memo").await?);

    Ok(())
}

#[tokio::test]
async fn testing_a_connection_answers_rather_than_failing() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let foreign = TestDb::new().await?;
    foreign
        .client()
        .await?
        .batch_execute("create table invoice (id bigint primary key)")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    // A test that works reports what it found, and saves nothing.
    let (status, result) = client
        .send(
            "POST",
            "/api/db-connections/test",
            Some(body_for("reporting", &foreign)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["connected"], json!(true));
    assert_eq!(result["tables"], json!(1));
    let (_, listing) = client.send("GET", "/api/db-connections", None).await;
    assert!(listing.as_array().unwrap().is_empty(), "Test saves nothing");

    // A test that fails is an **answer**, not a request that went wrong: 200
    // with the reason, so the form shows it beside the button rather than as a
    // red banner about the API.
    let mut bad = body_for("reporting", &foreign);
    bad["host"] = json!("no-such-host.invalid");
    let (status, result) = client
        .send("POST", "/api/db-connections/test", Some(bad))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["connected"], json!(false));
    assert!(result["error"].as_str().is_some_and(|e| !e.is_empty()));

    Ok(())
}

#[tokio::test]
async fn a_connection_cannot_claim_the_primary_databases_name() -> sc_error::Result<()> {
    let (mut client, _catalog, _db) = setup().await?;
    let foreign = TestDb::new().await?;

    // `primary` is the id every one of Saltcorn's own tables carries, so a
    // connection holding it would make a table's origin ambiguous in the one
    // direction the query routing depends on.
    let (status, body) = client
        .send(
            "POST",
            "/api/db-connections",
            Some(body_for("primary", &foreign)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And a second connection cannot take a name that is already in use.
    let (status, _) = client
        .send(
            "POST",
            "/api/db-connections",
            Some(body_for("reporting", &foreign)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = client
        .send(
            "POST",
            "/api/db-connections",
            Some(body_for("reporting", &foreign)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    Ok(())
}

/// A **SQLite file in a file store** connected through the same four endpoints.
///
/// The catalog half is `sc-catalog`'s `sqlite_connections.rs`; what only this
/// layer can get right is the form's half: that a body naming `backend:
/// "sqlite"` plus a store and a path reaches the driver at all, that the
/// response describes a file rather than a host, and that the file's tables join
/// the tables list stamped with the connection like any other.
#[tokio::test]
async fn a_sqlite_file_can_be_connected_from_a_file_store() -> sc_error::Result<()> {
    let (mut client, catalog, _db) = setup().await?;

    // A directory with a SQLite database in it, and a file store over it — what
    // an admin would already have under Files.
    let dir = std::env::temp_dir().join(format!("sc-api-sqlite-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    {
        let driver = sc_db_sqlite::SqliteDriver::open(dir.join("reporting.sqlite"))?;
        driver
            .apply_schema(&sc_db::SchemaChange::CreateTable {
                name: "invoice".into(),
                columns: vec![
                    sc_db::ColumnDef::new("id", "int8").not_null().identity(),
                    sc_db::ColumnDef::new("total", "numeric"),
                ],
                primary_key: vec!["id".into()],
                unlogged: false,
            })
            .await?;
    }
    let store = sc_files::FileStoreDef::local("data", dir.display().to_string());
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_catalog::save_file_store(&catalog, &store).await?;
    sc_catalog::connect_file_store_def(&catalog, &store)?;

    // --- test before saving, exactly as the dialog's Test button does --------
    let body = json!({
        "name": "reporting",
        "description": "",
        "backend": "sqlite",
        "host": "",
        "port": 0,
        "database": "",
        "username": "",
        "password": "",
        "schema": "",
        "file_store": "data",
        "file_path": "reporting.sqlite",
    });
    let (status, tested) = client
        .send("POST", "/api/db-connections/test", Some(body.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{tested}");
    assert_eq!(tested["connected"], json!(true), "{tested}");
    assert_eq!(tested["tables"], json!(1));

    // --- created and connected ----------------------------------------------
    let (status, created) = client.send("POST", "/api/db-connections", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["connected"], json!(true), "{created}");
    assert_eq!(created["backend"], json!("sqlite"));
    assert_eq!(created["file_store"], json!("data"));
    assert_eq!(created["file_path"], json!("reporting.sqlite"));
    assert_eq!(created["tables"], json!(1));

    // The file's table is in the tables list, stamped with the connection —
    // and, as for any non-primary database, row-level security is not offered.
    let listed = tables(&mut client).await;
    let invoice = listed
        .iter()
        .find(|t| t["name"] == json!("invoice"))
        .expect("the file's table is in the tables list");
    assert_eq!(invoice["database"], json!("reporting"));
    assert_eq!(invoice["rls_available"], json!(false));

    // --- a body that names a file store nobody defined is refused ------------
    let (status, refused) = client
        .send(
            "POST",
            "/api/db-connections",
            Some(json!({
                "name": "elsewhere",
                "description": "",
                "backend": "sqlite",
                "host": "",
                "port": 0,
                "database": "",
                "username": "",
                "password": "",
                "schema": "",
                "file_store": "nowhere",
                "file_path": "a.sqlite",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(refused.to_string().contains("nowhere"), "{refused}");

    // --- removing the connection leaves the file alone -----------------------
    let id = created["id"].as_str().expect("an id").to_owned();
    let (status, _) = client
        .send("DELETE", &format!("/api/db-connections/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !tables(&mut client)
            .await
            .iter()
            .any(|t| t["name"] == json!("invoice"))
    );
    assert!(
        dir.join("reporting.sqlite").is_file(),
        "nothing was deleted"
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
