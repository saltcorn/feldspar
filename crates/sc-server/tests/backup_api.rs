//! Backup and restore, driven through the assembled router as an admin's browser
//! drives it (design §16).
//!
//! The claim worth an integration test is the **round trip between two
//! installations**, because that is what a backup is for and it is the one thing
//! no unit test can reach: a table with rows, a file store with a file in it, a
//! role, a trigger and the SSL settings are set up on one server, backed up as a
//! zip through the HTTP route an admin's browser posts to, and restored into a
//! *second*, empty server through the two requests the Backup screen makes. What
//! arrives there is then read back through the ordinary admin API — not out of the
//! zip — because "the rows are in the file" is not the claim; "the second server
//! has them" is.
//!
//! Around that, six things the screen depends on and that are easy to get wrong:
//!
//! - a **Saltcorn 1** backup is restored by the same two requests, translated on
//!   the way in — its tables, rows, users, files and actions arrive, and what v1
//!   has and this system does not is *reported* rather than silently dropped;
//! - a column an admin added to the **users** table travels with the accounts;
//! - the **choice is remembered** as what was left out, so a table added later is
//!   in the next backup, and the dialog reopens on the admin's tuned selection;
//! - **rows cannot be backed up without their table's metadata**, whatever a
//!   client sends;
//! - a zip that is **not a backup** is refused by name rather than restored as
//!   nothing;
//! - and every one of these is **admin-only**, like every other configuration
//!   endpoint.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_auth::SessionStore;
use sc_catalog::{Catalog, DataField};
use sc_config::{MODE_CUSTOM, SSL_MODE, SSL_PRIVATE_KEY, stored_config};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::FileStoreDef;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value, json};
use tower::ServiceExt;

const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// A cookie-jar-carrying client over the router (CSRF + session), as the other
/// admin-API tests use — plus the two calls whose bodies are not JSON.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, bytes, _) = self
            .raw(
                method,
                path,
                body.map(|b| (serde_json::to_vec(&b).unwrap(), "application/json")),
            )
            .await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// The bytes of a response, and its headers — what a download has to be read
    /// through.
    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        body: Option<(Vec<u8>, &str)>,
    ) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
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
            Some((bytes, content_type)) => builder
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(bytes))
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
        let headers = response.headers().clone();
        // Generous: the largest backup here is a few tens of megabytes of
        // blobs and video.
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        (status, bytes, headers)
    }
}

/// One server: a real database with every platform table bootstrapped, agents and
/// triggers installed, and an admin logged in.
struct Server {
    client: Client,
    catalog: Arc<Catalog>,
    /// Where this server's file store keeps its files.
    files: std::path::PathBuf,
    db: TestDb,
    _engine: Arc<sc_workflow::WorkflowEngineTask>,
}

async fn setup() -> sc_error::Result<Server> {
    setup_in(TestDb::new().await?).await
}

