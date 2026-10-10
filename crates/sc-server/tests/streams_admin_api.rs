//! Phase 6 integration test: the **stream** admin API and the Observe socket
//! (TODO "Streams", task 6.5), driven through the assembled router as the admin
//! SPA drives it.
//!
//! No broker. The provider is `sc_stream::testing::ScriptedProvider` behind the
//! production [`StreamProvider`](sc_stream::StreamProvider) seam, so everything
//! under test — the endpoints, the handlers, the supervisor, the broadcast
//! channel, the socket — is the code that ships, and only what publishes is
//! scripted.
//!
//! What each test is for:
//!
//! 1. **The whole lifecycle over HTTP.** The providers list with their settings
//!    and the element type *this configuration* yields, the stream saves, it is
//!    running by the time the response is written (the save reloads the
//!    supervisor, task 6.2), its status and counters are readable, and the
//!    delete removes it.
//! 2. **The refusals are the ones the design names.** A name that is not an
//!    identifier, a provider that does not exist, and a `min_role` off the
//!    scale are all refused with a sentence; so is a second stream taking a
//!    name that is already used.
//! 3. **A secret is never handed back.** A password is stored, comes back as
//!    the sentinel, and **survives an edit that did not retype it** (§2.3) —
//!    which is the one thing a redaction scheme can get wrong in a way nobody
//!    notices until the broker stops accepting the connection.
//! 4. **A delete is refused while a trigger listens.** The refusal names the
//!    trigger, because a stream deleted out from under one leaves it waiting
//!    for an event nothing will ever raise.
//! 5. **A trigger cannot name a stream that does not exist** (task 6.4), and
//!    the refusal lists the streams there are.
//! 6. **The socket.** Unauthenticated is refused *before* the upgrade with a
//!    status, because that is the one refusal a browser can read; an admin gets
//!    `ready` with the element type, then the ring replay, then live elements.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use futures::StreamExt;
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_stream::testing::ScriptedProvider;
use sc_stream::{ElementField, ElementType, StreamConfig, StreamProvider, StreamRegistry};
use sc_test_harness::TestDb;
use sc_types::{BasicType, FormField, SECRET_SENTINEL, TypeRef};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tower::ServiceExt;

const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

/// The element type the scripted boiler publishes.
fn boiler_reading() -> ElementType {
    ElementType::json([
        ElementField::new("temperature", BasicType::Float).required(),
        ElementField::new("unit", BasicType::Text),
    ])
}

/// The settings the scripted provider declares: one ordinary and one secret, so
/// a test can tell the two treatments apart.
fn scripted_config_spec() -> Vec<FormField> {
    vec![
        FormField::new("broker", TypeRef::Basic(BasicType::Text)),
        FormField::new("password", TypeRef::Basic(BasicType::Text)).secret(),
    ]
}

/// A cookie-jar-carrying client over the router (CSRF + session), as the other
/// admin-API tests use.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
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
        if method != "GET"
            && method != "HEAD"
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
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
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

struct Server {
    client: Client,
    catalog: Arc<Catalog>,
    /// The same router, also bound to a real port: the socket tests cannot go
    /// through `tower::ServiceExt::oneshot`, because their whole subject is
    /// what happens after the `101 Switching Protocols`.
    addr: std::net::SocketAddr,
    _db: TestDb,
}

impl Server {
    /// The session cookie the HTTP half is using, for the socket half to carry.
    fn session(&self) -> Option<&String> {
        self.client.cookies.get(sc_server::SESSION_COOKIE)
    }

    /// Open the Observe socket for `id`, optionally as a session.
    async fn observe(
        &self,
        id: &str,
        session: Option<&str>,
    ) -> std::result::Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsError,
    > {
        let mut request = format!("ws://{}/api/streams/{id}/observe", self.addr)
            .into_client_request()
            .unwrap();
        // A browser sends its page's origin on every handshake, and the server
        // accepts an upgrade only from its own (TODO.md "Live updates" §3).
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{}", self.addr)).unwrap(),
        );
        if let Some(token) = session {
            request.headers_mut().insert(
                header::COOKIE,
                HeaderValue::from_str(&format!("{}={token}", sc_server::SESSION_COOKIE)).unwrap(),
            );
        }
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(stream, _)| stream)
    }
}

/// A server whose stream registry holds `provider` and nothing else.
async fn serve_with(provider: ScriptedProvider) -> Result<Server> {
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
    registry.register(Arc::new(provider) as Arc<dyn StreamProvider>)?;
    let streams = sc_server::install_streams_with(
        &catalog,
        &triggers,
        Arc::new(registry),
        StreamConfig::default(),
    )
    .await?;

    let apps = Arc::new(
        sc_server::AppMounts::new(catalog.clone())
            .with_triggers(triggers)
            .with_models(models)
            .with_streams(streams),
    );
    let router = sc_server::build_router_with_apps(
        &sc_api::admin_endpoints(),
        sc_server::admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &sc_server::ServerConfig::default(),
        apps,
    )?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });

    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    Ok(Server {
        client,
        catalog,
        addr,
        _db: db,
    })
}

