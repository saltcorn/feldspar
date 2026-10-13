//! Phase 4 end-to-end integration test: an application serves and accepts the
//! bytes behind a `File` field, at the **stricter** of the table's rules and the
//! file's own path-cumulative `min_role` (design §4, §7, §14.1).
//!
//! What is asserted, through the real router with real roles:
//!
//! - a `File` field's bytes are addressed **by table, row id and field name**
//!   (`GET /api/posts/{id}/attachment`), never by raw store path;
//! - reads run at the table's `min_role_read` **and** the path-cumulative file
//!   rule — each is decisive in one case below: the table's rule refuses a
//!   visitor the directory would admit, and the directory's rule refuses a
//!   reader the table would admit, for the **same file**;
//! - uploads run at the table's `min_role_write`, land in the field's store and
//!   folder, and the field's MIME restrictions are applied server-side — a
//!   client-side accept filter is a convenience, not a control;
//! - a file is **readable by its owner** whatever its folder's rule, and an
//!   upload records the uploader as the owner (F2 §2.1) — so a min_role 1
//!   folder is a private drop box: each user reads back their own upload, and
//!   another user (or anonymous) gets the 404 a missing file gets;
//! - the file endpoints appear on the **already-mounted** app when the admin
//!   creates the `File` field, with no restart — the same live re-projection a
//!   table-settings change gets (§13.2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_app::{
    ApiConfig, Application, CodeFramework, FrameworkRef, app_source_from_config, build_application,
};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::{FileMeta, FileStore, LocalFileStore};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-filefield-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A cookie-carrying client addressing one host, echoing CSRF on mutations —
/// and able to send and receive **raw bytes**, which is what this phase adds to
/// the app API.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router, host: &str) -> Client {
        Client {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
        }
    }

    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.host);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
        }
        if let Some(ct) = content_type {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        let request = builder.body(Body::from(body.unwrap_or_default())).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        for raw in headers.get_all(header::SET_COOKIE) {
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
        (status, headers, bytes.to_vec())
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let payload = body
            .as_ref()
            .map(|b| serde_json::to_vec(b).unwrap())
            .map(|bytes| (Some("application/json"), Some(bytes)))
            .unwrap_or((None, None));
        let (status, _, bytes) = self.raw(method, path, payload.0, payload.1).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// Upload raw bytes under their own content type, reading a JSON answer.
    async fn upload(
        &mut self,
        path: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> (StatusCode, Value) {
        let (status, _, body) = self
            .raw("POST", path, Some(content_type), Some(bytes.to_vec()))
            .await;
        let value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// Download raw bytes, returning the served content type alongside them.
    async fn download(&mut self, path: &str) -> (StatusCode, Option<String>, Vec<u8>) {
        let (status, headers, bytes) = self.raw("GET", path, None, None).await;
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (status, content_type, bytes)
    }

    /// Load the SPA (a safe GET) to obtain the CSRF cookie, then log in — the
    /// order a browser follows before it may mutate anything.
    async fn login(&mut self, email: &str, password: &str) -> StatusCode {
        self.raw("GET", "/", None, None).await;
        let (status, _) = self
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": email, "password": password })),
            )
            .await;
        status
    }
}

/// The app's source tree: a git repo whose stand-in bundler emits an SPA.
fn write_app_source(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let web = root.join("web");
    std::fs::create_dir_all(web.join("src")).unwrap();
    let script = web.join("build.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         set -e\n\
         test -f src/client.ts\n\
         mkdir -p dist/assets\n\
         printf '<!doctype html><div id=root></div>' > dist/index.html\n\
         cp src/client.ts dist/assets/client.js\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn code_framework() -> FrameworkRef {
    FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts")
}

fn blog_app() -> Application {
    Application::new("Blog", "blog", code_framework())
        .with_table(TableId("posts".to_owned()))
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"))
}

/// Build and mount the app, bootstrap every platform table, and return a router
/// serving both the admin (base domain) and the app — plus the `uploads` store
/// the `File` field will point at, held directly so the test can set
/// per-directory rules on it.
async fn setup(
    tmp: &TempDir,
) -> sc_error::Result<(Router, Arc<Catalog>, Arc<LocalFileStore>, TestDb)> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$; \
             CREATE TABLE posts (\
               id bigint generated by default as identity primary key, \
               title text not null)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    // The overlays and the store-definition table have to exist for the admin
    // to configure through, just as `connect_catalog` bootstraps them on the
    // real boot path: `_fd_tables` for the roles, `_fd_fields` for the `File`
    // field, `_fd_file_stores` for the store's own access floor.
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    // The store the `File` field references — distinct from the app-source store.
    let uploads_dir = tmp.path().join("uploads-store");
    std::fs::create_dir_all(&uploads_dir).map_err(sc_error::Error::from)?;
    let uploads = Arc::new(LocalFileStore::new("uploads", &uploads_dir)?);
    catalog.connect_file_store(uploads.clone())?;

    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &blog_app(), &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    apps.mount(MountedApp::new(blog_app(), framework, &catalog)?)?;

    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
    )?;
    Ok((router, catalog, uploads, db))
}

#[tokio::test]
async fn a_file_fields_bytes_are_served_at_the_stricter_of_table_and_path_rules()
-> sc_error::Result<()> {
    let tmp = TempDir::new("serve");
    let (router, catalog, uploads, _db) = setup(&tmp).await?;

    // Four roles: admin, editor (40), reader (80), visitor (100 — the public
    // built-in, but logged in, so the table check rather than authentication is
    // what refuses them).
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    save_role(&catalog, &Role::new(40, "Editor")).await?;
    save_role(&catalog, &Role::new(80, "Reader")).await?;
    create_user(&catalog, "editor@example.com", "editor-pw", 40).await?;
    create_user(&catalog, "reader@example.com", "reader-pw", 80).await?;
    create_user(&catalog, "visitor@example.com", "visitor-pw", ROLE_PUBLIC).await?;

    // The admin opens the table: reads to 80, writes to 40.
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let (status, _) = admin
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": 80,
                "min_role_write": 40,
                "ownership_formula": "",
                "rls_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // The admin creates the `File` field — on a table of an **already-mounted**
    // app, so the download/upload endpoints must appear live (§13.2), exactly as
    // an access change takes effect live.
    let (status, body) = admin
        .send(
            "POST",
            "/api/tables/posts/fields",
            Some(json!({
                "name": "attachment",
                "type": "text",
                "kind": { "type": "file", "store": "uploads", "folder": "docs",
                          "mime_allow": ["text/plain"] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The editor (40) writes a row and uploads into its `File` field.
    let mut editor = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        editor.login("editor@example.com", "editor-pw").await,
        StatusCode::OK
    );
    let (status, created) = editor
        .send("POST", "/api/posts", Some(json!({ "title": "hello" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_i64().unwrap();

    let (status, updated) = editor
        .upload(
            &format!("/api/posts/{id}/attachment/notes.txt"),
            "text/plain",
            b"attached words",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{updated}");
    // The bytes landed in the field's store **and folder** — the caller named
    // only a filename — and the row now references the stored path.
    assert_eq!(updated["attachment"], json!("docs/notes.txt"));
    assert_eq!(
        &uploads.read("docs/notes.txt").await?[..],
        b"attached words"
    );

    // --- uploads run at the table's write rule --------------------------------
    let mut reader = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        reader.login("reader@example.com", "reader-pw").await,
        StatusCode::OK
    );
    let (status, _) = reader
        .upload(
            &format!("/api/posts/{id}/attachment/sneaky.txt"),
            "text/plain",
            b"nope",
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "min_role_write 40 excludes role 80"
    );

    // --- MIME restrictions are enforced server-side ---------------------------
    let (status, body) = editor
        .upload(
            &format!("/api/posts/{id}/attachment/evil.html"),
            "text/plain", // the *declared* content type is not the control
            b"<script>alert(1)</script>",
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["error"].as_str().unwrap_or_default();
    assert!(message.contains("attachment"), "names the field: {message}");
    assert!(message.contains("text/html"), "names the type: {message}");
    // Nothing was written and the row still references the first upload.
    assert!(uploads.read("docs/evil.html").await.is_err());

    // A traversing filename cannot escape the folder either.
    let (status, _) = editor
        .upload(
            &format!("/api/posts/{id}/attachment/.."),
            "text/plain",
            b"x",
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // --- downloads: the table's rule is decisive for the visitor --------------
    let path = format!("/api/posts/{id}/attachment");

    let (status, content_type, bytes) = reader.download(&path).await;
    assert_eq!(status, StatusCode::OK, "role 80 clears min_role_read 80");
    assert_eq!(content_type.as_deref(), Some("text/plain"));
    assert_eq!(&bytes[..], b"attached words");

    let mut visitor = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        visitor.login("visitor@example.com", "visitor-pw").await,
        StatusCode::OK
    );
    let (status, _, _) = visitor.download(&path).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "nothing on the file's path restricts it, so the refusal is the table's"
    );

    // Anonymous callers are refused before any rule is consulted.
    let mut anon = Client::new(router.clone(), APP_HOST);
    let (status, _, _) = anon.download(&path).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // --- the directory's rule is decisive for the reader ----------------------
    // Restrict the *folder* to role 40. The table still admits the reader (80);
    // the path-cumulative rule now does not — the same file is served to the
    // editor and refused to the reader, so the stricter rule decided.
    uploads
        .set_meta(
            "docs",
            &FileMeta {
                min_role: Some(40),
                ..Default::default()
            },
        )
        .await?;

    let (status, _, _) = reader.download(&path).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the folder's min_role 40 refuses role 80 even though the table admits it — \
         and a file the caller may not read is not found, as a missing one is (F2 §2.1)"
    );
    let (status, _, bytes) = editor.download(&path).await;
    assert_eq!(status, StatusCode::OK, "role 40 clears the folder's rule");
    assert_eq!(&bytes[..], b"attached words");

    // A row with no file is a plain not-found, for a caller every rule admits.
    let (status, created) = editor
        .send("POST", "/api/posts", Some(json!({ "title": "empty" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let empty_id = created["id"].as_i64().unwrap();
    let (status, _, _) = editor
        .download(&format!("/api/posts/{empty_id}/attachment"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    Ok(())
}

/// F2.1/F2.2: an upload is its uploader's, and a min_role 1 folder is private to
/// each uploader — served to them, a 404 to another user and to anonymous, and
/// not overwritable by another user who may write the row.
#[tokio::test]
async fn an_upload_into_a_private_folder_is_its_uploaders_alone() -> sc_error::Result<()> {
    let tmp = TempDir::new("owner");
    let (router, catalog, uploads, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    save_role(&catalog, &Role::new(80, "Reader")).await?;
    let ada = create_user(&catalog, "ada@example.com", "ada-pw", 80).await?;
    create_user(&catalog, "bob@example.com", "bob-pw", 80).await?;

    // Rows are public to read and writable at 80; the folder is admin-only.
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let (status, _) = admin
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "", "description": "",
                "min_role_read": 100, "min_role_write": 80,
                "ownership_formula": "", "rls_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = admin
        .send(
            "POST",
            "/api/tables/posts/fields",
            Some(json!({
                "name": "attachment", "type": "text",
                "kind": { "type": "file", "store": "uploads", "folder": "private",
                          "mime_allow": ["text/plain"] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    uploads.mkdir("private").await?;
    uploads
        .set_meta(
            "private",
            &FileMeta {
                min_role: Some(1),
                ..Default::default()
            },
        )
        .await?;

    // Ada uploads into the private folder: a new file needs nothing but the
    // table's write rule, and it is recorded as hers.
    let mut ada_client = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        ada_client.login("ada@example.com", "ada-pw").await,
        StatusCode::OK
    );
    let (status, created) = ada_client
        .send("POST", "/api/posts", Some(json!({ "title": "mine" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_i64().unwrap();
    let (status, updated) = ada_client
        .upload(
            &format!("/api/posts/{id}/attachment/ada.txt"),
            "text/plain",
            b"ada's words",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{updated}");
    assert_eq!(
        uploads.get_meta("private/ada.txt").await?.owner,
        Some(ada.id.to_string()),
        "the uploader owns the upload"
    );

    let path = format!("/api/posts/{id}/attachment");
    let (status, _, bytes) = ada_client.download(&path).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner reads past the folder's rule"
    );
    assert_eq!(&bytes[..], b"ada's words");
    // Through the shared builder (F2.3): ranged, and named for the file.
    let (status, headers, bytes) = ada_client
        .raw("GET", &format!("{path}?download=1"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&bytes[..], b"ada's words");
    assert_eq!(
        headers.get(header::CONTENT_DISPOSITION).unwrap(),
        "attachment; filename=\"ada.txt\"; filename*=UTF-8''ada.txt"
    );
    assert_eq!(headers.get(header::ACCEPT_RANGES).unwrap(), "bytes");

    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login("bob@example.com", "bob-pw").await, StatusCode::OK);
    let (status, _, bytes) = bob.download(&path).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "another user at the same role"
    );
    assert!(!String::from_utf8_lossy(&bytes).contains("ada's words"));
    let mut anon = Client::new(router.clone(), APP_HOST);
    let (status, _, _) = anon.download(&path).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "anonymous");

    // Bob may write the row, but not replace Ada's file under the same name.
    let (status, _) = bob
        .upload(
            &format!("/api/posts/{id}/attachment/ada.txt"),
            "text/plain",
            b"bob's",
        )
        .await;
    assert!(!status.is_success(), "{status}");
    assert_eq!(&uploads.read("private/ada.txt").await?[..], b"ada's words");

    Ok(())
}

/// A File field set up in the admin to keep what is there (`on_conflict:
/// suffix`) and to take its file with the row (`on_delete: remove_file`): a
/// second upload of the same name lands beside the first, and deleting a row
/// removes its file — unless another row still references it.
#[tokio::test]
async fn a_file_field_can_suffix_taken_names_and_remove_files_with_their_rows()
-> sc_error::Result<()> {
    let tmp = TempDir::new("options");
    let (router, catalog, uploads, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );

    // The field editor is offered both options, as pick-lists.
    let (status, types) = admin.send("GET", "/api/field-types", None).await;
    assert_eq!(status, StatusCode::OK);
    let file = types
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "file")
        .expect("the file kind");
    let spec = file["config_spec"].to_string();
    assert!(
        spec.contains("remove_file") && spec.contains("suffix"),
        "{spec}"
    );

    let (status, body) = admin
        .send(
            "POST",
            "/api/tables/posts/fields",
            Some(json!({
                "name": "attachment", "type": "text",
                "kind": { "type": "file", "store": "uploads", "folder": "docs",
                          "on_delete": "remove_file", "on_conflict": "suffix" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, fields) = admin.send("GET", "/api/tables/posts/fields", None).await;
    assert_eq!(status, StatusCode::OK);
    let attachment = fields
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "attachment")
        .expect("the field");
    assert_eq!(attachment["kind"]["on_delete"], json!("remove_file"));
    assert_eq!(attachment["kind"]["on_conflict"], json!("suffix"));

    let mut app = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        app.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    // Three rows; the same filename uploaded into two of them.
    let mut ids = Vec::new();
    for title in ["one", "two", "three"] {
        let (status, created) = app
            .send("POST", "/api/posts", Some(json!({ "title": title })))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        ids.push(created["id"].as_i64().unwrap());
    }
    let (status, first) = app
        .upload(
            &format!("/api/posts/{}/attachment/notes.txt", ids[0]),
            "text/plain",
            b"first",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["attachment"], json!("docs/notes.txt"));
    let (status, second) = app
        .upload(
            &format!("/api/posts/{}/attachment/notes.txt", ids[1]),
            "text/plain",
            b"second",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(second["attachment"], json!("docs/notes_1.txt"));
    assert_eq!(&uploads.read("docs/notes.txt").await?[..], b"first");
    assert_eq!(&uploads.read("docs/notes_1.txt").await?[..], b"second");

    // The third row shares the second's file.
    let (status, body) = app
        .send(
            "PUT",
            &format!("/api/posts/{}", ids[2]),
            Some(json!({ "attachment": "docs/notes_1.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Deleting the first row takes its file with it.
    let (status, _) = app
        .send("DELETE", &format!("/api/posts/{}", ids[0]), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(uploads.stat("docs/notes.txt").await?.is_none());

    // The second row's file is still the third's, so it stays…
    let (status, _) = app
        .send("DELETE", &format!("/api/posts/{}", ids[1]), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(uploads.stat("docs/notes_1.txt").await?.is_some());
    // …until the last row referencing it goes.
    let (status, _) = app
        .send("DELETE", &format!("/api/posts/{}", ids[2]), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(uploads.stat("docs/notes_1.txt").await?.is_none());
    Ok(())
}

/// The security report this guards against: a `File` field stored whatever path
/// a caller sent, so a user could put the path of **somebody else's upload** in
/// their own row and download it through the row. A path is only accepted from
/// a caller it belongs to — a file that is there, and theirs (or nobody's, and
/// reachable at their role).
#[tokio::test]
async fn a_row_cannot_be_pointed_at_somebody_elses_upload() -> sc_error::Result<()> {
    let tmp = TempDir::new("attach");
    let (router, catalog, uploads, _db) = setup(&tmp).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    save_role(&catalog, &Role::new(80, "Reader")).await?;
    create_user(&catalog, "ada@example.com", "ada-pw", 80).await?;
    create_user(&catalog, "bob@example.com", "bob-pw", 80).await?;

    // Readers read and write the rows, and the folder carries no rule of its
    // own: nothing but this check stands between Bob and Ada's file.
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let (status, _) = admin
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "", "description": "",
                "min_role_read": 80, "min_role_write": 80,
                "ownership_formula": "", "rls_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = admin
        .send(
            "POST",
            "/api/tables/posts/fields",
            Some(json!({
                "name": "attachment", "type": "text",
                "kind": { "type": "file", "store": "uploads", "folder": "shots",
                          "on_delete": "remove_file" }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let mut ada = Client::new(router.clone(), APP_HOST);
    assert_eq!(ada.login("ada@example.com", "ada-pw").await, StatusCode::OK);
    let (status, created) = ada
        .send("POST", "/api/posts", Some(json!({ "title": "ada's" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let ada_row = created["id"].as_i64().unwrap();
    let (status, body) = ada
        .upload(
            &format!("/api/posts/{ada_row}/attachment/photo.txt"),
            "text/plain",
            b"ada's photo",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login("bob@example.com", "bob-pw").await, StatusCode::OK);

    // The report's repro, on insert and on update: both refused, naming the field.
    let (status, body) = bob
        .send(
            "POST",
            "/api/posts",
            Some(json!({ "title": "bob's", "attachment": "shots/photo.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("attachment"), "{body}");
    let (status, created) = bob
        .send("POST", "/api/posts", Some(json!({ "title": "bob's" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let bob_row = created["id"].as_i64().unwrap();
    let (status, theirs) = bob
        .send(
            "PUT",
            &format!("/api/posts/{bob_row}"),
            Some(json!({ "attachment": "shots/photo.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{theirs}");
    let (status, _, bytes) = bob
        .download(&format!("/api/posts/{bob_row}/attachment"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!String::from_utf8_lossy(&bytes).contains("ada's photo"));

    // A path nothing is at yet is refused in the same words, so a write neither
    // reserves a name somebody will upload to nor says which names are taken.
    let (status, missing) = bob
        .send(
            "PUT",
            &format!("/api/posts/{bob_row}"),
            Some(json!({ "attachment": "shots/later.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{missing}");
    assert_eq!(
        theirs.to_string().replace("photo.txt", "later.txt"),
        missing.to_string()
    );

    // Saving Ada's row back with the path it already holds is not attaching it.
    let (status, body) = bob
        .send(
            "PUT",
            &format!("/api/posts/{ada_row}"),
            Some(json!({ "title": "retitled", "attachment": "shots/photo.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Bob's own upload is his to attach to another of his rows…
    let (status, body) = bob
        .upload(
            &format!("/api/posts/{bob_row}/attachment/bob.txt"),
            "text/plain",
            b"bob's",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = bob
        .send(
            "POST",
            "/api/posts",
            Some(json!({ "title": "again", "attachment": "shots/bob.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // …and so is a file nobody owns that his role reaches…
    uploads
        .write("shots/shared.txt", bytes::Bytes::from_static(b"shared"))
        .await?;
    let (status, body) = bob
        .send(
            "PUT",
            &format!("/api/posts/{bob_row}"),
            Some(json!({ "attachment": "shots/shared.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // …but not one behind a rule it does not clear.
    uploads.mkdir("shots/staff").await?;
    uploads
        .set_meta(
            "shots/staff",
            &FileMeta {
                min_role: Some(1),
                ..Default::default()
            },
        )
        .await?;
    uploads
        .write("shots/staff/memo.txt", bytes::Bytes::from_static(b"memo"))
        .await?;
    let (status, body) = bob
        .send(
            "PUT",
            &format!("/api/posts/{bob_row}"),
            Some(json!({ "attachment": "shots/staff/memo.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Admin authority attaches any file, as it may read any.
    let mut app_admin = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        app_admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let (status, body) = app_admin
        .send(
            "POST",
            "/api/posts",
            Some(json!({ "title": "admin's", "attachment": "shots/photo.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    assert_eq!(&uploads.read("shots/photo.txt").await?[..], b"ada's photo");
    Ok(())
}
