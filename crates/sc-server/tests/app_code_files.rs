//! Files and bytes in **code queries**, served (TODO F2.8–F2.10, §2.5–§2.6):
//! JavaScript custom queries through the real router against a real database
//! (SQLite, a file per test) and a real local store.
//!
//! - F2.8: a body joins three parts into a 21 MB file host-side, hashes it,
//!   reads a piece of it and appends to another — none of which is refused
//!   for being over the 8 MB a single read may carry, while a whole read of
//!   the joined file still is; `file.url()` names the static directory that
//!   covers a file and `null` for one nothing serves;
//! - F2.9: a `bytes` query receives a raw PNG as a `Uint8Array` of the right
//!   length, with its declared parameters from the query string and the
//!   request's headers (credentials removed); a query returning
//!   `response({ body: bytes, headers: { "content-type": "image/png" } })`
//!   is served as that image;
//! - F2.10: a GET query that checks a share's password returns a 50 MB file
//!   handle, served with Range (206) under the original name, beyond the 8 MB
//!   limit; a wrong password is the query's own 403; once the share row is
//!   deleted the same range request is refused; and a handle opened `asUser`
//!   on a file the caller may not read is a 404.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use bytes::Bytes;
use sc_api::{BodyType, CustomParam, CustomQuery, Method, QueryLanguage, ResponseType, ValueType};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, StaticDir};
use sc_auth::{Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, DataField, FileStoreId};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_files::{FileMeta, FileStore, LocalFileStore};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_triggers,
};
use sc_types::{BasicType, TypeRef};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "share.example.com";
const PASSWORD: &str = "correct-horse";
const ROLE_MEMBER: u8 = 80;
const MB: usize = 1024 * 1024;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-code-files-{}-{tag}-{:?}",
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

/// A cookie-carrying browser on one host, sending the CSRF header a page would.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

impl Client {
    fn new(router: Router) -> Client {
        Client {
            router,
            cookies: HashMap::new(),
        }
    }

