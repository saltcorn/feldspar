//! A guest's **live socket** carries their guest session (TODO-payments P2.8,
//! design C9 and §6.1 step 2): a basket kept as rows owned by `session.id` is
//! live for the guest exactly as it is over REST.
//!
//! - two guests each watch the `baskets` change stream; each sees their own
//!   rows arrive and nothing of the other's;
//! - a guest session that has ended (its guest signed in) closes the socket at
//!   the next re-check, as a signed-in session's end does.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use futures::{SinkExt, StreamExt};
use sc_api::schema_edit::{self, ApplyOptions, FieldSpec, Operation, TableSettings};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef, StreamRef};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_live::LiveLimits;
use sc_stream::{Stream, StreamConfig, StreamProvider, StreamRegistry, TableChanges, save_stream};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const HOST: &str = "shop.example.com";

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A browser on the shop.
struct Browser {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Browser {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, HOST);
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
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Some((name, value)) = raw
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .and_then(|p| p.split_once('='))
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// A visitor's first page, and `api.startGuestSession()`.
    async fn guest(router: &Router) -> Browser {
        let mut browser = Browser {
            router: router.clone(),
            cookies: HashMap::new(),
        };
        browser.send("GET", "/", None).await;
        let (status, body) = browser.send("POST", "/api/guest-session", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        browser
    }

    /// The page's live socket, with the cookies a browser sends with it.
    async fn live(&self, addr: std::net::SocketAddr) -> Socket {
        let mut request = format!("ws://{addr}/api/live")
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert(header::HOST, HeaderValue::from_static(HOST));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{HOST}")).unwrap(),
        );
        let jar = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        headers.insert(header::COOKIE, HeaderValue::from_str(&jar).unwrap());
        tokio_tungstenite::connect_async(request)
            .await
            .expect("the live socket opens")
            .0
    }
}

async fn next_frame(socket: &mut Socket) -> Option<Value> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(20), socket.next())
            .await
            .expect("a frame within the timeout")?;
        match message.expect("readable") {
            Message::Text(text) => return Some(serde_json::from_str(&text).unwrap()),
            Message::Close(_) => return None,
            _ => continue,
        }
    }
}

/// Subscribe to the basket changes and wait for `ready`.
async fn watch(socket: &mut Socket) {
    socket
        .send(Message::text(
            json!({ "type": "subscribe", "sub": "basket", "stream": "basket_changes" }).to_string(),
        ))
        .await
        .unwrap();
    loop {
        let frame = next_frame(socket).await.expect("open");
        assert_ne!(frame["type"], "error", "{frame}");
        if frame["type"] == "ready" {
            return;
        }
    }
}

/// The next change's row's item, skipping status frames.
async fn next_item(socket: &mut Socket) -> String {
    loop {
        let frame = next_frame(socket).await.expect("open");
        assert_ne!(frame["type"], "error", "{frame}");
        if frame["type"] == "element" {
            return frame["envelope"]["value"]["row"]["item"]
                .as_str()
                .unwrap_or_else(|| panic!("{frame}"))
                .to_owned();
        }
    }
}

async fn setup() -> Result<(Router, std::net::SocketAddr, TestDb)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    let field = |name: &str, type_name: &str| FieldSpec {
        name: name.into(),
        type_name: type_name.into(),
        ..FieldSpec::default()
    };
    schema_edit::apply(
        &catalog,
        &[Operation::CreateTable {
            name: "baskets".into(),
            database: String::new(),
            settings: TableSettings {
                min_role_read: Some(1),
                min_role_write: Some(1),
                ownership_formula: Some("session_id === session.id".into()),
                ..TableSettings::default()
            },
            fields: vec![
                FieldSpec {
                    primary_key: true,
                    ..field("id", "int")
                },
                field("item", "text"),
                // Nullable, with a default (P2.8): a signed-in buyer has no session.
                FieldSpec {
                    default_expression: Some("session && session.id".into()),
                    ..field("session_id", "text")
                },
            ],
        }],
        &ApplyOptions::default(),
    )
    .await?;

    let evaluator = sc_server::default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let triggers =
        sc_server::install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(TableChanges) as Arc<dyn StreamProvider>)?;
    let registry = Arc::new(registry);
    let streams = sc_server::install_streams_with(
        &catalog,
        &triggers,
        registry.clone(),
        StreamConfig::default(),
    )
    .await?;
    let mut stream = Stream::new("basket_changes", "table_changes").min_role(100);
    stream.configuration = json!({ "table": "baskets" }).as_object().cloned().unwrap();
    save_stream(&catalog, &registry, &stream).await?;
    streams.reload(&catalog).await?;

    let apps = Arc::new(
        sc_server::AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_evaluator(evaluator)
            .with_triggers(triggers)
            .with_models(models)
            .with_streams(streams)
            .with_live_limits(LiveLimits {
                recheck_interval: Duration::from_millis(200),
                ..LiveLimits::default()
            }),
    );
    let app = Application::new("Shop", "shop", FrameworkRef::new("code"))
        .with_api(ApiConfig::new("rest", "/api"))
        .with_table(TableId("baskets".to_owned()))
        .with_stream(StreamRef::new("basket_changes"))
        .attribute(sc_app::GUEST_SESSIONS_ATTRIBUTE, true);
    apps.mount(sc_server::MountedApp::new_with(
        app,
        Arc::new(CodeFramework::new(
            "code",
            AssetBundle::new().with("index.html", "<!doctype html>"),
        )),
        &catalog,
        apps.evaluator(),
        apps.triggers(),
    )?)?;
    let config = sc_server::ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..sc_server::ServerConfig::default()
    };
    let router = sc_server::build_router_with_apps(
        &sc_api::admin_endpoints(),
        sc_server::admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    Ok((router, addr, db))
}

#[tokio::test]
async fn each_guest_sees_their_own_basket_change_live() -> Result<()> {
    let (router, addr, db) = setup().await?;
    let mut ann = Browser::guest(&router).await;
    let mut bob = Browser::guest(&router).await;
    let mut ann_live = ann.live(addr).await;
    let mut bob_live = bob.live(addr).await;
    watch(&mut ann_live).await;
    watch(&mut bob_live).await;

    // Bob adds first, then Ann: each socket's next change is its own guest's.
    for (browser, item) in [(&mut bob, "kettle"), (&mut ann, "teapot")] {
        let (status, body) = browser
            .send("POST", "/api/baskets", Some(json!({ "item": item })))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    assert_eq!(next_item(&mut ann_live).await, "teapot");
    assert_eq!(next_item(&mut bob_live).await, "kettle");

    // Ann's guest session ends (as signing in ends it): her socket is closed at
    // the next re-check, and Bob's carries on.
    db.client()
        .await?
        .batch_execute("UPDATE _fd_guest_sessions SET expires_at = now() - interval '1 minute' WHERE public_id = (SELECT session_id FROM baskets WHERE item = 'teapot')")
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let closed = loop {
        match next_frame(&mut ann_live).await {
            None => break None,
            Some(frame) if frame["type"] == "error" => break Some(frame),
            Some(_) => continue,
        }
    };
    if let Some(frame) = closed {
        assert_eq!(frame["code"], json!("signed_out"), "{frame}");
        assert!(
            frame["message"]
                .as_str()
                .unwrap()
                .contains("guest session ended"),
            "{frame}"
        );
    }
    let (status, _) = bob
        .send("POST", "/api/baskets", Some(json!({ "item": "cups" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(next_item(&mut bob_live).await, "cups");
    Ok(())
}
