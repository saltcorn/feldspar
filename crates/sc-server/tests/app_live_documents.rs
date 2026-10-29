//! Milestone L5: **collaborative documents** (TODO.md "Live updates"), through
//! the assembled router on a real port against a real database, with real
//! WebSocket clients — several users, several connections.
//!
//! The notes: Alice owns note 1; Bob has a share that may only read it, Carol
//! one that may edit it, and Dave none. `note_body` is a `document` stream on
//! `notes.body` (a `collab_doc` field, an `xml` root as TipTap binds it),
//! mirrored as plain text into `body_text`. The "browsers" here are `yrs`
//! documents (`sc_live::CollabDoc`) — the format test in `sc-live` proves an
//! update made by the JS `yjs` package is the same bytes.
//!
//! On **SQLite** (a database file in a temporary directory), so it runs where
//! no Postgres is available; the row lock a persist takes is Postgres's `FOR
//! UPDATE`, which SQLite's write transaction stands in for.
//!
//! What is claimed, by task:
//!
//! - **L5.3** a subscription is sent `ready`, the member list (presence is
//!   always on) and `doc_sync` with `read_only` set unless the caller may update
//!   the row: Alice and Carol may edit, Bob reads — his `doc_update` is
//!   `unavailable` and reaches nobody — and Dave, who may not read the note,
//!   is `unavailable` like a misspelt name. An edit reaches every other member
//!   as `doc_update` and never comes back to its sender. A member who loses
//!   update access is sent `doc_sync` again with `read_only: true`; one who
//!   loses read access is `revoked`. A document over 1 MB opens; a paste over
//!   64 KB is accepted; a frame over the `doc_update` cap is `too_large`.
//! - **L5.4** the merged text is persisted, debounced and when the room closes:
//!   `GET /api/notes` has it in `body_text`, and has no `body`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{SinkExt, StreamExt};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, StreamRef};
use sc_auth::{ROLE_ADMIN, Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, DataField, DataFieldKind, FieldId, FieldMeta, TableId, save_field_meta};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_live::{CollabDoc, DocShape, LiveLimits, RootKind};
use sc_stream::{Document, Stream, StreamConfig, StreamProvider, StreamRegistry, save_stream};
use sc_test_harness::Teardown;
use sc_types::{BasicType, TypeRef};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;
use uuid::Uuid;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "notes.example.com";
const ADMIN: &str = "admin@example.com";
const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";
const CAROL: &str = "carol@example.com";
const DAVE: &str = "dave@example.com";
const PASSWORD: &str = "correct-horse";
const ROLE_MEMBER: u8 = 40;

/// The owner, or anybody it is shared with — and for anything but a read, only
/// a share that may edit.
const NOTES_RULE: &str =
    "owner === user.id || sharesↃnote.some(s => s.member === user.id && (_read || s.can_edit))";

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A cookie-carrying client addressing one host through `oneshot`.
struct Client {
    router: Router,
    host: &'static str,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, self.host);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET"
            && let Some(csrf) = self.cookies.get(sc_server::CSRF_COOKIE)
        {
            builder = builder.header(sc_server::CSRF_HEADER, csrf);
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
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    /// Sign in through this host's own login, answering the session token.
    async fn login(&mut self, email: &str) -> String {
        self.send("GET", "/api/whoami", None).await;
        self.send("GET", "/api/auth/status", None).await;
        let (status, body) = self
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        self.cookies
            .get(sc_server::SESSION_COOKIE)
            .expect("the login set a session")
            .clone()
    }
}

struct Server {
    router: Router,
    catalog: Arc<Catalog>,
    addr: std::net::SocketAddr,
    users: HashMap<&'static str, Uuid>,
    _dir: Scratch,
}

/// A directory removed when the test ends.
struct Scratch(std::path::PathBuf, Teardown);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Server {
    fn client(&self, host: &'static str) -> Client {
        Client {
            router: self.router.clone(),
            host,
            cookies: HashMap::new(),
        }
    }

    fn id(&self, email: &str) -> String {
        self.users[email].hyphenated().to_string()
    }

    /// A page on the app, signed in as `email`.
    async fn page(&self, email: &str) -> Socket {
        let session = self.client(APP_HOST).login(email).await;
        self.socket("/api/live", APP_HOST, &session).await
    }

