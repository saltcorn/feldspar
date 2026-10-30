//! TODO F7: **streams opened by a code query**, through the assembled router on
//! a real port against a real database, with real WebSocket clients.
//!
//! The case is a share link held by somebody who is not signed in. Alice owns
//! note 1. `note_body` is a `document` stream on `notes.body` that only a
//! member may subscribe to, and only to a note they own. `share_links` holds
//! tokens with a mode: `edit-token` may edit note 1, `read-token` may read it,
//! `stale-token` is expired. The public query `openNote` checks a token and
//! ends in `return live.stream(...)`; an anonymous page opens it with the
//! socket's `open` frame.
//!
//! What is claimed:
//!
//! - an anonymous page with an edit token is sent `ready` (with `opened`
//!   saying what it is) and `doc_sync` with `read_only: false`, and its edit
//!   reaches Alice, who opened the stream directly; a read token is
//!   `read_only`, and its `doc_update` is refused;
//! - a wrong token, an expired one, a stream the app does not expose and a
//!   query that is not a stream query are the same `unavailable`;
//! - deleting the share revokes at once (the re-check clock is an hour), and
//!   switching it to `read` sends `doc_sync` again with `read_only: true`;
//! - an `internal` row stream opened by the query says `kind: "row"`, and
//!   `can_publish` follows the token's mode;
//! - the query's per-IP rate limit is `rate_limited`, and re-checks do not
//!   count against it;
//! - over HTTP the query is a 400.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{SinkExt, StreamExt};
use sc_api::{CustomParam, CustomQuery, Method, QueryLanguage, QueryRateLimit, ResponseType};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, StreamRef};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, DataField, FieldMeta, TableId, save_field_meta};
use sc_db::{ColumnGenerator, DatabaseDriver};
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_live::{CollabDoc, DocShape, LiveLimits, RootKind};
use sc_stream::{
    Document, Internal, Stream, StreamConfig, StreamProvider, StreamRegistry, save_stream,
};
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
const PASSWORD: &str = "correct-horse";
const ROLE_MEMBER: u8 = 40;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The query a share link is opened with: the token decides the row and the
/// mode, the page's flags only which of the app's streams it is asked for.
const OPEN_NOTE: &str = r#"
// The token is the check: whoever holds it may not read `share_links` themselves.
const link = await db.asAdmin().share_links.where({ token: body.token }).first();
if (!link || link.mode === "expired") return null;
const stream = body.cursor ? "note_cursor" : body.hidden ? "hidden_doc" : "note_body";
return live.stream(stream, link.note, { write: link.mode === "edit" });
"#;

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
    addr: SocketAddr,
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

    /// A page on the app, signed in as `email`, or anonymous for `None`.
    async fn page(&self, email: Option<&str>) -> Socket {
        let session = match email {
            Some(email) => Some(self.client(APP_HOST).login(email).await),
            None => None,
        };
        let mut request = format!("ws://{}/api/live", self.addr)
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert(header::HOST, HeaderValue::from_static(APP_HOST));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{APP_HOST}")).unwrap(),
        );
        if let Some(session) = session {
            headers.insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("{}={session}", sc_server::SESSION_COOKIE)).unwrap(),
            );
        }
        tokio_tungstenite::connect_async(request)
            .await
            .expect("the socket opens from its own page")
            .0
    }

    /// Set the mode of the share with `token`, or delete it for `None` —
    /// through the row layer, as the app's own code would.
    async fn share_mode(&self, token: &str, mode: Option<&str>) {
        let links = self.catalog.require("share_links").unwrap();
        let id = match token {
            "edit-token" => "1",
            "read-token" => "2",
            _ => "3",
        };
        match mode {
            Some(mode) => {
                sc_api::rows::update_row(&self.catalog, &links, id, &json!({ "mode": mode }))
                    .await
                    .unwrap();
            }
            None => {
                sc_api::rows::delete_row(&self.catalog, &links, id)
                    .await
                    .unwrap();
            }
        }
    }
}

