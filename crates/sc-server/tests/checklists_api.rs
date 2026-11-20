//! A checklist run over its live socket (TODO-checklists C5.1): the Run
//! buttons open `/api/checklists/{id}/run/live` (or `/api/checklist-runs/live`
//! for Run all), and the server runs the checklist on that connection,
//! pushing a `progress` frame as each step begins and ends and then the runs.
//!
//! What the runner reports, step by step, is asserted in sc-check's runner
//! test; here it is the socket: the frames arrive in order and end with the
//! recorded run, a bad id or a plain fetch runs nothing, and another origin is
//! refused before the upgrade.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test code: it serves its own router on a port; the replacements are for production paths"
)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode, header};
use futures::StreamExt;
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_auth::SessionStore;
use sc_server::{
    AppMounts, CHECKLIST_RUN_LIVE_ROUTE, CHECKLISTS_RUN_LIVE_ROUTE, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::{TestDb, TestServer};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tower::ServiceExt;

use crate::support::{Socket, next_frame};

/// A cookie-carrying client over the router, as the SPA is.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let cookies = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookies);
        }
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
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
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }
}

/// The admin router over a fresh catalog with checklists, served on a port
/// for the sockets, with an admin signed in.
async fn setup() -> sc_error::Result<(Client, std::net::SocketAddr, TestDb)> {
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_check::bootstrap_checklists(&catalog).await?;

    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog, apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
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
    let (status, _) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok((client, addr, db))
}

impl Client {
    /// Open a run socket at `path` with this client's cookies, from `origin`.
    async fn open(
        &self,
        addr: std::net::SocketAddr,
        path: &str,
        origin: &str,
    ) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
        let mut request = format!("ws://{addr}{path}").into_client_request().unwrap();
        let headers = request.headers_mut();
        headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        let cookies = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        headers.insert(header::COOKIE, HeaderValue::from_str(&cookies).unwrap());
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(socket, _)| socket)
    }
}

/// Every frame up to and including the last (`finished` or `failed`), and
/// that the socket then closes.
async fn read_all(socket: &mut Socket) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let frame = next_frame(socket).await;
        let last = frame["type"] != json!("progress");
        out.push(frame);
        if last {
            break;
        }
    }
    let close = tokio::time::timeout(std::time::Duration::from_secs(20), socket.next())
        .await
        .expect("the socket closes");
    assert!(
        matches!(close, None | Some(Ok(Message::Close(_)))),
        "expected a close, got {close:?}"
    );
    out
}

#[tokio::test]
async fn a_run_over_its_live_socket_pushes_each_step_then_the_run() -> sc_error::Result<()> {
    let (mut client, addr, _db) = setup().await?;
    let origin = format!("http://{addr}");
    let (status, checklist) = client
        .send(
            "POST",
            "/api/checklists",
            Some(json!({
                "name": "Two looks",
                "steps": [
                    { "type": "core.manual", "params": { "instruction": "Look" }, "label": "First look" },
                    { "type": "core.manual", "params": { "instruction": "Look again" }, "label": "Second look" },
                ],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{checklist}");
    let id = checklist["id"].as_str().unwrap();
    let path = CHECKLIST_RUN_LIVE_ROUTE.replace("{id}", id);

    let mut socket = client.open(addr, &path, &origin).await.unwrap();
    let frames = read_all(&mut socket).await;
    let (last, progress) = frames.split_last().unwrap();

    // Progress frames, each the whole state so far, in order: the finished
    // steps only ever grow. (These steps are over in a millisecond, so not
    // every one gets a frame of its own; the runner's own test sees each.)
    assert!(!progress.is_empty(), "{frames:#?}");
    let done: Vec<usize> = progress
        .iter()
        .map(|f| f["progress"]["steps"].as_array().map_or(0, Vec::len))
        .collect();
    assert!(done.windows(2).all(|w| w[0] <= w[1]), "{frames:#?}");
    let end = &progress.last().unwrap()["progress"];
    assert_eq!(end["checklist"], json!("Two looks"));
    assert_eq!(end["total"], json!(2));
    assert_eq!(end["steps"].as_array().map(Vec::len), Some(2));

    // Then the run, recorded as one `POST …/run` would have made.
    assert_eq!(last["type"], json!("finished"), "{last}");
    let run = &last["runs"][0];
    assert_eq!(run["checklist"], json!(id));
    assert_eq!(run["trigger"], json!("manual"));
    let (status, stored) = client
        .send(
            "GET",
            &format!("/api/checklist-runs/{}", run["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{stored}");

    // Run all counts the checklists it runs.
    let mut socket = client
        .open(
            addr,
            &format!("{CHECKLISTS_RUN_LIVE_ROUTE}?application=none"),
            &origin,
        )
        .await
        .unwrap();
    let frames = read_all(&mut socket).await;
    let (last, progress) = frames.split_last().unwrap();
    assert_eq!(last["type"], json!("finished"), "{last}");
    assert_eq!(last["runs"].as_array().map(Vec::len), Some(1));
    assert_eq!(progress[0]["progress"]["checklists_total"], json!(1));

    // A checklist that does not exist is a `failed` frame, not a run.
    let missing = CHECKLIST_RUN_LIVE_ROUTE.replace("{id}", &uuid::Uuid::new_v4().to_string());
    let mut socket = client.open(addr, &missing, &origin).await.unwrap();
    let frames = read_all(&mut socket).await;
    assert_eq!(frames.len(), 1, "{frames:#?}");
    assert_eq!(frames[0]["type"], json!("failed"));

    // Nothing runs without an upgrade from this host: not a plain fetch, not
    // a malformed id, not another site's page.
    // (A fetch has no handshake `Origin`, so the origin check refuses it.)
    let (status, _) = client.send("GET", &path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = client
        .send(
            "GET",
            &CHECKLIST_RUN_LIVE_ROUTE.replace("{id}", "nope"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        client
            .open(addr, &path, "http://evil.example")
            .await
            .is_err(),
        "another origin's upgrade is refused"
    );
    let (_, runs) = client
        .send("GET", &format!("/api/checklists/{id}/runs"), None)
        .await;
    // The single run and Run all's: nothing else ran.
    assert_eq!(runs.as_array().map(Vec::len), Some(2), "{runs}");
    Ok(())
}

/// The run sockets' paths are written by hand at both ends (a socket has no
/// shape in an `EndpointSet`), so this holds the admin's spelling to the
/// server's.
#[test]
fn the_run_socket_routes_are_the_paths_the_spa_connects_to() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../ui/admin/src/checklistProgress.tsx");
    let source = std::fs::read_to_string(&path).expect("read checklistProgress.tsx");
    for route in [CHECKLIST_RUN_LIVE_ROUTE, CHECKLISTS_RUN_LIVE_ROUTE] {
        assert!(
            source.contains(&format!("\"{route}\"")),
            "{} must connect to {route}",
            path.display()
        );
    }
}
