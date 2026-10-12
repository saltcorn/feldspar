//! An application's **upload slots**, served (TODO F2.5–F2.7, §2.3): the tus
//! protocol on `{mount}/_upload/{name}`, through the real router against a real
//! database (SQLite, a file per test).
//!
//! - a 30 MB file sent in 5 MB chunks, broken off in the middle of the third,
//!   resumes from what `HEAD` says arrived and completes; the file is the
//!   uploader's, keeps its original name, and is served to them — with Range —
//!   and to nobody else;
//! - another user's `PATCH`, `HEAD` and `DELETE` of that upload are 404s;
//! - a hook's `httpError(403)` refuses at creation, and its `path` and `meta`
//!   are where and what the file becomes;
//! - a slot's `max_bytes` refuses a declared size over it, and stops reading a
//!   body that runs past it **while it streams**;
//! - the sweep removes an upload abandoned for a day, bytes and all;
//! - `--max-request-body` and `--max-upload-bytes` are honoured, and a body
//!   over either is a JSON `413`, not axum's plain text.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use base64::Engine;
use bytes::Bytes;
use sc_api::{CustomQuery, Method, QueryLanguage, set_custom_queries};
use sc_app::{
    ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, StaticDir, UploadSlot,
};
use sc_auth::{Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, FileStoreId};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_files::{FileMeta, FileStore, LocalFileStore};
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, FILE_PATH_HEADER, FILE_URL_HEADER, MountedApp,
    ServerConfig, admin_handlers, build_router_with_apps, default_js_evaluator, install_triggers,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "drop.example.com";
const PASSWORD: &str = "correct-horse";
const ROLE_MEMBER: u8 = 80;
const MB: usize = 1024 * 1024;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-uploads-{}-{tag}-{:?}",
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

/// A cookie-carrying browser on one host. Every request it makes carries the
/// CSRF header a page would send.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

/// A response, read to the end.
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
    fn new(router: Router, host: &str) -> Client {
        Client {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
        }
    }

    async fn send(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> Reply {
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
        if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        for (name, value) in headers {
            builder = builder.header(*name, value);
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

    async fn get(&mut self, path: &str, headers: &[(&str, String)]) -> Reply {
        self.send("GET", path, headers, Body::empty()).await
    }

    /// The first contact a page makes, which mints its CSRF cookie.
    async fn prime(&mut self) {
        self.get("/", &[]).await;
    }

    async fn login(&mut self, email: &str) {
        self.prime().await;
        let body = serde_json::to_vec(&json!({ "email": email, "password": PASSWORD })).unwrap();
        let reply = self
            .send(
                "POST",
                "/api/login",
                &[("content-type", "application/json".to_owned())],
                Body::from(body),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());
    }

    /// `POST {slot}`: begin an upload of `length` bytes named `filename`.
    async fn create(&mut self, slot: &str, filename: &str, length: usize) -> Reply {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        self.send(
            "POST",
            &format!("/api/_upload/{slot}"),
            &[
                ("tus-resumable", "1.0.0".to_owned()),
                ("upload-length", length.to_string()),
                (
                    "upload-metadata",
                    format!("filename {},filetype {}", b64(filename), b64("video/mp4")),
                ),
            ],
            Body::empty(),
        )
        .await
    }

    /// `PATCH {location}` with one chunk at `offset`.
    async fn patch(&mut self, location: &str, offset: usize, body: Body) -> Reply {
        self.send(
            "PATCH",
            location,
            &[
                ("tus-resumable", "1.0.0".to_owned()),
                ("upload-offset", offset.to_string()),
                ("content-type", "application/offset+octet-stream".to_owned()),
            ],
            body,
        )
        .await
    }

    async fn head(&mut self, location: &str) -> Reply {
        self.send(
            "HEAD",
            location,
            &[("tus-resumable", "1.0.0".to_owned())],
            Body::empty(),
        )
        .await
    }
}

/// A file's bytes: not all one value, so a misplaced chunk shows.
fn film(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// A body that sends `first` and then breaks off — a dropped connection.
fn broken_body(first: Vec<u8>) -> Body {
    let items: Vec<Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from(first)),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "the network went away",
        )),
    ];
    Body::from_stream(futures::stream::iter(items))
}

/// A body of `chunks` chunks of `size` bytes, counting how many were taken.
fn counted_body(chunks: usize, size: usize, taken: Arc<AtomicUsize>) -> Body {
    let stream = futures::stream::iter((0..chunks).map(move |_| {
        taken.fetch_add(1, Ordering::SeqCst);
        Ok::<_, std::io::Error>(Bytes::from(vec![7u8; size]))
    }));
    Body::from_stream(stream)
}

