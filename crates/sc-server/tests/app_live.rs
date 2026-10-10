//! Milestone L1: an application's **live socket**, `GET {mount}/live` (TODO.md
//! "Live updates"), driven through the assembled router on a real port against
//! a real database, with real WebSocket clients.
//!
//! No broker: the provider is `sc_stream::testing::ScriptedProvider` behind the
//! production seam, so everything under test — the app's mount, the Origin
//! check, the caller, the hub's fan-out, the supervisor, the broadcast channel
//! and the session store — is the code that ships.
//!
//! What is claimed, by task:
//!
//! - **L1.2** an upgrade is accepted only from the page's own origin: a sibling
//!   subdomain carrying a shared session cookie is refused with 403, as is a
//!   cookie with no `Origin` at all; the app's own origin and its preview host
//!   are accepted. The admin's sockets ask the same function.
//! - **L1.4/L1.5** one socket carries several subscriptions; the stream's
//!   `min_role`, the app's exposure and the stream's existence are one
//!   `unavailable` frame; the old per-stream route is gone.
//! - **L1.6** access is re-checked on a clock that is a parameter: a lowered
//!   role is `revoked`, a session deleted behind the store's back closes the
//!   socket with `signed_out`, and a sign-out through the app closes it at once.
//! - **L1.8** every limit is crossed once and answered with its error frame.
//! - **L1.9** the generated client reaches the same socket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use futures::{SinkExt, StreamExt};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, StreamRef};
use sc_auth::{ROLE_ADMIN, Role, SessionStore, UserUpdate, create_user, save_role};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_live::LiveLimits;
use sc_stream::testing::ScriptedProvider;
use sc_stream::{
    ElementField, ElementType, Stream, StreamConfig, StreamProvider, StreamRegistry, save_stream,
};
use sc_test_harness::TestDb;
use sc_types::BasicType;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";
const SIBLING_HOST: &str = "shop.example.com";

const ADMIN: &str = "admin@example.com";
const EDITOR: &str = "editor@example.com";
const READER: &str = "reader@example.com";
const PASSWORD: &str = "correct-horse";

/// The floor the boiler is observable at: an editor may watch it, a reader may
/// not.
const ROLE_EDITOR: u8 = 80;
const ROLE_READER: u8 = 100;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What the scripted boiler publishes: one required key and one that may be
/// absent, so the generated envelope has one of each.
fn boiler_reading() -> ElementType {
    ElementType::json([
        ElementField::new("temperature", BasicType::Float).required(),
        ElementField::new("unit", BasicType::Text),
    ])
}

/// The blog exposes three streams: `boiler` (editors and up), `pulse` (open
/// to the public role) and `dormant` (disabled). It does not expose `meter`.
fn blog_app() -> Application {
    Application::new("Blog", "blog", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_stream(StreamRef::new("boiler"))
        .with_stream(StreamRef::new("pulse"))
        .with_stream(StreamRef::new("dormant"))
}

/// A sibling application on the same base domain.
fn shop_app() -> Application {
    Application::new("Shop", "shop", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_stream(StreamRef::new("boiler"))
}

/// A cookie-carrying client addressing one host through `oneshot`.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
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

    /// Log in through the **app's own** login, as its code would.
    async fn login(&mut self, email: &str) -> String {
        self.send("GET", "/api/whoami", None).await;
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
            .expect("the app login set a session")
            .clone()
    }
}

struct Server {
    router: Router,
    catalog: Arc<Catalog>,
    apps: Arc<sc_server::AppMounts>,
    addr: std::net::SocketAddr,
    _db: TestDb,
}

/// How a socket is opened: to which host, from which page, as whom.
struct Open<'a> {
    host: &'a str,
    origin: Option<String>,
    session: Option<&'a str>,
    native: bool,
}

