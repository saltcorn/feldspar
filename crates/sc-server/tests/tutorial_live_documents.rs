//! The definition of done for part 5 of `docs/tutorial-live.md` — **a
//! collaborative notes app** (TODO.md "Live updates" L5.8) — walked through the
//! admin API, the scaffold and real sockets, on **SQLite** (a database file in a
//! temporary directory), so it runs wherever the server builds.
//!
//! Step 1 makes `notes` with a `collab_doc` field through the admin API and
//! the read rule from the tutorial; step 2 saves the stream from the
//! tutorial's JSON, refuses an application without `notes`, and checks the
//! generated `live-doc.ts`, client and row type; step 3 type-checks the
//! tutorial's TipTap page when TipTap is installed beside the admin UI (and
//! says so when it is not); step 4 has the member and the colleague edit
//! together while the reader watches read-only and the outsider is refused,
//! then takes the colleague's edit right and their share away; step 5 closes
//! everything, finds the text in `body_text` (and no `body`) over REST, reopens
//! the note, and runs the tutorial's `live.document` code. Last, **two headless
//! Chromium contexts type into one document** — the scaffold's own generated
//! runtime, bundled by `esbuild`, in a real browser — when Chromium, `esbuild`
//! and `yjs` are available (it says so and stops short when they are not).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{SinkExt, StreamExt};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, StreamRef};
use sc_auth::{ROLE_ADMIN, Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_live::{CollabDoc, DocShape, LiveLimits, RootKind};
use sc_stream::StreamConfig;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const TODO_HOST: &str = "todo.example.com";
const ADMIN: &str = "admin@example.com";
const MEMBER: &str = "member@example.com";
const COLLEAGUE: &str = "colleague@example.com";
const READER: &str = "reader@example.com";
const OUTSIDER: &str = "outsider@example.com";
const PASSWORD: &str = "correct-horse";
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

/// A scratch directory, removed when the test ends.
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
}

impl Tutorial {
    fn on(&self, host: &'static str) -> Client {
        Client {
            router: self.router.clone(),
            host,
            cookies: HashMap::new(),
        }
    }

    /// A page of the todo app, signed in as `email`: its live socket.
    async fn page(&self, email: &str) -> Socket {
        let session = self.on(TODO_HOST).login(email).await;
        let mut request = format!("ws://{}/api/live", self.addr)
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert(header::HOST, HeaderValue::from_static(TODO_HOST));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{TODO_HOST}")).unwrap(),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{}={session}", sc_server::SESSION_COOKIE)).unwrap(),
        );
        tokio_tungstenite::connect_async(request)
            .await
            .expect("the todo app's own page opens its live socket")
            .0
    }
}