/// [`setup`] over a database the caller made — one with pgvector, say.
async fn setup_in(db: TestDb) -> sc_error::Result<Server> {
    // Neutralise any `users` table inherited from the template database before
    // bootstrap introspects, exactly as the other server tests do.
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
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_mcp::bootstrap_mcp_servers(&catalog).await?;
    sc_viewpattern::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_db_connections(&catalog).await?;
    sc_analytics::bootstrap_workspaces(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    // Standing in for `serve`, which starts the engine: a restored workflow is
    // run here, not only read back.
    let (engine, _handle) = sc_server::start_workflow_engine(&catalog, &dispatcher);
    // A scripted stream provider, so a stream can be saved and restored without
    // a broker: what matters here is the row, secret setting and all.
    let mut streams = sc_stream::StreamRegistry::new();
    streams.register(Arc::new(
        sc_stream::testing::ScriptedProvider::new(
            "scripted",
            sc_stream::ElementType::json([sc_stream::ElementField::new(
                "temperature",
                BasicType::Float,
            )]),
        )
        .config(vec![
            sc_types::FormField::new("broker", TypeRef::Basic(BasicType::Text)),
            sc_types::FormField::new("password", TypeRef::Basic(BasicType::Text)).secret(),
        ]),
    ) as Arc<dyn sc_stream::StreamProvider>)?;
    let streams = sc_server::install_streams_with(
        &catalog,
        &dispatcher,
        Arc::new(streams),
        sc_stream::StreamConfig::default(),
    )
    .await?;

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_agents(agents)
            .with_models(models)
            .with_streams(streams)
            .with_triggers(dispatcher),
    );
    let router = build_router_with_apps(
        &admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;

    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    Ok(Server {
        client,
        catalog,
        files: temp_dir(),
        db,
        _engine: engine,
    })
}

/// A table with rows, a file store with a file in it, an extra role, a trigger on
/// the table, and a stored SSL setting — one of everything the backup carries.
async fn furnish(server: &mut Server) -> sc_error::Result<()> {
    let (status, body) = server
        .client
        .send("POST", "/api/tables", Some(json!({ "name": "books" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A key that numbers itself, so the restore has a sequence to get right as
    // well as rows: the backup carries the keys the rows already have, and the
    // next row written after a restore must not be handed one of them again.
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/tables/books/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/tables/books/fields",
            Some(json!({ "name": "title", "type": "text", "label": "The title" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A configured table, so the overlay has something to restore.
    let (status, body) = server
        .client
        .send(
            "PUT",
            "/api/tables/books",
            Some(json!({
                "label": "Books",
                "description": "Everything on the shelf",
                "min_role_read": 100,
                "min_role_write": 1,
                "ownership_formula": "",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A rule the table's rows are kept to. Not stored in any `_fd_*` table
    // (§5.1), so a backup that carried the columns and not this would hand back
    // a table that accepts what the original refused.
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/tables/books/constraints",
            Some(json!({
                "type": "unique",
                "fields": ["title"],
                "error_message": "that book is on the shelf already",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    for title in ["Dune", "Solaris"] {
        let (status, body) = server
            .client
            .send(
                "POST",
                "/api/tables/books/rows",
                Some(json!({ "title": title })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // A file store on disk, with one file written through the API that puts it
    // there.
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "assets",
                "description": "Uploaded files",
                "backend": "local",
                "config": { "path": server.files.to_string_lossy() },
                "min_role": null,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/file-stores/assets/write",
            Some(json!({ "path": "notes/readme.txt", "text": "hello from the backup" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // An application whose source tree lives in that store, with a bundler that is
    // a shell script — enough for a real build, without npm.
    write_project(&server.files);
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/applications",
            Some(json!({
                "name": "Blog",
                "description": "",
                "subdomain": "blog",
                "framework": { "name": "code", "config": {
                    "store": "assets",
                    "source": "web",
                    "output": "web/dist",
                    "command": "sh build.sh",
                    "client": "web/src/client.ts",
                }},
                "extra_frameworks": [],
                "tables": ["books"],
                "file_stores": ["assets"],
                "triggers": [],
                "apis": [{ "provider": "rest", "mount": "/api", "config": {} }],
                "static_dirs": [],
                "attributes": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/roles",
            Some(json!({ "role": 40, "name": "Librarian", "description": "Keeps the shelf" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "note_new_book",
                "description": "",
                "when": "insert",
                "channel": "books",
                "action": "run_js_code",
                "configuration": { "code": "row.title" },
                "min_role": null,
                "enabled": true,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (certificate, private_key) = self_signed();
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SSL_MODE: MODE_CUSTOM,
                "ssl_certificate": certificate,
                SSL_PRIVATE_KEY: private_key,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

/// A fresh, unique temp directory (created on disk) for one server's file store.
fn temp_dir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "sc-backup-test-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// An application's source tree inside a file store's directory: a bundler that is
/// a shell script, and the one file it produces.
///
/// A real build without a real bundler — what is under test is that a restore
/// *runs* the build over the tree it just restored, not what a bundler does with
/// it. `sh build.sh` rather than `./build.sh` because a zip entry carries no
/// executable bit for the restore to put back.
fn write_project(store: &std::path::Path) {
    let web = store.join("web");
    std::fs::create_dir_all(web.join("src")).unwrap();
    std::fs::write(
        web.join("build.sh"),
        "#!/bin/sh\n\
         set -e\n\
         mkdir -p dist/assets\n\
         printf '<!doctype html><div id=root>restored</div>' > dist/index.html\n\
         printf 'console.log(1)' > dist/assets/app.js\n",
    )
    .unwrap();
}

/// A self-signed certificate for `localhost`, and its key.
fn self_signed() -> (String, String) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    (key.cert.pem(), key.key_pair.serialize_pem())
}

/// Take a backup of everything the server offers, returning the zip.
async fn backup_everything(server: &mut Server) -> Vec<u8> {
    let (status, options) = server.client.send("GET", "/api/backup", None).await;
    assert_eq!(status, StatusCode::OK, "{options}");
    let (status, bytes, headers) = server
        .client
        .raw(
            "POST",
            "/backup/create",
            Some((
                serde_json::to_vec(&json!({ "include": options["include"] })).unwrap(),
                "application/json",
            )),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/zip"
    );
    // Named, so a browser's save dialog offers something an admin can find again.
    let disposition = headers
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        disposition.starts_with("attachment; filename=\"feldspar-backup-"),
        "{disposition}"
    );
    bytes
}

/// One entry of a zip, as text.
fn entry(archive: &[u8], path: &str) -> String {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("a zip");
    let mut file = zip
        .by_name(path)
        .unwrap_or_else(|_| panic!("no `{path}` in the backup"));
    let mut text = String::new();
    file.read_to_string(&mut text).expect("readable text");
    text
}

/// Whether a zip holds an entry at all.
fn has_entry(archive: &[u8], path: &str) -> bool {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("a zip");
    zip.by_name(path).is_ok()
}

#[tokio::test]
async fn a_backup_restores_into_a_second_installation() -> sc_error::Result<()> {
    let mut source = setup().await?;
    furnish(&mut source).await?;
    let archive = backup_everything(&mut source).await;

    // The file is a zip an unarchiver can open, and it says what it holds.
    let manifest: Value = serde_json::from_str(&entry(&archive, "manifest.json")).unwrap();
    assert_eq!(manifest["format"], json!("feldspar-backup"));
    assert_eq!(
        manifest["feldspar_version"],
        json!(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(manifest["contents"]["users"], json!(1));
    assert_eq!(
        entry(&archive, "file-stores/assets/files/notes/readme.txt"),
        "hello from the backup"
    );

    // --- and now the other server -------------------------------------------
    let mut target = setup().await?;
    // Its own store, at its own path: the backup's definition points at the source
    // server's directory, and the restore must not repoint a working store at it.
    // This is the arrangement an admin restoring onto a new machine has.
    sc_catalog::save_file_store(
        &target.catalog,
        &FileStoreDef::local("assets", target.files.to_string_lossy()),
    )
    .await?;
    sc_catalog::connect_file_store_def(
        &target.catalog,
        &sc_catalog::load_file_store_by_name(&target.catalog, "assets")
            .await?
            .expect("the store just saved"),
    )?;

    let (status, uploaded, _) = {
        let (status, bytes, _) = target
            .client
            .raw(
                "POST",
                "/backup/upload",
                Some((archive.clone(), "application/zip")),
            )
            .await;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value, ())
    };
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    // What the dialog is built from: everything the file holds, ticked.
    assert_eq!(uploaded["available"]["tables"][0]["name"], json!("books"));
    assert_eq!(uploaded["available"]["tables"][0]["count"], json!(2));
    assert_eq!(uploaded["include"]["table_data"], json!(["books"]));
    assert_eq!(uploaded["include"]["ssl"], json!(true));

    let (status, report) = target
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": uploaded["id"], "include": uploaded["include"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");

    // The constraint came across, with its message — and it is *enforced*, which
    // is the only claim worth making about a restored rule.
    let (status, constraints) = target
        .client
        .send("GET", "/api/tables/books/constraints", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{constraints}");
    let restored = constraints
        .as_array()
        .and_then(|list| {
            list.iter()
                .find(|c| c["name"] == json!("sc_uq_books_title"))
        })
        .unwrap_or_else(|| panic!("the unique constraint should be restored: {constraints}"));
    assert_eq!(restored["fields"], json!(["title"]));
    assert_eq!(
        restored["error_message"],
        json!("that book is on the shelf already")
    );
    // --- read it back through the ordinary admin API -------------------------
    let (status, tables) = target.client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK, "{tables}");
    let books = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("books"))
        .expect("the restored table");
    // The settings came with it, not just the columns.
    assert_eq!(books["label"], json!("Books"));
    assert_eq!(books["description"], json!("Everything on the shelf"));
    assert_eq!(books["min_role_read"], json!(100));

    let (status, fields) = target
        .client
        .send("GET", "/api/tables/books/fields", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{fields}");
    let title = fields
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("title"))
        .expect("the restored column");
    assert_eq!(title["label"], json!("The title"));

    let (status, rows) = target
        .client
        .send("GET", "/api/tables/books/rows", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let titles: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["title"].as_str())
        .collect();
    assert_eq!(titles, vec!["Dune", "Solaris"]);

    // The rows kept the keys they were backed up with, and the numbering was
    // wound past them: a sequence still sitting at 1 would hand the next row a
    // key `Dune` already has.
    let keys: Vec<i64> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_i64())
        .collect();
    assert_eq!(keys, vec![1, 2]);
    let (status, added) = target
        .client
        .send(
            "POST",
            "/api/tables/books/rows",
            Some(json!({ "title": "Roadside Picnic" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["id"], json!(3), "{added}");

    // …and the restored rule is *enforced*, in the words it was written in,
    // which is the only claim about a restored constraint worth making. (After
    // the key assertion above, because a refused insert has still taken a number
    // from the sequence.)
    let (status, refused) = target
        .client
        .send(
            "POST",
            "/api/tables/books/rows",
            Some(json!({ "title": "Dune" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused.to_string().contains("on the shelf already"),
        "{refused}"
    );

    // The file's bytes went into *this* server's store.
    let (status, file) = target
        .client
        .send(
            "POST",
            "/api/file-stores/assets/read",
            Some(json!({ "path": "notes/readme.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{file}");
    assert_eq!(file["text"], json!("hello from the backup"));
    assert!(
        target.files.join("notes/readme.txt").exists(),
        "the bytes are in the target server's own directory"
    );
    // …and the store it already had was left pointing where it pointed.
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains("already defined")),
        "the kept definition is reported: {report}"
    );

    // The application came back **built**: the bundler ran over the source tree the
    // file store restored a moment earlier, and its output is in this server's own
    // directory. "Restore and go" is the claim, so the build is part of the restore
    // rather than a button somebody has to know to press.
    let (status, applications) = target.client.send("GET", "/api/applications", None).await;
    assert_eq!(status, StatusCode::OK, "{applications}");
    assert!(
        applications
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["subdomain"] == json!("blog")),
        "{applications}"
    );
    assert!(
        report["restored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line
                .as_str()
                .unwrap_or_default()
                .contains("`blog` built and serving")),
        "the build is reported: {report}"
    );
    assert_eq!(
        std::fs::read_to_string(target.files.join("web/dist/index.html")).unwrap(),
        "<!doctype html><div id=root>restored</div>"
    );
    // …and the client the build generates is typed against *this* server's schema.
    assert!(
        target.files.join("web/src/client.ts").exists(),
        "the generated client was written into the restored source tree"
    );

    let (status, roles) = target.client.send("GET", "/api/roles", None).await;
    assert_eq!(status, StatusCode::OK, "{roles}");
    assert!(
        roles
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == json!("Librarian")),
        "{roles}"
    );

    let (status, triggers) = target.client.send("GET", "/api/triggers", None).await;
    assert_eq!(status, StatusCode::OK, "{triggers}");
    let trigger = triggers
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("note_new_book"))
        .expect("the restored trigger");
    // Live, not merely stored: a restored trigger with an error would have one
    // here, and it fires on the table that was restored beside it.
    assert_eq!(trigger["error"], Value::Null);
    assert_eq!(trigger["channel"], json!("books"));

    // The private key travelled — a backup that redacted it would restore a server
    // that cannot serve HTTPS — and it is *stored*, not merely echoed.
    assert_eq!(
        stored_config(&target.catalog, SSL_MODE).await?,
        Some(json!(MODE_CUSTOM))
    );
    let restored_key = stored_config(&target.catalog, SSL_PRIVATE_KEY)
        .await?
        .expect("the restored private key");
    assert!(
        restored_key.as_str().unwrap().contains("PRIVATE KEY"),
        "the key itself, not the sentinel: {restored_key}"
    );

    // The admin who ran the restore is still the admin who ran it: the backup's
    // own account has the same address, and a restore that overwrote it would have
    // replaced their password with the source server's hash.
    let (status, users) = target.client.send("GET", "/api/users", None).await;
    assert_eq!(status, StatusCode::OK, "{users}");
    assert_eq!(users.as_array().unwrap().len(), 1, "{users}");
    assert!(
        report["warnings"].as_array().unwrap().iter().any(|w| w
            .as_str()
            .unwrap_or_default()
            .contains("already on this server")),
        "the kept account is reported: {report}"
    );
    Ok(())
}

/// A column an admin added to the users table (§7.1 invites them to) travels with
/// the accounts, and arrives as a column on the other server.
///
/// Its own test because the failure it guards against is silent: the rows carry
/// the value either way, and a restore into a users table without the column
/// inserts the rest of the row and drops it — so the account arrives looking
/// complete, minus whatever the admin added it for.
#[tokio::test]
async fn a_column_added_to_the_users_table_travels_with_the_accounts() -> sc_error::Result<()> {
    let mut source = setup().await?;
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/tables/users/fields",
            Some(json!({ "name": "nickname", "type": "string" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/users",
            Some(json!({
                "email": "sam@example.com",
                "password": "sams-password",
                "role": 100,
                "extra": { "nickname": "Sam" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A role Sam holds for a while (TODO-payments P3): the grant is Sam's, so it
    // travels with the accounts and still names Sam on the other side.
    let sam_id = body["user"]["id"].as_str().unwrap().to_owned();
    let (status, body) = source
        .client
        .send(
            "POST",
            &format!("/api/users/{sam_id}/role-grants"),
            Some(json!({ "role": 1, "expires_in_days": 30, "note": "on call" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let archive = backup_everything(&mut source).await;

    let mut target = setup().await?;
    let (status, bytes, _) = target
        .client
        .raw(
            "POST",
            "/backup/upload",
            Some((archive.clone(), "application/zip")),
        )
        .await;
    let uploaded: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let (status, report) = target
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": uploaded["id"], "include": uploaded["include"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");

    let (status, users) = target.client.send("GET", "/api/users", None).await;
    assert_eq!(status, StatusCode::OK, "{users}");
    let sam = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == json!("sam@example.com"))
        .unwrap_or_else(|| panic!("the restored account: {users}"));
    assert_eq!(sam["extra"]["nickname"], json!("Sam"));
    let (status, grants) = target
        .client
        .send(
            "GET",
            &format!("/api/users/{}/role-grants", sam["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{grants}");
    assert_eq!(grants["base_role"], json!(100));
    assert_eq!(grants["effective_role"], json!(1), "{grants}");
    assert_eq!(grants["grants"][0]["note"], json!("on call"));
    Ok(())
}

/// A real Saltcorn 1 backup, as `saltcorn backup` wrote it: the sample BooksDB
/// application, with three tables, an account, a file and two v1 actions.
///
/// Checked in rather than built here, because the claim is about *the file a v1
/// server produces* — a fixture assembled in this test would only assert that the
/// converter agrees with this test's idea of v1.
const V1_BACKUP: &[u8] = include_bytes!("fixtures/saltcorn-v1-BooksDB.zip");

/// Restoring a **Saltcorn 1** backup, through the same two requests the Backup
/// screen makes for one of ours.
///
/// The point of the test is that it goes through those two requests and reads the
/// result back through the ordinary admin API: an import is not a separate
/// feature with a separate screen, it is the same restore over an archive that
/// was translated on the way in. What is asserted is the half-dozen things that
/// translation has to get right — v1's types, its numbering of users, its one file
/// area, its actions — and, just as much, that everything it left behind is *said*.
#[tokio::test]
async fn a_saltcorn_1_backup_is_imported() -> sc_error::Result<()> {
    let mut server = setup().await?;
    // The store the import would create points into this machine's data
    // directory, so it is defined here first, at this test's own temp path. That
    // is also the arrangement an admin who prepared the server has, and the
    // restore keeps the definition it finds.
    sc_catalog::save_file_store(
        &server.catalog,
        &FileStoreDef::local("BooksDB", server.files.to_string_lossy()),
    )
    .await?;
    sc_catalog::connect_file_store_def(
        &server.catalog,
        &sc_catalog::load_file_store_by_name(&server.catalog, "BooksDB")
            .await?
            .expect("the store just saved"),
    )?;

    // Somebody is already served on `booksdb`, so the import has to find the
    // application a subdomain of its own (8.2).
    sc_app::save_application(
        &server.catalog,
        &sc_app::Application::new(
            "Squatter",
            "booksdb",
            sc_app::FrameworkRef::new(sc_viewpattern::SALTCORN_UI_FRAMEWORK),
        ),
    )
    .await?;

    let (status, bytes, _) = server
        .client
        .raw(
            "POST",
            "/backup/upload",
            Some((V1_BACKUP.to_vec(), "application/zip")),
        )
        .await;
    let uploaded: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    // The application the backup becomes is on offer like any other, with its
    // views and pages (8.5).
    assert_eq!(
        uploaded["available"]["applications"],
        json!([{ "name": "booksdb", "label": "BooksDB", "count": null }])
    );
    assert_eq!(uploaded["available"]["views"], json!(7));
    assert_eq!(uploaded["available"]["pages"], json!(1));
    assert_eq!(uploaded["include"]["views"], json!(true));
    assert_eq!(uploaded["include"]["pages"], json!(true));
    // The dialog is told what it is looking at, and when the data is from.
    assert_eq!(
        uploaded["source"],
        json!("Saltcorn 1.7.0-alpha.1, imported")
    );
    assert_eq!(uploaded["created_at"], json!("2026-09-12T16:50:42.895Z"));
    let tables: Vec<&str> = uploaded["available"]["tables"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(tables, vec!["Books", "Authors", "Publishers"]);
    assert_eq!(uploaded["available"]["users"], json!(1));
    assert_eq!(
        uploaded["available"]["file_stores"][0]["name"],
        json!("BooksDB")
    );

    let (status, report) = server
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": uploaded["id"], "include": uploaded["include"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let warned = |text: &str| {
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains(text))
    };

    // --- the tables, read back through the admin API -------------------------
    let (status, fields) = server
        .client
        .send("GET", "/api/tables/Books/fields", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{fields}");
    let field = |name: &str| -> Value {
        fields
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == json!(name))
            .unwrap_or_else(|| panic!("no `{name}` column: {fields}"))
            .clone()
    };
    // A v1 day-only Date is a date here, and a v1 Key is a reference to the table
    // it named, with the summary field it was displayed by.
    assert_eq!(field("published_on")["type"], json!("date"));
    assert_eq!(field("author")["kind"]["target_table"], json!("Authors"));
    assert_eq!(field("author")["kind"]["summary_field"], json!("last_name"));
    assert_eq!(field("author")["required"], json!(true));

    let (status, rows) = server
        .client
        .send("GET", "/api/tables/Books/rows", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["title"], json!("Moby Dick"));
    assert_eq!(rows[0]["published_on"], json!("2026-09-02"));
    // The foreign key still points at the author it pointed at, which is only
    // true because the rows kept the keys they had.
    assert_eq!(rows[0]["author"], json!(1));

    // …and the numbering was wound past them, so the next book written does not
    // collide with a restored one.
    let (status, added) = server
        .client
        .send(
            "POST",
            "/api/tables/Books/rows",
            Some(json!({ "title": "Solaris", "author": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert_eq!(added["id"], json!(3), "{added}");

    // --- the account ---------------------------------------------------------
    let (status, users) = server.client.send("GET", "/api/users", None).await;
    assert_eq!(status, StatusCode::OK, "{users}");
    let imported = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == json!("admin@foo.com"))
        .unwrap_or_else(|| panic!("the imported account: {users}"));
    // v1 numbers its users and this system gives them UUIDs, so the number it had
    // is the only way to recognise it in the v1 database afterwards.
    assert_eq!(imported["extra"]["legacy_id"], json!(1));
    assert_eq!(imported["role"], json!(1));
    // Its v1 bcrypt hash came with it, so it signs in with the password it had.
    assert!(!warned("with no password"), "{report}");

    // --- the file ------------------------------------------------------------
    let (status, listing) = server
        .client
        .send(
            "POST",
            "/api/file-stores/BooksDB/browse",
            Some(json!({ "dir": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    assert!(
        listing
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["path"] == json!("Each+session+is+30+mins.png")),
        "{listing}"
    );
    assert!(
        server.files.join("Each+session+is+30+mins.png").exists(),
        "the v1 upload is in this server's own directory"
    );

    // --- and what v1 has and this system does not ----------------------------
    //
    // Every one of these is a line an admin reads on the screen that ran the
    // restore, which is the difference between an import and a surprise.
    // v1's workflow crossed step for step: one form, its answer under the step.
    assert!(!warned("`AddBook`"), "{report}");
    assert!(
        report["restored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == &json!("trigger `AddBook`: with its workflow (1 step)")),
        "{report}"
    );
    // A v1 action this system does not have is refused by name by the ordinary
    // trigger validation, not quietly turned into the nearest thing that exists.
    assert!(warned("TrimPages") && warned("modify_row"), "{report}");

    // --- the application its views and page became (§13) --------------------
    let did = |report: &Value, text: &str| {
        report["restored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap_or_default().contains(text))
    };
    assert!(
        did(
            &report,
            "application `booksdb-2`: `booksdb` is another application's subdomain"
        ),
        "{report}"
    );
    assert!(
        did(&report, "7 views into application `booksdb-2`"),
        "{report}"
    );
    assert!(
        did(&report, "1 page into application `booksdb-2`"),
        "{report}"
    );
    // Every view came, and nothing links to a view that did not.
    assert!(!warned("view `"), "{report}");
    assert!(!warned("page `"), "{report}");
    // v1's menu is admin and user pages only; each kind is said (8.3).
    assert!(
        warned("Admin Page menu entries were not imported"),
        "{report}"
    );
    assert!(
        warned("User Page menu entries were not imported"),
        "{report}"
    );
    assert!(
        warned("the menu header `Settings` was not imported"),
        "{report}"
    );
    assert!(warned("1 tag was not imported"), "{report}");
    // This server was built with no Saltcorn UI bundle, so the application is
    // saved and not serving — and the report says which.
    assert!(
        warned("application `booksdb-2` is restored but could not be mounted"),
        "{report}"
    );

    let app = sc_app::load_application_by_subdomain(&server.catalog, "booksdb-2")
        .await?
        .unwrap_or_else(|| panic!("the imported application: {report}"));
    assert_eq!(app.name, "BooksDB");
    assert_eq!(app.framework.name, sc_viewpattern::SALTCORN_UI_FRAMEWORK);
    assert_eq!(app.framework.config["site_name"], json!("BooksDB"));
    assert_eq!(app.framework.config["menu_items"], json!([]));
    let names = |ids: Vec<&String>| ids.into_iter().cloned().collect::<Vec<_>>();
    assert_eq!(
        names(app.tables.iter().map(|t| &t.0).collect()),
        vec!["Books", "Authors", "Publishers"]
    );
    assert_eq!(
        names(app.file_stores.iter().map(|s| &s.0).collect()),
        vec!["BooksDB"]
    );
    assert_eq!(app.csp, sc_viewpattern::saltcorn_ui_csp());

    let pack: Value = serde_json::from_str(&entry(V1_BACKUP, "pack.json")).unwrap();
    let views = sc_viewpattern::list_views(&server.catalog, app.id).await?;
    assert_eq!(
        views.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
        vec![
            "Edit Authors",
            "Edit Books",
            "Filter books",
            "List Authors",
            "List Books",
            "Show Authors",
            "Show Books",
        ]
    );
    let view = |name: &str| views.iter().find(|v| v.name == name).unwrap();
    let v1_view = |name: &str| -> Value {
        pack["views"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == json!(name))
            .unwrap()
            .clone()
    };
    // The configuration crosses unchanged, and so do the role, the slug and the
    // attributes (8.5).
    for v in &views {
        assert_eq!(
            Value::Object(v.configuration.clone()),
            v1_view(&v.name)["configuration"],
            "{}",
            v.name
        );
        assert_eq!(v.min_role, 1, "{}", v.name);
    }
    assert_eq!(view("List Books").viewpattern, "List");
    assert_eq!(view("List Books").table_name.as_deref(), Some("Books"));
    assert_eq!(
        view("Filter books").slug,
        Some(json!({ "label": "", "steps": [] }))
    );
    assert_eq!(
        view("Filter books").attributes["popup_link_out"],
        json!(false)
    );
    assert_eq!(view("List Books").slug, None);

    let pages = sc_viewpattern::list_pages(&server.catalog, app.id).await?;
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].name, "BooksOverview");
    assert_eq!(pages[0].min_role, 1);
    assert_eq!(pages[0].layout, pack["pages"][0]["layout"]);
    assert_eq!(pages[0].attributes["root_page_for_roles"], json!([]));

    // --- and again, over an application an admin has changed since (8.6) ----
    let mut changed = app.clone();
    changed
        .framework
        .config
        .insert("allow_signup".to_owned(), json!(true));
    changed.csp = sc_app::CspPolicy::strict();
    sc_app::save_application(&server.catalog, &changed).await?;
    // A view deleted since comes back: the backup's views replace the app's.
    sc_viewpattern::delete_view(&server.catalog, app.id, "Show Books").await?;

    let (status, bytes, _) = server
        .client
        .raw(
            "POST",
            "/backup/upload",
            Some((V1_BACKUP.to_vec(), "application/zip")),
        )
        .await;
    let uploaded: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let (status, again) = server
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": uploaded["id"], "include": uploaded["include"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert!(
        did(&again, "application `booksdb-2`: already here as `BooksDB`"),
        "{again}"
    );
    assert!(
        did(&again, "7 views into application `booksdb-2`"),
        "{again}"
    );

    let imported: Vec<sc_app::Application> = sc_app::list_applications(&server.catalog)
        .await?
        .into_iter()
        .filter(|a| a.name == "BooksDB")
        .collect();
    assert_eq!(imported.len(), 1, "one application, not two");
    let kept = &imported[0];
    assert_eq!(kept.id, app.id);
    assert_eq!(kept.subdomain, "booksdb-2");
    assert_eq!(kept.framework.config["allow_signup"], json!(true));
    assert_eq!(kept.csp, sc_app::CspPolicy::strict());
    assert_eq!(
        sc_viewpattern::list_views(&server.catalog, app.id)
            .await?
            .len(),
        7,
        "seven views, not fourteen"
    );
    assert_eq!(
        sc_viewpattern::list_pages(&server.catalog, app.id)
            .await?
            .len(),
        1
    );
    Ok(())
}

/// The BooksDB backup with a v1 library and a page with legacy fixed states
/// added; `fixtures/README.md` says how it was made and from what.
const V1_LIBRARY_BACKUP: &[u8] = include_bytes!("fixtures/saltcorn-v1-BooksDB-library.zip");

/// Upload `archive` and restore everything it offers, returning the report.
async fn restore_everything(server: &mut Server, archive: &[u8]) -> Value {
    let (status, bytes, _) = server
        .client
        .raw(
            "POST",
            "/backup/upload",
            Some((archive.to_vec(), "application/zip")),
        )
        .await;
    let uploaded: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    let (status, report) = server
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": uploaded["id"], "include": uploaded["include"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    report
}

/// A Saltcorn 1 backup of an order-entry app with the things the first import
/// left behind (a redacted copy of a real one: its LLM key is a placeholder):
/// stored calculated fields — two aggregations, a calculation and a join — a
/// key from `users` onto `order`, a bcrypt password, an `Agent` action from
/// v1's agents plugin and a workflow.
const V1_ORDERS_BACKUP: &[u8] = include_bytes!("fixtures/saltcorn-v1-feldsparimport.zip");

/// What the import of [`V1_ORDERS_BACKUP`] has to get right, read back through
/// the admin API and then *used*: the calculated fields compute, the user's
/// reference is there, the agent has its tools, and the workflow runs.
#[tokio::test]
async fn a_v1_backup_with_calculated_fields_an_agent_and_a_workflow_is_imported()
-> sc_error::Result<()> {
    let mut server = setup().await?;
    let report = restore_everything(&mut server, V1_ORDERS_BACKUP).await;
    let warned = |text: &str| report_has(&report, "warnings", text);

    // --- stored calculated fields, as calculated fields ----------------------
    let calc = |fields: &Value, name: &str| -> Value {
        fields
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == json!(name))
            .unwrap_or_else(|| panic!("no `{name}`: {fields} / {report}"))["kind"]
            .clone()
    };
    let (_, order) = server
        .client
        .send("GET", "/api/tables/order/fields", None)
        .await;
    let (_, lines) = server
        .client
        .send("GET", "/api/tables/order_line/fields", None)
        .await;
    assert_eq!(
        calc(&order, "total")["expression"],
        json!("order_lineↃorder.sum(r => r.qty*r.unit_price)")
    );
    assert_eq!(
        calc(&order, "n_for_gift")["expression"],
        json!("order_lineↃorder.filter(r => r.gift === true).length")
    );
    assert_eq!(calc(&lines, "sum")["expression"], json!("qty*unit_price"));
    assert_eq!(
        calc(&lines, "for_export")["expression"],
        json!("orderⱵfor_export")
    );

    // And they compute, from the rows v1 stored the inputs of.
    let (status, rows) = server
        .client
        .send("GET", "/api/tables/order/rows", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let first = &rows[0];
    assert_eq!(first["name"], json!("My things"));
    assert!(
        (first["total"].as_f64().unwrap_or_default() - 9.95).abs() < 1e-9,
        "{rows}"
    );
    assert_eq!(first["n_for_gift"], json!(1), "{rows}");

    // --- the users table's key onto a table of the backup ---------------------
    let (_, user_fields) = server
        .client
        .send("GET", "/api/tables/users/fields", None)
        .await;
    assert_eq!(
        calc(&user_fields, "current_order")["target_table"],
        json!("order")
    );
    assert!(!warned("current_order"), "{report}");

    // --- the account keeps its v1 password ------------------------------------
    let hash: String = server
        .db
        .client()
        .await?
        .query_one(
            "SELECT password_hash FROM users WHERE email = 'tanielsen@gmail.com'",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?
        .get(0);
    assert!(hash.starts_with("$2a$10$"), "{hash}");
    assert!(!warned("with no password"), "{report}");

    // --- the agent, its tools and its provider --------------------------------
    let (_, agents) = server.client.send("GET", "/api/agents", None).await;
    let agent = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!("MyAgent"))
        .unwrap_or_else(|| panic!("the agent: {agents} / {report}"));
    assert_eq!(agent["provider"], json!("openai"));
    let traits: Vec<&str> = agent["traits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["trait"].as_str())
        .collect();
    assert_eq!(
        traits,
        vec!["run_js_code", "query_table", "insert_row"],
        "{agent}"
    );
    // The JavaScript tool is the agent's own, its code as v1 wrote it.
    assert_eq!(
        agent["traits"][0]["config"]["tool_name"],
        json!("word_of_the_day")
    );
    assert_eq!(
        agent["traits"][0]["config"]["code"],
        json!("return \"Strawberry\"")
    );

    // --- the workflow, which runs ---------------------------------------------
    let (_, triggers) = server.client.send("GET", "/api/triggers", None).await;
    let id_of = |name: &str| {
        triggers
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == json!(name))
            .unwrap_or_else(|| panic!("no trigger `{name}`: {triggers} / {report}"))["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        report_has(&report, "restored", "with its workflow (4 steps)"),
        "{report}"
    );
    let (status, ran) = server
        .client
        .send(
            "POST",
            &format!("/api/triggers/{}/run", id_of("MyWorkflow")),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ran}");
    assert_eq!(ran["result"]["state"], json!("done"), "{ran}");
    // v1's `{id}` reads `id` out of the run's context, which this loop never
    // sets — v1 binds it as `id = NULL` — so, faithfully, it matches no order
    // and nothing is discounted.
    let (_, rows) = server
        .client
        .send("GET", "/api/tables/order/rows", None)
        .await;
    assert_eq!(rows[0]["discount"], Value::Null, "{ran} {rows}");

    // The same workflow with the query v1 needed — `{id: order.id}`, the loop's
    // item — into a fresh server: the loop and the row update do run.
    let fixed = with_pack(V1_ORDERS_BACKUP, |pack| {
        let steps = pack["triggers"][1]["steps"].as_array_mut().unwrap();
        let modify = steps
            .iter_mut()
            .find(|s| s["name"] == json!("step3"))
            .unwrap();
        modify["configuration"]["query"] = json!("{id: order.id}");
    });
    let mut second = setup().await?;
    restore_everything(&mut second, &fixed).await;
    let (_, triggers) = second.client.send("GET", "/api/triggers", None).await;
    let id = triggers
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("MyWorkflow"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, ran) = second
        .client
        .send("POST", &format!("/api/triggers/{id}/run"), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{ran}");
    assert_eq!(ran["result"]["state"], json!("done"), "{ran}");
    let (_, rows) = second
        .client
        .send("GET", "/api/tables/order/rows", None)
        .await;
    assert_eq!(rows[0]["discount"], json!(0.15), "{ran} {rows}");

    // …and not a trigger: nothing was added to the instance's trigger list.
    assert!(
        !triggers
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == json!("word_of_the_day")),
        "{triggers}"
    );
    Ok(())
}

/// Two things a real v1 import tripped on: a key whose summary field is a
/// (stored) calculated field of its target, and an `Array_Agg` aggregation. And
/// a summary field v1 kept after the field it named was deleted.
#[tokio::test]
async fn a_v1_calculated_summary_field_and_an_array_aggregation_are_imported()
-> sc_error::Result<()> {
    let archive = with_pack(V1_ORDERS_BACKUP, |pack| {
        let tables = pack["tables"].as_array_mut().unwrap();
        for table in tables.iter_mut() {
            let name = table["name"].as_str().unwrap().to_owned();
            let fields = table["fields"].as_array_mut().unwrap();
            match name.as_str() {
                "order" => {
                    fields.push(json!({
                        "name": "label", "label": "Label", "type": "String",
                        "calculated": true, "stored": true, "expression": "name + '!'",
                        "attributes": {}
                    }));
                    fields.push(json!({
                        "name": "qtys", "label": "Qtys", "type": "JSON",
                        "calculated": true, "stored": true, "expression": "__aggregation",
                        "attributes": { "table": "order_line", "ref": "order",
                                        "agg_field": "qty@Integer", "aggregate": "Array_Agg",
                                        "aggwhere": "" }
                    }));
                }
                "order_line" | "users" => {
                    let summary = if name == "users" {
                        "deleted_long_ago"
                    } else {
                        "label"
                    };
                    for field in fields.iter_mut() {
                        if field["type"] == json!("Key") {
                            field["attributes"]["summary_field"] = json!(summary);
                        }
                    }
                }
                _ => {}
            }
        }
    });
    let mut server = setup().await?;
    let report = restore_everything(&mut server, &archive).await;
    let kind = |fields: &Value, name: &str| -> Value {
        fields
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == json!(name))
            .unwrap_or_else(|| panic!("no `{name}`: {fields} / {report}"))["kind"]
            .clone()
    };
    let (_, lines) = server
        .client
        .send("GET", "/api/tables/order_line/fields", None)
        .await;
    assert_eq!(
        kind(&lines, "order")["summary_field"],
        json!("label"),
        "{report}"
    );
    let (_, users) = server
        .client
        .send("GET", "/api/tables/users/fields", None)
        .await;
    assert_eq!(
        kind(&users, "current_order")["summary_field"],
        json!("id"),
        "{report}"
    );
    let (_, order) = server
        .client
        .send("GET", "/api/tables/order/fields", None)
        .await;
    assert_eq!(
        kind(&order, "qtys")["expression"],
        json!("order_lineↃorder.map(r => r.qty)")
    );
    assert!(!report_has(&report, "warnings", "qtys"), "{report}");
    assert!(!report_has(&report, "warnings", "summary"), "{report}");

    let (status, rows) = server
        .client
        .send("GET", "/api/tables/order/rows", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[0]["label"], json!("My things!"), "{rows}");
    assert_eq!(rows[0]["qtys"], json!([5]), "{rows}");
    Ok(())
}

/// A server with the `BooksDB` store defined at its own temp path, which is
/// where a BooksDB import's files go.
async fn setup_for_booksdb() -> sc_error::Result<Server> {
    let server = setup().await?;
    sc_catalog::save_file_store(
        &server.catalog,
        &FileStoreDef::local("BooksDB", server.files.to_string_lossy()),
    )
    .await?;
    sc_catalog::connect_file_store_def(
        &server.catalog,
        &sc_catalog::load_file_store_by_name(&server.catalog, "BooksDB")
            .await?
            .expect("the store just saved"),
    )?;
    Ok(server)
}

fn report_has(report: &Value, key: &str, text: &str) -> bool {
    report[key]
        .as_array()
        .unwrap()
        .iter()
        .any(|line| line.as_str().unwrap_or_default().contains(text))
}

/// The builder 4.1, 4.2 and 4.4: a v1 library and a legacy page, imported, then
/// carried by this server's own backup into a second installation.
///
/// The import gives each item a UUID and rewrites every placement to it — in a
/// view, in a page, and in the item that places the other — folds the page's
/// `fixed_states` into its `view` segment, and says how many items came. A
/// second import replaces the library rather than doubling it. This server's
/// backup then writes `library.json` beside `views.json`, and the restore puts
/// the items back with their ids, before the views and pages that place them,
/// reporting a placement of an item that is not there.
#[tokio::test]
async fn a_v1_library_is_imported_and_travels_in_a_backup() -> sc_error::Result<()> {
    let mut server = setup_for_booksdb().await?;
    let report = restore_everything(&mut server, V1_LIBRARY_BACKUP).await;
    let did = |text: &str| report_has(&report, "restored", text);
    let warned = |text: &str| report_has(&report, "warnings", text);
    assert!(
        did("2 library items into application `booksdb`"),
        "{report}"
    );
    assert!(did("7 views into application `booksdb`"), "{report}");
    assert!(did("2 pages into application `booksdb`"), "{report}");
    // The library is imported now, and every placement found its item.
    assert!(!warned("library"), "{report}");

    let app = sc_app::load_application_by_subdomain(&server.catalog, "booksdb")
        .await?
        .unwrap_or_else(|| panic!("the imported application: {report}"));
    let library = sc_viewpattern::list_library(&server.catalog, app.id).await?;
    assert_eq!(
        library.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        vec!["Book header", "Book note"]
    );
    let (header, note) = (&library[0], &library[1]);
    assert_eq!(header.icon, "fas fa-book");
    let id_of = |item: &sc_viewpattern::LibraryItem| json!(item.id.0.to_string());
    // Serial 2, inside serial 1's own layout.
    assert_eq!(header.layout["above"][2]["library_id"], id_of(note));

    let show = sc_viewpattern::load_view(&server.catalog, app.id, "Show Books")
        .await?
        .expect("Show Books");
    let placed = &show.configuration["layout"]["above"][0];
    assert_eq!(placed["library_id"], id_of(header));
    assert_eq!(placed["slots"][0]["field"], json!("title"));

    let featured = sc_viewpattern::load_page(&server.catalog, app.id, "Featured book")
        .await?
        .expect("Featured book");
    assert_eq!(featured.layout["above"][0]["library_id"], id_of(note));
    // The fold: the legacy state is on the segment, and `fixed_states` is gone.
    assert_eq!(
        featured.layout["above"][1]["configuration"],
        json!({ "id": 2 })
    );
    assert!(featured.attributes.get("fixed_states").is_none());
    // The references walk finds the placements, through the nested item too.
    let set = sc_viewpattern::ViewSet::load(&server.catalog, app.id, 0).await?;
    let refs = set.library_references(note.id);
    assert_eq!(refs.views, vec!["Show Books"]);
    assert_eq!(refs.pages, vec!["Featured book"]);
    assert_eq!(refs.library, vec!["Book header"]);

    // --- again: replaced, not doubled, and still consistent ------------------
    let again = restore_everything(&mut server, V1_LIBRARY_BACKUP).await;
    assert!(
        report_has(
            &again,
            "restored",
            "2 library items into application `booksdb`"
        ),
        "{again}"
    );
    assert!(!report_has(&again, "warnings", "library"), "{again}");
    let library = sc_viewpattern::list_library(&server.catalog, app.id).await?;
    assert_eq!(library.len(), 2, "two items, not four");
    let show = sc_viewpattern::load_view(&server.catalog, app.id, "Show Books")
        .await?
        .expect("Show Books");
    assert_eq!(
        show.configuration["layout"]["above"][0]["library_id"],
        json!(library[0].id.0.to_string())
    );

    // --- this server's backup, into a second installation --------------------
    // A page placing an item that does not exist, which the restore reports.
    let dangling = sc_viewpattern::LibraryItemId::new().0.to_string();
    sc_viewpattern::save_page(
        &server.catalog,
        &sc_viewpattern::Page::new(app.id, "Dangling")
            .layout(json!({ "type": "library", "library_id": dangling })),
    )
    .await?;
    let archive = backup_everything(&mut server).await;
    let written: Value =
        serde_json::from_str(&entry(&archive, "applications/booksdb/library.json"))
            .expect("library.json is JSON");
    assert_eq!(written.as_array().map(Vec::len), Some(2), "{written}");
    assert_eq!(written[0]["id"], json!(library[0].id.0.to_string()));

    let mut second = setup_for_booksdb().await?;
    let restored = restore_everything(&mut second, &archive).await;
    assert!(
        report_has(
            &restored,
            "restored",
            "2 library items into application `booksdb`"
        ),
        "{restored}"
    );
    assert!(
        report_has(
            &restored,
            "warnings",
            &format!(
                "page `Dangling` places the library item `{dangling}`, which was not imported"
            )
        ),
        "{restored}"
    );
    // Nothing else is reported about the library.
    let library_warnings = restored["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w.as_str().unwrap_or_default().contains("library"))
        .count();
    assert_eq!(library_warnings, 1, "{restored}");

    let copy = sc_app::load_application_by_subdomain(&second.catalog, "booksdb")
        .await?
        .unwrap_or_else(|| panic!("the restored application: {restored}"));
    assert_eq!(copy.id, app.id);
    let copied = sc_viewpattern::list_library(&second.catalog, copy.id).await?;
    assert_eq!(copied, library, "the same items, ids and layouts");
    let copied_show = sc_viewpattern::load_view(&second.catalog, copy.id, "Show Books")
        .await?
        .expect("Show Books");
    assert_eq!(copied_show.configuration, show.configuration);
    Ok(())
}

#[tokio::test]
async fn what_a_backup_includes_is_remembered_as_what_was_left_out() -> sc_error::Result<()> {
    let mut server = setup().await?;
    furnish(&mut server).await?;

    // Everything, to begin with.
    let (status, options) = server.client.send("GET", "/api/backup", None).await;
    assert_eq!(status, StatusCode::OK, "{options}");
    assert_eq!(options["include"]["tables"], json!(["books"]));
    assert_eq!(options["include"]["table_data"], json!(["books"]));
    assert_eq!(options["include"]["users"], json!(true));
    assert_eq!(options["available"]["tables"][0]["count"], json!(2));

    // A narrowed backup: the definitions but not the rows, and no users.
    let (status, bytes, _) = server
        .client
        .raw(
            "POST",
            "/backup/create",
            Some((
                serde_json::to_vec(&json!({ "include": {
                    "tables": ["books"],
                    "table_data": [],
                    "applications": [],
                    "file_stores": ["assets"],
                    "users": false,
                    "agents": false,
                    "triggers": true,
                    "ssl": true,
                }}))
                .unwrap(),
                "application/json",
            )),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    // The rows really were left out of the file, not merely absent from the dialog.
    assert!(has_entry(&bytes, "tables/books/table.json"));
    assert!(!has_entry(&bytes, "tables/books/rows.ndjson"));
    assert!(!has_entry(&bytes, "users/users.json"));
    assert!(has_entry(&bytes, "triggers.json"));

    // Reopened, the dialog shows the tuned selection…
    let (status, options) = server.client.send("GET", "/api/backup", None).await;
    assert_eq!(status, StatusCode::OK, "{options}");
    assert_eq!(options["include"]["table_data"], json!([]));
    assert_eq!(options["include"]["users"], json!(false));
    assert_eq!(options["include"]["triggers"], json!(true));

    // …and it is stored as *exclusions*, which is what makes a table created later
    // part of the next backup without anybody going back to the dialog.
    let stored = stored_config(&server.catalog, sc_config::BACKUP_INCLUDE)
        .await?
        .expect("the remembered selection");
    assert_eq!(stored["exclude_table_data"], json!(["books"]));
    assert_eq!(stored["exclude_tables"], json!([]));
    assert_eq!(stored["users"], json!(false));

    server
        .catalog
        .create_table(
            "reviews",
            &[DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key()],
        )
        .await?;
    let (status, options) = server.client.send("GET", "/api/backup", None).await;
    assert_eq!(status, StatusCode::OK, "{options}");
    assert_eq!(options["include"]["tables"], json!(["books", "reviews"]));
    // The new table's rows are in; the one that was unticked stays unticked.
    assert_eq!(options["include"]["table_data"], json!(["reviews"]));
    Ok(())
}

/// Rows without their table's metadata would be rows nothing could read, so the
/// server narrows the payload rather than trusting it.
/// A vector field, its values and its HNSW index cross a backup
/// (TODO-agent-skills AS2.6): the field as `vector(3)` with its attributes,
/// each value as the array it was, and the index as the constraint it is.
#[tokio::test]
async fn a_vector_field_its_values_and_its_index_travel_in_a_backup() -> sc_error::Result<()> {
    let Some(db) = TestDb::with_pgvector().await? else {
        return Ok(());
    };
    let mut source = setup_in(db).await?;
    let client = &mut source.client;
    let (status, body) = client
        .send("POST", "/api/tables", Some(json!({ "name": "docs" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for field in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "title", "type": "text" }),
        json!({ "name": "embedding", "type": "vector", "attributes": { "dimensions": 3 } }),
    ] {
        let (status, body) = client
            .send("POST", "/api/tables/docs/fields", Some(field))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/docs/constraints",
            Some(json!({ "type": "vector_index", "fields": ["embedding"] })),
        )
        .await;
    assert!(status.is_success(), "{body}");
    for (title, v) in [("east", json!([1, 0, 0])), ("north", json!([0, 0.5, 0.25]))] {
        let (status, body) = client
            .send(
                "POST",
                "/api/tables/docs/rows",
                Some(json!({ "title": title, "embedding": v })),
            )
            .await;
        assert!(status.is_success(), "{body}");
    }
    let archive = backup_everything(&mut source).await;
    let rows = entry(&archive, "tables/docs/rows.ndjson");
    assert!(rows.contains("[0.0,0.5,0.25]"), "{rows}");

    let Some(db) = TestDb::with_pgvector().await? else {
        return Ok(());
    };
    let mut target = setup_in(db).await?;
    restore_everything(&mut target, &archive).await;
    let (status, fields) = target
        .client
        .send("GET", "/api/tables/docs/fields", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{fields}");
    let embedding = fields
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "embedding")
        .cloned()
        .expect("the vector field");
    assert_eq!(embedding["type"], json!("vector(3)"), "{embedding}");
    let (status, rows) = target
        .client
        .send("GET", "/api/tables/docs/rows?order=id", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows[1]["embedding"], json!([0.0, 0.5, 0.25]), "{rows}");
    let (_, constraints) = target
        .client
        .send("GET", "/api/tables/docs/constraints", None)
        .await;
    assert!(
        constraints
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "vector_index" && c["fields"] == json!(["embedding"])),
        "{constraints}"
    );
    Ok(())
}

#[tokio::test]
async fn rows_are_never_backed_up_without_their_table() -> sc_error::Result<()> {
    let mut server = setup().await?;
    furnish(&mut server).await?;

    let (status, bytes, _) = server
        .client
        .raw(
            "POST",
            "/backup/create",
            Some((
                serde_json::to_vec(&json!({ "include": {
                    "tables": [],
                    "table_data": ["books"],
                    "applications": [],
                    "file_stores": [],
                    "users": true,
                    "agents": false,
                    "triggers": false,
                    "ssl": false,
                }}))
                .unwrap(),
                "application/json",
            )),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(!has_entry(&bytes, "tables/books/rows.ndjson"));
    assert!(!has_entry(&bytes, "tables/books/table.json"));
    assert!(has_entry(&bytes, "users/users.json"));
    Ok(())
}

/// A TLS key this host pins in `feldspar.toml` is not restored over: the file
/// wins, so the backup's value is reported as kept from the file and the rest
/// of the section is restored beside it. The settings screen is told which keys
/// are pinned, so it can show them read-only.
#[tokio::test]
async fn a_restore_leaves_the_tls_keys_the_host_pins_alone() -> sc_error::Result<()> {
    let mut source = setup().await?;
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": {
                SSL_MODE: sc_config::MODE_LETSENCRYPT,
                sc_config::ACME_CONTACT_EMAIL: "ops@example.com",
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let archive = backup_everything(&mut source).await;
    assert!(has_entry(&archive, "settings/ssl.json"));

    let mut target = setup().await?;
    sc_config::set_host_config(
        &target.catalog,
        json!({ SSL_MODE: sc_config::MODE_OFF })
            .as_object()
            .unwrap()
            .clone(),
    )?;
    let (_, settings) = target.client.send("GET", "/api/settings", None).await;
    assert_eq!(settings["host_keys"], json!([SSL_MODE]), "{settings}");
    assert_eq!(settings["values"][SSL_MODE], json!(sc_config::MODE_OFF));

    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(&report, "restored", "kept from this host's feldspar.toml"),
        "{report}"
    );
    assert!(!report_has(&report, "warnings", "SSL"), "{report}");
    assert_eq!(stored_config(&target.catalog, SSL_MODE).await?, None);
    assert_eq!(
        stored_config(&target.catalog, sc_config::ACME_CONTACT_EMAIL).await?,
        Some(json!("ops@example.com"))
    );
    Ok(())
}

/// A zip that is not a backup is refused where the admin can see it, rather than
/// restored as nothing.
#[tokio::test]
async fn a_file_that_is_not_a_backup_is_refused_by_name() -> sc_error::Result<()> {
    let mut server = setup().await?;

    let (status, body, _) = server
        .client
        .raw(
            "POST",
            "/backup/upload",
            Some((b"this is not a zip at all".to_vec(), "application/zip")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = String::from_utf8_lossy(&body);
    assert!(message.contains("not a zip"), "{message}");

    // A real zip with nothing of ours in it: a different mistake, and told apart.
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("holiday.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut zip, b"photographs").unwrap();
    let not_a_backup = zip.finish().unwrap().into_inner();
    let (status, body, _) = server
        .client
        .raw(
            "POST",
            "/backup/upload",
            Some((not_a_backup, "application/zip")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = String::from_utf8_lossy(&body);
    assert!(message.contains("manifest.json"), "{message}");

    // And a restore naming an upload nobody made.
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "include": { "tables": [], "table_data": [], "applications": [],
                             "file_stores": [], "users": false, "agents": false,
                             "triggers": false, "ssl": false },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    Ok(())
}

/// Backing up is reading the whole installation and restoring is writing it, so
/// both are admin-only — and so is asking what there is to back up.
#[tokio::test]
async fn every_backup_route_is_admin_only() -> sc_error::Result<()> {
    let mut server = setup().await?;
    let (status, _) = server.client.send("POST", "/api/logout", None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = server.client.send("GET", "/api/backup", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = server
        .client
        .raw(
            "POST",
            "/backup/create",
            Some((b"{}".to_vec(), "application/json")),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = server
        .client
        .raw(
            "POST",
            "/backup/upload",
            Some((b"zip".to_vec(), "application/zip")),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = server
        .client
        .send("POST", "/api/backup/restore", Some(json!({ "id": "x" })))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = server
        .client
        .send(
            "POST",
            "/api/backup/unlock",
            Some(json!({ "id": "x", "password": "p" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

/// The data directory a store restored from another machine is moved into.
///
/// Set once for the whole binary, as `git_store_api`'s is: the environment is
/// process-wide and these tests run in parallel. Only this test defines a store
/// the restore has to place; every other one prepares its store first.
fn data_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = temp_dir();
        unsafe { std::env::set_var(sc_files::DATA_DIR_ENV, &dir) };
        dir
    })
}

/// A store restored onto a server where its directory does not exist goes where
/// this server puts its stores; one whose directory does exist (a shared drive)
/// keeps it.
#[tokio::test]
async fn a_restored_store_whose_directory_is_missing_is_placed_in_the_data_directory()
-> sc_error::Result<()> {
    let data = data_dir().to_owned();
    let mut source = setup().await?;
    let shared = temp_dir();
    for (name, path) in [("moved", &source.files), ("shared", &shared)] {
        let (status, body) = source
            .client
            .send(
                "POST",
                "/api/file-stores",
                Some(json!({
                    "name": name,
                    "backend": "local",
                    "config": { "path": path.to_string_lossy() },
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = source
            .client
            .send(
                "POST",
                &format!("/api/file-stores/{name}/write"),
                Some(json!({ "path": "a/b.txt", "text": format!("from {name}") })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    // An executable script and a read-only file, whose modes travel with them.
    {
        use std::os::unix::fs::PermissionsExt;
        let script = source.files.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let frozen = source.files.join("frozen.txt");
        std::fs::write(&frozen, "do not touch").unwrap();
        std::fs::set_permissions(&frozen, std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    let archive = backup_everything(&mut source).await;
    // The machine the backup was taken on, gone: its store directory with it.
    std::fs::remove_dir_all(&source.files).unwrap();

    let mut target = setup().await?;
    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(&report, "restored", "does not exist on this server"),
        "{report}"
    );

    let (status, stores) = target.client.send("GET", "/api/file-stores", None).await;
    assert_eq!(status, StatusCode::OK, "{stores}");
    let path_of = |name: &str| {
        stores
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == json!(name))
            .unwrap_or_else(|| panic!("store `{name}` restored: {stores}"))["config"]["path"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let moved = data.join("local-stores").join("moved");
    assert_eq!(path_of("moved"), moved.to_string_lossy());
    assert_eq!(path_of("shared"), shared.to_string_lossy());
    assert!(!source.files.exists(), "nothing recreated at the old path");

    // The files went with each store to where it now is.
    assert_eq!(
        std::fs::read_to_string(moved.join("a/b.txt")).unwrap(),
        "from moved"
    );
    assert_eq!(
        std::fs::read_to_string(shared.join("a/b.txt")).unwrap(),
        "from shared"
    );

    // …and so did their modes.
    let mode = |path: std::path::PathBuf| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    };
    assert_eq!(mode(moved.join("run.sh")), 0o755);
    assert_eq!(mode(moved.join("frozen.txt")), 0o444);

    // The same backup restored again overwrites the read-only file rather than
    // failing on it, and leaves it read-only.
    let again = restore_everything(&mut target, &archive).await;
    assert!(!report_has(&again, "warnings", "frozen.txt"), "{again}");
    assert_eq!(mode(moved.join("frozen.txt")), 0o444);
    Ok(())
}

/// Run git in `dir`, returning its output and asserting it worked.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("running `git {}`: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "`git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The same zip with its `pack.json` edited by `edit` — a variant of a v1
/// fixture without a second binary checked in.
fn with_pack(archive: &[u8], edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut source = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("a zip");
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut edit = Some(edit);
    for i in 0..source.len() {
        let mut file = source.by_index(i).unwrap();
        let name = file.name().to_owned();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        if name == "pack.json" {
            let mut pack: Value = serde_json::from_slice(&bytes).unwrap();
            (edit.take().unwrap())(&mut pack);
            bytes = serde_json::to_vec(&pack).unwrap();
        }
        out.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut out, &bytes).unwrap();
    }
    out.finish().unwrap().into_inner()
}

/// The same zip without the entries `drop` picks out.
fn without_entries(archive: &[u8], drop: impl Fn(&str) -> bool) -> Vec<u8> {
    let mut source = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("a zip");
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for i in 0..source.len() {
        let file = source.by_index_raw(i).unwrap();
        if !drop(file.name()) {
            out.raw_copy_file(file).unwrap();
        }
    }
    out.finish().unwrap().into_inner()
}

/// A git store restored onto another machine comes back as a working copy: from
/// the backup's own `.git` when it carries one (unpushed commits and uncommitted
/// edits included, no network needed), and by cloning its URL when it does not.
#[tokio::test]
async fn a_restored_git_store_is_checked_out_from_the_backup_or_cloned() -> sc_error::Result<()> {
    let data = data_dir().to_owned();
    // The remote both stores were cloned from: one commit.
    let remotes = temp_dir();
    let bare = remotes.join("origin.git");
    let seed = remotes.join("seed");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&seed).unwrap();
    git(&bare, &["init", "--bare", "--initial-branch=main", "."]);
    git(&seed, &["init", "--initial-branch=main", "."]);
    std::fs::write(seed.join("README.md"), "# from the remote\n").unwrap();
    // Executable in the repository: a restore that lost the bit would show it
    // as a change.
    std::fs::write(seed.join("build.sh"), "#!/bin/sh\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            seed.join("build.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    git(&seed, &["add", "-A"]);
    git(
        &seed,
        &[
            "-c",
            "user.email=s@example.com",
            "-c",
            "user.name=S",
            "commit",
            "-m",
            "initial",
        ],
    );
    git(&seed, &["push", &bare.to_string_lossy(), "main"]);

    let mut source = setup().await?;
    let checkouts = temp_dir();
    for name in ["restoredrepo", "clonedrepo"] {
        let (status, body) = source
            .client
            .send(
                "POST",
                "/api/file-stores",
                Some(json!({
                    "name": name,
                    "backend": "git",
                    "config": {
                        "url": bare.to_string_lossy(),
                        "directory": checkouts.join(name).to_string_lossy(),
                    },
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        // A commit that was never pushed, and an edit that was never committed.
        let tree = checkouts.join(name);
        std::fs::write(tree.join("local.txt"), "committed here only\n").unwrap();
        git(&tree, &["add", "-A"]);
        git(
            &tree,
            &[
                "-c",
                "user.email=s@example.com",
                "-c",
                "user.name=S",
                "commit",
                "-m",
                "unpushed",
            ],
        );
        std::fs::write(tree.join("README.md"), "# edited, not committed\n").unwrap();
    }
    let archive = backup_everything(&mut source).await;
    assert!(has_entry(
        &archive,
        "file-stores/restoredrepo/files/.git/HEAD"
    ));
    // A backup of `clonedrepo` with no checkout in it.
    let archive = without_entries(&archive, |entry| {
        entry.starts_with("file-stores/clonedrepo/files/.git/")
    });
    // The machine the backup was taken on, gone.
    std::fs::remove_dir_all(&checkouts).unwrap();

    let mut target = setup().await?;
    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(&report, "restored", "working copy of `restoredrepo`"),
        "{report}"
    );

    let (status, stores) = target.client.send("GET", "/api/file-stores", None).await;
    assert_eq!(status, StatusCode::OK, "{stores}");
    for name in ["restoredrepo", "clonedrepo"] {
        let store = stores
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == json!(name))
            .unwrap_or_else(|| panic!("store `{name}` restored: {stores}"));
        assert_eq!(store["connected"], json!(true), "{store}");
    }

    // From the backup: the checkout exactly as it was.
    let restored = data.join("git-stores").join("restoredrepo");
    assert_eq!(git(&restored, &["log", "-1", "--format=%s"]), "unpushed");
    assert_eq!(
        std::fs::read_to_string(restored.join("README.md")).unwrap(),
        "# edited, not committed\n"
    );
    assert_eq!(git(&restored, &["status", "--porcelain"]), "M README.md");

    // Cloned: the remote's history, with the backup's files laid over it.
    let cloned = data.join("git-stores").join("clonedrepo");
    assert_eq!(git(&cloned, &["log", "-1", "--format=%s"]), "initial");
    assert_eq!(
        std::fs::read_to_string(cloned.join("README.md")).unwrap(),
        "# edited, not committed\n"
    );
    assert!(cloned.join("local.txt").exists());
    Ok(())
}

/// An LLM provider — key, models and default included — and the agent calling
/// through it, so a restore has the dependency the agent's validation checks.
async fn furnish_llm(server: &mut Server) -> sc_error::Result<()> {
    sc_llm::save_llm_provider(
        &server.catalog,
        &sc_llm::LlmProviderDef::new("house", sc_llm::ANTHROPIC_BACKEND)
            .with(sc_llm::CFG_API_KEY, "sk-ant-backed-up"),
    )
    .await?;
    let provider = sc_llm::require_llm_provider(&server.catalog, "house").await?;
    sc_llm::save_llm_model(
        &server.catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-sonnet-4-5").default_model(),
    )
    .await?;
    sc_llm::save_llm_model(
        &server.catalog,
        &sc_llm::LlmModelDef::new(provider.id, "claude-opus-4-1"),
    )
    .await?;
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/agents",
            Some(json!({
                "name": "librarian",
                "description": "answers questions",
                "provider": "house",
                "model": "claude-opus-4-1",
                "system_prompt": "You are helpful.",
                "traits": [],
                "min_role": null,
                "attributes": {},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Ok(())
}

/// An agent whose trait still fails once the triggers are in — it names a
/// trigger the backup does not carry — is restored without that trait, which is
/// reported, rather than refused whole.
#[tokio::test]
async fn an_agent_is_restored_without_the_trait_naming_a_missing_trigger() -> sc_error::Result<()> {
    let mut source = setup().await?;
    furnish_llm(&mut source).await?;
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "say_hello", "description": "says hello", "when": "none",
                "channel": null, "action": "run_js_code",
                "configuration": { "code": "return 'hello'" },
                "min_role": null, "enabled": true,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/agents",
            Some(json!({
                "name": "greeter", "description": "", "provider": "house", "model": null,
                "system_prompt": "", "min_role": null, "attributes": {},
                "traits": [
                    { "trait": "run_trigger", "config": { "trigger": "say_hello" } },
                    { "trait": "run_js_code", "config": { "tool_name": "word",
                        "description": "a word", "code": "return 'yes'", "arguments": [] } },
                ],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let archive = without_entries(&backup_everything(&mut source).await, |name| {
        name == "triggers.json"
    });

    let mut target = setup().await?;
    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(&report, "restored", "agent `greeter`"),
        "{report}"
    );
    assert!(
        report_has(
            &report,
            "warnings",
            "agent `greeter`: the trait `run_trigger`"
        ),
        "{report}"
    );
    let agent = sc_agent::load_agent_by_name(&target.catalog, "greeter")
        .await?
        .expect("the agent, restored");
    let traits: Vec<&str> = agent.traits.iter().map(|t| t.trait_.as_str()).collect();
    assert_eq!(traits, ["run_js_code"]);
    Ok(())
}

/// The provider the configuration file supplies is the operator's, and so is
/// its key: a backup leaves it out, beside the admin's provider it carries.
#[tokio::test]
async fn the_configuration_files_llm_provider_is_not_backed_up() -> sc_error::Result<()> {
    let mut source = setup().await?;
    furnish_llm(&mut source).await?;
    let def = sc_llm::LlmProviderDef::new("hosted", sc_llm::ANTHROPIC_BACKEND)
        .with(sc_llm::CFG_API_KEY, "sk-operator");
    let host = sc_llm::HostLlmProvider::new(def, vec![], "claude-sonnet-5")?;
    sc_llm::set_host_llm_provider(&source.catalog, Some(host)).await?;

    let (_, options) = source.client.send("GET", "/api/backup", None).await;
    assert_eq!(options["available"]["llm_providers"], json!(1), "{options}");
    let archive = backup_everything(&mut source).await;
    let text = entry(&archive, "llm-providers.json");
    let providers: Value = serde_json::from_str(&text).unwrap();
    let names: Vec<&str> = providers
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["house"]);
    assert!(!text.contains("sk-operator"), "{text}");
    Ok(())
}

#[tokio::test]
async fn llm_providers_travel_with_their_key_and_their_agents_come_back() -> sc_error::Result<()> {
    let mut source = setup().await?;
    furnish_llm(&mut source).await?;

    let (_, options) = source.client.send("GET", "/api/backup", None).await;
    assert_eq!(options["available"]["llm_providers"], json!(1), "{options}");
    assert_eq!(
        options["include"]["llm_providers"],
        json!(true),
        "{options}"
    );
    let archive = backup_everything(&mut source).await;

    // The key is in the file as it is stored — not the sentinel the admin API
    // shows — because a provider restored without it is one no agent can use.
    let providers: Value = serde_json::from_str(&entry(&archive, "llm-providers.json")).unwrap();
    assert_eq!(providers[0]["name"], json!("house"));
    assert_eq!(
        providers[0]["config"][sc_llm::CFG_API_KEY],
        json!("sk-ant-backed-up")
    );
    assert_eq!(providers[0]["models"].as_array().unwrap().len(), 2);

    let mut target = setup().await?;
    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(&report, "restored", "LLM provider `house`: 2 models"),
        "{report}"
    );
    assert!(
        report_has(&report, "restored", "agent `librarian`"),
        "{report}"
    );

    let provider = sc_llm::require_llm_provider(&target.catalog, "house").await?;
    assert_eq!(
        provider.config.get(sc_llm::CFG_API_KEY),
        Some(&json!("sk-ant-backed-up"))
    );
    let models = sc_llm::list_llm_models(&target.catalog, &provider).await?;
    let default: Vec<&str> = models
        .iter()
        .filter(|m| m.is_default)
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(default, vec!["claude-sonnet-4-5"]);
    assert_eq!(models.len(), 2);
    let agents = sc_agent::list_agents(&target.catalog).await?;
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].provider, "house");
    assert_eq!(agents[0].model.as_deref(), Some("claude-opus-4-1"));

    // A second restore adds nothing and overwrites nothing: the provider here
    // keeps the key it has, even when the backup's differs.
    let mut changed = provider.clone();
    changed.config.insert(
        sc_llm::CFG_API_KEY.to_owned(),
        json!("sk-ant-rotated-since"),
    );
    sc_llm::save_llm_provider(&target.catalog, &changed).await?;
    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(
            &report,
            "warnings",
            "LLM provider `house` is already defined here"
        ),
        "{report}"
    );
    let kept = sc_llm::require_llm_provider(&target.catalog, "house").await?;
    assert_eq!(
        kept.config.get(sc_llm::CFG_API_KEY),
        Some(&json!("sk-ant-rotated-since"))
    );
    assert_eq!(sc_llm::list_llm_providers(&target.catalog).await?.len(), 1);
    assert_eq!(
        sc_llm::list_llm_models(&target.catalog, &kept).await?.len(),
        2
    );
    Ok(())
}

/// Send, and insist it worked.
async fn ok(server: &mut Server, method: &str, path: &str, body: Option<Value>) -> Value {
    let (status, value) = server.client.send(method, path, body).await;
    assert!(status.is_success(), "{method} {path}: {status} {value}");
    value
}

/// Fit model `id`, wait for it, and activate it.
async fn fit(server: &mut Server, id: &str) -> String {
    let started = ok(
        server,
        "POST",
        &format!("/api/models/{id}/fit"),
        Some(json!({})),
    )
    .await;
    let instance = started["id"].as_str().unwrap().to_owned();
    for _ in 0..600 {
        let body = ok(
            server,
            "GET",
            &format!("/api/model-instances/{instance}"),
            None,
        )
        .await;
        if body["status"] != json!("fitting") {
            assert_eq!(body["status"], json!("fitted"), "{body}");
            ok(
                server,
                "POST",
                &format!("/api/model-instances/{instance}/activate"),
                None,
            )
            .await;
            return instance;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the fit never finished");
}

/// Everything the second round of backup work added, on one server: analytics
/// (two datasets, one built on the other; a model; a fit with an output frame
/// and a draw; a workspace), a stream with a secret, a workflow trigger with
/// steps, the email settings, a Saltcorn UI application's translations, and a
/// SQLite connection whose file lives in a file store.
async fn furnish_the_rest(server: &mut Server) -> sc_error::Result<()> {
    // --- a table to analyse -----------------------------------------------
    server
        .db
        .client()
        .await?
        .batch_execute(
            "CREATE TABLE houses (id bigint primary key, area double precision, \
                 bedrooms bigint, price double precision); \
             INSERT INTO houses (id, area, bedrooms, price) \
               SELECT i, 50 + i, 1 + (i % 5), 1000 * (50 + i) + 20000 * (1 + (i % 5)) \
               FROM generate_series(1, 60) AS i;",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    server.catalog.reload().await?;

    // --- datasets, the second built on the first, a model over the first ----
    let op = |id: &str, kind: &str, params: Value| json!({ "id": id, "enabled": true, "kind": kind, "params": params });
    let prices = ok(
        server,
        "POST",
        "/api/datasets",
        Some(json!({
            "name": "prices",
            "base": { "kind": "table", "table": "houses" },
            "operations": [op("a", "select", json!({ "columns": [
                { "column": "price" }, { "column": "area" }, { "column": "bedrooms" }
            ]}))],
        })),
    )
    .await;
    let prices_id = prices["dataset"]["id"].as_str().unwrap().to_owned();
    ok(
        server,
        "POST",
        "/api/datasets",
        Some(json!({
            "name": "big houses",
            "base": { "kind": "dataset", "dataset": prices_id },
            "operations": [op("f", "filter", json!({ "formula": "area > 80" }))],
        })),
    )
    .await;
    let model = ok(
        server,
        "POST",
        "/api/models",
        Some(json!({
            "name": "House prices",
            "provider": "linear_regression",
            "dataset": { "dataset_id": prices_id },
            "configuration": { "label": "price" },
            "split": { "train": 0.8, "validation": 0.0, "test": 0.2, "seed": 7 },
        })),
    )
    .await;
    let model_id = model["id"].as_str().unwrap().to_owned();
    let instance = fit(server, &model_id).await;
    // A posterior's draws, by hand: a linear regression has none, and the
    // backup's one large thing should still be exercised.
    server
        .db
        .client()
        .await?
        .execute(
            "INSERT INTO _fd_model_draws (id, instance, variable, element, chain, warmup, draws) \
             VALUES ($1, $2, 'alpha', '[]', 1, false, '[1.0, 2.0, 3.0]')",
            &[
                &uuid::Uuid::new_v4(),
                &uuid::Uuid::parse_str(&instance).unwrap(),
            ],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let workspace = ok(
        server,
        "POST",
        "/api/workspaces",
        Some(json!({ "name": "Explore prices", "kind": "data_explorer" })),
    )
    .await;
    ok(
        server,
        "PUT",
        &format!(
            "/api/workspaces/{}/state",
            workspace["id"].as_str().unwrap()
        ),
        Some(json!({ "state": { "dataset": prices_id } })),
    )
    .await;

    // --- a stream with a secret ------------------------------------------
    ok(
        server,
        "POST",
        "/api/streams",
        Some(json!({
            "name": "boiler",
            "description": "",
            "provider": "scripted",
            "configuration": { "broker": "tcp://localhost:1883", "password": "hunter2" },
            "min_role": null,
            "enabled": false,
        })),
    )
    .await;

    // --- a workflow trigger, with steps saved after the create ------------
    let trigger = ok(
        server,
        "POST",
        "/api/triggers",
        Some(json!({
            "name": "note_title",
            "description": "",
            "when": "insert",
            "channel": "books",
            "only_if": null,
            "body": "workflow",
            "action": null,
            "configuration": null,
            "min_role": null,
            "enabled": true,
        })),
    )
    .await;
    ok(
        server,
        "POST",
        &format!("/api/workflows/{}", trigger["id"].as_str().unwrap()),
        Some(json!({ "workflow": {
            "start": "note",
            "trace": false,
            "steps": [
                {
                    "name": "note",
                    "kind": {
                        "type": "set",
                        "assignments": [{ "target": "title", "formula": "row.title" }]
                    },
                    "next": { "type": "step", "step": "again" }
                },
                {
                    "name": "again",
                    "kind": {
                        "type": "set",
                        "assignments": [{ "target": "twice", "formula": "row.title" }]
                    },
                    "next": { "type": "end" }
                }
            ]
        }})),
    )
    .await;

    // --- the email settings --------------------------------------------------
    ok(
        server,
        "POST",
        "/api/settings",
        Some(json!({ "values": {
            "smtp_host": "mail.example.com",
            "smtp_password": "smtp-secret",
            "email_from": "shop@example.com",
            "smtp_username": "shop",
        }})),
    )
    .await;

    // --- a Saltcorn UI application, translated into French ---------------
    let mut app = sc_app::Application::new(
        "Shop",
        "shop",
        sc_app::FrameworkRef::new(sc_viewpattern::SALTCORN_UI_FRAMEWORK),
    );
    sc_app::set_app_locales(&mut app, &[sc_i18n::Locale::parse("fr").unwrap()], None);
    let app = sc_app::save_application(&server.catalog, &app).await?;
    use sc_app::CatalogStore;
    sc_app::RowCatalogStore::new(app.id)
        .save(
            &server.catalog,
            &sc_i18n::Catalog::from_json(
                sc_i18n::Locale::parse("fr").unwrap(),
                &json!({ "Hello": "Bonjour" }),
            )?,
        )
        .await?;

    // --- a SQLite file in the `assets` store, and a connection to it --------
    {
        let driver = sc_db_sqlite::SqliteDriver::open(server.files.join("reporting.sqlite"))?;
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
    let connection = ok(
        server,
        "POST",
        "/api/db-connections",
        Some(json!({
            "name": "reporting",
            "description": "",
            "backend": "sqlite",
            "host": "",
            "port": 0,
            "database": "",
            "username": "",
            "password": "",
            "schema": "",
            "file_store": "assets",
            "file_path": "reporting.sqlite",
        })),
    )
    .await;
    assert_eq!(connection["connected"], json!(true), "{connection}");
    Ok(())
}

#[tokio::test]
async fn analytics_streams_workflows_settings_translations_and_connections_travel()
-> sc_error::Result<()> {
    let mut source = setup().await?;
    furnish(&mut source).await?;
    furnish_the_rest(&mut source).await?;

    let (_, options) = source.client.send("GET", "/api/backup", None).await;
    let available = &options["available"];
    assert_eq!(available["datasets"], json!(2), "{options}");
    assert_eq!(available["models"], json!(1), "{options}");
    assert_eq!(available["workspaces"], json!(1), "{options}");
    assert_eq!(available["fits"], json!(1), "{options}");
    assert_eq!(available["streams"], json!(1), "{options}");
    assert_eq!(available["db_connections"], json!(1), "{options}");
    // Everything is ticked except the fits, which wait to be asked for.
    let mut include = options["include"].clone();
    assert_eq!(include["analytics"], json!(true));
    assert_eq!(include["fits"], json!(false));
    assert_eq!(include["settings"], json!(true));
    // A table on another database is that connection's, not this archive's.
    assert!(
        !include["tables"]
            .as_array()
            .unwrap()
            .contains(&json!("invoice")),
        "{include}"
    );
    include["fits"] = json!(true);
    let (status, archive, _) = source
        .client
        .raw(
            "POST",
            "/backup/create",
            Some((
                serde_json::to_vec(&json!({ "include": include })).unwrap(),
                "application/json",
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // The secrets are in the file as they are stored.
    let streams: Value = serde_json::from_str(&entry(&archive, "streams.json")).unwrap();
    assert_eq!(streams[0]["configuration"]["password"], json!("hunter2"));
    let email: Value = serde_json::from_str(&entry(&archive, "settings/email.json")).unwrap();
    assert_eq!(email["smtp_password"], json!("smtp-secret"));
    // The workflow's steps ride inside its trigger.
    let triggers: Value = serde_json::from_str(&entry(&archive, "triggers.json")).unwrap();
    let workflow = triggers
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("note_title"))
        .unwrap();
    assert_eq!(workflow["workflow"]["steps"].as_array().unwrap().len(), 2);
    assert!(has_entry(&archive, "applications/shop/translations.json"));
    assert!(!has_entry(&archive, "tables/invoice/table.json"));

    // --- into a second installation ----------------------------------------
    let mut target = setup().await?;
    let report = restore_everything(&mut target, &archive).await;
    for line in [
        "dataset `prices`",
        "dataset `big houses`",
        "model `House prices`",
        "model fits: 1 fit",
        "workspace `Explore prices`",
        "stream `boiler`",
        "trigger `note_title`: with its workflow (2 steps)",
        "the email settings",
        "translations of `shop`: fr",
        "database connection `reporting`",
    ] {
        assert!(report_has(&report, "restored", line), "{line}: {report}");
    }

    let catalog = &target.catalog.clone();
    // The derived dataset is built on the restored one.
    let prices = sc_dataset::load_dataset_by_name(catalog, "prices")
        .await?
        .unwrap();
    let big = sc_dataset::load_dataset_by_name(catalog, "big houses")
        .await?
        .unwrap();
    assert_eq!(big.base, sc_dataset::Base::dataset(prices.id));
    // The model, its active fit, the fit's output frames and its draw.
    let model = sc_model::require_model(catalog, "House prices").await?;
    assert_eq!(model.dataset.id, prices.id);
    let fits = sc_model::list_model_instances(catalog, model.id).await?;
    assert_eq!(fits.len(), 1);
    assert!(fits[0].active);
    assert_eq!(fits[0].status, sc_model::FitStatus::Fitted);
    let count = |table: &'static str| {
        let id = fits[0].id.0;
        let db = &target.db;
        async move {
            let row = db
                .client()
                .await
                .unwrap()
                .query_one(
                    &format!("SELECT count(*) FROM {table} WHERE instance = $1"),
                    &[&id],
                )
                .await
                .unwrap();
            row.get::<_, i64>(0)
        }
    };
    assert_eq!(count("_fd_model_draws").await, 1);
    assert!(count("_fd_model_outputs").await > 0);
    let workspaces = sc_analytics::list_workspaces(catalog).await?;
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].state["dataset"], json!(prices.id.to_string()));
    // The stream, secret intact.
    let stream = sc_stream::load_stream_by_name(catalog, "boiler")
        .await?
        .unwrap();
    assert_eq!(stream.configuration["password"], json!("hunter2"));
    // The workflow's steps, as a first version here.
    let trigger = sc_action::list_triggers(catalog)
        .await?
        .into_iter()
        .find(|t| t.name == "note_title")
        .unwrap();
    let workflow = sc_workflow::current_workflow(catalog, trigger.id)
        .await?
        .unwrap();
    assert_eq!(workflow.steps.len(), 2);
    // The settings.
    let stored = sc_config::all_config(catalog).await?;
    assert_eq!(stored.get("smtp_host"), Some(&json!("mail.example.com")));
    // The translations.
    let shop = sc_app::list_applications(catalog)
        .await?
        .into_iter()
        .find(|a| a.subdomain == "shop")
        .unwrap();
    use sc_app::CatalogStore;
    let french = sc_app::RowCatalogStore::new(shop.id)
        .load(catalog, &sc_i18n::Locale::parse("fr").unwrap())
        .await?
        .unwrap();
    assert_eq!(french.to_json()["Hello"], json!("Bonjour"));
    // The connection, dialled once its store was back, with its table listed.
    let invoice = catalog.get("invoice")?.expect("the connection's table");
    assert_eq!(invoice.database, sc_catalog::DbId("reporting".to_owned()));

    // --- a second restore adds nothing and overwrites nothing ----------------
    let report = restore_everything(&mut target, &archive).await;
    for line in [
        "dataset `prices` is already defined here",
        "model `House prices` is already defined here",
        "workspace `Explore prices` is already here",
        "stream `boiler` is already defined here",
        "database connection `reporting` is already defined here",
        "translations of `shop`: fr already here and kept",
    ] {
        assert!(
            report_has(&report, "warnings", line) || report_has(&report, "restored", line),
            "{line}: {report}"
        );
    }
    assert!(
        report_has(
            &report,
            "restored",
            "its workflow was already here and was kept"
        ),
        "{report}"
    );
    assert_eq!(sc_dataset::list_datasets(catalog).await?.len(), 2);
    assert_eq!(sc_model::list_models(catalog).await?.len(), 1);
    assert_eq!(
        sc_model::list_model_instances(catalog, model.id)
            .await?
            .len(),
        1
    );
    assert_eq!(sc_analytics::list_workspaces(catalog).await?.len(), 1);
    assert_eq!(sc_stream::list_streams(catalog).await?.len(), 1);
    assert_eq!(
        sc_workflow::list_workflow_versions(catalog, trigger.id)
            .await?
            .len(),
        1
    );
    Ok(())
}

/// A calculated field that reads through a reference (`writerⱵname`) comes back.
///
/// The restore adds the plain columns, then the references, then the calculated
/// fields: an expression is checked against the columns it reads, and a join
/// reads through a `Key` that is not there until the references are in. The
/// `books` field also reads a calculated field of `writers`, a table restored
/// after it, which only goes in on a second round.
#[tokio::test]
async fn a_calculated_field_reading_through_a_reference_is_restored() -> sc_error::Result<()> {
    let mut source = setup().await?;
    for (table, field) in [
        (
            "writers",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        ("writers", json!({ "name": "name", "type": "text" })),
        (
            "writers",
            json!({ "name": "surname", "kind": { "type": "calc",
                    "expression": "name.split(' ')[1]" } }),
        ),
        (
            "books",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        (
            "books",
            json!({ "name": "writer", "kind": { "type": "key", "target_table": "writers",
                    "target_field": "id", "summary_field": "name" } }),
        ),
        (
            "books",
            json!({ "name": "first_name", "kind": { "type": "calc",
                    "expression": "(writerⱵname).split(' ')[0]" } }),
        ),
        (
            "books",
            json!({ "name": "writer_surname", "kind": { "type": "calc",
                    "expression": "writerⱵsurname" } }),
        ),
    ] {
        if field["name"] == json!("id") {
            let (status, body) = source
                .client
                .send("POST", "/api/tables", Some(json!({ "name": table })))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let (status, body) = source
            .client
            .send("POST", &format!("/api/tables/{table}/fields"), Some(field))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let archive = backup_everything(&mut source).await;

    let mut target = setup().await?;
    let report = restore_everything(&mut target, &archive).await;
    for column in [
        "books.first_name",
        "books.writer_surname",
        "writers.surname",
    ] {
        assert!(
            report_has(&report, "restored", &format!("column `{column}`")),
            "`{column}` should be restored: {report}"
        );
    }
    let (status, fields) = target
        .client
        .send("GET", "/api/tables/books/fields", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{fields}");
    let first_name = fields
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == json!("first_name"))
        .unwrap_or_else(|| panic!("the calculated field should be restored: {fields}"));
    assert_eq!(first_name["kind"]["type"], json!("calc"));
    assert_eq!(
        first_name["kind"]["expression"],
        json!("(writerⱵname).split(' ')[0]")
    );
    Ok(())
}

/// A table's ownership formula comes back, whether it reads one of the table's
/// own columns (`therapist`) or reads through a reference
/// (`assignmentⱵassigned_by`), and with row-level security on.
///
/// The table is created with no columns (they go in on later passes), and the
/// formula is checked against the columns it reads. So the formula and RLS are
/// applied only after the columns and the rows are in.
#[tokio::test]
async fn an_ownership_formula_is_restored_after_the_columns_it_reads() -> sc_error::Result<()> {
    let mut source = setup().await?;
    let users_key = |name: &str| {
        json!({ "name": name, "kind": { "type": "key", "target_table": "users",
                "target_field": "id", "summary_field": "email" } })
    };
    for (table, field) in [
        (
            "assignment",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        ("assignment", users_key("assigned_by")),
        (
            "run",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        (
            "run",
            json!({ "name": "assignment", "kind": { "type": "key",
                    "target_table": "assignment", "target_field": "id",
                    "summary_field": "id" } }),
        ),
    ] {
        if field["name"] == json!("id") {
            let (status, body) = source
                .client
                .send("POST", "/api/tables", Some(json!({ "name": table })))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let (status, body) = source
            .client
            .send("POST", &format!("/api/tables/{table}/fields"), Some(field))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    for (table, formula, rls) in [
        ("assignment", "assigned_by === user.id", true),
        ("run", "assignmentⱵassigned_by === user.id", false),
    ] {
        let (status, body) = source
            .client
            .send(
                "PUT",
                &format!("/api/tables/{table}"),
                Some(json!({
                    "label": table,
                    "description": "",
                    "min_role_read": 100,
                    "min_role_write": 100,
                    "ownership_formula": formula,
                    "rls_enabled": rls,
                    "sync_enabled": false,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/tables/assignment/rows",
            Some(json!({ "id": 1, "assigned_by": null })),
        )
        .await;
    assert!(status.is_success(), "{body}");
    let archive = backup_everything(&mut source).await;

    let mut target = setup().await?;
    let report = restore_everything(&mut target, &archive).await;
    for table in ["assignment", "run"] {
        assert!(
            report_has(
                &report,
                "restored",
                &format!("the ownership rule of `{table}`")
            ),
            "`{table}`'s ownership rule should be restored: {report}"
        );
    }
    assert!(
        report_has(&report, "restored", "1 rows into `assignment`"),
        "the row should go in before RLS is switched on: {report}"
    );
    let (status, tables) = target.client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK, "{tables}");
    for (table, formula, rls) in [
        ("assignment", "assigned_by === user.id", true),
        ("run", "assignmentⱵassigned_by === user.id", false),
    ] {
        let body = tables
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == json!(table))
            .unwrap_or_else(|| panic!("`{table}` should be restored: {tables}"));
        assert_eq!(body["ownership_formula"], json!(formula), "{body}");
        assert_eq!(body["rls_enabled"], json!(rls), "{body}");
    }
    Ok(())
}

/// The id an account has on one server.
async fn user_id(server: &Server, email: &str) -> uuid::Uuid {
    server
        .db
        .client()
        .await
        .unwrap()
        .query_one("SELECT id FROM users WHERE email = $1", &[&email])
        .await
        .unwrap()
        .get(0)
}

/// A row that refers to an account the other server already has, under its
/// own id, refers to that account there.
///
/// The restore keeps an account that is already on the server, matched by email.
/// Its id there is not the backup's, so a row keeping the backup's id broke the
/// foreign key, and so did every row that referred to that row.
#[tokio::test]
async fn a_row_referring_to_a_kept_account_refers_to_it_by_its_id_here() -> sc_error::Result<()> {
    let mut source = setup().await?;
    for field in [
        json!({ "name": "id", "type": "int", "primary_key": true }),
        json!({ "name": "owner", "kind": { "type": "key", "target_table": "users",
                "target_field": "id", "summary_field": "email" } }),
    ] {
        if field["name"] == json!("id") {
            let (status, body) = source
                .client
                .send("POST", "/api/tables", Some(json!({ "name": "notes" })))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let (status, body) = source
            .client
            .send("POST", "/api/tables/notes/fields", Some(field))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let there = user_id(&source, "admin@example.com").await;
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/tables/notes/rows",
            Some(json!({ "id": 1, "owner": there.to_string() })),
        )
        .await;
    assert!(status.is_success(), "{body}");
    let archive = backup_everything(&mut source).await;

    let mut target = setup().await?;
    let here = user_id(&target, "admin@example.com").await;
    assert_ne!(
        there, here,
        "two servers should give their admin different ids"
    );
    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(&report, "restored", "1 rows into `notes`"),
        "the row should go in: {report}"
    );
    let owner: uuid::Uuid = target
        .db
        .client()
        .await
        .unwrap()
        .query_one("SELECT owner FROM notes WHERE id = 1", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(owner, here);
    Ok(())
}

/// `node_modules` is neither backed up nor restored.
///
/// Its `.bin` entries are symlinks, which a backup held as copies of their
/// targets, so the restored `tsc` imported `../lib/tsc.js` from the wrong place.
/// And with a `node_modules` there, the build does not install, so the broken
/// copy stayed. A backup taken before this change still carries one, and the
/// restore leaves it out.
#[tokio::test]
async fn node_modules_is_neither_backed_up_nor_restored() -> sc_error::Result<()> {
    let mut source = setup().await?;
    sc_catalog::save_file_store(
        &source.catalog,
        &FileStoreDef::local("code", source.files.to_string_lossy()),
    )
    .await?;
    sc_catalog::connect_file_store_def(
        &source.catalog,
        &sc_catalog::load_file_store_by_name(&source.catalog, "code")
            .await?
            .expect("the store just saved"),
    )?;
    std::fs::create_dir_all(source.files.join("web/src")).unwrap();
    std::fs::create_dir_all(source.files.join("web/node_modules/.bin")).unwrap();
    std::fs::write(source.files.join("web/src/main.ts"), "export {}\n").unwrap();
    std::fs::write(
        source.files.join("web/node_modules/.bin/tsc"),
        "import \"../lib/tsc.js\";\n",
    )
    .unwrap();
    let archive = backup_everything(&mut source).await;
    assert!(has_entry(
        &archive,
        "file-stores/code/files/web/src/main.ts"
    ));
    assert!(!has_entry(
        &archive,
        "file-stores/code/files/web/node_modules/.bin/tsc"
    ));

    // A backup from before: the same, with the `node_modules` entry in it.
    let old = {
        let mut source = zip::ZipArchive::new(std::io::Cursor::new(&archive)).expect("a zip");
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for i in 0..source.len() {
            out.raw_copy_file(source.by_index_raw(i).unwrap()).unwrap();
        }
        out.start_file(
            "file-stores/code/files/web/node_modules/.bin/tsc",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        std::io::Write::write_all(&mut out, b"import \"../lib/tsc.js\";\n").unwrap();
        out.finish().unwrap().into_inner()
    };
    let mut target = setup().await?;
    sc_catalog::save_file_store(
        &target.catalog,
        &FileStoreDef::local("code", target.files.to_string_lossy()),
    )
    .await?;
    sc_catalog::connect_file_store_def(
        &target.catalog,
        &sc_catalog::load_file_store_by_name(&target.catalog, "code")
            .await?
            .expect("the store just saved"),
    )?;
    let report = restore_everything(&mut target, &old).await;
    assert!(
        report_has(&report, "warnings", "1 file in `node_modules` of `code`"),
        "{report}"
    );
    assert!(target.files.join("web/src/main.ts").is_file());
    assert!(!target.files.join("web/node_modules").exists());
    Ok(())
}

/// What the automated backup in the test below includes: the books table's
/// definition but not its rows, the file store, the triggers — and no users.
fn schedule_include() -> Value {
    json!({
        "tables": ["books"],
        "table_data": [],
        "applications": [],
        "file_stores": ["assets"],
        "users": false,
        "triggers": true,
        "ssl": true,
    })
}

/// `body` with [`schedule_include`] as its `include`, and `include_all` off so
/// the lists decide what is in each backup.
fn with_include(mut body: Value) -> Value {
    body["include"] = schedule_include();
    body["include_all"] = no_include_all();
    body
}

/// An `include_all` with every kind left to `include`'s lists.
fn no_include_all() -> Value {
    json!({ "tables": false, "applications": false, "file_stores": false })
}

/// A destination in a directory on the server.
fn local(dir: impl AsRef<std::path::Path>) -> Value {
    json!({ "kind": "local", "directory": dir.as_ref().to_string_lossy() })
}

/// Automated backups, end to end: a schedule is created through the API the card
/// uses, with a selection of its own; the backup task's tick writes a real backup
/// into its directory holding exactly that selection, prunes what is past its
/// retention, and records what it did where the list reads it. Editing and
/// deleting go through the same API, and the checks the dialog relies on refuse
/// what they should.
#[tokio::test]
async fn an_automated_backup_writes_and_prunes_its_directory() -> sc_error::Result<()> {
    let mut server = setup().await?;
    furnish(&mut server).await?;
    let dest = temp_dir().join("nightly");

    // Refused: a relative path, an unknown frequency, a zero retention, and no
    // word on what to include.
    for bad in [
        with_include(
            json!({ "destination": local("backups"), "frequency": "daily", "retention_days": 7 }),
        ),
        with_include(
            json!({ "destination": local(&dest), "frequency": "hourly", "retention_days": 7 }),
        ),
        with_include(
            json!({ "destination": local(&dest), "frequency": "daily", "retention_days": 0 }),
        ),
        json!({ "destination": local(&dest), "frequency": "daily", "retention_days": 7 }),
    ] {
        let (status, body) = server
            .client
            .send("POST", "/api/backup/schedules", Some(bad))
            .await;
        assert!(status.is_client_error(), "{status} {body}");
    }

    // Created; the directory did not exist and is made on save.
    let created = ok(
        &mut server,
        "POST",
        "/api/backup/schedules",
        Some(with_include(json!({
            "destination": local(format!("{}/", dest.to_string_lossy())),
            "frequency": "daily",
            "retention_days": 7,
        }))),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["destination"], local(&dest));
    assert_eq!(created["location"], json!(dest.to_string_lossy()));
    // Read back as the selection it means now, the shape the dialog's pickers take.
    assert_eq!(created["include"]["tables"], json!(["books"]));
    assert_eq!(created["include"]["table_data"], json!([]));
    assert_eq!(created["include"]["users"], json!(false));
    assert_eq!(created["last_success_at"], Value::Null);
    assert!(dest.is_dir());

    // A second schedule into the same directory would prune the first's files.
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/backup/schedules",
            Some(with_include(
                json!({ "destination": local(&dest), "frequency": "weekly", "retention_days": 30 }),
            )),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");

    // An old backup past its retention, a recent one, and a file that is not a
    // backup at all.
    let old = "feldspar-backup-2000-01-01-020000.zip";
    let now = chrono::Utc::now();
    let recent = format!(
        "feldspar-backup-{}.zip",
        (now - chrono::Duration::days(2)).format("%Y-%m-%d-%H%M%S")
    );
    for name in [old, recent.as_str(), "README.txt"] {
        std::fs::write(dest.join(name), b"x").unwrap();
    }

    let scheduler = sc_server::BackupScheduler::new(server.catalog.clone());
    let ran = scheduler.tick(now).await;
    assert_eq!(ran.len(), 1);
    // Not due again a minute later.
    assert!(
        scheduler
            .tick(now + chrono::Duration::minutes(1))
            .await
            .is_empty()
    );

    let list = ok(&mut server, "GET", "/api/backup/schedules", None).await;
    let entry = &list.as_array().unwrap()[0];
    assert_eq!(entry["last_error"], Value::Null, "{entry}");
    assert!(entry["last_success_at"].is_string());
    let written = std::path::PathBuf::from(entry["last_file"].as_str().unwrap());
    assert_eq!(written.parent(), Some(dest.as_path()));
    // The schedule's own selection, not the Backup card's (which still has
    // everything ticked).
    let archive = std::fs::read(&written).unwrap();
    assert!(has_entry(&archive, "tables/books/table.json"));
    assert!(!has_entry(&archive, "tables/books/rows.ndjson"));
    assert!(!has_entry(&archive, "users/users.json"));
    assert!(has_entry(&archive, "triggers.json"));
    let (_, options) = server.client.send("GET", "/api/backup", None).await;
    assert_eq!(options["include"]["users"], json!(true));
    assert_eq!(options["include"]["table_data"], json!(["books"]));

    // Stored as what was left out: a table created later is in the schedule,
    // rows and all, while the books' rows stay out.
    server
        .catalog
        .create_table(
            "reviews",
            &[DataField::plain("id", TypeRef::Basic(BasicType::Int))
                .required()
                .primary_key()],
        )
        .await?;
    let list = ok(&mut server, "GET", "/api/backup/schedules", None).await;
    assert_eq!(list[0]["include"]["tables"], json!(["books", "reviews"]));
    assert_eq!(list[0]["include"]["table_data"], json!(["reviews"]));

    assert!(!dest.join(old).exists(), "the old backup should be pruned");
    assert!(dest.join(&recent).exists());
    assert!(dest.join("README.txt").exists());
    // No temporary file left behind.
    assert!(std::fs::read_dir(&dest).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")
    }));

    // Edited: weekly, longer retention, and the users ticked. What it last did
    // is kept.
    let mut include = list[0]["include"].clone();
    include["users"] = json!(true);
    let updated = ok(
        &mut server,
        "PUT",
        &format!("/api/backup/schedules/{id}"),
        Some(json!({
            "destination": local(&dest),
            "frequency": "weekly",
            "retention_days": 30,
            "include": include,
            "include_all": no_include_all(),
        })),
    )
    .await;
    assert_eq!(updated["frequency"], json!("weekly"));
    assert_eq!(updated["retention_days"], json!(30));
    assert_eq!(updated["include"]["users"], json!(true));
    assert_eq!(updated["include"]["table_data"], json!(["reviews"]));
    assert!(updated["last_success_at"].is_string());

    // Deleted: gone from the list, and the backups it wrote stay.
    let deleted = ok(
        &mut server,
        "DELETE",
        &format!("/api/backup/schedules/{id}"),
        None,
    )
    .await;
    assert_eq!(deleted["deleted"], json!(true));
    let list = ok(&mut server, "GET", "/api/backup/schedules", None).await;
    assert_eq!(list, json!([]));
    assert!(written.exists());

    // Admin-only, like the rest of the Backup tab.
    let (status, _) = server.client.send("POST", "/api/logout", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .client
        .send("GET", "/api/backup/schedules", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}

/// Automated backups sent to an S3-compatible bucket and to an SFTP server,
/// each played by a server in this process.
#[path = "backup_remote.rs"]
mod remote;

fn db_error(e: impl std::fmt::Display) -> sc_error::Error {
    sc_error::Error::database(e.to_string())
}

/// Deterministic, incompressible-looking bytes: `len` of them from `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// Format 2's reason for being: a table of multi-megabyte `bytea` rows, a table
/// of thousands of small ones, and a file store with a video in it go out
/// through `/backup/create` and back in through `/backup/upload`, byte for byte.
///
/// What it pins down along the way: the blobs are paged out a row or so at a
/// time and written as base64, not as arrays of numbers; a table longer than
/// one page is read past its first page by key; non-finite floats and
/// sub-second timestamps survive; the video is stored in the zip rather than
/// deflated again, while text is deflated; and a file whose path is not ASCII
/// keeps its name.
#[tokio::test]
async fn large_binary_rows_and_files_stream_through_a_backup_and_restore() -> sc_error::Result<()> {
    const BLOB: usize = 5 * 1024 * 1024;
    const SMALL_ROWS: i32 = 2_500;
    const VIDEO: &str = "clips/intro.mp4";
    const NOTE: &str = "Ærø/über-日本.txt";

    let mut source = setup().await?;
    {
        let client = source.db.client().await?;
        let db = db_error;
        client
            .batch_execute(
                "CREATE TABLE blobs (id int PRIMARY KEY, name text NOT NULL, data bytea); \
                 CREATE TABLE readings (id int PRIMARY KEY, value float8, at timestamptz); \
                 INSERT INTO readings \
                   SELECT n, n * 0.5, timestamptz '2026-10-19 12:00:00.123456+00' \
                     + n * interval '1 second' \
                   FROM generate_series(1, 2497) AS n; \
                 INSERT INTO readings VALUES \
                   (2498, 'NaN', NULL), (2499, 'Infinity', NULL), (2500, '-Infinity', NULL);",
            )
            .await
            .map_err(db)?;
        for id in 1..=4_i32 {
            let bytes = noise(id as u64, BLOB + id as usize);
            client
                .execute(
                    "INSERT INTO blobs VALUES ($1, $2, $3)",
                    &[&id, &format!("blob {id}"), &bytes],
                )
                .await
                .map_err(db)?;
        }
        // An empty value and a missing one are values too.
        client
            .execute(
                "INSERT INTO blobs VALUES (5, 'empty', ''::bytea), (6, 'none', NULL)",
                &[],
            )
            .await
            .map_err(db)?;
    }
    source.catalog.reload().await?;

    // A store holding a video and a note whose path is not ASCII.
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "media",
                "description": "",
                "backend": "local",
                "config": { "path": source.files.to_string_lossy() },
                "min_role": null,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let video = noise(99, 6 * 1024 * 1024 + 7);
    std::fs::create_dir_all(source.files.join("clips")).unwrap();
    std::fs::write(source.files.join(VIDEO), &video).unwrap();
    std::fs::create_dir_all(source.files.join("Ærø")).unwrap();
    let note = "Grüße aus Ærøskøbing — 日本語\n".repeat(1000);
    std::fs::write(source.files.join(NOTE), &note).unwrap();

    let archive = backup_everything(&mut source).await;

    // --- what the file holds ------------------------------------------------
    let manifest: Value = serde_json::from_str(&entry(&archive, "manifest.json")).unwrap();
    assert_eq!(manifest["version"], json!(2));
    let rows = entry(&archive, "tables/blobs/rows.ndjson");
    let lines: Vec<Value> = rows
        .lines()
        .map(|line| serde_json::from_str(line).expect("one row per line"))
        .collect();
    assert_eq!(lines.len(), 6);
    // Bytes are base64, one string — not an array of numbers.
    use base64::Engine as _;
    let first = lines[0]["data"].as_str().expect("bytes as a string");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(first)
            .unwrap(),
        noise(1, BLOB + 1)
    );
    let readings = entry(&archive, "tables/readings/rows.ndjson");
    assert_eq!(readings.lines().count(), SMALL_ROWS as usize);
    assert!(readings.contains(r#""value":"NaN""#), "NaN is named");
    assert!(readings.contains(r#""value":"-Infinity""#));
    assert!(
        readings.contains("2026-10-19T12:00:01.123456Z"),
        "a timestamp keeps its microseconds"
    );
    {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&archive[..])).unwrap();
        let mp4 = zip
            .by_name(&format!("file-stores/media/files/{VIDEO}"))
            .unwrap();
        assert_eq!(mp4.compression(), zip::CompressionMethod::Stored);
        drop(mp4);
        let text = zip
            .by_name(&format!("file-stores/media/files/{NOTE}"))
            .expect("the non-ASCII name survives");
        assert_eq!(text.compression(), zip::CompressionMethod::Deflated);
    }
    let store: Value =
        serde_json::from_str(&entry(&archive, "file-stores/media/store.json")).unwrap();
    let listed = store["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == json!(VIDEO))
        .expect("the video is listed");
    assert_eq!(listed["size"], json!(video.len()));

    // --- and back, into a server with its own store --------------------------
    let mut target = setup().await?;
    sc_catalog::save_file_store(
        &target.catalog,
        &FileStoreDef::local("media", target.files.to_string_lossy()),
    )
    .await?;
    sc_catalog::connect_file_store_def(
        &target.catalog,
        &sc_catalog::load_file_store_by_name(&target.catalog, "media")
            .await?
            .expect("the store just saved"),
    )?;
    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(&report, "restored", "6 rows into `blobs`"),
        "{report}"
    );
    assert!(
        report_has(&report, "restored", "2500 rows into `readings`"),
        "{report}"
    );
    assert!(
        !report_has(&report, "warnings", "may be damaged"),
        "{report}"
    );
    assert!(!report_has(&report, "warnings", "a row of"), "{report}");

    let client = target.db.client().await?;
    let db = db_error;
    let blobs = client
        .query("SELECT id::int4, data FROM blobs ORDER BY id", &[])
        .await
        .map_err(db)?;
    assert_eq!(blobs.len(), 6);
    for row in &blobs[..4] {
        let id: i32 = row.get(0);
        let data: Vec<u8> = row.get(1);
        assert!(data == noise(id as u64, BLOB + id as usize), "blob {id}");
    }
    assert_eq!(blobs[4].get::<_, Option<Vec<u8>>>(1), Some(Vec::new()));
    assert_eq!(blobs[5].get::<_, Option<Vec<u8>>>(1), None);

    let readings = client
        .query(
            "SELECT count(*), \
                    count(*) FILTER (WHERE value = 'NaN'), \
                    count(*) FILTER (WHERE value = 'Infinity'), \
                    count(*) FILTER (WHERE value = '-Infinity'), \
                    max(at) = timestamptz '2026-10-19 12:41:37.123456+00' \
             FROM readings",
            &[],
        )
        .await
        .map_err(db)?;
    let row = &readings[0];
    assert_eq!(row.get::<_, i64>(0), i64::from(SMALL_ROWS));
    assert_eq!(row.get::<_, i64>(1), 1);
    assert_eq!(row.get::<_, i64>(2), 1);
    assert_eq!(row.get::<_, i64>(3), 1);
    assert!(
        row.get::<_, bool>(4),
        "the last reading, to the microsecond"
    );

    assert!(std::fs::read(target.files.join(VIDEO)).unwrap() == video);
    assert_eq!(
        std::fs::read_to_string(target.files.join(NOTE)).unwrap(),
        note
    );
    Ok(())
}

/// Secret fields (TODO-imap M2), through the admin API and a backup.
///
/// The data editor writes a secret and reads back only its sentinel; a
/// trigger on the table sees the plaintext. A backup carries the envelopes and
/// the key that opens them (`keys/secrets.key`, flagged in the manifest) and
/// no plaintext; restored into a second server with **a key of its own**, each
/// value is sealed again under that key, which stays. A backup without the key
/// restores the rows with the secrets left empty, and says so for the table.
#[tokio::test]
async fn secret_fields_are_write_only_and_a_backup_reseals_them_under_the_target_key()
-> sc_error::Result<()> {
    const PLAINTEXT: &str = "Grüße-€-pässwort";
    let mut source = setup().await?;
    source
        .catalog
        .set_secret_key(sc_catalog::SecretKey::generate()?);
    for (table, field) in [
        (
            "mailboxes",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        ("mailboxes", json!({ "name": "label", "type": "text" })),
        (
            "mailboxes",
            json!({ "name": "password", "kind": { "type": "secret" } }),
        ),
        (
            "seen",
            json!({ "name": "id", "type": "int", "primary_key": true }),
        ),
        ("seen", json!({ "name": "password", "type": "text" })),
    ] {
        if field["name"] == json!("id") {
            let (status, body) = source
                .client
                .send("POST", "/api/tables", Some(json!({ "name": table })))
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let (status, body) = source
            .client
            .send("POST", &format!("/api/tables/{table}/fields"), Some(field))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    // A trigger is server-side code: it sees the plaintext.
    let (status, body) = source
        .client
        .send(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "copy_password",
                "description": "",
                "when": "insert",
                "channel": "mailboxes",
                "action": "insert_row",
                "configuration": { "table": "seen", "values": { "password": "row.password" } },
                "min_role": null,
                "enabled": true,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The data editor: written, and never read back.
    let (status, created) = source
        .client
        .send(
            "POST",
            "/api/tables/mailboxes/rows",
            Some(json!({ "label": "orders@", "password": PLAINTEXT })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["password"], json!(sc_types::SECRET_SENTINEL));
    assert!(!created.to_string().contains(PLAINTEXT), "{created}");
    let (status, listed) = source
        .client
        .send("GET", "/api/tables/mailboxes/rows", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed[0]["password"], json!(sc_types::SECRET_SENTINEL));
    assert!(!listed.to_string().contains(PLAINTEXT), "{listed}");
    let (_, seen) = source
        .client
        .send("GET", "/api/tables/seen/rows", None)
        .await;
    assert_eq!(
        seen[0]["password"],
        json!(PLAINTEXT),
        "the trigger saw it: {seen}"
    );

    // The backup: envelopes and the key, flagged in the manifest.
    let archive = backup_everything(&mut source).await;
    let rows = entry(&archive, "tables/mailboxes/rows.ndjson");
    assert!(!rows.contains(PLAINTEXT) && rows.contains("v1:"), "{rows}");
    let manifest: Value = serde_json::from_str(&entry(&archive, "manifest.json")).unwrap();
    assert_eq!(manifest["secrets_key"], json!("keys/secrets.key"));
    let mut key = Vec::new();
    zip::ZipArchive::new(std::io::Cursor::new(&archive))
        .unwrap()
        .by_name("keys/secrets.key")
        .unwrap()
        .read_to_end(&mut key)
        .unwrap();
    assert_eq!(key, source.catalog.secret_key()?.to_bytes());

    // Restored into a server with a key of its own: sealed again under it.
    let mut target = setup().await?;
    let target_key = sc_catalog::SecretKey::generate()?;
    target.catalog.set_secret_key(target_key.clone());
    restore_everything(&mut target, &archive).await;
    let stored = stored_passwords(&target).await?;
    assert_eq!(stored.len(), 1, "{stored:?}");
    let envelope = stored[0].as_deref().expect("a value came back");
    assert_eq!(target_key.open(envelope)?, PLAINTEXT);
    assert_eq!(
        *target.catalog.secret_key()?,
        target_key,
        "the target's key is kept"
    );
    let (_, listed) = target
        .client
        .send("GET", "/api/tables/mailboxes/rows", None)
        .await;
    assert_eq!(listed[0]["password"], json!(sc_types::SECRET_SENTINEL));

    // Without the key: the rows come back, the secrets empty, and the report
    // says so for the table.
    let keyless = without_entries(&archive, |name| name == "keys/secrets.key");
    let mut bare = setup().await?;
    bare.catalog
        .set_secret_key(sc_catalog::SecretKey::generate()?);
    let report = restore_everything(&mut bare, &keyless).await;
    assert!(
        report_has(
            &report,
            "warnings",
            "secret value(s) of `mailboxes` were left empty"
        ),
        "{report}"
    );
    assert_eq!(stored_passwords(&bare).await?, vec![None]);
    Ok(())
}

/// `mailboxes.password` as the column holds it, past the row layer.
async fn stored_passwords(server: &Server) -> sc_error::Result<Vec<Option<String>>> {
    let rows = server
        .db
        .client()
        .await?
        .query("SELECT password FROM mailboxes", &[])
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    Ok(rows.iter().map(|r| r.get::<_, Option<String>>(0)).collect())
}

/// MCP servers travel beside the LLM providers, their headers (a credential)
/// with them, as a provider's API key does (TODO-agent-skills AS4.5).
#[tokio::test]
async fn mcp_servers_travel_with_their_headers() -> sc_error::Result<()> {
    let mut source = setup().await?;
    let def = sc_mcp::McpServerDef::new("docs", "https://mcp.example.com/mcp")
        .header("Authorization", "Bearer mcp-backed-up");
    sc_mcp::save_mcp_server(&source.catalog, &def).await?;

    let (_, options) = source.client.send("GET", "/api/backup", None).await;
    assert_eq!(options["available"]["llm_providers"], json!(1), "{options}");
    let archive = backup_everything(&mut source).await;
    assert!(entry(&archive, "mcp-servers.json").contains("Bearer mcp-backed-up"));

    let mut target = setup().await?;
    let report = restore_everything(&mut target, &archive).await;
    assert!(
        report_has(&report, "restored", "MCP server `docs`"),
        "{report}"
    );
    let restored = sc_mcp::require_mcp_server(&target.catalog, "docs").await?;
    assert_eq!(restored.url, def.url);
    assert_eq!(restored.headers, def.headers);
    assert_eq!(restored.id, def.id, "its identity, where it is free");
    Ok(())
}

/// [`V1_ORDERS_BACKUP`] with an agent using every skill of v1's agents plugin,
/// its memory table, a vector table and a memory consolidation
/// (`fixtures/make-v1-agents-fixture.py`).
const V1_AGENTS_BACKUP: &[u8] = include_bytes!("fixtures/saltcorn-v1-feldsparimport-agents.zip");

/// TODO-agent-skills AS12: every v1 skill crosses as a trait the restore
/// accepts, and what the skills need outside the agent is there — the
/// memories, the MCP server, the full-text constraint, the vector field's
/// model, the provider's media models and the consolidation trigger.
#[tokio::test]
async fn a_v1_agent_with_every_skill_is_restored_with_every_trait() -> sc_error::Result<()> {
    // The vector table needs pgvector.
    let Some(db) = TestDb::with_pgvector().await? else {
        return Ok(());
    };
    let mut server = setup_in(db).await?;
    let report = restore_everything(&mut server, V1_AGENTS_BACKUP).await;

    // Nothing about the agent is skipped but the SKILL.md to copy, and no
    // trait was left out of it.
    let about_agent: Vec<&Value> = report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w.as_str().unwrap_or_default().contains("EverySkill"))
        .collect();
    assert_eq!(about_agent.len(), 1, "{report}");
    assert!(
        about_agent[0].as_str().unwrap().contains("External Skill"),
        "{report}"
    );

    let (_, agents) = server.client.send("GET", "/api/agents", None).await;
    let agent = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!("EverySkill"))
        .unwrap_or_else(|| panic!("the agent: {agents} / {report}"));
    assert_eq!(agent["traits"].as_array().unwrap().len(), 21, "{agent}");
    assert_eq!(
        agent["attributes"]["pickable_models"][0]["model"],
        json!("gpt-5-mini"),
        "{agent}"
    );

    // The memories moved into the memory store, filed under the agent.
    let (status, memories) = server
        .client
        .send("GET", "/api/agent-memories?agent=EverySkill", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{memories}");
    let memories = memories.as_array().unwrap().clone();
    assert_eq!(memories.len(), 3, "{memories:?} / {report}");
    assert!(
        memories
            .iter()
            .any(|m| m["kind"] == json!("preferences") && m["personal"] == json!(true)),
        "{memories:?}"
    );
    let (_, tables) = server.client.send("GET", "/api/tables", None).await;
    assert!(
        !tables.to_string().contains("AgentLongTermMemory"),
        "{tables}"
    );

    // What the skills need of the rest of the installation.
    let (_, servers) = server.client.send("GET", "/api/mcp-servers", None).await;
    assert!(servers.to_string().contains("\"docs\""), "{servers}");
    let (_, constraints) = server
        .client
        .send("GET", "/api/tables/document/constraints", None)
        .await;
    assert!(
        constraints
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == json!("full_text_search")),
        "{constraints}"
    );
    let (_, triggers) = server.client.send("GET", "/api/triggers", None).await;
    let consolidate = triggers
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == json!("ConsolidateMemories"))
        .unwrap_or_else(|| panic!("the consolidation: {triggers} / {report}"));
    assert_eq!(consolidate["action"], json!("consolidate_memory"));

    // Restoring the same file again does not write the memories twice.
    restore_everything(&mut server, V1_AGENTS_BACKUP).await;
    let (_, again) = server
        .client
        .send("GET", "/api/agent-memories?agent=EverySkill", None)
        .await;
    assert_eq!(again.as_array().unwrap().len(), 3, "{again}");
    Ok(())
}

/// Whether every file entry of a zip is encrypted — and none can be read
/// without the password.
fn all_encrypted(archive: &[u8]) -> bool {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("a zip");
    (0..zip.len()).all(|i| {
        let raw = zip.by_index_raw(i).unwrap();
        raw.is_dir() || raw.encrypted()
    })
}

/// One entry of a protected zip, decrypted with `password`, as text — what
/// 7-Zip does with the same file.
fn decrypted_entry(archive: &[u8], path: &str, password: &str) -> String {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("a zip");
    let mut file = zip
        .by_name_decrypt(path, password.as_bytes())
        .unwrap_or_else(|e| panic!("`{path}` would not open with the password: {e}"));
    let mut text = String::new();
    file.read_to_string(&mut text).expect("readable text");
    text
}

/// A password-protected backup, end to end: the download is a standard
/// AES-encrypted zip that opens with the password and with nothing else; the
/// upload of it is held as `locked` until the password is given; a wrong one is
/// refused and can be retried; the right one says what the file holds and the
/// restore that follows uses it. The password is not remembered for the next
/// backup.
#[tokio::test]
async fn a_password_protected_backup_restores_with_its_password() -> sc_error::Result<()> {
    const SECRET: &str = "correct horse battery staple";
    let mut source = setup().await?;
    furnish(&mut source).await?;

    let (_, options) = source.client.send("GET", "/api/backup", None).await;
    // A password that is not text is refused, before anything is written.
    let (status, bytes, _) = source
        .client
        .raw(
            "POST",
            "/backup/create",
            Some((
                serde_json::to_vec(&json!({ "include": options["include"], "password": 7 }))
                    .unwrap(),
                "application/json",
            )),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&bytes)
    );

    let (status, archive, _) = source
        .client
        .raw(
            "POST",
            "/backup/create",
            Some((
                serde_json::to_vec(&json!({ "include": options["include"], "password": SECRET }))
                    .unwrap(),
                "application/json",
            )),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&archive)
    );
    assert!(all_encrypted(&archive));
    // The names are a zip's directory and readable; the contents are not.
    {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&archive[..])).unwrap();
        assert!(zip.index_for_name("tables/books/rows.ndjson").is_some());
        assert!(
            zip.by_name("manifest.json").is_err(),
            "readable without the password"
        );
        assert!(zip.by_name_decrypt("manifest.json", b"wrong").is_err());
    }
    let manifest: Value =
        serde_json::from_str(&decrypted_entry(&archive, "manifest.json", SECRET)).unwrap();
    assert_eq!(manifest["format"], json!("feldspar-backup"));
    assert_eq!(
        decrypted_entry(
            &archive,
            "file-stores/assets/files/notes/readme.txt",
            SECRET
        ),
        "hello from the backup"
    );
    // The next backup is not protected unless asked again.
    assert!(!all_encrypted(&backup_everything(&mut source).await));

    // --- the other server ---------------------------------------------------
    let mut target = setup().await?;
    let (status, bytes, _) = target
        .client
        .raw(
            "POST",
            "/backup/upload",
            Some((archive.clone(), "application/zip")),
        )
        .await;
    let uploaded: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{uploaded}");
    assert_eq!(uploaded["locked"], json!(true));
    assert_eq!(uploaded["encrypted"], json!(true));
    assert_eq!(
        uploaded["available"],
        Value::Null,
        "nothing is read without the password"
    );
    let id = uploaded["id"].clone();

    // Restoring before the password is given is refused, and the file stays
    // held; so it does after a wrong password.
    let include_books = json!({
        "tables": ["books"], "table_data": ["books"], "applications": [],
        "file_stores": [], "users": false, "agents": false, "triggers": false, "ssl": false,
    });
    let (status, body) = target
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": id, "include": include_books })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = target
        .client
        .send(
            "POST",
            "/api/backup/unlock",
            Some(json!({ "id": id, "password": "nope" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("password"), "{body}");

    let (status, unlocked) = target
        .client
        .send(
            "POST",
            "/api/backup/unlock",
            Some(json!({ "id": id, "password": SECRET })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{unlocked}");
    assert_eq!(unlocked["locked"], json!(false));
    assert_eq!(unlocked["encrypted"], json!(true));
    assert_eq!(unlocked["available"]["tables"][0]["name"], json!("books"));
    assert_eq!(unlocked["include"]["table_data"], json!(["books"]));

    let (status, report) = target
        .client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": id, "include": include_books })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    let (status, rows) = target
        .client
        .send("GET", "/api/tables/books/rows", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let titles: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["title"].as_str())
        .collect();
    assert_eq!(titles, vec!["Dune", "Solaris"]);

    // The library calls the CLI makes: the same file, by path.
    let path = temp_dir().join("protected.zip");
    std::fs::write(&path, &archive).unwrap();
    assert!(sc_server::backup_is_encrypted(&path)?);
    assert!(sc_server::inspect_backup(&path, None).is_err());
    assert!(sc_server::inspect_backup(&path, Some("nope")).is_err());
    let (contents, _) = sc_server::inspect_backup(&path, Some(SECRET))?;
    assert_eq!(contents.tables[0].name, "books");
    Ok(())
}

/// An automated backup with a password writes every backup encrypted with it;
/// the password goes out as the mask, the mask handed back keeps it, and an
/// empty one removes it.
#[tokio::test]
async fn an_automated_backup_can_be_password_protected() -> sc_error::Result<()> {
    let mut server = setup().await?;
    furnish(&mut server).await?;
    let dest = temp_dir().join("protected-nightly");
    let created = ok(
        &mut server,
        "POST",
        "/api/backup/schedules",
        Some(with_include(json!({
            "destination": local(&dest),
            "frequency": "daily",
            "retention_days": 7,
            "password": "s3cret pass",
        }))),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["password"], json!(sc_types::SECRET_SENTINEL));

    let scheduler = sc_server::BackupScheduler::new(server.catalog.clone());
    assert_eq!(scheduler.tick(chrono::Utc::now()).await.len(), 1);
    let list = ok(&mut server, "GET", "/api/backup/schedules", None).await;
    let entry = &list.as_array().unwrap()[0];
    assert_eq!(entry["last_error"], Value::Null, "{entry}");
    assert_eq!(entry["password"], json!(sc_types::SECRET_SENTINEL));
    let archive = std::fs::read(entry["last_file"].as_str().unwrap()).unwrap();
    assert!(all_encrypted(&archive));
    let manifest: Value =
        serde_json::from_str(&decrypted_entry(&archive, "manifest.json", "s3cret pass")).unwrap();
    assert_eq!(manifest["format"], json!("feldspar-backup"));

    // Saved with the mask: kept.
    let path = format!("/api/backup/schedules/{id}");
    let mut body = with_include(json!({
        "destination": local(&dest),
        "frequency": "daily",
        "retention_days": 7,
        "password": sc_types::SECRET_SENTINEL,
    }));
    let kept = ok(&mut server, "PUT", &path, Some(body.clone())).await;
    assert_eq!(kept["password"], json!(sc_types::SECRET_SENTINEL));
    // Saved empty: removed.
    body["password"] = json!("");
    let removed = ok(&mut server, "PUT", &path, Some(body)).await;
    assert_eq!(removed["password"], Value::Null);
    Ok(())
}