impl<'a> Open<'a> {
    /// A browser page on the blog, signed in as `session` (or not).
    fn blog(session: Option<&'a str>) -> Open<'a> {
        Open {
            host: APP_HOST,
            origin: Some(format!("http://{APP_HOST}")),
            session,
            native: false,
        }
    }
}

impl Server {
    fn app_client(&self) -> Client {
        Client {
            router: self.router.clone(),
            host: APP_HOST.to_owned(),
            cookies: HashMap::new(),
        }
    }

    /// Open a socket on `path`.
    async fn open(&self, path: &str, how: Open<'_>) -> std::result::Result<Socket, WsError> {
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert(header::HOST, HeaderValue::from_str(how.host).unwrap());
        if let Some(origin) = &how.origin {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        if let Some(token) = how.session {
            headers.insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("{}={token}", sc_server::SESSION_COOKIE)).unwrap(),
            );
        }
        if how.native {
            headers.insert(
                sc_api::auth::CLIENT_KIND_HEADER,
                HeaderValue::from_static(sc_api::auth::NATIVE_CLIENT),
            );
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(stream, _)| stream)
    }

    /// The blog's live socket, from a page on the blog.
    async fn live(&self, session: Option<&str>) -> Socket {
        self.open("/api/live", Open::blog(session))
            .await
            .expect("the live socket opens from the app's own page")
    }

    async fn set_role(&self, email: &str, role: u8) {
        let user = sc_auth::load_user_by_email(&self.catalog, email)
            .await
            .unwrap()
            .unwrap();
        sc_auth::update_user(
            &self.catalog,
            user.id,
            UserUpdate {
                role: Some(role),
                ..UserUpdate::default()
            },
        )
        .await
        .unwrap();
    }
}

/// The scripted boiler: it publishes on a timer fast enough for a test and slow
/// enough that a connection is not racing a thousand elements.
fn boiler_provider() -> ScriptedProvider {
    ScriptedProvider::new("scripted", boiler_reading())
        .json_elements([
            json!({ "temperature": 31.2, "unit": "C" }),
            json!({ "temperature": 31.4, "unit": "C" }),
            json!({ "temperature": 31.6 }),
        ])
        .every(Duration::from_millis(100))
        .repeating()
}

async fn setup(limits: LiveLimits) -> Result<Server> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$;",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    save_role(&catalog, &Role::new(ROLE_EDITOR, "Editor")).await?;
    create_user(&catalog, ADMIN, PASSWORD, ROLE_ADMIN).await?;
    create_user(&catalog, EDITOR, PASSWORD, ROLE_EDITOR).await?;
    create_user(&catalog, READER, PASSWORD, ROLE_READER).await?;

    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let triggers = sc_server::install_triggers(
        &catalog,
        sc_server::default_js_evaluator(),
        &agents,
        &models,
    )
    .await?;

    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(boiler_provider()) as Arc<dyn StreamProvider>)?;
    let registry = Arc::new(registry);
    let streams = sc_server::install_streams_with(
        &catalog,
        &triggers,
        registry.clone(),
        StreamConfig::default(),
    )
    .await?;

    // `meter` is identical to `boiler` but for its name, and the blog does not
    // expose it: what makes "unavailable" a claim about the app's declaration
    // rather than about the stream's existence.
    for (name, min_role) in [
        ("boiler", ROLE_EDITOR),
        ("meter", ROLE_EDITOR),
        ("pulse", ROLE_READER),
    ] {
        let stream = Stream::new(name, "scripted")
            .description("a scripted flow")
            .min_role(min_role);
        save_stream(&catalog, &registry, &stream).await?;
    }
    let mut dormant = Stream::new("dormant", "scripted").min_role(ROLE_READER);
    dormant.attributes.insert(
        sc_stream::ATTR_ENABLED.to_owned(),
        serde_json::Value::Bool(false),
    );
    save_stream(&catalog, &registry, &dormant).await?;
    streams.reload(&catalog).await?;

    let apps = Arc::new(
        sc_server::AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_triggers(triggers)
            .with_models(models)
            .with_streams(streams)
            .with_live_limits(limits),
    );
    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    for app in [blog_app(), shop_app()] {
        apps.mount(sc_server::MountedApp::new_with(
            app,
            framework.clone(),
            &catalog,
            apps.evaluator(),
            apps.triggers(),
        )?)?;
    }

    let config = sc_server::ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..sc_server::ServerConfig::default()
    };
    // The database-backed store with no cache, so a role change is seen on
    // the next re-read rather than a minute later — the clock under test is
    // the socket's, not the cache's.
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

    Ok(Server {
        router,
        catalog,
        apps,
        addr,
        _db: db,
    })
}