/// The world part 5 assumes: the todo app's store, the Member role, the
/// member and the colleague, and this part's reader and outsider.
async fn assumed() -> Result<Tutorial> {
    let dir =
        TempDir(std::env::temp_dir().join(format!("sc-tutorial-docs-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(dir.0.join("apps")).unwrap();
    let driver = Arc::new(SqliteDriver::open(dir.0.join("tutorial.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", dir.0.join("apps"))?))?;
    save_role(&catalog, &Role::new(ROLE_MEMBER, "Member")).await?;
    create_user(&catalog, ADMIN, PASSWORD, ROLE_ADMIN).await?;
    for email in [MEMBER, COLLEAGUE, READER, OUTSIDER] {
        create_user(&catalog, email, PASSWORD, ROLE_MEMBER).await?;
    }
    let evaluator = sc_server::default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let triggers =
        sc_server::install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    // The built-in providers, as `serve` has them: `document` among them.
    let streams = sc_server::install_streams_with(
        &catalog,
        &triggers,
        Arc::new(sc_stream::builtin_registry()?),
        StreamConfig::default(),
    )
    .await?;
    let apps = Arc::new(
        sc_server::AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_triggers(triggers)
            .with_models(models)
            .with_streams(streams)
            .with_evaluator(evaluator)
            // The tutorial's two seconds, shortened: a test does not wait.
            .with_live_limits(LiveLimits {
                recheck_interval: Duration::from_secs(3600),
                doc_persist_debounce: Duration::from_millis(100),
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
    Ok(Tutorial {
        router,
        catalog,
        apps,
        addr,
        project: dir.0.join("apps/todo"),
        _dir: dir,
    })
}

/// The todo app with part 5's ticks: the `notes` table and `note_body`.
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
    .with_table(TableId("notes".to_owned()))
    .with_stream(StreamRef::new("note_body"))
}

/// The first ```{lang} block after `heading` in the tutorial.
fn doc_block(heading: &str, lang: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc = std::fs::read_to_string(root.join("docs/tutorial-live.md")).unwrap();
    let section = doc
        .split_once(heading)
        .unwrap_or_else(|| panic!("the tutorial has `{heading}`"))
        .1;
    let fence = format!("```{lang}\n");
    let after = section
        .split_once(&fence)
        .unwrap_or_else(|| panic!("`{heading}` shows a {lang} block"))
        .1;
    // Indented inside a list item, as step 1's keys are.
    let block = after.split_once("```").unwrap().0;
    block
        .lines()
        .map(|line| line.strip_prefix("  ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n")
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

/// A browser's copy of the note: what `doc_sync` handed it.
fn browser(sync: &Value) -> CollabDoc {
    let bytes = BASE64.decode(sync["update"].as_str().unwrap()).unwrap();
    CollabDoc::load(DocShape::new("default", RootKind::Xml), Some(&bytes)).unwrap()
}

fn apply(doc: &CollabDoc, frame: &Value) {
    doc.apply(&BASE64.decode(frame["update"].as_str().unwrap()).unwrap())
        .unwrap();
}

fn doc_update(sub: &str, update: &[u8]) -> Value {
    json!({ "type": "doc_update", "sub": sub, "update": BASE64.encode(update) })
}

/// Open note `note` as a page signed in as `email`: `ready`, the members,
/// then the document.
async fn open(tutorial: &Tutorial, email: &str, note: i64) -> (Socket, Value) {
    let mut socket = tutorial.page(email).await;
    let ready = subscribe(&mut socket, on_note("n", note)).await;
    assert_eq!(ready["type"], "ready", "{email}: {ready}");
    frame_for(&mut socket, "n", "presence").await;
    let sync = frame_for(&mut socket, "n", "doc_sync").await;
    (socket, sync)
}

/// The app's REST read of `notes`, as the member.
async fn rest_notes(tutorial: &Tutorial) -> Vec<Value> {
    let mut client = tutorial.on(TODO_HOST);
    client.login(MEMBER).await;
    let listed = client.ok("GET", "/api/notes", None).await;
    listed.as_array().cloned().unwrap_or_default()
}

async fn mirrored(tutorial: &Tutorial, text: &str) -> Vec<Value> {
    for _ in 0..100 {
        let rows = rest_notes(tutorial).await;
        if rows.iter().any(|r| r["body_text"] == json!(text)) {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "the note never mirrored {text:?}: {:?}",
        rest_notes(tutorial).await
    );
}

fn admin_node_modules() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/admin/node_modules")
}

#[tokio::test]
async fn part_five_a_collaborative_notes_app() -> Result<()> {
    let tutorial = assumed().await?;
    let mut admin = tutorial.on(BASE_DOMAIN);
    admin.login(ADMIN).await;
    let id_of = async |email: &str| {
        sc_auth::load_user_by_email(&tutorial.catalog, email)
            .await
            .unwrap()
            .unwrap()
            .id
            .hyphenated()
            .to_string()
    };

    // --- Step 1: a note with a document in it --------------------------------
    for (table, fields) in [
        (
            "notes",
            vec![
                json!({ "name": "id", "type": "int", "primary_key": true }),
                json!({ "name": "title", "type": "string" }),
                json!({ "name": "owner",
                        "kind": { "type": "key", "target_table": "users", "target_field": "id" } }),
                json!({ "name": "body", "type": "collab_doc" }),
                json!({ "name": "body_text", "type": "string" }),
            ],
        ),
        (
            "shares",
            vec![
                json!({ "name": "id", "type": "int", "primary_key": true }),
                json!({ "name": "note",
                        "kind": { "type": "key", "target_table": "notes", "target_field": "id" } }),
                json!({ "name": "user",
                        "kind": { "type": "key", "target_table": "users", "target_field": "id" } }),
                json!({ "name": "can_edit", "type": "bool" }),
            ],
        ),
    ] {
        // **Tables → New** makes a table with no columns, which SQLite cannot
        // hold; the key comes with it here, and every other field through the
        // admin API, as the tutorial adds them.
        tutorial
            .catalog
            .create_table(
                table,
                &[sc_catalog::DataField::plain(
                    "id",
                    sc_types::TypeRef::Basic(sc_types::BasicType::Int),
                )
                .required()
                .primary_key()
                .generated(sc_db::ColumnGenerator::Identity)],
            )
            .await?;
        tutorial.catalog.reload().await?;
        for field in fields.into_iter().skip(1) {
            let path = format!("/api/tables/{table}/fields");
            let (status, body) = admin.send("POST", &path, Some(field.clone())).await;
            assert_eq!(status, StatusCode::CREATED, "{field}: {body}");
        }
    }
    let body = tutorial.catalog.require("notes")?;
    assert!(
        body.field("body").unwrap().is_collab_doc(),
        "`body` is a document"
    );
    let rule = doc_block("### Step 1 — A note with a document in it", "");
    admin
        .ok(
            "PUT",
            "/api/tables/notes",
            Some(json!({
                "label": "", "description": "",
                "min_role_read": ROLE_ADMIN, "min_role_write": ROLE_ADMIN,
                "ownership_formula": rule.trim(), "rls_enabled": false, "sync_enabled": false,
            })),
        )
        .await;
    assert!(
        tutorial.catalog.require("notes")?.ownership.is_some(),
        "the tutorial's rule is live"
    );
    let (member, colleague, reader) = (
        id_of(MEMBER).await,
        id_of(COLLEAGUE).await,
        id_of(READER).await,
    );
    let notes = tutorial.catalog.require("notes")?;
    sc_api::rows::create_row(
        &tutorial.catalog,
        &notes,
        &json!({ "id": 1, "title": "Plans", "owner": member }),
    )
    .await?;
    // The admin's row editor cannot write the document either.
    let refused = sc_api::rows::create_row(
        &tutorial.catalog,
        &notes,
        &json!({ "id": 2, "title": "x", "body": [1] }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(refused.contains("collaborative document"), "{refused}");
    let shares = tutorial.catalog.require("shares")?;
    for (id, user, can_edit) in [(1, &colleague, true), (2, &reader, false)] {
        sc_api::rows::create_row(
            &tutorial.catalog,
            &shares,
            &json!({ "id": id, "note": 1, "user": user, "can_edit": can_edit }),
        )
        .await?;
    }

    // --- Step 2: the document stream ------------------------------------------
    let configuration: Value =
        serde_json::from_str(&doc_block("### Step 2 — The document stream", "json"))
            .expect("step 2's settings are JSON");
    admin
        .ok(
            "POST",
            "/api/streams",
            Some(json!({
                "name": "note_body", "provider": "document", "description": "",
                "min_role": ROLE_MEMBER, "configuration": configuration,
            })),
        )
        .await;
    // An application without `notes` may not expose it.
    let without = Application::new(
        "Other",
        "other",
        FrameworkRef::new("react")
            .with("store", "apps")
            .with("project", "other"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(ApiConfig::new("rest", "/api"))
    .with_stream(StreamRef::new("note_body"));
    let refused = sc_app::save_application(&tutorial.catalog, &without)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("documents kept in `notes`"), "{refused}");

    let app = todo_app();
    sc_app::save_application(&tutorial.catalog, &app).await?;
    sc_app::scaffold_app(&tutorial.catalog, &app, tutorial.apps.triggers()).await?;
    tutorial.apps.remount(sc_server::MountedApp::new_with(
        app.clone(),
        Arc::new(CodeFramework::new("react", AssetBundle::new())),
        &tutorial.catalog,
        tutorial.apps.evaluator(),
        tutorial.apps.triggers(),
    )?);
    let runtime = tutorial.project.join("src/feldspar");
    let live_doc = std::fs::read_to_string(runtime.join("live-doc.ts")).unwrap();
    assert!(
        live_doc.contains("note_body: documentStream(api.live.note_body),"),
        "{live_doc}"
    );
    let client = std::fs::read_to_string(runtime.join("client.ts")).unwrap();
    assert!(
        client.contains("connection.documentStream<\"xml\">(\"note_body\", \"default\", \"xml\")"),
        "{client}"
    );
    let row_type = client
        .split_once("export interface NotesRow {")
        .expect("a row type for notes")
        .1
        .split_once('}')
        .unwrap()
        .0;
    assert!(row_type.contains("body_text"), "{row_type}");
    assert!(
        !row_type
            .lines()
            .any(|l| l.trim_start().starts_with("body:") || l.trim_start().starts_with("body?:")),
        "the row type has no `body`: {row_type}"
    );
    for file in ["live-react.ts", "helper.ts", "client.ts"] {
        let text = std::fs::read_to_string(runtime.join(file)).unwrap();
        assert!(!text.contains("from \"yjs\""), "{file} imports no yjs");
    }
    let package = std::fs::read_to_string(tutorial.project.join("package.json")).unwrap();
    assert!(package.contains("\"yjs\": \"^13"), "{package}");

    // --- Step 3: an editor ----------------------------------------------------
    type_check_step_three(&tutorial.project);

    // --- Step 4: two people writing, one reading -----------------------------
    let (mut mia, sync) = open(&tutorial, MEMBER, 1).await;
    assert_eq!(sync["read_only"], false, "the owner edits");
    let (mut cole, sync_c) = open(&tutorial, COLLEAGUE, 1).await;
    assert_eq!(sync_c["read_only"], false, "an editing share edits");
    let (mut rita, sync_r) = open(&tutorial, READER, 1).await;
    assert_eq!(sync_r["read_only"], true, "a reading share reads");
    let mut otto = tutorial.page(OUTSIDER).await;
    let refused = subscribe(&mut otto, on_note("n", 1)).await;
    let missing = subscribe(&mut otto, on_note("m", 99)).await;
    assert_eq!(refused["code"], "unavailable", "{refused}");
    assert_eq!(
        refused["message"], missing["message"],
        "a note Otto may not read looks like one that is not there"
    );

    let (doc_m, doc_c, doc_r) = (browser(&sync), browser(&sync_c), browser(&sync_r));
    send(
        &mut mia,
        doc_update("n", &doc_m.append_paragraph("Agenda").unwrap()),
    )
    .await;
    apply(&doc_c, &frame_for(&mut cole, "n", "doc_update").await);
    apply(&doc_r, &frame_for(&mut rita, "n", "doc_update").await);
    send(
        &mut cole,
        doc_update("n", &doc_c.append_paragraph("Budget").unwrap()),
    )
    .await;
    apply(&doc_m, &frame_for(&mut mia, "n", "doc_update").await);
    apply(&doc_r, &frame_for(&mut rita, "n", "doc_update").await);
    assert_eq!(doc_m.text(), "Agenda\nBudget");
    assert_eq!(
        doc_r.text(),
        "Agenda\nBudget",
        "the reader watches it happen"
    );
    // The reader cannot type: refused, applied nowhere.
    send(
        &mut rita,
        doc_update("n", &doc_r.append_paragraph("Rita").unwrap()),
    )
    .await;
    assert_eq!(
        frame_for(&mut rita, "n", "error").await["code"],
        "unavailable"
    );
    for socket in [&mut mia, &mut cole] {
        assert!(
            drain(socket)
                .await
                .iter()
                .all(|f| f["type"] != "doc_update")
        );
    }
    // Cursors are presence: Mia's awareness reaches Cole as her member state.
    send(
        &mut mia,
        json!({ "type": "presence", "sub": "n", "state": "AQLEyAEBAA==" }),
    )
    .await;
    let diff = frame_for(&mut cole, "n", "presence_diff").await;
    assert_eq!(diff["updated"][0]["state"], "AQLEyAEBAA==", "{diff}");

    // The colleague's edit right goes, then the share.
    sc_api::rows::update_row(
        &tutorial.catalog,
        &shares,
        "1",
        &json!({ "can_edit": false }),
    )
    .await?;
    let again = frame_for(&mut cole, "n", "doc_sync").await;
    assert_eq!(again["read_only"], true, "{again}");
    sc_api::rows::delete_row(&tutorial.catalog, &shares, "1").await?;
    assert_eq!(frame_for(&mut cole, "n", "revoked").await["sub"], "n");

    // --- Step 5: saved ----------------------------------------------------------
    drop((mia, cole, rita, otto));
    let rows = mirrored(&tutorial, "Agenda\nBudget").await;
    assert!(rows.iter().all(|r| r.get("body").is_none()), "{rows:?}");
    let (mut mia, sync) = open(&tutorial, MEMBER, 1).await;
    assert_eq!(browser(&sync).text(), "Agenda\nBudget", "the text is there");

    // The tutorial's trigger code, run as a code body with the server's hub.
    let code = doc_block("### Step 5 — Saved", "js");
    let live = tutorial
        .catalog
        .live_host()
        .expect("the router installed the hub");
    let runtime = sc_expr::CodeRuntime::with_workers(1);
    runtime
        .run(sc_expr::CodeCall {
            code: code.clone(),
            bindings: [("row".to_owned(), json!({ "id": 1 }))]
                .into_iter()
                .collect(),
            live: Some(&live),
            ..sc_expr::CodeCall::default()
        })
        .await?;
    // The open page was sent the server's edit at once.
    let doc_m = browser(&sync);
    apply(&doc_m, &frame_for(&mut mia, "n", "doc_update").await);
    assert_eq!(doc_m.text(), "Agenda\nBudget\nSummary: 2 paragraphs");
    drop(mia);
    mirrored(&tutorial, "Agenda\nBudget\nSummary: 2 paragraphs").await;

    two_browsers_type_into_one_note(&tutorial).await;
    Ok(())
}

/// Step 3's page, type-checked against the generated runtime when TipTap is
/// installed beside the admin UI's compiler; said and skipped when it is not.
fn type_check_step_three(project: &Path) {
    let node_modules = admin_node_modules();
    let tsc = std::env::var("SC_TSC")
        .map(PathBuf::from)
        .unwrap_or_else(|_| node_modules.join(".bin/tsc"));
    let needs = [
        "@tiptap/react",
        "@tiptap/extension-collaboration",
        "yjs",
        "@types/react",
    ];
    if !tsc.exists() || needs.iter().any(|m| !node_modules.join(m).is_dir()) {
        eprintln!(
            "step 3 not type-checked: it needs tsc, yjs, @types/react and TipTap in \
             ui/admin/node_modules"
        );
        return;
    }
    let page = project.join("src/pages/NotePage.tsx");
    std::fs::create_dir_all(page.parent().unwrap()).unwrap();
    std::fs::write(&page, doc_block("### Step 3 — An editor", "tsx")).unwrap();
    let _ = std::os::unix::fs::symlink(&node_modules, project.join("node_modules"));
    let output = std::process::Command::new(&tsc)
        .current_dir(project)
        .args([
            "--noEmit",
            "--strict",
            "--noUnusedLocals",
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
            "src/pages/NotePage.tsx",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "step 3's page does not type-check:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// The scaffold's own runtime in a page: a textarea bound to the note's
/// paragraphs, one per line. Naive on purpose — it rewrites the paragraphs on
/// every input — which is right for one person typing at a time.
const PAGE_TS: &str = r#"
import * as Y from "yjs";
import { docs } from "./feldspar/live-doc";

const note = docs.note_body.open(1);
const area = document.querySelector("textarea") as HTMLTextAreaElement;
const lines = () =>
  note.root.toArray().map((p) => (p instanceof Y.XmlElement ? p.toArray().map(String).join("") : "")).join("\n");
const render = () => {
  if (area.value !== lines()) area.value = lines();
  area.readOnly = note.readOnly;
  document.body.dataset.status = note.status;
};
note.root.observeDeep(render);
note.onChange(render);
area.addEventListener("input", () => {
  // Chrome fires two inputs for a line break and its text; the second changes nothing.
  if (area.value === lines()) return;
  note.doc.transact(() => {
    note.root.delete(0, note.root.length);
    note.root.insert(
      0,
      area.value.split("\n").map((line) => {
        const p = new Y.XmlElement("paragraph");
        if (line) p.insert(0, [new Y.XmlText(line)]);
        return p;
      }),
    );
  });
});
(window as unknown as { lines: () => string }).lines = lines;
"#;

/// Bundle `PAGE_TS` with the scaffold's runtime, or say why not.
fn bundle(project: &Path) -> Option<Vec<u8>> {
    let node_modules = admin_node_modules();
    let esbuild = node_modules.join(".bin/esbuild");
    if !esbuild.exists()
        || !node_modules.join("yjs").is_dir()
        || !node_modules.join("react").is_dir()
    {
        eprintln!(
            "the browser check is skipped: it needs esbuild, react and yjs in ui/admin/node_modules"
        );
        return None;
    }
    let _ = std::os::unix::fs::symlink(&node_modules, project.join("node_modules"));
    std::fs::write(project.join("src/page.ts"), PAGE_TS).unwrap();
    let out = project.join("page.js");
    let output = std::process::Command::new(&esbuild)
        .current_dir(project)
        .args([
            "src/page.ts",
            "--bundle",
            "--format=esm",
            "--log-level=error",
        ])
        .arg(format!("--outfile={}", out.display()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "esbuild could not bundle the generated runtime:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(std::fs::read(out).unwrap())
}

/// Step 4 in two real browsers: the member types, the colleague sees it and
/// types too, and the member sees that — through the generated `live-doc.ts`,
/// `yjs` and the socket. Skipped, saying so, without Chromium or the bundler.
async fn two_browsers_type_into_one_note(tutorial: &Tutorial) {
    use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
    use chromiumoxide::cdp::browser_protocol::network::CookieParam;
    use chromiumoxide::cdp::browser_protocol::target::{
        CreateBrowserContextParams, CreateTargetParams,
    };
    use chromiumoxide::{Browser, BrowserConfig};

    let Ok(chromium) = sc_server::detect_browser(None) else {
        eprintln!("the browser check is skipped: no Chromium on PATH");
        return;
    };
    let Some(script) = bundle(&tutorial.project) else {
        return;
    };
    // The colleague's share was deleted in step 4; give it back, editing.
    let shares = tutorial.catalog.require("shares").unwrap();
    sc_api::rows::create_row(
        &tutorial.catalog,
        &shares,
        &json!({ "id": 3, "note": 1, "user": tutorial_user(tutorial, COLLEAGUE).await,
                 "can_edit": true }),
    )
    .await
    .unwrap();
    tutorial.apps.remount(
        sc_server::MountedApp::new_with(
            todo_app(),
            Arc::new(CodeFramework::new(
                "react",
                AssetBundle::new()
                    .with(
                        "index.html",
                        "<!doctype html><html><body><textarea rows=8 cols=60></textarea>\
                             <script type=\"module\" src=\"/page.js\"></script></body></html>",
                    )
                    .with("page.js", script),
            )),
            &tutorial.catalog,
            tutorial.apps.evaluator(),
            tutorial.apps.triggers(),
        )
        .unwrap(),
    );

    let profile = tutorial._dir.0.join("chromium");
    let config = BrowserConfig::builder()
        .chrome_executable(chromium)
        .no_sandbox()
        .user_data_dir(&profile)
        .arg(("host-resolver-rules", "MAP *.example.com 127.0.0.1"))
        .arg("no-proxy-server")
        .arg("disable-gpu")
        .launch_timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let (browser, mut handler) = Browser::launch(config).await.expect("Chromium starts");
    let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let origin = format!("http://{TODO_HOST}:{}", tutorial.addr.port());

    let mut pages = Vec::new();
    for email in [MEMBER, COLLEAGUE] {
        let session = tutorial.on(TODO_HOST).login(email).await;
        let context = browser
            .create_browser_context(CreateBrowserContextParams::default())
            .await
            .unwrap();
        let page = browser
            .new_page(
                CreateTargetParams::builder()
                    .url("about:blank")
                    .browser_context_id(context)
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();
        page.set_cookie(
            CookieParam::builder()
                .name(sc_server::SESSION_COOKIE)
                .value(session)
                .url(origin.clone())
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
        page.goto(format!("{origin}/")).await.unwrap();
        pages.push(page);
    }
    let eval = async |page: &chromiumoxide::Page, js: &str| -> Value {
        page.evaluate(js)
            .await
            .ok()
            .and_then(|r| r.into_value::<Value>().ok())
            .unwrap_or(Value::Null)
    };
    let until = async |page: &chromiumoxide::Page, js: &str, want: Value| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let got = eval(page, js).await;
            if got == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "`{js}` never became {want}, last {got}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    for page in &pages {
        until(page, "document.body.dataset.status", json!("synced")).await;
        until(
            page,
            "window.lines()",
            json!("Agenda\nBudget\nSummary: 2 paragraphs"),
        )
        .await;
    }
    let type_at_end = async |page: &chromiumoxide::Page, text: &str| {
        eval(
            page,
            "(() => { const a = document.querySelector('textarea'); a.focus(); \
               a.selectionStart = a.selectionEnd = a.value.length; return true; })()",
        )
        .await;
        page.execute(InsertTextParams::new(text)).await.unwrap();
    };
    type_at_end(&pages[0], "\nMia typed this").await;
    until(
        &pages[1],
        "window.lines().split('\\n').pop()",
        json!("Mia typed this"),
    )
    .await;
    type_at_end(&pages[1], "\nand Cole this").await;
    until(
        &pages[0],
        "window.lines().split('\\n').slice(-2).join('|')",
        json!("Mia typed this|and Cole this"),
    )
    .await;
    drop(pages);
    drop(browser);
    pump.abort();
    mirrored(
        tutorial,
        "Agenda\nBudget\nSummary: 2 paragraphs\nMia typed this\nand Cole this",
    )
    .await;
}

async fn tutorial_user(tutorial: &Tutorial, email: &str) -> String {
    sc_auth::load_user_by_email(&tutorial.catalog, email)
        .await
        .unwrap()
        .unwrap()
        .id
        .hyphenated()
        .to_string()
}