    async fn send(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> Reply {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, APP_HOST);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
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
        let body = axum::body::to_bytes(response.into_body(), 64 * MB)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn get(&mut self, path: &str, headers: &[(&str, &str)]) -> Reply {
        self.send("GET", path, headers, Body::empty()).await
    }

    async fn post_json(&mut self, path: &str, body: Value) -> Reply {
        self.send(
            "POST",
            path,
            &[("content-type", "application/json")],
            Body::from(serde_json::to_vec(&body).unwrap()),
        )
        .await
    }

    async fn login(&mut self, email: &str) {
        self.get("/", &[]).await;
        let reply = self
            .post_json(
                "/api/login",
                json!({ "email": email, "password": PASSWORD }),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());
    }
}

/// A JavaScript query.
fn js(name: &str, method: Method, path: &str, code: &str) -> CustomQuery {
    CustomQuery::new(name, method, path, code).language(QueryLanguage::Javascript)
}

/// The smallest PNG there is: one transparent pixel.
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

fn queries() -> Vec<CustomQuery> {
    vec![
        // F2.8: host-side joins, digests, pieces and appends.
        js(
            "joinParts",
            Method::Post,
            "/files/join",
            r#"const store = fs("files");
               const parts = store.dir("parts");
               const out = store.open("joined/big.bin");
               const size = await out.concat([parts.file("p0"), parts.file("p1"), "parts/p2"]);
               const hex = await out.sha256();
               const piece = await out.read({ offset: 15 * 1024 * 1024, length: 4 });
               const tail = await out.read({ offset: size - 2 });
               const log = store.open("logs/run.txt");
               await log.append("a");
               await log.append(new TextEncoder().encode("bc"));
               let whole = null;
               try { await out.bytes(); } catch (e) { whole = e.message; }
               let missing = null;
               try { await store.open("joined/none.bin").concat([parts.file("nope")]); }
               catch (e) { missing = e.message; }
               return {
                 size, hex, piece: Array.from(piece), tail: Array.from(tail),
                 log: await log.text(), whole, missing,
                 partial: await store.open("joined/none.bin").exists(),
                 url: await out.url(),
                 media: await store.open("media/a b.png").url({ download: true }),
               };"#,
        )
        .min_role(sc_auth::ROLE_ADMIN),
        // F2.9: a raw body in…
        js(
            "measure",
            Method::Post,
            "/images/measure",
            r#"return {
                 isBytes: body instanceof Uint8Array,
                 length: body.length,
                 first: body[1],
                 label: query.label,
                 method: request.method,
                 contentType: request.contentType,
                 sawCookie: "cookie" in request.headers,
                 custom: request.headers["x-custom"],
               };"#,
        )
        .body_type(BodyType::Bytes)
        .params([CustomParam::new("label", ValueType::Text)])
        .min_role(ROLE_MEMBER),
        // …and raw bytes out.
        js(
            "dot",
            Method::Get,
            "/images/dot",
            &format!(
                r#"const png = new Uint8Array([{}]);
                   return response({{ body: png, headers: {{ "content-type": "image/png", "x-pixels": "1" }} }});"#,
                PNG.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
            ),
        )
        .response_type(ResponseType::Bytes)
        .min_role(sc_auth::ROLE_PUBLIC),
        // F2.10: a password-protected share, kept in a table.
        js(
            "makeShare",
            Method::Post,
            "/shares",
            r#"const hash = await crypto.password.hash(body.password);
               await db.shares.insert({ token: body.token, path: body.path, filename: body.filename, hash });
               return { ok: true };"#,
        )
        .min_role(sc_auth::ROLE_ADMIN),
        js(
            "revokeShare",
            Method::Delete,
            "/shares",
            r#"return await db.shares.where({ token: query.token }).delete();"#,
        )
        .min_role(sc_auth::ROLE_ADMIN),
        js(
            "shareFile",
            Method::Get,
            "/share",
            r#"const share = await db.shares.where({ token: query.token }).first();
               if (!share) throw httpError(404, "no such share");
               if (!(await crypto.password.verify(query.password ?? "", share.hash))) {
                 throw httpError(403, "wrong password");
               }
               return response({
                 file: fs("files").open(share.path),
                 filename: share.filename,
                 download: true,
                 headers: { "cache-control": "private, no-store" },
               });"#,
        )
        .params([
            CustomParam::new("token", ValueType::Text),
            CustomParam::new("password", ValueType::Text).optional(),
        ])
        .response_type(ResponseType::Bytes)
        .min_role(sc_auth::ROLE_PUBLIC),
        // A handle opened as the caller is checked by the owner rule when served.
        js(
            "mine",
            Method::Get,
            "/mine",
            r#"return response({ file: fs("files").asUser().open(query.path) });"#,
        )
        .response_type(ResponseType::Bytes)
        .min_role(ROLE_MEMBER),
    ]
}

fn share_app() -> Application {
    let mut api = ApiConfig::new("rest", "/api");
    sc_api::set_custom_queries(&mut api.config, &queries()).unwrap();
    Application::new("Share", "share", FrameworkRef::new("code"))
        .with_file_store(FileStoreId("files".to_owned()))
        .with_api(api)
        .with_static_dir(StaticDir::new(
            "/media",
            FileStoreId("files".to_owned()),
            "media",
        ))
}

struct Server {
    router: Router,
    store: Arc<LocalFileStore>,
    users: HashMap<&'static str, String>,
}