fn drop_app() -> Application {
    let mut api = ApiConfig::new("rest", "/api");
    set_custom_queries(
        &mut api.config,
        &[CustomQuery::new(
            "beforeGuarded",
            Method::Post,
            "/hooks/guarded",
            "if (body.filename.startsWith('secret')) throw httpError(403, 'not that one');\n\
             return { path: user.email.split('@')[0] + '/' + body.filename, \
                      meta: { checked: 'yes', size: body.size } };",
        )
        .language(QueryLanguage::Javascript)],
    )
    .unwrap();
    Application::new("Drop", "drop", FrameworkRef::new("code"))
        .with_file_store(FileStoreId("files".to_owned()))
        .with_api(api)
        .with_static_dir(StaticDir::new(
            "/media",
            FileStoreId("files".to_owned()),
            "drops",
        ))
        .with_upload(
            UploadSlot::new("drop", "files", "drops")
                .min_role(ROLE_MEMBER)
                .max_bytes(40 * MB as u64),
        )
        .with_upload(
            UploadSlot::new("small", "files", "small")
                .min_role(ROLE_MEMBER)
                .max_bytes(MB as u64),
        )
        .with_upload(
            UploadSlot::new("guarded", "files", "guarded")
                .min_role(ROLE_MEMBER)
                .hook("beforeGuarded"),
        )
        .with_upload(
            UploadSlot::new("images", "files", "images")
                .min_role(ROLE_MEMBER)
                .mime_allow(["image/png"]),
        )
        .with_upload(UploadSlot::new("admins", "files", "admins"))
}

struct Server {
    router: Router,
    catalog: Arc<Catalog>,
    store: Arc<LocalFileStore>,
}