    async fn socket(&self, path: &str, host: &'static str, session: &str) -> Socket {
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert(header::HOST, HeaderValue::from_static(host));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{host}")).unwrap(),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{}={session}", sc_server::SESSION_COOKIE)).unwrap(),
        );
        tokio_tungstenite::connect_async(request)
            .await
            .expect("the socket opens from its own page")
            .0
    }
}

fn note_body() -> Stream {
    let mut stream = Stream::new("note_body", "document").min_role(ROLE_MEMBER);
    stream.configuration = json!({
        "table": "notes",
        "field": "body",
        "root_kind": "xml",
        "text_mirror": "body_text",
        "mirror_format": "text",
    })
    .as_object()
    .cloned()
    .unwrap();
    stream
}

fn app() -> Application {
    Application::new("Notes", "notes", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_table(TableId("notes".to_owned()))
        .with_stream(StreamRef::new("note_body"))
}

/// Persisting quickly, so a test sees it; the re-check clock turned up so
/// anything revoked was revoked by a write.
fn limits() -> LiveLimits {
    LiveLimits {
        status_interval: Duration::from_millis(100),
        recheck_interval: Duration::from_secs(3600),
        doc_persist_debounce: Duration::from_millis(100),
        ..LiveLimits::default()
    }
}

async fn setup() -> Result<Server> {
    let dir = Scratch(
        std::env::temp_dir().join(format!("sc-live-docs-{}", Uuid::new_v4())),
        Teardown::new(),
    );
    std::fs::create_dir_all(&dir.0).unwrap();
    let driver = Arc::new(SqliteDriver::open(dir.0.join("notes.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    dir.1.add(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    let id = || {
        DataField::plain("id", TypeRef::Basic(BasicType::Int))
            .required()
            .primary_key()
            .generated(ColumnGenerator::Identity)
    };
    let plain = |name: &str, ty: BasicType| DataField::plain(name, TypeRef::Basic(ty));
    catalog
        .create_table(
            "notes",
            &[
                id(),
                plain("title", BasicType::Text),
                plain("owner", BasicType::Uuid),
                plain("body", BasicType::Bytes),
                plain("body_text", BasicType::Text),
            ],
        )
        .await?;
    catalog
        .create_table(
            "shares",
            &[
                id(),
                DataField {
                    kind: DataFieldKind::Key {
                        target_table: TableId("notes".to_owned()),
                        target_field: FieldId("id".to_owned()),
                        summary_field: None,
                    },
                    ..plain("note", BasicType::Int)
                },
                plain("member", BasicType::Uuid),
                plain("can_edit", BasicType::Bool).required(),
            ],
        )
        .await?;
    catalog.reload().await?;
    // `body` is a document.
    save_field_meta(
        &catalog,
        &FieldMeta::new("notes", "body").rich_type(sc_types::COLLAB_DOC),
    )
    .await?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, ADMIN, PASSWORD, ROLE_ADMIN).await?;
    let mut users = HashMap::new();
    for email in [ALICE, BOB, CAROL, DAVE] {
        let user = create_user(&catalog, email, PASSWORD, ROLE_MEMBER).await?;
        users.insert(email, user.id);
    }

    let evaluator = sc_server::default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let triggers =
        sc_server::install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;

    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(Document) as Arc<dyn StreamProvider>)?;
    let registry = Arc::new(registry);
    let stream_services = sc_server::install_streams_with(
        &catalog,
        &triggers,
        registry.clone(),
        StreamConfig::default(),
    )
    .await?;
    save_stream(&catalog, &registry, &note_body()).await?;
    stream_services.reload(&catalog).await?;

    let apps = Arc::new(
        sc_server::AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_evaluator(evaluator)
            .with_triggers(triggers.clone())
            .with_models(models)
            .with_streams(stream_services)
            .with_live_limits(limits()),
    );
    let config = sc_server::ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..sc_server::ServerConfig::default()
    };
    let sessions = Arc::new(SessionStore::database_with(
        catalog.clone(),
        chrono::Duration::hours(24),
        chrono::Duration::zero(),
        100,
    ));
    let router = sc_server::build_router_with_apps(
        &sc_api::admin_endpoints(),
        sc_server::admin_handlers(catalog.clone(), apps.clone()),
        sessions,
        &config,
        apps.clone(),
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });

    let server = Server {
        router,
        catalog: catalog.clone(),
        addr,
        users,
        _dir: dir,
    };
    let (alice, bob, carol) = (server.id(ALICE), server.id(BOB), server.id(CAROL));
    let notes = catalog.require("notes")?;
    for (id, title) in [(1, "Plans"), (2, "Big")] {
        sc_api::rows::create_row(
            &catalog,
            &notes,
            &json!({ "id": id, "title": title, "owner": alice }),
        )
        .await?;
    }
    let shares = catalog.require("shares")?;
    for (id, member, can_edit) in [(1, &bob, false), (2, &carol, true)] {
        sc_api::rows::create_row(
            &catalog,
            &shares,
            &json!({ "id": id, "note": 1, "member": member, "can_edit": can_edit }),
        )
        .await?;
    }

    let mut admin = server.client(BASE_DOMAIN);
    admin.login(ADMIN).await;
    admin
        .ok(
            "PUT",
            "/api/tables/notes",
            Some(json!({
                "label": "", "description": "",
                "min_role_read": ROLE_ADMIN, "min_role_write": ROLE_ADMIN,
                "ownership_formula": NOTES_RULE, "rls_enabled": false, "sync_enabled": false,
            })),
        )
        .await;

    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    apps.mount(sc_server::MountedApp::new_with(
        app(),
        framework,
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;
    Ok(server)
}

// --- frames -------------------------------------------------------------------

async fn send(socket: &mut Socket, frame: Value) {
    socket
        .send(Message::text(frame.to_string()))
        .await
        .expect("the socket takes a frame");
}

async fn next_frame(socket: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await
            .expect("a frame arrives within the timeout")
            .expect("the socket is still open")
            .expect("the frame is readable");
        match message {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// Subscribe and wait for `ready` or `error`; with presence, the member list
/// follows `ready` and is read by [`frame_for`].
async fn subscribe(socket: &mut Socket, frame: Value) -> Value {
    let sub = frame["sub"].clone();
    send(socket, frame).await;
    loop {
        let frame = next_frame(socket).await;
        if frame["sub"] == sub && (frame["type"] == "ready" || frame["type"] == "error") {
            return frame;
        }
    }
}

/// The next frame of `kind` about `sub`, skipping status frames and others'.
async fn frame_for(socket: &mut Socket, sub: &str, kind: &str) -> Value {
    loop {
        let frame = next_frame(socket).await;
        if frame["type"] == json!(kind) && frame["sub"] == json!(sub) {
            return frame;
        }
        assert!(
            !(frame["sub"] == json!(sub) && frame["type"] == "error"),
            "expected `{kind}` for {sub}, got an error: {frame}"
        );
    }
}

/// Every frame up to the answer to a `ping` — what has arrived by now.
async fn drain(socket: &mut Socket) -> Vec<Value> {
    send(socket, json!({ "type": "ping" })).await;
    let mut frames = Vec::new();
    loop {
        let frame = next_frame(socket).await;
        if frame["type"] == "pong" {
            return frames;
        }
        frames.push(frame);
    }
}

fn on_note(sub: &str, note: i64) -> Value {
    json!({ "type": "subscribe", "sub": sub, "stream": "note_body", "topic": note })
}

/// A browser's copy of a document: what `doc_sync` handed it.
fn browser(sync: &Value) -> CollabDoc {
    let bytes = BASE64.decode(sync["update"].as_str().unwrap()).unwrap();
    CollabDoc::load(DocShape::new("default", RootKind::Xml), Some(&bytes)).unwrap()
}

/// Apply a `doc_update` frame to a browser's copy.
fn apply(doc: &CollabDoc, frame: &Value) {
    let bytes = BASE64.decode(frame["update"].as_str().unwrap()).unwrap();
    doc.apply(&bytes).unwrap();
}

fn doc_update(sub: &str, update: &[u8]) -> Value {
    json!({ "type": "doc_update", "sub": sub, "update": BASE64.encode(update) })
}

/// The next `error` frame about no subscription — a frame refused unread.
async fn unaddressed_error(socket: &mut Socket) -> Value {
    loop {
        let frame = next_frame(socket).await;
        if frame["type"] == "error" && frame.get("sub").is_none() {
            return frame;
        }
    }
}

/// Open note `note` as a page signed in as `email`: `ready`, the member list
/// and the document.
async fn open(server: &Server, email: &str, note: i64) -> (Socket, Value) {
    let mut socket = server.page(email).await;
    let ready = subscribe(&mut socket, on_note("d", note)).await;
    assert_eq!(ready["type"], "ready", "{email}: {ready}");
    assert_eq!(
        ready["can_publish"], false,
        "a document takes edits, not publishes"
    );
    frame_for(&mut socket, "d", "presence").await;
    let sync = frame_for(&mut socket, "d", "doc_sync").await;
    (socket, sync)
}

/// The app's REST read of `notes`, as Alice.
async fn rest_notes(server: &Server) -> Vec<Value> {
    let mut client = server.client(APP_HOST);
    client.login(ALICE).await;
    let listed = client.ok("GET", "/api/notes", None).await;
    listed
        .as_array()
        .cloned()
        .or_else(|| listed["rows"].as_array().cloned())
        .unwrap_or_default()
}

/// Wait until the REST read of note `id` has `body_text` equal to `text`.
async fn mirrored(server: &Server, id: i64, text: &str) -> Vec<Value> {
    for _ in 0..100 {
        let rows = rest_notes(server).await;
        if rows
            .iter()
            .any(|r| r["id"] == json!(id) && r["body_text"] == json!(text))
        {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "note {id} never mirrored {text:?}: {:?}",
        rest_notes(server).await
    );
}

// --- L5.3: who may read and edit, and what the others receive -----------------

#[tokio::test]
async fn editors_see_each_others_edits_and_a_reader_cannot_type() -> Result<()> {
    let server = setup().await?;
    let (mut alice, sync) = open(&server, ALICE, 1).await;
    assert_eq!(sync["read_only"], false, "the owner edits");
    let (mut carol, sync_c) = open(&server, CAROL, 1).await;
    assert_eq!(sync_c["read_only"], false, "an editing share edits");
    let (mut bob, sync_b) = open(&server, BOB, 1).await;
    assert_eq!(sync_b["read_only"], true, "a reading share reads");

    // Dave may not read the note: the same `unavailable` as a misspelt name.
    let mut dave = server.page(DAVE).await;
    let refused = subscribe(&mut dave, on_note("d", 1)).await;
    let misspelt = subscribe(
        &mut dave,
        json!({ "type": "subscribe", "sub": "x", "stream": "note_bdy", "topic": 1 }),
    )
    .await;
    assert_eq!(refused["code"], "unavailable", "{refused}");
    assert_eq!(
        refused["message"]
            .as_str()
            .map(|m| m.replace("note_body", "?")),
        misspelt["message"]
            .as_str()
            .map(|m| m.replace("note_bdy", "?"))
    );

    // Alice types; Carol and Bob receive it, Alice does not get it back.
    let (doc_a, doc_c, doc_b) = (browser(&sync), browser(&sync_c), browser(&sync_b));
    let edit = doc_a.append_paragraph("Agenda").unwrap();
    send(&mut alice, doc_update("d", &edit)).await;
    let got = frame_for(&mut carol, "d", "doc_update").await;
    apply(&doc_c, &got);
    let got = frame_for(&mut bob, "d", "doc_update").await;
    apply(&doc_b, &got);
    assert_eq!(doc_c.text(), "Agenda");
    assert_eq!(doc_b.text(), "Agenda");
    assert!(
        drain(&mut alice)
            .await
            .iter()
            .all(|f| f["type"] != "doc_update"),
        "an edit is not sent back to its sender"
    );

    // Carol types too, and Alice receives it.
    let edit = doc_c.append_paragraph("Budget").unwrap();
    send(&mut carol, doc_update("d", &edit)).await;
    let got = frame_for(&mut alice, "d", "doc_update").await;
    apply(&doc_a, &got);
    assert_eq!(doc_a.text(), "Agenda\nBudget");

    // Bob's edit is refused, worded like absence, and reaches nobody.
    let forged = doc_b.append_paragraph("Bob was here").unwrap();
    send(&mut bob, doc_update("d", &forged)).await;
    let refused = frame_for(&mut bob, "d", "error").await;
    assert_eq!(refused["code"], "unavailable", "{refused}");
    for socket in [&mut alice, &mut carol] {
        assert!(
            drain(socket)
                .await
                .iter()
                .all(|f| f["type"] != "doc_update")
        );
    }
    // Garbage is `invalid`, and the connection stays up.
    send(
        &mut alice,
        json!({ "type": "doc_update", "sub": "d", "update": "%%%" }),
    )
    .await;
    assert_eq!(frame_for(&mut alice, "d", "error").await["code"], "invalid");
    send(&mut alice, doc_update("d", b"\xff\xfe\xfd")).await;
    assert_eq!(frame_for(&mut alice, "d", "error").await["code"], "invalid");

    // Everybody leaves; the room persists. REST has the text, never `body`.
    drop((alice, bob, carol, dave));
    let rows = mirrored(&server, 1, "Agenda\nBudget").await;
    assert!(rows.iter().all(|r| r.get("body").is_none()), "{rows:?}");

    // Reopened, the document is there.
    let (_alice, sync) = open(&server, ALICE, 1).await;
    assert_eq!(browser(&sync).text(), "Agenda\nBudget");
    Ok(())
}

#[tokio::test]
async fn losing_edit_access_is_read_only_and_losing_read_access_is_revoked() -> Result<()> {
    let server = setup().await?;
    let (mut carol, sync) = open(&server, CAROL, 1).await;
    assert_eq!(sync["read_only"], false);
    let shares = server.catalog.require("shares")?;

    // The share stops allowing edits: Carol is sent the document again,
    // read-only, and her next edit is refused.
    sc_api::rows::update_row(&server.catalog, &shares, "2", &json!({ "can_edit": false })).await?;
    let again = frame_for(&mut carol, "d", "doc_sync").await;
    assert_eq!(again["read_only"], true, "{again}");
    let edit = browser(&again).append_paragraph("late").unwrap();
    send(&mut carol, doc_update("d", &edit)).await;
    assert_eq!(
        frame_for(&mut carol, "d", "error").await["code"],
        "unavailable"
    );

    // And back.
    sc_api::rows::update_row(&server.catalog, &shares, "2", &json!({ "can_edit": true })).await?;
    let again = frame_for(&mut carol, "d", "doc_sync").await;
    assert_eq!(again["read_only"], false, "{again}");

    // The share goes: the subscription is revoked.
    sc_api::rows::delete_row(&server.catalog, &shares, "2").await?;
    let revoked = frame_for(&mut carol, "d", "revoked").await;
    assert_eq!(revoked["sub"], "d");
    Ok(())
}

#[tokio::test]
async fn a_large_document_opens_and_a_long_paste_is_accepted() -> Result<()> {
    let server = setup().await?;
    // Note 2 holds a document over 1 MB.
    let big = CollabDoc::empty(DocShape::new("default", RootKind::Xml));
    for n in 0..2_000 {
        big.append_paragraph(&format!("{n:05} {}", "lorem ipsum dolor ".repeat(30)))
            .unwrap();
    }
    let state = big.state();
    assert!(state.len() > 1024 * 1024, "{}", state.len());
    let notes = server.catalog.require("notes")?;
    sc_api::rows::write_document_in(
        &server.catalog,
        &notes,
        "2",
        "body",
        &state,
        None,
        &sc_api::rows::Executor::Pooled,
    )
    .await?;
    let (mut alice, sync) = open(&server, ALICE, 2).await;
    let doc = browser(&sync);
    assert_eq!(doc.text(), big.text(), "the whole document arrived");

    // A paste of 100 KB — over the 64 KB every other frame is held to — is
    // accepted, and reaches another editor.
    let (mut again, sync2) = open(&server, ALICE, 2).await;
    let other = browser(&sync2);
    let paste = doc.append_paragraph(&"pasted ".repeat(15_000)).unwrap();
    let frame = doc_update("d", &paste).to_string();
    assert!(frame.len() > 64 * 1024, "{}", frame.len());
    send(&mut alice, doc_update("d", &paste)).await;
    apply(&other, &frame_for(&mut again, "d", "doc_update").await);
    assert_eq!(other.text(), doc.text());

    // A frame over the `doc_update` cap is not read: `too_large`.
    let huge = json!({ "type": "doc_update", "sub": "d", "update": "A".repeat(1024 * 1024 + 16) });
    send(&mut alice, huge).await;
    let refused = unaddressed_error(&mut alice).await;
    assert_eq!(refused["code"], "too_large", "{refused}");
    // And a 100 KB frame of any other kind is still over its cap.
    send(
        &mut alice,
        json!({ "type": "presence", "sub": "d", "state": "x".repeat(100_000) }),
    )
    .await;
    let refused = unaddressed_error(&mut alice).await;
    assert_eq!(refused["code"], "too_large", "{refused}");
    Ok(())
}