fn document(name: &str) -> Stream {
    let mut stream = Stream::new(name, "document").min_role(ROLE_MEMBER);
    stream.configuration = json!({ "table": "notes", "field": "body", "root_kind": "xml" })
        .as_object()
        .cloned()
        .unwrap();
    stream
}

fn cursor() -> Stream {
    let mut stream = Stream::new("note_cursor", "internal").min_role(ROLE_MEMBER);
    stream.configuration = json!({
        "keys": [{ "name": "at", "type": "int", "required": true }],
        "topics": "row",
        "table": "notes",
        "client_publish": true,
    })
    .as_object()
    .cloned()
    .unwrap();
    stream
}

fn queries() -> Vec<CustomQuery> {
    vec![
        CustomQuery::new("openNote", Method::Post, "/open-note", OPEN_NOTE)
            .language(QueryLanguage::Javascript)
            .params([CustomParam::new("token", sc_api::ValueType::Text)])
            .response_type(ResponseType::Stream)
            .min_role(ROLE_PUBLIC)
            .rate_limit(QueryRateLimit {
                per_ip: Some("6/min".to_owned()),
                per_user: None,
            }),
        // A JSON query: opening it is the same refusal as a misspelt one.
        CustomQuery::new("hello", Method::Get, "/hello", "return 1;")
            .language(QueryLanguage::Javascript)
            .min_role(ROLE_PUBLIC),
    ]
}

fn app() -> Application {
    let mut api = ApiConfig::new("rest", "/api");
    sc_api::set_custom_queries(&mut api.config, &queries()).unwrap();
    Application::new("Notes", "notes", FrameworkRef::new("code"))
        .with_api(api)
        .with_table(TableId("notes".to_owned()))
        .with_table(TableId("share_links".to_owned()))
        .with_stream(StreamRef::new("note_body"))
        .with_stream(StreamRef::new("note_cursor"))
}