/// Limits with the clocks turned down to test speed.
fn quick() -> LiveLimits {
    LiveLimits {
        status_interval: Duration::from_millis(100),
        ..LiveLimits::default()
    }
}

async fn send(socket: &mut Socket, frame: Value) {
    socket
        .send(Message::text(frame.to_string()))
        .await
        .expect("the socket takes a frame");
}

/// The next JSON frame, or a panic saying none arrived.
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

/// The next frame of `kind` about `sub`, skipping elements and statuses of
/// other subscriptions in between.
async fn frame_for(socket: &mut Socket, sub: &str, kind: &str) -> Value {
    loop {
        let frame = next_frame(socket).await;
        if frame["type"] == json!(kind) && frame["sub"] == json!(sub) {
            return frame;
        }
        assert!(
            !(frame["sub"] == json!(sub) && frame["type"] == json!("error")),
            "expected `{kind}` for {sub}, got an error: {frame}"
        );
    }
}

/// The next `error` frame, wherever it is.
async fn next_error(socket: &mut Socket) -> Value {
    loop {
        let frame = next_frame(socket).await;
        if frame["type"] == json!("error") {
            return frame;
        }
    }
}

/// Subscribe and wait for the answer: `ready` or `error`.
async fn subscribe(socket: &mut Socket, sub: &str, stream: &str) -> Value {
    send(
        socket,
        json!({ "type": "subscribe", "sub": sub, "stream": stream }),
    )
    .await;
    loop {
        let frame = next_frame(socket).await;
        if frame["sub"] == json!(sub) && (frame["type"] == "ready" || frame["type"] == "error") {
            return frame;
        }
    }
}

/// Whether the socket still answers a `ping`.
async fn still_open(socket: &mut Socket) -> bool {
    send(socket, json!({ "type": "ping" })).await;
    loop {
        let frame = next_frame(socket).await;
        if frame["type"] == "pong" {
            return true;
        }
    }
}

/// Read until the server closes the socket, returning every text frame seen.
async fn until_closed(socket: &mut Socket) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await
            .expect("the server closes the socket within the timeout")
        {
            Some(Ok(Message::Text(text))) => frames.push(serde_json::from_str(&text).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return frames,
            Some(Ok(_)) => continue,
        }
    }
}

fn http_status(error: &WsError) -> Option<StatusCode> {
    match error {
        WsError::Http(response) => Some(response.status()),
        _ => None,
    }
}

// --- L1.4 / L1.5: one socket, several subscriptions, one refusal -----------