/// A quiet scripted provider: it has a script, but delivers it slowly enough
/// that a CRUD test is not racing elements.
fn quiet() -> ScriptedProvider {
    ScriptedProvider::new("scripted", boiler_reading())
        .config(scripted_config_spec())
        .json_elements([json!({ "temperature": 31.2, "unit": "C" })])
        .every(Duration::from_secs(30))
}

/// The body `saveStream` takes.
fn stream_body(name: &str) -> Value {
    json!({
        "name": name,
        "description": "the boiler's temperature",
        "provider": "scripted",
        "configuration": { "broker": "tcp://localhost:1883", "password": "hunter2" },
        "min_role": 80,
        "enabled": true,
    })
}

#[tokio::test]
async fn a_stream_is_listed_saved_observed_and_deleted() -> Result<()> {
    let mut server = serve_with(quiet()).await?;

    // The providers, with the settings they declare — the "settings as data"
    // move that lets the form render a provider it has never heard of.
    let (status, body) = server
        .client
        .send("GET", "/api/stream-providers", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let providers = body["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1, "{body}");
    assert_eq!(providers[0]["name"], json!("scripted"));
    assert_eq!(providers[0]["module"], Value::Null, "a built-in has none");
    let spec = providers[0]["config_spec"].as_array().unwrap();
    assert_eq!(spec.len(), 2, "{body}");
    assert_eq!(spec[1]["name"], json!("password"));
    assert!(
        spec[1]["secret"].as_bool().unwrap(),
        "the form has to know to render a password input: {body}"
    );
    // Without a configuration there is no element type to answer with: it is a
    // function of one (§3), and this is what the picker shows before anything
    // is filled in.
    assert_eq!(providers[0]["element_type"], Value::Null, "{body}");

    // With one, the answer is what *this* configuration would produce.
    let configuration = serde_json::to_string(&json!({ "broker": "tcp://x" })).unwrap();
    let (status, body) = server
        .client
        .send(
            "GET",
            &format!(
                "/api/stream-providers?provider=scripted&configuration={}",
                urlencode(&configuration)
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["providers"][0]["element_type"]["kind"],
        json!("json"),
        "{body}"
    );

    // Empty before anything is created.
    let (status, body) = server.client.send("GET", "/api/streams", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0, "{body}");

    // Saved — and **running** by the time the response is written, because the
    // save reloads the supervisor (task 6.2). That is the whole of "the flow
    // follows the row without a restart".
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(stream_body("boiler")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(body["enabled"], json!(true), "{body}");
    assert_eq!(body["min_role"], json!(80), "{body}");
    assert_eq!(body["error"], Value::Null, "{body}");
    // Computed on read, never stored (§5).
    assert_eq!(body["element_type"]["kind"], json!("json"), "{body}");
    assert_eq!(body["status"]["status"], json!("running"), "{body}");

    // And readable one at a time, and in the list.
    let (status, one) = server
        .client
        .send("GET", &format!("/api/streams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["name"], json!("boiler"));
    let (status, list) = server.client.send("GET", "/api/streams", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");

    // The live half, which is the one endpoint a stream needs that a model does
    // not: the status is in memory and cannot be read back from the row.
    let (status, live) = server
        .client
        .send("GET", &format!("/api/streams/{id}/status"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{live}");
    assert_eq!(live["running"], json!(true), "{live}");
    assert_eq!(live["status"]["status"], json!("running"), "{live}");
    assert_eq!(live["counters"]["elements"], json!(0), "{live}");
    assert_eq!(live["element_type"]["kind"], json!("json"), "{live}");

    // Disabling it stops it, and the row stays.
    let mut disabled = stream_body("boiler");
    disabled["id"] = json!(id);
    disabled["enabled"] = json!(false);
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(disabled))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, live) = server
        .client
        .send("GET", &format!("/api/streams/{id}/status"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{live}");
    assert_eq!(
        live["running"],
        json!(false),
        "a disabled stream is not subscribed: {live}"
    );
    // Still held, and still counted: the supervisor keeps a stopped stream in
    // its map so the list can say *why* nothing is arriving.
    assert_eq!(live["status"]["status"], json!("stopped"), "{live}");

    // Deleted.
    let (status, body) = server
        .client
        .send("DELETE", &format!("/api/streams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], json!(true));
    let (status, body) = server
        .client
        .send("GET", &format!("/api/streams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    Ok(())
}

#[tokio::test]
async fn the_refusals_name_what_is_wrong() -> Result<()> {
    let mut server = serve_with(quiet()).await?;

    // A name that is not an identifier: it becomes a socket path segment, a
    // trigger channel and part of a generated function name.
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(stream_body("house/boiler")))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains('/'),
        "the refusal names the character it refused: {body}"
    );

    // A provider nothing implements.
    let mut unknown = stream_body("boiler");
    unknown["provider"] = json!("nonesuch");
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(unknown))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("nonesuch"),
        "{body}"
    );

    // A `min_role` off the 1–100 scale.
    let mut bad_role = stream_body("boiler");
    bad_role["min_role"] = json!(400);
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(bad_role))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A setting the provider never declared: a stream carrying one would
    // connect with the setting the admin thinks they changed still at its
    // default.
    let mut stray = stream_body("boiler");
    stray["configuration"]["porrt"] = json!(1883);
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(stray))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And the name is the reference, so it is unique.
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(stream_body("boiler")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(stream_body("boiler")))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("already exists"),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn a_password_is_masked_on_the_way_out_and_survives_an_edit() -> Result<()> {
    let mut server = serve_with(quiet()).await?;

    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(stream_body("boiler")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        body["configuration"]["password"],
        json!(SECRET_SENTINEL),
        "a save must not echo the password back: {body}"
    );
    assert_eq!(
        body["configuration"]["broker"],
        json!("tcp://localhost:1883"),
        "and an ordinary setting must still be readable: {body}"
    );

    // What the form does next: post back exactly what it was handed, with one
    // other field changed. The sentinel must resolve to the stored password.
    let (_, shown) = server
        .client
        .send("GET", &format!("/api/streams/{id}"), None)
        .await;
    assert_eq!(shown["configuration"]["password"], json!(SECRET_SENTINEL));
    let edit = json!({
        "id": id,
        "name": "boiler",
        "description": "the boiler, still",
        "provider": "scripted",
        "configuration": shown["configuration"],
        "min_role": 80,
        "enabled": true,
    });
    let (status, body) = server.client.send("POST", "/api/streams", Some(edit)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Asked of the store directly, because the API deliberately cannot answer
    // it: this is the assertion that the password is still *there*.
    let stored = sc_stream::load_stream(
        &server.catalog,
        sc_stream::StreamId(id.parse::<uuid::Uuid>().unwrap()),
    )
    .await?
    .unwrap();
    assert_eq!(
        stored.configuration.get("password"),
        Some(&json!("hunter2")),
        "an edit that did not retype the password must not have stored the mask"
    );
    assert_eq!(stored.description, "the boiler, still");
    Ok(())
}

#[tokio::test]
async fn a_trigger_holds_a_stream_and_names_one_that_exists() -> Result<()> {
    let mut server = serve_with(quiet()).await?;

    // A trigger cannot name a stream that does not exist (task 6.4), and the
    // refusal says what there is.
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "store_reading",
                "description": "",
                "when": "stream",
                "channel": "boiler",
                "action": "run_js_code",
                "configuration": { "code": "return 1;" },
                "enabled": true,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let message = body["error"].as_str().unwrap();
    assert!(message.contains("no stream is named `boiler`"), "{message}");
    assert!(
        message.contains("there are no streams"),
        "the refusal distinguishes a typo from nothing created yet: {message}"
    );

    // With the stream created it saves.
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(stream_body("boiler")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();
    let (status, body) = server
        .client
        .send(
            "POST",
            "/api/triggers",
            Some(json!({
                "name": "store_reading",
                "description": "",
                "when": "stream",
                "channel": "boiler",
                "action": "run_js_code",
                "configuration": { "code": "return 1;" },
                "enabled": true,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // And now the stream cannot be deleted out from under it.
    let (status, body) = server
        .client
        .send("DELETE", &format!("/api/streams/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("store_reading"),
        "the refusal names the trigger holding it: {body}"
    );
    Ok(())
}

#[tokio::test]
async fn the_observe_socket_is_admin_only_and_replays_before_it_tails() -> Result<()> {
    // Elements every 60ms, repeating, so there is both a ring to replay and a
    // tail to follow by the time a socket connects.
    let provider = ScriptedProvider::new("scripted", boiler_reading())
        .config(scripted_config_spec())
        .json_elements([
            json!({ "temperature": 31.2, "unit": "C" }),
            json!({ "temperature": 31.4, "unit": "C" }),
        ])
        .every(Duration::from_millis(60))
        .repeating();
    let mut server = serve_with(provider).await?;

    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(stream_body("boiler")))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();

    // Let a few elements land, so the ring has something in it.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Unauthenticated: refused **before** the upgrade, with a status. That is
    // the one refusal a browser can read, which is why it is the one that is
    // not a close frame.
    match server.observe(&id, None).await {
        Err(WsError::Http(response)) => assert!(
            response.status() == StatusCode::UNAUTHORIZED
                || response.status() == StatusCode::FORBIDDEN,
            "got {}",
            response.status()
        ),
        Ok(_) => panic!("an anonymous socket must not be upgraded"),
        Err(e) => panic!("expected an HTTP refusal, got {e}"),
    }

    let session = server.session().expect("the admin has a session").clone();
    let mut socket = server.observe(&id, Some(&session)).await.unwrap();

    // `ready` first, with what this stream is.
    let ready = next_frame(&mut socket).await;
    assert_eq!(ready["type"], json!("ready"), "{ready}");
    assert_eq!(ready["stream"], json!("boiler"), "{ready}");
    assert_eq!(ready["element_type"]["kind"], json!("json"), "{ready}");
    assert_eq!(ready["status"]["status"], json!("running"), "{ready}");
    let replayed = ready["replayed"].as_u64().unwrap();
    assert!(
        replayed > 0,
        "a screen opened on a running stream is not blank: {ready}"
    );

    // Then the replay, then the tail — and a consumer cannot tell them apart
    // except by the count, which is exactly what `replayed` is for.
    for _ in 0..replayed {
        let frame = next_frame(&mut socket).await;
        assert_eq!(frame["type"], json!("element"), "{frame}");
        assert_eq!(frame["envelope"]["stream"], json!("boiler"), "{frame}");
        assert!(
            frame["envelope"]["value"]["temperature"].is_number(),
            "the envelope is the §4 wire contract: {frame}"
        );
        assert!(frame["envelope"]["received_at"].is_string(), "{frame}");
    }

    // Live: an element that had not been published when the socket opened.
    let live = next_frame(&mut socket).await;
    assert_eq!(live["type"], json!("element"), "{live}");
    assert!(
        live["envelope"]["value"]["temperature"].is_number(),
        "{live}"
    );

    // And the counters the list shows have been moving all along.
    let (status, counters) = server
        .client
        .send("GET", &format!("/api/streams/{id}/status"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{counters}");
    assert!(
        counters["counters"]["elements"].as_u64().unwrap() > 0,
        "{counters}"
    );
    assert!(
        counters["listeners"].as_u64().unwrap() >= 1,
        "the open socket is a listener: {counters}"
    );
    Ok(())
}

#[tokio::test]
async fn a_socket_on_a_stream_that_is_not_running_is_closed_with_a_reason() -> Result<()> {
    let mut server = serve_with(quiet()).await?;
    let mut disabled = stream_body("boiler");
    disabled["enabled"] = json!(false);
    let (status, body) = server
        .client
        .send("POST", "/api/streams", Some(disabled))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_owned();

    let session = server.session().expect("the admin has a session").clone();
    // The handshake **succeeds** — the auth was fine — and the reason arrives
    // as a close frame, because by then a browser can no longer read a body.
    let mut socket = server.observe(&id, Some(&session)).await.unwrap();
    let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .expect("a close frame arrives")
        .expect("the socket is open")
        .unwrap();
    match message {
        Message::Close(Some(frame)) => assert!(
            frame.reason.contains("not running"),
            "got `{}`",
            frame.reason
        ),
        other => panic!("expected a close frame, got {other:?}"),
    }
    Ok(())
}

/// The next JSON frame, or a panic saying none arrived.
async fn next_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
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

/// Percent-encode a query-parameter value — enough for the JSON this test
/// sends, which is the reason it is three characters rather than a dependency.
fn urlencode(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            c => c.to_string().bytes().map(|b| format!("%{b:02X}")).collect(),
        })
        .collect()
}

/// The route the admin SPA opens an Observe socket on and the route this crate
/// mounts are one contract with two spellings, and nothing else in the build
/// would notice them diverging: a page that connects to the wrong path gets a
/// 404, which looks exactly like a stream that never publishes.
///
/// The chat socket's test, for the chat socket's reason (§9 cites it). The
/// typed half of the admin API needs no such test — `client.ts` is generated
/// from the endpoint set and `admin_client_sync.rs` asserts the committed copy
/// matches — but a socket has no shape in an `EndpointSet`, so this path is
/// written by hand at both ends.
#[test]
fn the_observe_route_is_the_path_the_spa_connects_to() {
    assert_eq!(sc_server::STREAM_OBSERVE_ROUTE, "/api/streams/{id}/observe");
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/admin/src/streams.ts");
    let source = std::fs::read_to_string(&path).expect("read streams.ts");
    assert!(
        source.contains(sc_server::STREAM_OBSERVE_ROUTE),
        "{} must connect to {}",
        path.display(),
        sc_server::STREAM_OBSERVE_ROUTE
    );
}