async fn setup(tmp: &TempDir) -> sc_error::Result<Server> {
    let db_dir = tmp.path().join("db");
    std::fs::create_dir_all(&db_dir).map_err(sc_error::Error::from)?;
    let driver = Arc::new(SqliteDriver::open(db_dir.join("share.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    let mut users = HashMap::new();
    for who in ["alice", "bob"] {
        let user = create_user(
            &catalog,
            &format!("{who}@example.com"),
            PASSWORD,
            ROLE_MEMBER,
        )
        .await?;
        users.insert(who, user.id.to_string());
    }
    create_user(&catalog, "admin@example.com", PASSWORD, sc_auth::ROLE_ADMIN).await?;

    let text = |name: &str| DataField::plain(name, TypeRef::Basic(BasicType::Text));
    catalog
        .create_table(
            "shares",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key()
                    .generated(ColumnGenerator::Identity),
                text("token"),
                text("path"),
                text("filename"),
                text("hash"),
            ],
        )
        .await?;

    let files_dir = tmp.path().join("files");
    std::fs::create_dir_all(&files_dir).map_err(sc_error::Error::from)?;
    let store = Arc::new(LocalFileStore::new("files", &files_dir)?);
    catalog.connect_file_store(store.clone())?;
    // Everything under `private` is the administrator's: a share query is the
    // only way anyone else reads it.
    store.mkdir("private").await?;
    store
        .set_meta(
            "private",
            &FileMeta {
                min_role: Some(sc_auth::ROLE_ADMIN),
                ..FileMeta::default()
            },
        )
        .await?;

    let evaluator = default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(dispatcher),
    );
    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    apps.mount(MountedApp::new_with(
        share_app(),
        framework,
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;
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
    Ok(Server {
        router,
        store,
        users,
    })
}

/// Bytes that are not all one value, so a misplaced piece shows.
fn film(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn a_code_body_joins_hashes_and_reads_a_file_larger_than_one_read() -> sc_error::Result<()> {
    let tmp = TempDir::new("join");
    let server = setup(&tmp).await?;
    // Three parts, each under the 8 MB a read may carry, together 21 MB.
    let data = film(21 * MB);
    for (i, part) in data.chunks(7 * MB).enumerate() {
        server
            .store
            .write(&format!("parts/p{i}"), Bytes::from(part.to_vec()))
            .await?;
    }
    let mut admin = Client::new(server.router.clone());
    admin.login("admin@example.com").await;

    let reply = admin.post_json("/api/files/join", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());
    let out = reply.json();
    assert_eq!(out["size"], json!(data.len()));
    let expected: String = Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(out["hex"], json!(expected));
    let at = 15 * MB;
    assert_eq!(out["piece"], json!(data[at..at + 4].to_vec()));
    assert_eq!(out["tail"], json!(data[data.len() - 2..].to_vec()));
    assert_eq!(out["log"], json!("abc"));
    // A whole read of the joined file is still refused, naming the way round.
    let whole = out["whole"].as_str().unwrap();
    assert!(
        whole.contains("8 MB") && whole.contains("offset"),
        "{whole}"
    );
    // A missing part is named, and leaves no partial target behind.
    let missing = out["missing"].as_str().unwrap();
    assert!(missing.contains("parts/nope"), "{missing}");
    assert_eq!(out["partial"], json!(false));
    // `url()`: nothing serves `joined/`, and `/media` serves `media/`.
    assert_eq!(out["url"], Value::Null);
    assert_eq!(out["media"], json!("/media/a%20b.png?download=1"));
    // And the bytes are really there.
    assert_eq!(
        server.store.read("joined/big.bin").await?.as_ref(),
        &data[..]
    );
    Ok(())
}

#[tokio::test]
async fn a_bytes_query_takes_a_raw_body_and_another_answers_an_image() -> sc_error::Result<()> {
    let tmp = TempDir::new("bytes");
    let server = setup(&tmp).await?;
    let mut alice = Client::new(server.router.clone());
    alice.login("alice@example.com").await;

    let reply = alice
        .send(
            "POST",
            "/api/images/measure?label=dot",
            &[("content-type", "image/png"), ("x-custom", "yes")],
            Body::from(PNG.to_vec()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());
    assert_eq!(
        reply.json(),
        json!({
            "isBytes": true, "length": PNG.len(), "first": 0x50, "label": "dot",
            "method": "POST", "contentType": "image/png", "sawCookie": false, "custom": "yes",
        })
    );
    // A JSON-labelled body to a bytes query is its bytes too.
    let reply = alice
        .post_json("/api/images/measure?label=x", json!([1, 2]))
        .await;
    assert_eq!(reply.json()["length"], json!(5), "{}", reply.json());

    // The image, raw, under its own content type, to anybody.
    let mut stranger = Client::new(server.router.clone());
    let reply = stranger.get("/api/images/dot", &[]).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "{:?}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.header("content-type"), Some("image/png"));
    assert_eq!(reply.header("x-pixels"), Some("1"));
    assert_eq!(reply.body, PNG.to_vec());
    Ok(())
}

#[tokio::test]
async fn a_share_query_serves_a_large_file_by_its_own_rule() -> sc_error::Result<()> {
    let tmp = TempDir::new("share");
    let server = setup(&tmp).await?;
    let data = film(50 * MB);
    server
        .store
        .write("private/film.mp4", Bytes::from(data.clone()))
        .await?;
    let mut admin = Client::new(server.router.clone());
    admin.login("admin@example.com").await;
    let reply = admin
        .post_json(
            "/api/shares",
            json!({ "token": "t1", "password": "open sesame", "path": "private/film.mp4",
                    "filename": "naïve 名前.mp4" }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());

    // Not the store's rule: a stranger reads the private file through the
    // share, a range at a time, under the original name.
    let mut stranger = Client::new(server.router.clone());
    let url = "/api/share?token=t1&password=open%20sesame";
    let reply = stranger.get(url, &[("range", "bytes=0-99")]).await;
    assert_eq!(
        reply.status,
        StatusCode::PARTIAL_CONTENT,
        "{}",
        reply.json()
    );
    assert_eq!(reply.body, data[..100].to_vec());
    assert_eq!(
        reply.header("content-range"),
        Some(format!("bytes 0-99/{}", data.len()).as_str())
    );
    let disposition = reply.header("content-disposition").unwrap();
    assert!(disposition.starts_with("attachment"), "{disposition}");
    assert!(
        disposition.contains("filename*=UTF-8''na%C3%AFve%20%E5%90%8D%E5%89%8D.mp4"),
        "{disposition}"
    );
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    assert!(reply.header("etag").is_some());
    // Far past the 8 MB a body may read: the bytes never entered the isolate.
    let at = 40 * MB;
    let reply = stranger
        .get(url, &[("range", &format!("bytes={at}-{}", at + 9))])
        .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(reply.body, data[at..at + 10].to_vec());
    // And whole.
    let reply = stranger.get(url, &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body.len(), data.len());

    // A wrong password is the query's own answer.
    let reply = stranger
        .get(
            "/api/share?token=t1&password=guess",
            &[("range", "bytes=0-99")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.json(), json!({ "error": "wrong password" }));

    // Revoked: the next range request of the very same URL is refused.
    let reply = admin
        .send("DELETE", "/api/shares?token=t1", &[], Body::empty())
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());
    let reply = stranger.get(url, &[("range", "bytes=100-199")]).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.json());
    Ok(())
}

#[tokio::test]
async fn a_handle_opened_as_the_caller_is_checked_when_served() -> sc_error::Result<()> {
    let tmp = TempDir::new("as-user");
    let server = setup(&tmp).await?;
    // Alice's file, in the administrator's folder: hers by the owner rule.
    server
        .store
        .write("private/alice.txt", Bytes::from_static(b"alice's"))
        .await?;
    server
        .store
        .set_meta(
            "private/alice.txt",
            &FileMeta {
                owner: Some(server.users["alice"].clone()),
                ..FileMeta::default()
            },
        )
        .await?;

    let mut alice = Client::new(server.router.clone());
    alice.login("alice@example.com").await;
    let reply = alice.get("/api/mine?path=private/alice.txt", &[]).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());
    assert_eq!(reply.body, b"alice's".to_vec());

    // Bob may not read it, and is told what a missing file would tell him.
    let mut bob = Client::new(server.router.clone());
    bob.login("bob@example.com").await;
    let refused = bob.get("/api/mine?path=private/alice.txt", &[]).await;
    let missing = bob.get("/api/mine?path=private/nothing.txt", &[]).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(refused.json(), missing.json());
    Ok(())
}