#[tokio::test]
async fn one_socket_carries_two_subscriptions_and_both_receive() -> Result<()> {
    let server = setup(quick()).await?;
    let session = server.app_client().login(EDITOR).await;
    let mut socket = server.live(Some(&session)).await;

    let boiler = subscribe(&mut socket, "s1", "boiler").await;
    assert_eq!(boiler["type"], json!("ready"), "{boiler}");
    assert_eq!(boiler["stream"], json!("boiler"));
    assert_eq!(boiler["element_type"]["kind"], json!("json"));
    assert_eq!(boiler["can_publish"], json!(false));
    let pulse = subscribe(&mut socket, "s2", "pulse").await;
    assert_eq!(pulse["type"], json!("ready"), "{pulse}");

    // Both subscriptions receive, each labelled with its own `sub`, in the
    // §14.3 envelope.
    let mut seen = HashMap::new();
    while seen.len() < 2 {
        let frame = next_frame(&mut socket).await;
        if frame["type"] == "element" {
            let envelope = &frame["envelope"];
            assert!(envelope["value"]["temperature"].is_number(), "{frame}");
            assert!(envelope["received_at"].is_string(), "{frame}");
            assert!(
                envelope.get("topic").is_none(),
                "a stream without topics has no topic key: {frame}"
            );
            seen.insert(
                frame["sub"].as_str().unwrap().to_owned(),
                envelope["stream"].clone(),
            );
        }
    }
    assert_eq!(seen["s1"], json!("boiler"));
    assert_eq!(seen["s2"], json!("pulse"));
    // The admin list's number: two pages' worth of subscriptions, one each.
    let hub = server.apps.live().hub();
    let boiler_id = sc_stream::load_stream_by_name(&server.catalog, "boiler")
        .await?
        .unwrap()
        .id;
    assert_eq!(hub.subscribers(boiler_id), 1);

    // Unsubscribing one leaves the other flowing, and frees the count.
    send(&mut socket, json!({ "type": "unsubscribe", "sub": "s1" })).await;
    frame_for(&mut socket, "s2", "element").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.subscribers(boiler_id), 0);
    // And a name already in use is refused, while it is in use.
    let again = subscribe(&mut socket, "s2", "boiler").await;
    assert_eq!(again["code"], json!("invalid"), "{again}");
    Ok(())
}

#[tokio::test]
async fn below_the_floor_unexposed_and_misspelt_are_one_answer() -> Result<()> {
    let server = setup(quick()).await?;
    let session = server.app_client().login(READER).await;
    let mut socket = server.live(Some(&session)).await;

    // `boiler` runs and is exposed, but needs an editor; `meter` runs and the
    // reader's role would not matter, but the app did not expose it; `bioler`
    // is nothing at all.
    let forbidden = subscribe(&mut socket, "a", "boiler").await;
    let unexposed = subscribe(&mut socket, "b", "meter").await;
    let misspelt = subscribe(&mut socket, "c", "bioler").await;
    for (frame, name) in [
        (&forbidden, "boiler"),
        (&unexposed, "meter"),
        (&misspelt, "bioler"),
    ] {
        assert_eq!(frame["type"], json!("error"), "{frame}");
        assert_eq!(frame["code"], json!("unavailable"), "{frame}");
        // The same sentence, but for the name the caller already said.
        let message = frame["message"].as_str().unwrap().replace(name, "NAME");
        let reference = misspelt["message"]
            .as_str()
            .unwrap()
            .replace("bioler", "NAME");
        assert_eq!(message, reference);
    }

    // Anonymous holds the public role: refused the editors' stream, allowed
    // the public one — on the same socket.
    let mut anonymous = server.live(None).await;
    assert_eq!(
        subscribe(&mut anonymous, "x", "boiler").await["code"],
        json!("unavailable")
    );
    assert_eq!(
        subscribe(&mut anonymous, "y", "pulse").await["type"],
        json!("ready")
    );

    // An allowed stream given a topic it does not have is told so, now that
    // it may know the stream is there.
    send(
        &mut anonymous,
        json!({ "type": "subscribe", "sub": "z", "stream": "pulse", "topic": "7" }),
    )
    .await;
    let topical = frame_for(&mut anonymous, "z", "error").await;
    assert_eq!(topical["code"], json!("invalid"), "{topical}");
    Ok(())
}

#[tokio::test]
async fn a_frame_the_server_does_not_know_is_an_error_not_a_hang_up() -> Result<()> {
    let server = setup(quick()).await?;
    let mut socket = server.live(None).await;
    send(&mut socket, json!({ "type": "teleport", "sub": "q" })).await;
    let error = next_error(&mut socket).await;
    assert_eq!(error["code"], json!("invalid"), "{error}");
    assert_eq!(error["sub"], json!("q"));
    socket.send(Message::text("not json at all")).await.unwrap();
    assert_eq!(next_error(&mut socket).await["code"], json!("invalid"));
    send(
        &mut socket,
        json!({ "type": "unsubscribe", "sub": "ghost" }),
    )
    .await;
    assert_eq!(next_error(&mut socket).await["code"], json!("invalid"));
    // A page may not publish on an MQTT-like stream, and is told as it would
    // be about anything else that is not there.
    subscribe(&mut socket, "p", "pulse").await;
    send(
        &mut socket,
        json!({ "type": "publish", "sub": "p", "value": { "temperature": 1 } }),
    )
    .await;
    assert_eq!(
        frame_for(&mut socket, "p", "error").await["code"],
        json!("unavailable")
    );
    assert!(still_open(&mut socket).await);
    Ok(())
}

