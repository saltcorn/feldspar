//! End-to-end integration tests for the file-manager API (Phase 8, final item):
//! connect a file store to the catalog, then browse/read/write it through the
//! assembled router as a browser-based admin would, and round-trip per-file xattr
//! metadata through the connected store handle.
//!
//! Like `admin_api.rs` this drives the concrete [`admin_handlers`] over a **real
//! Postgres database** (needed only for auth/bootstrap here) with the session and
//! CSRF cookies a browser carries. The file store is a [`LocalFileStore`] rooted
//! at a fresh temp directory, connected to the catalog by name.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::{FileMeta, FileStore, LocalFileStore};
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A cookie-jar-carrying client over the router (CSRF + session), mirroring the
/// helper in `admin_api.rs`.
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
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
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

/// A fresh, unique temp directory (created on disk) for one store.
fn temp_dir() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "sc-file-manager-test-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Build a router whose catalog has one connected file store (`docs`) rooted at a
/// fresh temp directory, log in an admin, and return the client, the store handle,
/// and the `TestDb` (kept in scope: dropping it deletes the database).
async fn setup() -> sc_error::Result<(Client, Arc<dyn FileStore>, TestDb)> {
    let db = TestDb::new().await?;
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
    // `listFileStores` reads the stored definitions as well as the connected
    // registry (§1.4), so the table has to exist — the real boot path bootstraps
    // it in `connect_catalog` alongside `users` and `_fd_applications`.
    sc_catalog::bootstrap_file_stores(&catalog).await?;

    // Connect a local file store to the catalog, by name.
    let store = Arc::new(LocalFileStore::new("docs", temp_dir())?) as Arc<dyn FileStore>;
    catalog.connect_file_store(store.clone())?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog, apps),
        sessions,
        &ServerConfig::default(),
    )?;

    let mut client = Client::new(router);
    // Prime the CSRF cookie (a safe GET mints it) before the first mutation.
    client.send("GET", "/api/auth/status", None).await;
    // Bootstrap + log in an admin so the (admin-only) file endpoints are reachable.
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    Ok((client, store, db))
}

