//! Build hosts (`TODO-build-hosts.md` B1): an admin adds a Mac through the
//! admin API, and its agent's heartbeat is answered on `/build-agent/heartbeat`
//! by the token it was given, and by nothing else.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, BUILD_AGENT_HEARTBEAT_ROUTE, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The signed-in admin, by cookie, as the admin UI is.
struct Admin {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Admin {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        let jar = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        if !jar.is_empty() {
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or(Body::empty(), |b| {
                Body::from(serde_json::to_vec(&b).unwrap())
            }))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Some((name, value)) = raw
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .split_once('=')
            {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        read(response).await
    }

    /// The admin's session, cookie and CSRF token, to present where it must
    /// not count.
    fn session(&self) -> (String, String) {
        let cookies = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        (cookies, self.cookies[CSRF_COOKIE].clone())
    }
}

async fn read(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A heartbeat as an agent sends it: a bearer token, a report, no cookie and
/// no CSRF token.
async fn heartbeat(
    router: &Router,
    token: Option<&str>,
    session: Option<(String, String)>,
    report: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(BUILD_AGENT_HEARTBEAT_ROUTE)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some((cookies, csrf)) = session {
        builder = builder
            .header(header::COOKIE, cookies)
            .header(CSRF_HEADER, csrf);
    }
    let request = builder
        .body(Body::from(serde_json::to_vec(&report).unwrap()))
        .unwrap();
    read(router.clone().oneshot(request).await.unwrap()).await
}

fn report(protocol: i64) -> Value {
    json!({
        "protocol": protocol,
        "version": "0.1.0",
        "os": "macos",
        "arch": "aarch64",
        "readiness": { "ios": { "missing": [] } },
    })
}

async fn setup(db: &TestDb) -> sc_error::Result<(Admin, Router)> {
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    let mut admin = Admin {
        router: router.clone(),
        cookies: HashMap::new(),
    };
    admin.send("GET", "/api/auth/status", None).await;
    let (status, _) = admin
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    Ok((admin, router))
}

#[tokio::test]
async fn a_build_host_is_heard_from_by_its_token_and_nothing_else() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let (mut admin, router) = setup(&db).await?;

    let (status, created) = admin
        .send(
            "POST",
            "/api/build-hosts",
            Some(json!({ "name": "Mac mini", "targets": ["ios"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_owned();
    let id = created["host"]["id"].as_str().unwrap().to_owned();
    assert_eq!(created["host"]["online"], false);

    // The admin's own session is not a credential here, and neither is a
    // token of the wrong kind.
    let (status, body) = heartbeat(&router, None, Some(admin.session()), report(1)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert!(body["error"].as_str().unwrap().contains("Bearer"), "{body}");
    let (status, body) = heartbeat(&router, Some("fspk_not_a_host"), None, report(1)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("not a build host token"),
        "{body}"
    );

    // The agent's heartbeat: answered with its targets, and the host is online.
    let (status, answer) = heartbeat(&router, Some(&token), None, report(1)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["name"], "Mac mini");
    assert_eq!(answer["targets"][0]["name"], "ios");
    let (_, hosts) = admin.send("GET", "/api/build-hosts", None).await;
    assert_eq!(hosts[0]["online"], true, "{hosts}");
    assert_eq!(hosts[0]["report"]["os"], "macos");
    assert!(hosts[0].get("token_hash").is_none());

    // An agent on another protocol is turned away, and the admin sees why.
    let (status, body) = heartbeat(&router, Some(&token), None, report(99)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["error"].as_str().unwrap().contains("install feldspar"),
        "{body}"
    );
    let (_, hosts) = admin.send("GET", "/api/build-hosts", None).await;
    assert_eq!(hosts[0]["online"], false);
    assert!(
        hosts[0]["report"]["refused"]
            .as_str()
            .unwrap()
            .contains("protocol 99")
    );

    // Renamed, it answers by its new name; removed, its token stops working.
    let (status, renamed) = admin
        .send(
            "PUT",
            &format!("/api/build-hosts/{id}"),
            Some(json!({ "name": "Studio", "targets": ["ios", "ios_simulator"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    let (_, answer) = heartbeat(&router, Some(&token), None, report(1)).await;
    assert_eq!(answer["name"], "Studio");
    assert_eq!(answer["targets"].as_array().unwrap().len(), 2);
    let (status, _) = admin
        .send("DELETE", &format!("/api/build-hosts/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = heartbeat(&router, Some(&token), None, report(1)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    Ok(())
}
