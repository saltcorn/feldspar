//! The definition of done for live updates, part 1 (TODO.md L1.12):
//! `docs/tutorial-live.md`, walked against a running server.
//!
//! Every step of the tutorial is here, in its order, with one substitution: the
//! MQTT broker is the scripted provider registered under the name `mqtt`, so the
//! stream row is the tutorial's and what arrives is what `mosquitto_pub` would
//! have sent. Everything else is the shipping code — the admin API saving the
//! stream, the scaffold writing the React project, the router, the live socket,
//! the session store.
//!
//! Step 2's component is **the tutorial's own code block**, read out of the
//! document and type-checked against the runtime the scaffold generated, when a
//! TypeScript compiler and React's types are on this machine (`ui/admin`'s
//! `node_modules`), and skipped with a message when they are not — the
//! `typescript_typecheck` arrangement.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use futures::{SinkExt, StreamExt};
use sc_app::{ApiConfig, Application, CodeFramework, FrameworkRef, StreamRef};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, Role, SessionStore, UserUpdate, create_user, save_role};
use sc_catalog::{Catalog, FileStoreId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_live::LiveLimits;
use sc_stream::testing::ScriptedProvider;
use sc_stream::{ElementField, ElementType, StreamConfig, StreamProvider, StreamRegistry};
use sc_test_harness::TestDb;
use sc_types::BasicType;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const TODO_HOST: &str = "todo.example.com";
const ADMIN: &str = "admin@example.com";
const MEMBER: &str = "member@example.com";
const VISITOR: &str = "visitor@example.com";
const PASSWORD: &str = "correct-horse";
/// The tutorial's Member role.
const ROLE_MEMBER: u8 = 40;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A cookie-jar client addressing one host.
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
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn ok(&mut self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) = self.send(method, path, body).await;
        assert!(status.is_success(), "{method} {path}: {status} {value}");
        value
    }

    async fn login(&mut self, email: &str) -> String {
        self.send("GET", "/api/whoami", None).await;
        self.ok(
            "POST",
            "/api/login",
            Some(json!({ "email": email, "password": PASSWORD })),
        )
        .await;
        self.cookies[sc_server::SESSION_COOKIE].clone()
    }
}

/// A scratch directory for the application's file store.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Tutorial {
    router: Router,
    catalog: Arc<Catalog>,
    apps: Arc<sc_server::AppMounts>,
    addr: std::net::SocketAddr,
    project: PathBuf,
    _dir: TempDir,
    _db: TestDb,
}

impl Tutorial {
    fn on(&self, host: &'static str) -> Client {
        Client {
            router: self.router.clone(),
            host,
            cookies: HashMap::new(),
        }
    }

    /// The todo app's live socket, from a page on `origin`.
    async fn socket(
        &self,
        session: Option<&str>,
        origin: &str,
    ) -> std::result::Result<Socket, tokio_tungstenite::tungstenite::Error> {
        let mut request = format!("ws://{}/api/live", self.addr)
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert(header::HOST, HeaderValue::from_static(TODO_HOST));
        headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        if let Some(token) = session {
            headers.insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("{}={token}", sc_server::SESSION_COOKIE)).unwrap(),
            );
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(socket, _)| socket)
    }

    async fn page(&self, session: Option<&str>) -> Socket {
        self.socket(session, &format!("http://{TODO_HOST}"))
            .await
            .expect("the todo app's own page opens its live socket")
    }
}

/// What `mosquitto_pub` sends in the tutorial, on a timer.
fn boiler() -> ScriptedProvider {
    ScriptedProvider::new(
        "mqtt",
        ElementType::json([
            ElementField::new("temperature", BasicType::Float).required(),
            ElementField::new("unit", BasicType::Text),
        ]),
    )
    .json_elements([
        json!({ "temperature": 31.2, "unit": "C" }),
        json!({ "temperature": 64.5, "unit": "C" }),
    ])
    .every(Duration::from_millis(100))
    .repeating()
}