#[tokio::test]
async fn file_manager_browse_read_write() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    // --- the store is listed as connected --------------------------------
    let (status, body) = client.send("GET", "/api/file-stores", None).await;
    assert_eq!(status, StatusCode::OK);
    let stores = body.as_array().unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["name"], json!("docs"));
    assert_eq!(stores[0]["is_git_repo"], json!(false));

    // --- write a text file (edit-a-text-file / upload) -------------------
    let (status, entry) = client
        .send(
            "POST",
            "/api/file-stores/docs/write",
            Some(json!({ "path": "notes/readme.md", "text": "# Hello\n" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(entry["name"], json!("readme.md"));
    assert_eq!(entry["path"], json!("notes/readme.md"));
    assert_eq!(entry["is_dir"], json!(false));
    assert_eq!(entry["size"], json!(8));

    // The bytes really landed in the store.
    assert_eq!(&store.read("notes/readme.md").await?[..], b"# Hello\n");

    // --- browse the root, then the sub-directory -------------------------
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/browse",
            Some(json!({ "dir": "" })),
        )
        .await;
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert_eq!(names, ["notes"]);
    assert_eq!(body[0]["is_dir"], json!(true));

    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/browse",
            Some(json!({ "dir": "notes" })),
        )
        .await;
    assert_eq!(body[0]["name"], json!("readme.md"));
    assert_eq!(body[0]["path"], json!("notes/readme.md"));
    assert_eq!(body[0]["size"], json!(8));

    // --- read it back: text shortcut + base64 (download) -----------------
    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/read",
            Some(json!({ "path": "notes/readme.md" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["text"], json!("# Hello\n"));
    assert_eq!(body["size"], json!(8));
    let decoded = BASE64.decode(body["base64"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, b"# Hello\n");

    // --- upload arbitrary bytes via base64, read them back ---------------
    let raw: &[u8] = &[0u8, 159, 146, 150]; // not valid UTF-8
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/write",
            Some(json!({ "path": "blob.bin", "base64": BASE64.encode(raw) })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/read",
            Some(json!({ "path": "blob.bin" })),
        )
        .await;
    // Non-UTF-8 bytes → no `text`, but base64 round-trips exactly.
    assert_eq!(body["text"], Value::Null);
    let decoded = BASE64.decode(body["base64"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, raw);

    // --- error cases -----------------------------------------------------
    // Unknown store → 404.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/nope/browse",
            Some(json!({ "dir": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Both `text` and `base64` → 400.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/write",
            Some(json!({ "path": "x", "text": "a", "base64": "YQ==" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    Ok(())
}

#[tokio::test]
async fn xattr_metadata_round_trips_through_the_connected_store() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    // Create a file through the API, then round-trip per-file metadata (stored in
    // xattrs, no DB row — design §9) through the same store handle the catalog
    // resolved by name.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/write",
            Some(json!({ "path": "doc.md", "text": "# hi" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // No metadata set yet → defaults, except the owner the write recorded: the
    // request that created the file knew who made it, which is the one fact
    // nothing could recover afterwards.
    let created = store.get_meta("doc.md").await?;
    assert_eq!(created.min_role, None);
    assert!(created.attributes.is_empty());
    assert!(created.owner.is_some());

    let mut meta = FileMeta {
        min_role: Some(30),
        ..Default::default()
    };
    meta.attributes
        .insert("mime".into(), "text/markdown".into());
    store.set_meta("doc.md", &meta).await?;
    assert_eq!(store.get_meta("doc.md").await?, meta);

    // The metadata lives beside the bytes, so it never appears as a file in a
    // browse listing.
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/browse",
            Some(json!({ "dir": "" })),
        )
        .await;
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert_eq!(names, ["doc.md"]);

    // And the file's bytes are unchanged by the metadata write.
    assert_eq!(&store.read("doc.md").await?[..], b"# hi");

    Ok(())
}

/// The store-side search, through the endpoint the IDE's find-in-files runs on
/// (§12.1, TODO Phase 5).
///
/// Walking the tree through the filesystem provider is one request per
/// directory; this is the same walk done where the bytes are, in one. What is
/// pinned here is the contract the client codes against: literal and regex, the
/// glob, the 1-based line and column, and `truncated` — which a caller must
/// report, because "5 matches" without it claims there were only five.
#[tokio::test]
async fn find_in_files_searches_the_store_server_side() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    for (path, text) in [
        ("src/app.ts", "export function todo() {}\n"),
        ("src/deep/list.tsx", "// TODO: paginate\nconst n = 1;\n"),
        ("readme.md", "nothing here\n"),
        ("node_modules/pkg/index.js", "todo\n"),
    ] {
        store
            .write(path, bytes::Bytes::from(text.as_bytes().to_vec()))
            .await?;
    }

    // A literal, case-insensitive by default, across directories — and not into
    // `node_modules`.
    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/search",
            Some(json!({ "pattern": "todo" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut paths: Vec<&str> = body["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    paths.sort();
    assert_eq!(paths, ["src/app.ts", "src/deep/list.tsx"]);
    assert_eq!(body["truncated"], json!(false));

    // A regular expression, narrowed by a glob, with the position the editor
    // needs to put a cursor on it.
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/search",
            Some(json!({
                "pattern": "function\\s+\\w+",
                "regex": true,
                "glob": "*.ts",
            })),
        )
        .await;
    let matches = body["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{body}");
    assert_eq!(matches[0]["path"], json!("src/app.ts"));
    assert_eq!(matches[0]["line"], json!(1));
    assert_eq!(matches[0]["column"], json!(8));
    assert_eq!(matches[0]["length"], json!(13));
    assert_eq!(matches[0]["text"], json!("export function todo() {}"));

    // Whole-word and case, which the search box's two toggles send.
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/search",
            Some(json!({ "pattern": "TODO", "case_sensitive": true })),
        )
        .await;
    assert_eq!(body["matches"].as_array().unwrap().len(), 1);

    // The bound, and the flag that says it was reached.
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/search",
            Some(json!({ "pattern": "todo", "max_results": 1 })),
        )
        .await;
    assert_eq!(body["matches"].as_array().unwrap().len(), 1);
    assert_eq!(body["truncated"], json!(true));

    // A pattern that is not a valid regex is refused with the engine's reason
    // rather than returning nothing.
    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/search",
            Some(json!({ "pattern": "foo(", "regex": true })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And a store that is not there is a 404, like every other file endpoint.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/nope/search",
            Some(json!({ "pattern": "todo" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// The listing's **columns**, which are what turned a list of names into a file
/// browser: when it changed, who created it, and the rule that actually reaches
/// it.
///
/// The owner is the interesting one. It is recorded from the request that
/// created the entry and stored as the user's *id*, so it survives the account
/// being renamed — and it is reported as the account's **email**, because a
/// column of UUIDs tells a person nothing. The rest of the assertions are about
/// what must not happen to it: an overwrite does not take ownership, and
/// rewriting the access rule does not silently drop it.
#[tokio::test]
async fn a_listing_carries_the_columns_a_file_browser_shows() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/write",
            Some(json!({ "path": "notes/readme.md", "text": "# Hello\n" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // A folder made through the API is owned too — it is a thing somebody put
    // there, exactly as a file is.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/mkdir",
            Some(json!({ "path": "notes/private" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Written underneath by something that is not a request (a code body, a
    // scaffold, a hand on the disk): nobody to record, and the column says so
    // rather than inventing an owner.
    store
        .write("notes/stray.txt", bytes::Bytes::from_static(b"x"))
        .await?;
    // Admin-only on the folder, which every entry under it inherits.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/set-meta",
            Some(json!({ "path": "notes/private", "min_role": 1, "attributes": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/browse",
            Some(json!({ "dir": "notes" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let rows = body.as_array().unwrap();
    let row = |name: &str| -> Value {
        rows.iter()
            .find(|e| e["name"] == json!(name))
            .unwrap_or_else(|| panic!("no {name} in {body}"))
            .clone()
    };

    let readme = row("readme.md");
    assert_eq!(readme["size"], json!(8));
    // RFC 3339, like every other timestamp on this wire.
    let modified = readme["modified"].as_str().expect("a modification time");
    assert!(
        modified.contains('T') && modified.ends_with('Z'),
        "{modified}"
    );
    // The *email*, not the id that is stored.
    assert_eq!(readme["owner"], json!("admin@example.com"));
    assert_eq!(readme["min_role"], Value::Null);
    assert_eq!(readme["effective_min_role"], Value::Null);
    // What is stored underneath is the id, so renaming the account does not
    // orphan the file.
    let owner = store.get_meta("notes/readme.md").await?.owner;
    assert!(owner.is_some_and(|id| uuid::Uuid::parse_str(&id).is_ok()));

    // A directory has no size; the rule set on it is reported as its own.
    let private = row("private");
    assert_eq!(private["is_dir"], json!(true));
    assert_eq!(private["size"], Value::Null);
    assert_eq!(private["min_role"], json!(1));
    assert_eq!(private["effective_min_role"], json!(1));
    // Setting the access rule did not take the owner off it.
    assert_eq!(private["owner"], json!("admin@example.com"));

    assert_eq!(row("stray.txt")["owner"], Value::Null);

    // Everything under a restricted folder inherits the rule, and the column
    // says which rule reaches it rather than which rule is set on it — the
    // distinction the whole path-cumulative design turns on.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/write",
            Some(json!({ "path": "notes/private/secret.txt", "text": "s" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/browse",
            Some(json!({ "dir": "notes/private" })),
        )
        .await;
    assert_eq!(body[0]["name"], json!("secret.txt"));
    assert_eq!(body[0]["min_role"], Value::Null);
    assert_eq!(body[0]["effective_min_role"], json!(1));

    // An overwrite by somebody else does not take the file over: the owner is
    // who created it.
    let (status, _) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({ "email": "other@example.com", "password": "hunter2pass", "role": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    client.send("POST", "/api/logout", None).await;
    let (status, _) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "other@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/write",
            Some(json!({ "path": "notes/readme.md", "text": "# Hello again\n" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/browse",
            Some(json!({ "dir": "notes" })),
        )
        .await;
    let readme = body
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == json!("readme.md"))
        .unwrap()
        .clone();
    assert_eq!(readme["owner"], json!("admin@example.com"));

    Ok(())
}

/// A file setting's choices: the store's files of that kind, without the ones
/// in dependency folders or a generated native project.
#[tokio::test]
async fn list_store_files_skips_dependencies_and_native_projects() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;
    for path in [
        "todo/assets/icon.png",
        "todo/assets/Splash.JPG",
        "todo/src/App.tsx",
        "logo.svg",
        "todo/node_modules/pkg/img.png",
        "todo/android/app/src/main/res/mipmap-hdpi/ic_launcher.png",
        "todo/android/app/debug.keystore",
        "keys/release.jks",
    ] {
        store.write(path, bytes::Bytes::from_static(b"x")).await?;
    }
    let (status, body) = client
        .send(
            "GET",
            "/api/file-stores/docs/files-by-type?extensions=png,jpg,svg",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["paths"],
        json!(["logo.svg", "todo/assets/Splash.JPG", "todo/assets/icon.png"])
    );
    assert_eq!(body["truncated"], json!(false));
    // A keystore picker, which must not offer the generated debug keystore.
    let (_, body) = client
        .send(
            "GET",
            "/api/file-stores/docs/files-by-type?extensions=jks,keystore",
            None,
        )
        .await;
    assert_eq!(body["paths"], json!(["keys/release.jks"]));
    Ok(())
}

/// Finding a file by **name**, anywhere under a directory — the file manager's
/// search box.
///
/// Deliberately not `searchFiles`: that one reads every text file to find a
/// matching line. This walks names, returns listing entries so the results go in
/// the same table with the same columns, and — unlike the content search —
/// excludes nothing by default, because somebody looking for a file in
/// `node_modules` means it.
#[tokio::test]
async fn find_files_searches_names_across_the_tree() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    for path in [
        "src/app.ts",
        "src/deep/list.tsx",
        "readme.md",
        "node_modules/pkg/index.js",
    ] {
        store.write(path, bytes::Bytes::from_static(b"x")).await?;
    }

    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/find",
            Some(json!({ "query": "LIST" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["path"], json!("src/deep/list.tsx"));
    // A hit is a listing row, not a path: the columns beside the name are there
    // without a request per result.
    assert_eq!(entries[0]["size"], json!(1));
    assert!(entries[0]["modified"].is_string());
    assert_eq!(body["truncated"], json!(false));

    // Directories match, and nothing is excluded by default.
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/find",
            Some(json!({ "query": "index" })),
        )
        .await;
    assert_eq!(
        body["entries"][0]["path"],
        json!("node_modules/pkg/index.js")
    );
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/find",
            Some(json!({ "query": "deep" })),
        )
        .await;
    assert_eq!(body["entries"][0]["is_dir"], json!(true));

    // Rooted at the directory in view, and bounded — with the flag that says the
    // bound was reached.
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/find",
            Some(json!({ "query": "ts", "dir": "src" })),
        )
        .await;
    let mut paths: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["src/app.ts", "src/deep/list.tsx"]);

    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/find",
            Some(json!({ "query": "ts", "max_results": 1 })),
        )
        .await;
    assert_eq!(body["entries"].as_array().unwrap().len(), 1);
    assert_eq!(body["truncated"], json!(true));

    // A search with nothing to search for is refused rather than walking the
    // whole store to answer "everything".
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/find",
            Some(json!({ "query": "  " })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // And a store that is not there is a 404, like every other file endpoint.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/nope/find",
            Some(json!({ "query": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}