#[tokio::test]
async fn a_disabled_stream_is_waited_for_and_followed_when_it_starts() -> Result<()> {
    let server = setup(quick()).await?;
    let mut socket = server.live(None).await;
    let ready = subscribe(&mut socket, "d", "dormant").await;
    assert_eq!(ready["type"], json!("ready"), "{ready}");
    assert_eq!(ready["status"]["status"], json!("stopped"));
    assert_eq!(ready["element_type"], Value::Null);

    // Switch it on: the subscription follows by itself, with a second `ready`
    // that now knows what the elements are, and then the elements.
    let mut dormant = sc_stream::load_stream_by_name(&server.catalog, "dormant")
        .await?
        .unwrap();
    dormant
        .attributes
        .insert(sc_stream::ATTR_ENABLED.to_owned(), Value::Bool(true));
    let registry = server.apps.streams().unwrap().registry();
    save_stream(&server.catalog, &registry, &dormant).await?;
    server
        .apps
        .streams()
        .unwrap()
        .reload(&server.catalog)
        .await?;

    let followed = frame_for(&mut socket, "d", "ready").await;
    assert_eq!(
        followed["element_type"]["kind"],
        json!("json"),
        "{followed}"
    );
    frame_for(&mut socket, "d", "element").await;
    Ok(())
}

#[tokio::test]
async fn the_per_stream_app_route_is_gone() -> Result<()> {
    let server = setup(quick()).await?;
    let session = server.app_client().login(EDITOR).await;
    let error = server
        .open("/api/streams/boiler/observe", Open::blog(Some(&session)))
        .await
        .expect_err("the per-stream app socket was removed");
    assert_eq!(
        http_status(&error),
        Some(StatusCode::NOT_FOUND),
        "{error:?}"
    );

    // And the live path says what it is to an ordinary GET.
    let mut client = server.app_client();
    let (status, body) = client.send("GET", "/api/live", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("WebSocket"),
        "{body}"
    );
    Ok(())
}

// --- L1.2: the Origin check ------------------------------------------------