async fn setup(tmp: &TempDir, config: ServerConfig) -> sc_error::Result<Server> {
    let db_dir = tmp.path().join("db");
    std::fs::create_dir_all(&db_dir).map_err(sc_error::Error::from)?;
    let driver = Arc::new(SqliteDriver::open(db_dir.join("drop.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    for who in ["alice", "bob"] {
        create_user(
            &catalog,
            &format!("{who}@example.com"),
            PASSWORD,
            ROLE_MEMBER,
        )
        .await?;
    }
    create_user(&catalog, "admin@example.com", PASSWORD, sc_auth::ROLE_ADMIN).await?;

    let files_dir = tmp.path().join("files");
    std::fs::create_dir_all(&files_dir).map_err(sc_error::Error::from)?;
    let store = Arc::new(LocalFileStore::new("files", &files_dir)?);
    catalog.connect_file_store(store.clone())?;
    // The folder the drops land in is the administrator's: a member reads only
    // what they own there (§2.1).
    store.mkdir("drops").await?;
    store
        .set_meta(
            "drops",
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
    // Mounted from the record, as the other application tests here are (a
    // `code` framework with no source tree would not save); the slots are
    // still held to what a save checks.
    let app = drop_app();
    sc_app::validate_upload_slots(&app)?;
    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    apps.mount(MountedApp::new_with(
        app,
        framework,
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..config
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
        catalog,
        store,
    })
}

#[tokio::test]
async fn a_large_upload_resumes_after_a_dropped_connection_and_is_its_uploaders_alone()
-> sc_error::Result<()> {
    let tmp = TempDir::new("resume");
    let server = setup(&tmp, ServerConfig::default()).await?;
    let mut alice = Client::new(server.router.clone(), APP_HOST);
    alice.login("alice@example.com").await;

    let data = film(30 * MB);
    let created = alice.create("drop", "film.mp4", data.len()).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    assert_eq!(created.header("tus-resumable"), Some("1.0.0"));
    let location = created.header("location").unwrap().to_owned();
    assert!(location.starts_with("/api/_upload/drop/"), "{location}");

    // Two 5 MB chunks arrive whole…
    let chunk = 5 * MB;
    for i in 0..2 {
        let reply = alice
            .patch(
                &location,
                i * chunk,
                Body::from(data[i * chunk..(i + 1) * chunk].to_vec()),
            )
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.json());
        assert_eq!(
            reply.header("upload-offset"),
            Some(&*((i + 1) * chunk).to_string())
        );
    }
    // …and the third breaks off after 2 MB.
    let broken = alice
        .patch(
            &location,
            2 * chunk,
            broken_body(data[2 * chunk..2 * chunk + 2 * MB].to_vec()),
        )
        .await;
    assert!(broken.header(FILE_PATH_HEADER).is_none());

    // The client asks how far it got, and carries on from there.
    let head = alice.head(&location).await;
    assert_eq!(head.status, StatusCode::OK);
    let resume_at: usize = head.header("upload-offset").unwrap().parse().unwrap();
    assert_eq!(resume_at, 2 * chunk + 2 * MB, "what arrived is kept");
    assert_eq!(head.header("upload-length"), Some(&*data.len().to_string()));

    // Starting from the wrong place is a conflict that says where it is.
    let wrong = alice
        .patch(&location, 2 * chunk, Body::from(vec![0u8; 10]))
        .await;
    assert_eq!(wrong.status, StatusCode::CONFLICT);
    assert_eq!(wrong.header("upload-offset"), Some(&*resume_at.to_string()));

    let mut offset = resume_at;
    let mut last = None;
    while offset < data.len() {
        let end = (offset + chunk).min(data.len());
        let reply = alice
            .patch(&location, offset, Body::from(data[offset..end].to_vec()))
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.json());
        offset = end;
        last = Some(reply);
    }
    let last = last.unwrap();
    let path = last.header(FILE_PATH_HEADER).unwrap().to_owned();
    assert!(
        path.starts_with("drops/") && path.ends_with("-film.mp4"),
        "{path}"
    );
    let url = last.header(FILE_URL_HEADER).unwrap().to_owned();
    assert_eq!(url, format!("/media/{}", path.trim_start_matches("drops/")));

    // The file is all there, in place, and the temporary one is gone.
    assert_eq!(&server.store.read(&path).await?[..], &data[..]);
    let temp = sc_server::UPLOAD_TEMP_DIR;
    assert!(server.store.list(temp).await?.is_empty());

    // Owned by its uploader, and named as it was on their disk.
    let meta = server.store.get_meta(&path).await?;
    let alice_id = sc_auth::load_user_by_email(&server.catalog, "alice@example.com")
        .await?
        .unwrap()
        .id
        .to_string();
    assert_eq!(meta.owner.as_deref(), Some(alice_id.as_str()));
    assert_eq!(
        meta.attributes
            .get(sc_files::FILENAME_ATTRIBUTE)
            .map(String::as_str),
        Some("film.mp4")
    );

    // Served to her at the URL the upload answered with, and seekable.
    let whole = alice.get(&url, &[]).await;
    assert_eq!(whole.status, StatusCode::OK);
    assert_eq!(whole.body.len(), data.len());
    let ranged = alice
        .get(&url, &[("range", "bytes=1000-1099".to_owned())])
        .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&ranged.body[..], &data[1000..1100]);
    assert!(
        ranged
            .header("content-disposition")
            .unwrap()
            .contains("film.mp4")
    );

    // Nobody else's: another member and a stranger get what a missing file gets.
    let mut bob = Client::new(server.router.clone(), APP_HOST);
    bob.login("bob@example.com").await;
    assert_eq!(bob.get(&url, &[]).await.status, StatusCode::NOT_FOUND);
    let mut stranger = Client::new(server.router.clone(), APP_HOST);
    assert_eq!(stranger.get(&url, &[]).await.status, StatusCode::NOT_FOUND);

    // A HEAD after the end still says where the file went.
    let done = alice.head(&location).await;
    assert_eq!(done.header("upload-offset"), Some(&*data.len().to_string()));
    assert_eq!(done.header(FILE_PATH_HEADER), Some(path.as_str()));
    Ok(())
}