/// The world the tutorial assumes: the streams tutorial's `boiler` (admin-only,
/// as that tutorial leaves it) and the React tutorial's `todo` app, a Member
/// role, `member@example.com`, and `visitor@example.com` below Member.
async fn assumed() -> Result<Tutorial> {
    let db = TestDb::new().await?;
    let dir =
        TempDir(std::env::temp_dir().join(format!("sc-tutorial-live-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(&dir.0).unwrap();

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", &dir.0)?))?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, ADMIN, PASSWORD, ROLE_ADMIN).await?;
    create_user(&catalog, MEMBER, PASSWORD, ROLE_MEMBER).await?;
    create_user(&catalog, VISITOR, PASSWORD, ROLE_PUBLIC).await?;

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
    registry.register(Arc::new(boiler()) as Arc<dyn StreamProvider>)?;
    let streams = sc_server::install_streams_with(
        &catalog,
        &triggers,
        Arc::new(registry),
        StreamConfig::default(),
    )
    .await?;

    let apps = Arc::new(
        sc_server::AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_triggers(triggers)
            .with_models(models)
            .with_streams(streams)
            // The re-check clock is a parameter; a minute is the tutorial's,
            // and a test does not wait a minute.
            .with_live_limits(LiveLimits {
                recheck_interval: Duration::from_millis(200),
                ..LiveLimits::default()
            }),
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

    let tutorial = Tutorial {
        router,
        catalog,
        apps,
        addr,
        project: dir.0.join("todo"),
        _dir: dir,
        _db: db,
    };
    // The streams tutorial's step 3: `boiler`, minimum role blank.
    let mut admin = tutorial.on(BASE_DOMAIN);
    admin.login(ADMIN).await;
    admin
        .ok(
            "POST",
            "/api/streams",
            Some(json!({ "name": "boiler", "provider": "mqtt", "description": "the boiler" })),
        )
        .await;
    Ok(tutorial)
}

/// The todo app as the React tutorial leaves it, plus this tutorial's tick.
fn todo_app() -> Application {
    Application::new(
        "Todo",
        "todo",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "todo"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"))
    .with_stream(StreamRef::new("boiler"))
}

async fn send(socket: &mut Socket, frame: Value) {
    socket.send(Message::text(frame.to_string())).await.unwrap();
}

async fn next_frame(socket: &mut Socket) -> Option<Value> {
    loop {
        match tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await
            .expect("a frame within the timeout")
        {
            Some(Ok(Message::Text(text))) => return Some(serde_json::from_str(&text).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => continue,
        }
    }
}

/// The next frame of `kind` about `sub`.
async fn frame_for(socket: &mut Socket, sub: &str, kind: &str) -> Value {
    loop {
        let frame = next_frame(socket)
            .await
            .unwrap_or_else(|| panic!("the socket closed waiting for `{kind}` on {sub}"));
        if frame["sub"] == json!(sub) && (frame["type"] == json!(kind) || frame["type"] == "error")
        {
            assert_eq!(frame["type"], json!(kind), "{frame}");
            return frame;
        }
    }
}

/// The tutorial's first ```tsx block: step 2's component.
fn step_two_component() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc = std::fs::read_to_string(root.join("docs/tutorial-live.md")).unwrap();
    let after = doc
        .split_once("```tsx\n")
        .expect("the tutorial shows step 2's component")
        .1;
    after.split_once("```").unwrap().0.to_owned()
}

/// Type-check the tutorial's component against the generated runtime, if this
/// machine has a compiler and React's types.
fn type_check_step_two(project: &Path) {
    let admin_modules = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/admin/node_modules");
    let tsc = std::env::var("SC_TSC")
        .map(PathBuf::from)
        .unwrap_or_else(|_| admin_modules.join(".bin/tsc"));
    if !tsc.exists() || !admin_modules.join("@types/react").is_dir() {
        eprintln!(
            "skipping the type check of step 2: no tsc or React types (run `npm ci` in ui/admin)"
        );
        return;
    }
    let pages = project.join("src/pages");
    std::fs::create_dir_all(&pages).unwrap();
    std::fs::write(pages.join("BoilerReading.tsx"), step_two_component()).unwrap();
    let modules = project.join("node_modules");
    if !modules.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&admin_modules, &modules).unwrap();
    }
    let output = std::process::Command::new(&tsc)
        .current_dir(project)
        .args([
            "--noEmit",
            "--strict",
            "--skipLibCheck",
            "--jsx",
            "react-jsx",
            "--target",
            "es2020",
            "--lib",
            "es2020,dom",
            "--module",
            "esnext",
            "--moduleResolution",
            "bundler",
            "src/pages/BoilerReading.tsx",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the tutorial's step 2 does not type-check against the generated runtime:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[tokio::test]
async fn part_one_the_live_socket() -> Result<()> {
    let tutorial = assumed().await?;
    let mut admin = tutorial.on(BASE_DOMAIN);
    admin.login(ADMIN).await;

    // --- Step 1: open the stream to Members, and expose it -----------------
    let streams = admin.ok("GET", "/api/streams", None).await;
    let boiler = streams
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "boiler")
        .unwrap()
        .clone();
    assert_eq!(
        boiler["min_role"],
        Value::Null,
        "admin-only, as the streams tutorial left it"
    );
    let mut edited = boiler.clone();
    edited["min_role"] = json!(ROLE_MEMBER);
    admin.ok("POST", "/api/streams", Some(edited)).await;

    let app = todo_app();
    sc_app::save_application(&tutorial.catalog, &app).await?;
    sc_app::scaffold_app(&tutorial.catalog, &app, None).await?;
    tutorial.apps.remount(sc_server::MountedApp::new(
        app.clone(),
        Arc::new(CodeFramework::new("react", sc_app::AssetBundle::new())),
        &tutorial.catalog,
    )?);

    // --- Step 2: show the latest reading -----------------------------------
    let live_react =
        std::fs::read_to_string(tutorial.project.join("src/feldspar/live-react.ts")).unwrap();
    assert!(
        live_react.contains("export const live = api.live;"),
        "{live_react}"
    );
    let client = std::fs::read_to_string(tutorial.project.join("src/feldspar/client.ts")).unwrap();
    assert!(client.contains("temperature: number"), "{client}");
    assert!(client.contains("unit: string | null"), "{client}");
    type_check_step_two(&tutorial.project);

    let mut member = tutorial.on(TODO_HOST);
    let session = member.login(MEMBER).await;
    let mut page = tutorial.page(Some(&session)).await;
    send(
        &mut page,
        json!({ "type": "subscribe", "sub": "s1", "stream": "boiler" }),
    )
    .await;
    let ready = frame_for(&mut page, "s1", "ready").await;
    assert_eq!(ready["can_publish"], json!(false));
    assert!(ready["replayed"].is_number(), "{ready}");
    // The reading `mosquitto_pub` sends in the tutorial arrives.
    loop {
        let element = frame_for(&mut page, "s1", "element").await;
        if element["envelope"]["value"]["temperature"] == json!(64.5) {
            assert_eq!(element["envelope"]["stream"], json!("boiler"));
            break;
        }
    }

    // --- Step 3: one socket ------------------------------------------------
    // A second component on the same page subscribes over the same socket.
    send(
        &mut page,
        json!({ "type": "subscribe", "sub": "s2", "stream": "boiler" }),
    )
    .await;
    frame_for(&mut page, "s2", "ready").await;
    let hub = tutorial.apps.live().hub();
    assert_eq!(hub.connection_count(), 1, "one socket per page");
    let id = sc_stream::load_stream_by_name(&tutorial.catalog, "boiler")
        .await?
        .unwrap()
        .id;
    assert_eq!(hub.subscribers(id), 2);
    // …and the admin's Streams list shows them.
    let listed = admin.ok("GET", "/api/streams", None).await;
    assert_eq!(listed[0]["subscribers"], json!(2), "{listed}");

    // --- Step 4: who may watch, and for how long ---------------------------
    // Sign out in another tab: the same session, closed at once.
    member.ok("POST", "/api/logout", None).await;
    let mut signed_out = false;
    while let Some(frame) = next_frame(&mut page).await {
        if frame["type"] == "error" && frame["code"] == "signed_out" {
            signed_out = true;
        }
    }
    assert!(signed_out, "the socket said why it closed");

    // Sign in as the visitor: `unavailable`, the same answer as a stream that
    // does not exist.
    let visitor_session = tutorial.on(TODO_HOST).login(VISITOR).await;
    let mut visitor = tutorial.page(Some(&visitor_session)).await;
    send(
        &mut visitor,
        json!({ "type": "subscribe", "sub": "v", "stream": "boiler" }),
    )
    .await;
    send(
        &mut visitor,
        json!({ "type": "subscribe", "sub": "w", "stream": "nothing" }),
    )
    .await;
    let mut refusals = Vec::new();
    while refusals.len() < 2 {
        let frame = next_frame(&mut visitor).await.unwrap();
        if frame["type"] == "error" {
            refusals.push(frame["code"].clone());
        }
    }
    assert_eq!(refusals, vec![json!("unavailable"), json!("unavailable")]);

    // Lower a Member's role while they watch: `revoked`.
    let mut member = tutorial.on(TODO_HOST);
    let session = member.login(MEMBER).await;
    let mut page = tutorial.page(Some(&session)).await;
    send(
        &mut page,
        json!({ "type": "subscribe", "sub": "s1", "stream": "boiler" }),
    )
    .await;
    frame_for(&mut page, "s1", "ready").await;
    let member_id = sc_auth::load_user_by_email(&tutorial.catalog, MEMBER)
        .await?
        .unwrap()
        .id;
    sc_auth::update_user(
        &tutorial.catalog,
        member_id,
        UserUpdate {
            role: Some(ROLE_PUBLIC),
            ..UserUpdate::default()
        },
    )
    .await?;
    frame_for(&mut page, "s1", "revoked").await;

    // And from another site: with sign-in shared, the shop's page carries the
    // session to the todo app's socket, and is refused before it opens.
    tutorial.apps.set_shared_session_cookie(true);
    let refused = tutorial
        .socket(Some(&session), "https://shop.example.com")
        .await
        .expect_err("another application's page is refused");
    assert!(
        matches!(&refused, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == StatusCode::FORBIDDEN),
        "{refused:?}"
    );
    Ok(())
}