#[tokio::test]
async fn an_upgrade_is_accepted_only_from_the_pages_own_origin() -> Result<()> {
    let server = setup(quick()).await?;
    // Shared sign-in: the session cookie is scoped to the base domain, so a
    // browser on the shop sends the blog's session along. That is the case
    // the check exists for.
    server.apps.set_shared_session_cookie(true);
    let session = server.app_client().login(EDITOR).await;

    // A page on the sibling subdomain opening the blog's socket.
    let sibling = server
        .open(
            "/api/live",
            Open {
                host: APP_HOST,
                origin: Some(format!("https://{SIBLING_HOST}")),
                session: Some(&session),
                native: false,
            },
        )
        .await
        .expect_err("a sibling subdomain's page is refused");
    assert_eq!(http_status(&sibling), Some(StatusCode::FORBIDDEN));

    // A cookie with no Origin at all: not a browser, and not saying it is a
    // native app either.
    let headless = server
        .open(
            "/api/live",
            Open {
                host: APP_HOST,
                origin: None,
                session: Some(&session),
                native: false,
            },
        )
        .await
        .expect_err("a cookie with no origin is refused");
    assert_eq!(http_status(&headless), Some(StatusCode::FORBIDDEN));

    // The app's own page, and a native app that says it is one.
    let mut own = server.live(Some(&session)).await;
    assert_eq!(
        subscribe(&mut own, "s", "boiler").await["type"],
        json!("ready")
    );
    let mut native = server
        .open(
            "/api/live",
            Open {
                host: APP_HOST,
                origin: None,
                session: Some(&session),
                native: true,
            },
        )
        .await
        .expect("a native app's socket is accepted");
    assert_eq!(
        subscribe(&mut native, "s", "boiler").await["type"],
        json!("ready")
    );

    // The blog's preview host: its page connects to itself.
    let preview = sc_server::MountedApp::new_with(
        blog_app(),
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        )),
        &server.catalog,
        server.apps.evaluator(),
        server.apps.triggers(),
    )?;
    let run = sc_agent::RunId::new();
    let info = server.apps.mount_preview(run, preview);
    server.apps.allow_preview_token(run, &session);
    let mut previewed = server
        .open(
            "/api/live",
            Open {
                host: &info.host,
                origin: Some(format!("https://{}", info.host)),
                session: Some(&session),
                native: false,
            },
        )
        .await
        .expect("the preview host's own page is accepted");
    assert_eq!(
        subscribe(&mut previewed, "s", "boiler").await["type"],
        json!("ready")
    );
    // …and the preview's page may not open the live app's socket.
    let cross = server
        .open(
            "/api/live",
            Open {
                host: APP_HOST,
                origin: Some(format!("https://{}", info.host)),
                session: Some(&session),
                native: false,
            },
        )
        .await
        .expect_err("a preview's page is another origin");
    assert_eq!(http_status(&cross), Some(StatusCode::FORBIDDEN));

    // The admin's sockets ask the same function.
    let mut admin = Client {
        router: server.router.clone(),
        host: BASE_DOMAIN.to_owned(),
        cookies: HashMap::new(),
    };
    let admin_session = admin.login(ADMIN).await;
    let boiler = sc_stream::load_stream_by_name(&server.catalog, "boiler")
        .await?
        .unwrap();
    let observe = format!("/api/streams/{}/observe", boiler.id.0);
    let refused = server
        .open(
            &observe,
            Open {
                host: BASE_DOMAIN,
                origin: Some(format!("https://{APP_HOST}")),
                session: Some(&admin_session),
                native: false,
            },
        )
        .await
        .expect_err("an application's page may not open the admin's socket");
    assert_eq!(http_status(&refused), Some(StatusCode::FORBIDDEN));
    server
        .open(
            &observe,
            Open {
                host: BASE_DOMAIN,
                origin: Some(format!("https://{BASE_DOMAIN}")),
                session: Some(&admin_session),
                native: false,
            },
        )
        .await
        .expect("the admin's own page may");
    Ok(())
}

// --- L1.6: re-checking -----------------------------------------------------

#[tokio::test]
async fn a_lowered_role_is_revoked_at_the_next_recheck() -> Result<()> {
    let server = setup(LiveLimits {
        recheck_interval: Duration::from_millis(100),
        ..quick()
    })
    .await?;
    let session = server.app_client().login(EDITOR).await;
    let mut socket = server.live(Some(&session)).await;
    assert_eq!(
        subscribe(&mut socket, "s1", "boiler").await["type"],
        json!("ready")
    );
    assert_eq!(
        subscribe(&mut socket, "s2", "pulse").await["type"],
        json!("ready")
    );

    server.set_role(EDITOR, ROLE_READER).await;
    let revoked = frame_for(&mut socket, "s1", "revoked").await;
    assert_eq!(revoked, json!({ "type": "revoked", "sub": "s1" }));
    // The public stream still passes, and still flows.
    frame_for(&mut socket, "s2", "element").await;

    // An admin raising the stream's own floor is re-checked the same way.
    let mut pulse = sc_stream::load_stream_by_name(&server.catalog, "pulse")
        .await?
        .unwrap();
    pulse.min_role = Some(ROLE_EDITOR);
    let registry = server.apps.streams().unwrap().registry();
    save_stream(&server.catalog, &registry, &pulse).await?;
    frame_for(&mut socket, "s2", "revoked").await;
    Ok(())
}

