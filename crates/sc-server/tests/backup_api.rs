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
        // Generous: a backup of a handful of rows and one small file, plus the
        // zip's own overhead.
        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
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
}

async fn setup() -> sc_error::Result<Server> {
    let db = TestDb::new().await?;
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
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_viewpattern::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_db_connections(&catalog).await?;
    sc_analytics::bootstrap_workspaces(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
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
    assert!(warned("account was imported with no password"), "{report}");

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
    assert!(warned("`AddBook`") && warned("workflow"), "{report}");
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
    assert!(!has_entry(&bytes, "tables/books/rows.json"));
    assert!(!has_entry(&bytes, "users.json"));
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
    assert!(!has_entry(&bytes, "tables/books/rows.json"));
    assert!(!has_entry(&bytes, "tables/books/table.json"));
    assert!(has_entry(&bytes, "users.json"));
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