#[tokio::test]
async fn only_the_uploader_may_continue_look_at_or_abandon_an_upload() -> sc_error::Result<()> {
    let tmp = TempDir::new("owner");
    let server = setup(&tmp, ServerConfig::default()).await?;
    let mut alice = Client::new(server.router.clone(), APP_HOST);
    alice.login("alice@example.com").await;
    let mut bob = Client::new(server.router.clone(), APP_HOST);
    bob.login("bob@example.com").await;

    let created = alice.create("drop", "mine.mp4", 100).await;
    let location = created.header("location").unwrap().to_owned();

    let theirs = bob.patch(&location, 0, Body::from(vec![1u8; 100])).await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND);
    assert_eq!(bob.head(&location).await.status, StatusCode::NOT_FOUND);
    let deleted = bob
        .send(
            "DELETE",
            &location,
            &[("tus-resumable", "1.0.0".to_owned())],
            Body::empty(),
        )
        .await;
    assert_eq!(deleted.status, StatusCode::NOT_FOUND);
    // A stranger's, the same; and an id that was never one, the same again.
    let mut stranger = Client::new(server.router.clone(), APP_HOST);
    stranger.prime().await;
    assert_eq!(stranger.head(&location).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        alice.head("/api/_upload/drop/not-an-id").await.status,
        StatusCode::NOT_FOUND
    );
    // Nor may a stranger begin one in a members' slot, nor a member in an
    // administrator's.
    assert_eq!(
        stranger.create("drop", "x.mp4", 10).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        alice.create("admins", "x.mp4", 10).await.status,
        StatusCode::FORBIDDEN
    );

    // Untouched by all that: still at 0, and still hers to abandon.
    assert_eq!(
        alice.head(&location).await.header("upload-offset"),
        Some("0")
    );
    let abandoned = alice
        .send(
            "DELETE",
            &location,
            &[("tus-resumable", "1.0.0".to_owned())],
            Body::empty(),
        )
        .await;
    assert_eq!(abandoned.status, StatusCode::NO_CONTENT);
    assert_eq!(alice.head(&location).await.status, StatusCode::NOT_FOUND);
    assert!(
        server
            .store
            .list(sc_server::UPLOAD_TEMP_DIR)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_hook_refuses_or_decides_where_the_file_goes() -> sc_error::Result<()> {
    let tmp = TempDir::new("hook");
    let server = setup(&tmp, ServerConfig::default()).await?;
    let mut alice = Client::new(server.router.clone(), APP_HOST);
    alice.login("alice@example.com").await;

    let refused = alice.create("guarded", "secret.mp4", 4).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.json(), json!({ "error": "not that one" }));
    assert_eq!(refused.header("tus-resumable"), Some("1.0.0"));

    let created = alice.create("guarded", "holiday.mp4", 4).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let location = created.header("location").unwrap().to_owned();
    let done = alice
        .patch(&location, 0, Body::from(b"abcd".to_vec()))
        .await;
    assert_eq!(done.status, StatusCode::NO_CONTENT, "{}", done.json());
    assert_eq!(
        done.header(FILE_PATH_HEADER),
        Some("guarded/alice/holiday.mp4")
    );
    // No route serves that folder, so there is no URL to give.
    assert!(done.header(FILE_URL_HEADER).is_none());
    let meta = server.store.get_meta("guarded/alice/holiday.mp4").await?;
    assert_eq!(
        meta.attributes.get("checked").map(String::as_str),
        Some("yes")
    );
    assert_eq!(meta.attributes.get("size").map(String::as_str), Some("4"));

    // The same path again is a conflict at creation, before any bytes move.
    let again = alice.create("guarded", "holiday.mp4", 4).await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // A type the slot does not take is refused by its name's type.
    let typed = alice.create("images", "film.mp4", 4).await;
    assert_eq!(typed.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        alice.create("images", "logo.png", 4).await.status,
        StatusCode::CREATED
    );
    Ok(())
}