#[tokio::test]
async fn a_session_gone_from_the_store_closes_the_socket() -> Result<()> {
    let server = setup(LiveLimits {
        recheck_interval: Duration::from_millis(100),
        ..quick()
    })
    .await?;
    let session = server.app_client().login(EDITOR).await;
    let mut socket = server.live(Some(&session)).await;
    subscribe(&mut socket, "s1", "boiler").await;

    // Deleted behind the store's back — what another node's sign-out looks
    // like from here — so only the re-check can notice.
    let editor = sc_auth::load_user_by_email(&server.catalog, EDITOR)
        .await?
        .unwrap();
    sc_auth::delete_sessions_for_user(&server.catalog, editor.id).await?;

    let frames = until_closed(&mut socket).await;
    let error = frames
        .iter()
        .find(|f| f["type"] == "error")
        .unwrap_or_else(|| panic!("an error frame says why: {frames:?}"));
    assert_eq!(error["code"], json!("signed_out"), "{error}");
    Ok(())
}

#[tokio::test]
async fn a_sign_out_on_this_node_closes_the_socket_at_once() -> Result<()> {
    // A re-check clock far longer than the test: only the hook can close it.
    let server = setup(LiveLimits {
        recheck_interval: Duration::from_secs(3600),
        ..quick()
    })
    .await?;
    let mut client = server.app_client();
    let session = client.login(EDITOR).await;
    let mut socket = server.live(Some(&session)).await;
    subscribe(&mut socket, "s1", "boiler").await;
    // Somebody else's socket, which must stay open.
    let mut other = server.app_client();
    let other_session = other.login(READER).await;
    let mut bystander = server.live(Some(&other_session)).await;

    let (status, body) = client.send("POST", "/api/logout", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let started = std::time::Instant::now();
    let frames = until_closed(&mut socket).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        frames
            .iter()
            .any(|f| f["type"] == "error" && f["code"] == "signed_out"),
        "{frames:?}"
    );
    assert!(still_open(&mut bystander).await);
    Ok(())
}

// --- L1.8: limits ----------------------------------------------------------

#[tokio::test]
async fn one_subscription_too_many_is_refused_and_the_socket_stays() -> Result<()> {
    let server = setup(LiveLimits {
        max_subscriptions: 2,
        ..quick()
    })
    .await?;
    let mut socket = server.live(None).await;
    assert_eq!(
        subscribe(&mut socket, "a", "pulse").await["type"],
        json!("ready")
    );
    assert_eq!(
        subscribe(&mut socket, "b", "pulse").await["type"],
        json!("ready")
    );
    let third = subscribe(&mut socket, "c", "pulse").await;
    assert_eq!(third["code"], json!("too_many_subscriptions"), "{third}");
    // The limit is checked before the stream is: a name that is nothing gets
    // the limit, not a hint about whether it exists.
    let ghost = subscribe(&mut socket, "d", "bioler").await;
    assert_eq!(ghost["code"], json!("too_many_subscriptions"), "{ghost}");
    assert!(still_open(&mut socket).await);
    Ok(())
}

#[tokio::test]
async fn one_connection_too_many_is_told_and_closed() -> Result<()> {
    let server = setup(LiveLimits {
        max_connections_per_user: 1,
        ..quick()
    })
    .await?;
    let session = server.app_client().login(EDITOR).await;
    let mut first = server.live(Some(&session)).await;
    let mut second = server.live(Some(&session)).await;
    let frames = until_closed(&mut second).await;
    assert_eq!(
        frames.first().map(|f| f["code"].clone()),
        Some(json!("too_many_connections")),
        "{frames:?}"
    );
    assert!(still_open(&mut first).await);
    // Anonymous connections are not counted against anyone.
    let mut a = server.live(None).await;
    let mut b = server.live(None).await;
    assert!(still_open(&mut a).await && still_open(&mut b).await);
    Ok(())
}