/// Persisting quickly; the re-check clock turned up so anything revoked was
/// revoked by a write.
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
        std::env::temp_dir().join(format!("sc-live-query-{}", Uuid::new_v4())),
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
            ],
        )
        .await?;
    catalog
        .create_table(
            "share_links",
            &[
                id(),
                plain("note", BasicType::Int),
                plain("token", BasicType::Text),
                plain("mode", BasicType::Text),
            ],
        )
        .await?;
    catalog.reload().await?;
    save_field_meta(
        &catalog,
        &FieldMeta::new("notes", "body").rich_type(sc_types::COLLAB_DOC),
    )
    .await?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, ADMIN, PASSWORD, ROLE_ADMIN).await?;
    let alice = create_user(&catalog, ALICE, PASSWORD, ROLE_MEMBER).await?;

    let evaluator = sc_server::default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let triggers =
        sc_server::install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;

    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(Document) as Arc<dyn StreamProvider>)?;
    registry.register(Arc::new(Internal) as Arc<dyn StreamProvider>)?;
    let registry = Arc::new(registry);
    let stream_services = sc_server::install_streams_with(
        &catalog,
        &triggers,
        registry.clone(),
        StreamConfig::default(),
    )
    .await?;
    for stream in [document("note_body"), document("hidden_doc"), cursor()] {
        save_stream(&catalog, &registry, &stream).await?;
    }
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
    // With the peer's address, so a per-IP rate limit has something to count.
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            served.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    let server = Server {
        router,
        catalog: catalog.clone(),
        addr,
        _dir: dir,
    };
    let owner = alice.id.hyphenated().to_string();
    let notes = catalog.require("notes")?;
    sc_api::rows::create_row(
        &catalog,
        &notes,
        &json!({ "id": 1, "title": "Plans", "owner": owner }),
    )
    .await?;
    let links = catalog.require("share_links")?;
    for (id, token, mode) in [
        (1, "edit-token", "edit"),
        (2, "read-token", "read"),
        (3, "stale-token", "expired"),
    ] {
        sc_api::rows::create_row(
            &catalog,
            &links,
            &json!({ "id": id, "note": 1, "token": token, "mode": mode }),
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
                "ownership_formula": "owner === user.id", "rls_enabled": false, "sync_enabled": false,
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

/// Send `frame` and wait for its `ready` or `error`.
async fn answer(socket: &mut Socket, frame: Value) -> Value {
    let sub = frame["sub"].clone();
    send(socket, frame).await;
    loop {
        let frame = next_frame(socket).await;
        if frame["sub"] == sub && (frame["type"] == "ready" || frame["type"] == "error") {
            return frame;
        }
    }
}

/// The next frame of `kind` about `sub`, skipping others.
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

fn open(sub: &str, params: Value) -> Value {
    json!({ "type": "open", "sub": sub, "query": "openNote", "params": params })
}

/// A browser's copy of the note: the `doc_sync` applied to a fresh document.
fn browser(sync: &Value) -> CollabDoc {
    let bytes = BASE64.decode(sync["update"].as_str().unwrap()).unwrap();
    CollabDoc::load(DocShape::new("default", RootKind::Xml), Some(&bytes)).unwrap()
}

/// `doc` with a paragraph appended, as the update a page would send.
fn typed(doc: &CollabDoc, text: &str) -> String {
    BASE64.encode(doc.append_paragraph(text).unwrap())
}

// --- the tests ----------------------------------------------------------------

#[tokio::test]
async fn a_share_link_opens_a_document_for_an_anonymous_page() {
    let server = setup().await.unwrap();
    let mut alice = server.page(Some(ALICE)).await;
    let ready = answer(
        &mut alice,
        json!({ "type": "subscribe", "sub": "own", "stream": "note_body", "topic": 1 }),
    )
    .await;
    assert_eq!(
        ready["type"], "ready",
        "the owner opens it directly: {ready}"
    );
    assert!(
        ready.get("opened").is_none(),
        "a subscribe is not told `opened`"
    );
    let alice_sync = frame_for(&mut alice, "own", "doc_sync").await;
    assert_eq!(alice_sync["read_only"], false);

    // The stream is below the anonymous page's floor; the query lets it in.
    let mut editor = server.page(None).await;
    let refused = answer(
        &mut editor,
        json!({ "type": "subscribe", "sub": "direct", "stream": "note_body", "topic": 1 }),
    )
    .await;
    assert_eq!(refused["code"], "unavailable", "{refused}");
    let ready = answer(&mut editor, open("e", json!({ "token": "edit-token" }))).await;
    assert_eq!(ready["type"], "ready", "{ready}");
    assert_eq!(ready["stream"], "note_body");
    assert_eq!(
        ready["opened"],
        json!({ "stream": "note_body", "kind": "document", "key": "1",
                "root": "default", "root_kind": "xml" })
    );
    let sync = frame_for(&mut editor, "e", "doc_sync").await;
    assert_eq!(sync["read_only"], false, "an edit token may edit");

    // Its edit reaches the owner.
    let doc = browser(&sync);
    let update = typed(&doc, "From the link");
    send(
        &mut editor,
        json!({ "type": "doc_update", "sub": "e", "update": update }),
    )
    .await;
    let heard = frame_for(&mut alice, "own", "doc_update").await;
    let mine = browser(&alice_sync);
    mine.apply(&BASE64.decode(heard["update"].as_str().unwrap()).unwrap())
        .unwrap();
    assert!(mine.text().contains("From the link"));

    // A read token reads, and its edit is refused and heard by nobody.
    let mut reader = server.page(None).await;
    answer(&mut reader, open("r", json!({ "token": "read-token" }))).await;
    let sync = frame_for(&mut reader, "r", "doc_sync").await;
    assert_eq!(sync["read_only"], true);
    let doc = browser(&sync);
    assert!(doc.text().contains("From the link"));
    send(
        &mut reader,
        json!({ "type": "doc_update", "sub": "r", "update": typed(&doc, "nope") }),
    )
    .await;
    let refusal = frame_for(&mut reader, "r", "error").await;
    assert_eq!(refusal["code"], "unavailable", "{refusal}");
    assert!(
        drain(&mut alice)
            .await
            .iter()
            .all(|f| f["type"] != "doc_update"),
        "a reader's edit reaches nobody"
    );
}

#[tokio::test]
async fn every_refusal_is_the_same_unavailable() {
    let server = setup().await.unwrap();
    let mut page = server.page(None).await;
    let cases = [
        open("wrong", json!({ "token": "no-such-token" })),
        open("stale", json!({ "token": "stale-token" })),
        // A document stream the application does not expose.
        open("hidden", json!({ "token": "edit-token", "hidden": true })),
        json!({ "type": "open", "sub": "json", "query": "hello" }),
        json!({ "type": "open", "sub": "absent", "query": "noSuchQuery" }),
    ];
    for frame in cases {
        let sub = frame["sub"].clone();
        let answer = answer(&mut page, frame).await;
        assert_eq!(answer["type"], "error", "{sub}: {answer}");
        assert_eq!(answer["code"], "unavailable", "{sub}: {answer}");
    }
    // A required parameter left out is the caller's mistake, said as one.
    let invalid = answer(&mut page, open("typed", json!({}))).await;
    assert_eq!(invalid["code"], "invalid", "{invalid}");
}

#[tokio::test]
async fn deleting_the_share_revokes_and_downgrading_it_makes_the_page_a_reader() {
    let server = setup().await.unwrap();
    let mut editor = server.page(None).await;
    answer(&mut editor, open("e", json!({ "token": "edit-token" }))).await;
    assert_eq!(
        frame_for(&mut editor, "e", "doc_sync").await["read_only"],
        false
    );

    // The query read `share_links`, so a write to it runs the query again —
    // at once, with the clock set to an hour.
    server.share_mode("edit-token", Some("read")).await;
    let again = frame_for(&mut editor, "e", "doc_sync").await;
    assert_eq!(again["read_only"], true, "downgraded to a reader");

    server.share_mode("edit-token", None).await;
    let revoked = frame_for(&mut editor, "e", "revoked").await;
    assert_eq!(revoked["sub"], "e");
}

#[tokio::test]
async fn a_row_stream_opened_by_a_query_publishes_with_an_edit_token_only() {
    let server = setup().await.unwrap();
    let mut editor = server.page(None).await;
    let ready = answer(
        &mut editor,
        open("c", json!({ "token": "edit-token", "cursor": true })),
    )
    .await;
    assert_eq!(ready["opened"]["kind"], "row", "{ready}");
    assert!(ready["opened"].get("root").is_none());
    assert_eq!(ready["can_publish"], true);

    let mut reader = server.page(None).await;
    let ready = answer(
        &mut reader,
        open(
            "c",
            json!({ "token": "read-token", "cursor": true, "echo": true }),
        ),
    )
    .await;
    assert_eq!(ready["can_publish"], false);

    send(
        &mut editor,
        json!({ "type": "publish", "sub": "c", "value": { "at": 3 } }),
    )
    .await;
    let element = frame_for(&mut reader, "c", "element").await;
    assert_eq!(element["envelope"]["value"], json!({ "at": 3 }));
    assert_eq!(element["envelope"]["topic"], "1");
}

#[tokio::test]
async fn the_query_is_rate_limited_per_address_and_is_not_an_http_endpoint() {
    let server = setup().await.unwrap();
    // Over HTTP, the method is a 400 naming the socket.
    let mut http = server.client(APP_HOST);
    let (status, body) = http
        .send(
            "POST",
            "/api/open-note",
            Some(json!({ "token": "edit-token" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("live"), "{body}");

    // Six opens a minute from one address; a re-check is not one of them.
    let mut page = server.page(None).await;
    for n in 0..6 {
        let ready = answer(
            &mut page,
            open(&format!("s{n}"), json!({ "token": "read-token" })),
        )
        .await;
        assert_eq!(ready["type"], "ready", "{n}: {ready}");
    }
    let limited = answer(&mut page, open("s6", json!({ "token": "read-token" }))).await;
    assert_eq!(limited["code"], "rate_limited", "{limited}");
    // A write to `share_links` runs all six again; none is refused for it.
    server
        .share_mode("stale-token", Some("expired-again"))
        .await;
    let frames = drain(&mut page).await;
    assert!(
        frames.iter().all(|f| f["type"] != "revoked"),
        "re-checks do not count against the limit: {frames:?}"
    );
}