#[tokio::test]
async fn a_slots_limit_is_enforced_up_front_and_mid_stream() -> sc_error::Result<()> {
    let tmp = TempDir::new("limit");
    let server = setup(&tmp, ServerConfig::default()).await?;
    let mut alice = Client::new(server.router.clone(), APP_HOST);
    alice.login("alice@example.com").await;

    // Declared too large: refused before a byte is sent.
    let declared = alice.create("small", "big.mp4", 3 * MB).await;
    assert_eq!(declared.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(declared.header("tus-max-size"), Some(&*MB.to_string()));
    assert!(declared.json()["error"].is_string());

    // Declared within it, then sent past it: the server stops reading at the
    // limit rather than after the body.
    let created = alice.create("small", "liar.mp4", MB).await;
    let location = created.header("location").unwrap().to_owned();
    let taken = Arc::new(AtomicUsize::new(0));
    let reply = alice
        .patch(&location, 0, counted_body(12, MB / 4, taken.clone()))
        .await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(reply.json()["error"].is_string());
    assert!(
        taken.load(Ordering::SeqCst) <= 6,
        "read {} chunks of a body that passed the limit at the 5th",
        taken.load(Ordering::SeqCst)
    );
    let offset: usize = reply.header("upload-offset").unwrap().parse().unwrap();
    assert_eq!(offset, MB, "everything up to the limit was kept");
    // Nothing became a file.
    assert!(
        server
            .store
            .list("small")
            .await
            .map_or(true, |e| e.is_empty())
    );

    // The server's own ceiling caps a slot's.
    let tmp2 = TempDir::new("limit-server");
    let capped = setup(
        &tmp2,
        ServerConfig {
            max_upload_bytes: 10 * MB as u64,
            ..ServerConfig::default()
        },
    )
    .await?;
    let mut alice = Client::new(capped.router.clone(), APP_HOST);
    alice.login("alice@example.com").await;
    let over = alice.create("drop", "film.mp4", 20 * MB).await;
    assert_eq!(over.status, StatusCode::PAYLOAD_TOO_LARGE);
    Ok(())
}

#[tokio::test]
async fn an_upload_abandoned_for_a_day_is_swept() -> sc_error::Result<()> {
    let tmp = TempDir::new("sweep");
    let server = setup(&tmp, ServerConfig::default()).await?;
    let mut alice = Client::new(server.router.clone(), APP_HOST);
    alice.login("alice@example.com").await;
    let created = alice.create("drop", "left.mp4", 100).await;
    let location = created.header("location").unwrap().to_owned();
    alice.patch(&location, 0, Body::from(vec![1u8; 40])).await;
    assert_eq!(
        server.store.list(sc_server::UPLOAD_TEMP_DIR).await?.len(),
        1
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // Not yet a day old: kept.
    assert_eq!(
        sc_server::sweep_uploads(&server.catalog, now - sc_server::ABANDONED_AFTER_SECS).await?,
        0
    );
    // A day later: gone, bytes and all.
    assert_eq!(
        sc_server::sweep_uploads(&server.catalog, now + sc_server::ABANDONED_AFTER_SECS).await?,
        1
    );
    assert!(
        server
            .store
            .list(sc_server::UPLOAD_TEMP_DIR)
            .await?
            .is_empty()
    );
    assert_eq!(alice.head(&location).await.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn the_body_limits_are_settings_and_refuse_with_json() -> sc_error::Result<()> {
    let tmp = TempDir::new("bodies");
    let server = setup(
        &tmp,
        ServerConfig {
            max_request_body: 4096,
            max_upload_bytes: 8192,
            ..ServerConfig::default()
        },
    )
    .await?;
    let mut alice = Client::new(server.router.clone(), APP_HOST);
    alice.prime().await;
    let login = |pad: usize| {
        serde_json::to_vec(&json!({
            "email": "alice@example.com", "password": PASSWORD, "pad": "x".repeat(pad)
        }))
        .unwrap()
    };
    let json_ct = [("content-type", "application/json".to_owned())];

    // Under the limit, the request is served.
    let small = alice
        .send("POST", "/api/login", &json_ct, Body::from(login(3000)))
        .await;
    assert_ne!(small.status, StatusCode::PAYLOAD_TOO_LARGE);
    // Over it, a JSON 413.
    let big = alice
        .send("POST", "/api/login", &json_ct, Body::from(login(5000)))
        .await;
    assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        big.json()["error"].as_str().unwrap().contains("4096"),
        "{}",
        big.json()
    );

    // An upload slot is not bound by it: it streams.
    let mut alice = Client::new(server.router.clone(), APP_HOST);
    alice.login("alice@example.com").await;
    let created = alice.create("drop", "film.mp4", 6000).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let location = created.header("location").unwrap().to_owned();
    let reply = alice.patch(&location, 0, Body::from(vec![3u8; 6000])).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.json());

    // The admin side: a `Bytes` route (the backup) over the request limit, and
    // the file manager's upload over `--max-upload-bytes`, are both JSON 413s.
    let mut admin = Client::new(server.router.clone(), "example.com");
    admin.prime().await;
    let backup = admin
        .send(
            "POST",
            sc_server::BACKUP_CREATE_ROUTE,
            &json_ct,
            Body::from(vec![b' '; 5000]),
        )
        .await;
    assert_eq!(backup.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        backup.json()["error"].is_string(),
        "{:?}",
        String::from_utf8_lossy(&backup.body)
    );

    let login_body = serde_json::to_vec(&json!({
        "email": "admin@example.com", "password": PASSWORD
    }))
    .unwrap();
    let logged_in = admin
        .send("POST", "/api/login", &json_ct, Body::from(login_body))
        .await;
    assert_eq!(logged_in.status, StatusCode::OK, "{}", logged_in.json());
    let upload = admin
        .send(
            "POST",
            "/upload/files/big.bin",
            &[],
            Body::from(vec![0u8; 9000]),
        )
        .await;
    assert_eq!(upload.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(upload.json()["error"].as_str().unwrap().contains("8192"));
    let fits = admin
        .send(
            "POST",
            "/upload/files/ok.bin",
            &[],
            Body::from(vec![0u8; 8000]),
        )
        .await;
    assert_eq!(fits.status, StatusCode::CREATED, "{}", fits.json());
    Ok(())
}
