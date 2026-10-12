//! Phase 1.4b integration test: file operations and the path-cumulative access
//! rule, driven through the assembled router.
//!
//! `file_store_admin_api.rs` covers managing the stores; this covers operating on
//! their contents — mkdir, delete, rename, and the `FileMeta` endpoints that
//! finally make `min_role` reachable — plus the binary upload route, which lives
//! outside the typed endpoint set because the endpoint model is JSON-only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::{FileStore, LocalFileStore};
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

    /// Send a raw binary body (for the upload route, which takes no JSON).
    async fn send_bytes(
        &mut self,
        method: &str,
        path: &str,
        bytes: Vec<u8>,
    ) -> (StatusCode, Value) {
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
        if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = builder
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from(bytes))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// `GET` a route whose answer is a file: the status, the headers and the
    /// bytes as they are.
    async fn get_raw(&mut self, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut builder = Request::builder().method("GET").uri(path);
        if !self.cookies.is_empty() {
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
        }
        let request = builder.body(Body::empty()).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, body.to_vec())
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
        "sc-file-ops-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// A router with one connected store (`docs`) and an admin logged in.
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
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;

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
    client.send("GET", "/api/auth/status", None).await;
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
async fn mkdir_delete_and_rename_through_the_api() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    // --- mkdir ------------------------------------------------------------
    let (status, entry) = client
        .send(
            "POST",
            "/api/file-stores/docs/mkdir",
            Some(json!({ "path": "assets/img" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(entry["path"], json!("assets/img"));
    assert_eq!(entry["is_dir"], json!(true));

    // Idempotent: asking again succeeds, because the caller wanted a directory
    // there and there is one.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/mkdir",
            Some(json!({ "path": "assets/img" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // --- rename -----------------------------------------------------------
    store
        .write("assets/img/old.png", bytes::Bytes::from_static(b"png"))
        .await?;
    let (status, entry) = client
        .send(
            "POST",
            "/api/file-stores/docs/rename",
            Some(json!({ "from": "assets/img/old.png", "to": "assets/img/new.png" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry["path"], json!("assets/img/new.png"));
    assert_eq!(&store.read("assets/img/new.png").await?[..], b"png");

    // Never clobbers: a silent replace on a file manager's drag-and-drop is a
    // lost file with no undo.
    store
        .write("assets/img/other.png", bytes::Bytes::from_static(b"other"))
        .await?;
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/rename",
            Some(json!({ "from": "assets/img/other.png", "to": "assets/img/new.png" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(&store.read("assets/img/new.png").await?[..], b"png");

    // --- delete -----------------------------------------------------------
    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/delete",
            Some(json!({ "path": "assets/img/new.png" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));

    // Already gone reports false rather than erroring.
    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/delete",
            Some(json!({ "path": "assets/img/new.png" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(false));

    // A directory goes with everything in it.
    let (_, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/delete",
            Some(json!({ "path": "assets" })),
        )
        .await;
    assert_eq!(body["deleted"], json!(true));
    assert!(store.list("").await?.is_empty());

    Ok(())
}

/// Path traversal must be rejected at the API edge for every operation, not only
/// the ones that existed before.
#[tokio::test]
async fn traversal_is_rejected_on_every_file_operation() -> sc_error::Result<()> {
    let (mut client, _store, _db) = setup().await?;

    for (path, body) in [
        ("mkdir", json!({ "path": "../escaped" })),
        ("delete", json!({ "path": "../../etc" })),
        ("rename", json!({ "from": "a.txt", "to": "../out.txt" })),
        ("read", json!({ "path": "../secret" })),
        ("write", json!({ "path": "../evil.txt", "text": "x" })),
        ("meta", json!({ "path": "../secret" })),
    ] {
        let (status, _) = client
            .send("POST", &format!("/api/file-stores/docs/{path}"), Some(body))
            .await;
        assert!(
            status.is_client_error() || status.is_server_error(),
            "{path} must reject traversal, got {status}"
        );
        assert_ne!(status, StatusCode::CREATED, "{path}");
    }

    // Deleting the store root is refused however it is spelled.
    for root in ["/", "."] {
        let (status, _) = client
            .send(
                "POST",
                "/api/file-stores/docs/delete",
                Some(json!({ "path": root })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "root {root:?}");
    }

    Ok(())
}

/// `FileMeta` finally has a way in. The rule it has documented since the MVP is
/// now both settable and reported as *effective*, which is what actually applies.
#[tokio::test]
async fn file_meta_round_trips_and_reports_the_effective_rule() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;
    store
        .write("private/secret.txt", bytes::Bytes::from_static(b"s"))
        .await?;

    // Unset to begin with.
    let (status, meta) = client
        .send(
            "POST",
            "/api/file-stores/docs/meta",
            Some(json!({ "path": "private/secret.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["min_role"], Value::Null);
    assert_eq!(meta["effective_min_role"], Value::Null);

    // Restrict the *directory*, setting nothing on the file.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/set-meta",
            Some(json!({ "path": "private", "min_role": 1, "attributes": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // The file now inherits it. `min_role` is still null — nothing is set on the
    // file — while `effective_min_role` is 1. Reporting only the former would let
    // an admin believe the file is reachable when its folder has locked it.
    let (_, meta) = client
        .send(
            "POST",
            "/api/file-stores/docs/meta",
            Some(json!({ "path": "private/secret.txt" })),
        )
        .await;
    assert_eq!(meta["min_role"], Value::Null);
    assert_eq!(meta["effective_min_role"], json!(1));

    // A laxer rule on the file cannot widen what the directory allows.
    let (_, meta) = client
        .send(
            "POST",
            "/api/file-stores/docs/set-meta",
            Some(json!({
                "path": "private/secret.txt",
                "min_role": 100,
                "attributes": { "mime": "text/plain" },
            })),
        )
        .await;
    assert_eq!(meta["min_role"], json!(100));
    assert_eq!(
        meta["effective_min_role"],
        json!(1),
        "a nested rule must only ever tighten"
    );
    assert_eq!(meta["attributes"]["mime"], json!("text/plain"));

    // Attributes round-trip through the xattr layer.
    let stored = store.get_meta("private/secret.txt").await?;
    assert_eq!(stored.min_role, Some(100));
    assert_eq!(
        stored.attributes.get("mime").map(String::as_str),
        Some("text/plain")
    );

    // A role outside the scale is rejected rather than clamped.
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/set-meta",
            Some(json!({ "path": "private/secret.txt", "min_role": 0, "attributes": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    Ok(())
}

/// The binary upload route: outside the typed endpoint set, but inside the same
/// auth and access model.
#[tokio::test]
async fn binary_upload_writes_the_body_and_still_requires_auth() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    let (status, entry) = client
        .send_bytes("POST", "/upload/docs/assets/blob.bin", payload.clone())
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(entry["path"], json!("assets/blob.bin"));
    assert_eq!(entry["size"], json!(4096));

    // The bytes land verbatim — no base64 round-trip, no UTF-8 assumption.
    assert_eq!(&store.read("assets/blob.bin").await?[..], &payload[..]);
    // And the admin who uploaded it is recorded as its owner (F2 §2.1).
    let owner = store.get_meta("assets/blob.bin").await?.owner;
    assert!(owner.is_some_and(|id| uuid::Uuid::parse_str(&id).is_ok()));

    // A nested destination arrives whole, because `{*path}` is a greedy capture.
    let (status, entry) = client
        .send_bytes("POST", "/upload/docs/a/b/c/deep.bin", b"deep".to_vec())
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(entry["path"], json!("a/b/c/deep.bin"));

    // An unknown store is a 404, not a silent success.
    let (status, _) = client
        .send_bytes("POST", "/upload/nope/x.bin", b"x".to_vec())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    Ok(())
}

/// A route outside the endpoint set must not be outside the auth model either —
/// the risk that comes with hand-rolling it.
#[tokio::test]
async fn upload_rejects_an_unauthenticated_caller() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    // Drop the session cookie, keeping CSRF so this tests auth and not CSRF.
    client.cookies.remove(sc_server::SESSION_COOKIE);

    let (status, _) = client
        .send_bytes("POST", "/upload/docs/sneak.bin", b"x".to_vec())
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        store.read("sneak.bin").await.is_err(),
        "nothing must have been written"
    );

    Ok(())
}

/// The download route: a store file as itself, however large, named for the
/// browser's save dialog, and behind the same auth and access model as upload.
#[tokio::test]
async fn download_serves_the_bytes_as_a_named_file() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    // Large and binary: what base64-in-JSON could not carry.
    let payload: Vec<u8> = (0u8..=255).cycle().take(3 * 1024 * 1024).collect();
    store
        .write(
            "app/outputs/app-debug.apk",
            bytes::Bytes::from(payload.clone()),
        )
        .await?;

    let (status, headers, body) = client
        .get_raw("/download/docs/app/outputs/app-debug.apk")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, payload);
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"app-debug.apk\"; filename*=UTF-8''app-debug.apk"
    );
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/vnd.android.package-archive"
    );

    // A missing file is a 404, and a caller without a session gets nothing.
    let (status, _, _) = client.get_raw("/download/docs/app/nothing.apk").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    client.cookies.remove(sc_server::SESSION_COOKIE);
    let (status, _, body) = client
        .get_raw("/download/docs/app/outputs/app-debug.apk")
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(body, payload);

    Ok(())
}

/// A path that is not there is **404, not 500** — the distinction the IDE's
/// filesystem (§12.1) is built on.
///
/// Asking whether a path exists by trying to read it is the ordinary thing for a
/// client to do: an editor stats a file before writing it, and looks for optional
/// files (`.vscode/settings.json`, a `.prettierrc`) that are usually absent. Every
/// one of those is a *value* — "not there" — and reporting it as a system failure
/// (§16) turns each into an error the admin is shown and a flow the editor cannot
/// complete.
#[tokio::test]
async fn a_missing_path_is_reported_as_not_found() -> sc_error::Result<()> {
    let (mut client, store, _db) = setup().await?;

    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/read",
            Some(json!({ "path": "nothing/here.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");

    // A directory that does not exist, listed: the same answer, because the
    // editor asks about a whole directory just as often as about one file.
    let (status, body) = client
        .send(
            "POST",
            "/api/file-stores/docs/browse",
            Some(json!({ "dir": "nothing" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");

    // The store root is always there, even when it is empty.
    let (status, listing) = client
        .send(
            "POST",
            "/api/file-stores/docs/browse",
            Some(json!({ "dir": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(listing.is_array());

    // And a path that *is* there still reads, so this is a mapping and not a
    // blanket refusal.
    store.write("here.txt", "x".into()).await?;
    let (status, _) = client
        .send(
            "POST",
            "/api/file-stores/docs/read",
            Some(json!({ "path": "here.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    Ok(())
}