#[tokio::test]
async fn a_frame_over_the_limit_is_not_read_and_the_socket_stays() -> Result<()> {
    let server = setup(LiveLimits {
        max_frame_bytes: 256,
        ..quick()
    })
    .await?;
    let mut socket = server.live(None).await;
    let padding = "x".repeat(400);
    send(
        &mut socket,
        json!({ "type": "subscribe", "sub": "big", "stream": padding }),
    )
    .await;
    let error = next_error(&mut socket).await;
    assert_eq!(error["code"], json!("too_large"), "{error}");
    assert!(
        error.get("sub").is_none(),
        "an unread frame names no sub: {error}"
    );
    assert!(still_open(&mut socket).await);
    Ok(())
}

#[tokio::test]
async fn ping_is_answered_and_a_silent_connection_is_closed() -> Result<()> {
    let server = setup(LiveLimits {
        ping_interval: Duration::from_millis(50),
        idle_timeout: Duration::from_millis(300),
        ..quick()
    })
    .await?;
    let hub = server.apps.live().hub().clone();

    // A client that never reads never pongs the server's pings, and is closed.
    let mut silent = server.live(None).await;
    assert_eq!(hub.connection_count(), 1);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        hub.connection_count(),
        0,
        "the server closed the silent connection by itself"
    );
    // What it was told, when the client's reads still reach it: a client that
    // queued pongs may find the connection reset before it reads the frame,
    // which is the client's half and not the server's.
    let frames = until_closed(&mut silent).await;
    if let Some(error) = frames.iter().find(|f| f["type"] == "error") {
        assert_eq!(error["code"], json!("idle"), "{error}");
    }

    // A client that keeps reading answers the pings, and is kept open well
    // past the idle timeout.
    let mut lively = server.live(None).await;
    for _ in 0..5 {
        assert!(still_open(&mut lively).await);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert_eq!(hub.connection_count(), 1);
    Ok(())
}

// --- L1.9: the generated client ------------------------------------------

#[tokio::test]
async fn the_generated_client_subscribes_over_the_live_socket() -> Result<()> {
    let server = setup(quick()).await?;
    let app = blog_app();
    // Resolved against the stored rows and the installed provider registry,
    // which is what an app's build does.
    let streams = sc_app::app_streams(&app, &server.catalog).await?;
    assert_eq!(streams.len(), 3);
    let exports = sc_app::stream_exports(&app, &streams);
    assert!(exports.iter().all(|e| e.path == "/api/live"));
    let ts = sc_api::generate_client_with_streams(&sc_api::EndpointSet::new(), &exports);
    // One connection, on the path the server serves, and an accessor per
    // exposed stream typed from its element type (§4).
    assert!(
        ts.contains("new LiveConnection(liveUrl(baseUrl, \"/api/live\")"),
        "{ts}"
    );
    assert!(
        ts.contains("boiler: connection.stream<BoilerEnvelope>(\"boiler\")"),
        "{ts}"
    );
    assert!(ts.contains("temperature: number"), "{ts}");
    assert!(ts.contains("unit: string | null"), "{ts}");
    assert!(!ts.contains("observeStream_"), "{ts}");
    // A stream this app does not expose is not in its client at all.
    assert!(!ts.contains("meter"), "{ts}");
    Ok(())
}

/// An app that names a stream the server no longer has does not quietly
/// generate a smaller client: it says which app and which stream, exactly as an
/// app naming a missing trigger does.
#[tokio::test]
async fn a_stream_that_is_gone_is_named_rather_than_skipped() -> Result<()> {
    let server = setup(quick()).await?;
    let app = Application::new("Ghost", "ghost", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_stream(StreamRef::new("nothing_here"));
    let error = sc_app::app_streams(&app, &server.catalog)
        .await
        .expect_err("a stream that does not exist is refused");
    let message = error.to_string();
    assert!(message.contains("Ghost"), "{message}");
    assert!(message.contains("nothing_here"), "{message}");
    Ok(())
}
