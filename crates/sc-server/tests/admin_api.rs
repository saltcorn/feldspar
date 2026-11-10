//! End-to-end integration tests for the admin API handlers, driven through the
//! assembled router against a **real Postgres database** (Phase 6 server
//! subphase, final item).
//!
//! Unlike `router.rs` — which exercises dispatch/auth/CSRF with stub handlers and
//! no database — this walks the whole admin story through the concrete
//! [`admin_handlers`]: bootstrap the first user, create a table and a field, CRUD
//! a row, and manage users, each as an HTTP request carrying the session and CSRF
//! cookies a browser would. A fresh, isolated database is created per test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_server::{AppMounts, SESSION_COOKIE, ServerConfig, admin_handlers, build_router_with_apps};
use sc_test_harness::{TestDb, TestServer};
use sc_types::{Role, SECRET_SENTINEL};
use serde_json::{Value, json};
use tower::ServiceExt;

/// A cookie-jar-carrying client over the router: it remembers `Set-Cookie`
/// values (CSRF + session) between requests and echoes the CSRF token on
/// mutations, exactly as a browser-based SPA would.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router) -> Client {
        Client {
            router,
            cookies: HashMap::new(),
        }
    }

    /// Issue a request, updating the stored cookies from the response.
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);

        // Send every stored cookie.
        if !self.cookies.is_empty() {
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
        }
        // Echo the CSRF token on mutating requests (double-submit).
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
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

        // Fold Set-Cookie values into the jar (empty value → cleared).
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
}

/// Build a router over the admin endpoints backed by a fresh catalog, plus the
/// client that drives it. The users table is bootstrapped so auth works. The
/// returned [`TestDb`] must be kept in scope: dropping it deletes the database.
async fn setup() -> sc_error::Result<(Client, TestDb)> {
    let (client, db, _) = setup_with_catalog().await?;
    Ok((client, db))
}

/// [`setup`], also handing back the catalog, with the configuration table
/// bootstrapped so a test can store settings before the first request.
async fn setup_with_catalog() -> sc_error::Result<(Client, TestDb, Arc<Catalog>)> {
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
    sc_auth::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    // The router reads the catalog too: where every sign-in's second factor is
    // decided (it starts no session without one).
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        sessions,
        &ServerConfig::default(),
        apps,
    )?;

    Ok((Client::new(router), db, catalog))
}

/// An installation given a first-user token before it first served (what a
/// hosting control panel does with `feldspar set-cfg first_user_token …`) is
/// claimed only with that token: the status says so, a create without it or with
/// a wrong one is refused `403` and creates nobody, and the right one creates
/// the admin. Once there is a user the token no longer matters.
#[tokio::test]
async fn a_first_user_token_is_needed_to_create_the_first_user() -> sc_error::Result<()> {
    let (mut client, _db, catalog) = setup_with_catalog().await?;
    sc_config::set_config(&catalog, sc_config::FIRST_USER_TOKEN, json!("s3cret-setup")).await?;

    let (_, body) = client.send("GET", "/api/auth/status", None).await;
    assert_eq!(body["any_user_exists"], json!(false));
    assert_eq!(body["first_user_token_required"], json!(true), "{body}");

    let credentials = |token: Option<&str>| {
        let mut body = json!({ "email": "admin@example.com", "password": "hunter2pass" });
        if let Some(token) = token {
            body["token"] = json!(token);
        }
        Some(body)
    };
    for token in [None, Some("wrong"), Some(""), Some("s3cret-setup-and-more")] {
        let (status, body) = client
            .send("POST", "/api/first-user", credentials(token))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{token:?}: {body}");
        assert!(
            body["error"].as_str().unwrap_or("").contains("setup link"),
            "{body}"
        );
        assert!(!client.cookies.contains_key(SESSION_COOKIE));
    }
    assert!(!sc_auth::any_user_exists(&catalog).await?);

    let (status, body) = client
        .send("POST", "/api/first-user", credentials(Some("s3cret-setup")))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], json!(1));
    assert!(client.cookies.contains_key(SESSION_COOKIE));

    let (_, body) = client.send("GET", "/api/auth/status", None).await;
    assert_eq!(body["any_user_exists"], json!(true));
    assert_eq!(body["first_user_token_required"], json!(false), "{body}");
    Ok(())
}

/// Without a token, the first user is created as before, and the status says
/// none is needed.
#[tokio::test]
async fn without_a_first_user_token_anyone_may_create_the_first_user() -> sc_error::Result<()> {
    let (mut client, _db, _catalog) = setup_with_catalog().await?;
    let (_, body) = client.send("GET", "/api/auth/status", None).await;
    assert_eq!(body["first_user_token_required"], json!(false), "{body}");
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok(())
}

/// Settings → System: the build's version, the database it runs on, and the
/// process's readings — for an admin only. (`restartServer` is not called: it
/// would exit this test binary.)
#[tokio::test]
async fn the_system_tab_names_the_version_and_the_database() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;
    let (status, _) = client.send("GET", "/api/system", None).await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
        "anonymous: {status}"
    );

    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = client.send("GET", "/api/system", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(env!("CARGO_PKG_VERSION")));
    // A test build is not stamped by the release script.
    assert_eq!(body["commit"], Value::Null, "{body}");
    assert_eq!(body["build_time"], Value::Null, "{body}");
    assert_eq!(body["version_line"], json!(sc_server::version_line()));
    assert_eq!(body["database"]["product"], json!("PostgreSQL"), "{body}");
    let version = body["database"]["version"].as_str().unwrap_or_default();
    assert!(
        version.chars().next().is_some_and(|c| c.is_ascii_digit()),
        "the bare server version: {body}"
    );
    assert_eq!(body["pid"], json!(std::process::id()));
    assert!(
        body["started_at"].as_str().is_some_and(|t| t.contains('T')),
        "{body}"
    );
    assert!(body["uptime_seconds"].as_u64().is_some(), "{body}");
    #[cfg(target_os = "linux")]
    {
        assert!(
            body["memory"]["process_bytes"]
                .as_u64()
                .is_some_and(|b| b > 0),
            "{body}"
        );
        let cpu = &body["cpu"];
        assert!(cpu["cores"].as_u64().is_some_and(|c| c >= 1), "{body}");
        assert!(
            cpu["system_percent"]
                .as_f64()
                .is_some_and(|p| (0.0..=100.0).contains(&p)),
            "{body}"
        );
        assert!(
            body["disk"]["total_bytes"].as_u64().is_some_and(|b| b > 0),
            "{body}"
        );
    }
    Ok(())
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn full_admin_api_story() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // --- bootstrap: no user yet -------------------------------------------
    let (status, body) = client.send("GET", "/api/auth/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["any_user_exists"], json!(false));
    assert_eq!(body["current_user"], Value::Null);

    // Admin routes are locked before login.
    let (status, _) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // --- create the first user (logs them straight in) --------------------
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"], json!("admin@example.com"));
    assert_eq!(body["role"], json!(1));
    assert!(client.cookies.contains_key(SESSION_COOKIE));

    // Now the status reflects the logged-in admin.
    let (_, body) = client.send("GET", "/api/auth/status", None).await;
    assert_eq!(body["any_user_exists"], json!(true));
    assert_eq!(body["current_user"]["email"], json!("admin@example.com"));

    // --- tables & fields ---------------------------------------------------
    let (status, body) = client
        .send("POST", "/api/tables", Some(json!({ "name": "book" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], json!("book"));

    // The key is a field like any other and nothing invents one (GOALS), so it
    // is added the same way the title is — and an `int` key numbers itself,
    // which is what the row CRUD below relies on.
    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["primary_key"], json!(true));
    assert_eq!(body["nullable"], json!(false), "a key column is NOT NULL");

    let (status, body) = client
        .send(
            "POST",
            "/api/tables/book/fields",
            Some(json!({ "name": "title", "type": "text", "required": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], json!("title"));
    assert_eq!(body["sql_type"], json!("text"));
    assert_eq!(body["type"], json!("text"));
    assert_eq!(body["nullable"], json!(false));

    let (_, body) = client.send("GET", "/api/tables", None).await;
    let names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&"book"), "book listed: {names:?}");

    let (_, body) = client.send("GET", "/api/tables/book/fields", None).await;
    let field_names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(field_names, ["id", "title"]);

    // --- row CRUD ----------------------------------------------------------
    let (status, row) = client
        .send(
            "POST",
            "/api/tables/book/rows",
            Some(json!({ "title": "Dune" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(row["title"], json!("Dune"));
    let id = row["id"].as_i64().expect("generated integer id");

    let (_, body) = client.send("GET", "/api/tables/book/rows", None).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["title"], json!("Dune"));

    let (status, updated) = client
        .send(
            "PUT",
            &format!("/api/tables/book/rows/{id}"),
            Some(json!({ "title": "Dune Messiah" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["title"], json!("Dune Messiah"));
    assert_eq!(updated["id"], json!(id));

    let (status, body) = client
        .send("DELETE", &format!("/api/tables/book/rows/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["deleted"], json!(true));

    // Deleting a row that no longer exists is a 404.
    let (status, _) = client
        .send("DELETE", &format!("/api/tables/book/rows/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, body) = client.send("GET", "/api/tables/book/rows", None).await;
    assert_eq!(body.as_array().unwrap().len(), 0);

    // --- roles & users -----------------------------------------------------
    // A user's role references `_fd_roles`, so the role has to exist before a
    // user can hold it. The two built-ins are seeded at bootstrap; role 40 is
    // created here, which is the ordinary flow (make the role, then the users).
    let (status, role) = client
        .send(
            "POST",
            "/api/roles",
            Some(json!({ "role": 40, "name": "Editor", "description": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(role["role"], json!(40));
    assert_eq!(role["builtin"], json!(false));

    let (status, body) = client
        .send(
            "POST",
            "/api/users",
            Some(json!({ "email": "editor@example.com", "password": "editorpass", "role": 40 })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["user"]["email"], json!("editor@example.com"));
    assert_eq!(body["user"]["role"], json!(40));
    // A password was given, so there is nothing to reveal.
    assert_eq!(body["generated_password"], Value::Null);

    let (_, body) = client.send("GET", "/api/users", None).await;
    let emails: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["email"].as_str())
        .collect();
    assert_eq!(emails.len(), 2);
    assert!(emails.contains(&"admin@example.com"));
    assert!(emails.contains(&"editor@example.com"));

    // --- logout / login ----------------------------------------------------
    let (status, _) = client.send("POST", "/api/logout", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!client.cookies.contains_key(SESSION_COOKIE));

    // Logged out → admin routes locked again.
    let (status, _) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A non-admin cannot log into the admin UI (role gate).
    let (status, _) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "editor@example.com", "password": "editorpass" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Wrong password is rejected.
    let (status, _) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "admin@example.com", "password": "wrong" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Correct admin credentials log back in and unlock admin routes.
    let (status, body) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["role"], json!(1));

    let (status, _) = client.send("GET", "/api/tables", None).await;
    assert_eq!(status, StatusCode::OK);

    Ok(())
}

/// The data grid's read: a page of rows, ordered, and narrowed by the same
/// `column=op.value` filters an application's REST read takes.
///
/// The grid is a spreadsheet over a table of any size, so it never asks for the
/// table — it asks for the rows under the viewport, in the order the header says,
/// out of the population the filter row leaves. Two things here are worth an
/// assertion rather than a reading of the code: that `limit`/`offset` slice the
/// *ordered* population (a page of an unordered read is a different set of rows
/// each time), and that `countRows` counts what `listRows` pages — the scrollbar
/// is as long as that number, so a count that ignored the filters would leave
/// the bottom of a filtered table empty.
#[tokio::test]
async fn rows_are_paged_ordered_and_filtered_by_the_query_string() -> sc_error::Result<()> {
    let (mut client, _db) = setup().await?;

    // A GET first: it is what mints the CSRF cookie the client echoes on every
    // mutation after it, exactly as a browser loading the SPA would.
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = client
        .send("POST", "/api/tables", Some(json!({ "name": "note" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    client
        .send(
            "POST",
            "/api/tables/note/fields",
            Some(json!({ "name": "id", "type": "int", "primary_key": true })),
        )
        .await;
    client
        .send(
            "POST",
            "/api/tables/note/fields",
            Some(json!({ "name": "title", "type": "text" })),
        )
        .await;
    client
        .send(
            "POST",
            "/api/tables/note/fields",
            Some(json!({ "name": "pages", "type": "int" })),
        )
        .await;

    // Twelve rows, inserted in an order that is not any of the orders asked for
    // below, so an assertion about ordering cannot pass by accident.
    for n in [7, 3, 11, 1, 9, 5, 12, 2, 8, 4, 10, 6] {
        let (status, _) = client
            .send(
                "POST",
                "/api/tables/note/rows",
                Some(json!({ "title": format!("note {n:02}"), "pages": n })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // A page of the ordered population: rows 5..9 by `pages`.
    let (status, body) = client
        .send(
            "GET",
            "/api/tables/note/rows?order=pages.asc&limit=4&offset=4",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pages: Vec<i64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["pages"].as_i64().unwrap())
        .collect();
    assert_eq!(pages, [5, 6, 7, 8]);

    // The same page descending is the other end of the same order.
    let (_, body) = client
        .send(
            "GET",
            "/api/tables/note/rows?order=pages.desc&limit=4&offset=0",
            None,
        )
        .await;
    let pages: Vec<i64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["pages"].as_i64().unwrap())
        .collect();
    assert_eq!(pages, [12, 11, 10, 9]);

    // A filter row with two boxes typed in: a comparison and a pattern, ANDed.
    let (status, body) = client
        .send(
            "GET",
            "/api/tables/note/rows?pages=gte.9&title=ilike.%25note%25&order=pages.asc",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pages: Vec<i64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["pages"].as_i64().unwrap())
        .collect();
    assert_eq!(pages, [9, 10, 11, 12]);

    // …and the count is of that population, not of the table.
    let (_, body) = client
        .send(
            "GET",
            "/api/tables/note/rows/count?pages=gte.9&title=ilike.%25note%25",
            None,
        )
        .await;
    assert_eq!(body["count"], json!(4));
    let (_, body) = client
        .send("GET", "/api/tables/note/rows/count", None)
        .await;
    assert_eq!(body["count"], json!(12));

    // A filter naming a column the table does not have is refused rather than
    // ignored: a silently dropped filter answers with rows nobody asked for.
    let (status, body) = client
        .send("GET", "/api/tables/note/rows?nope=eq.1", None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    Ok(())
}

/// The registry editor (APP_SYNC_PROPOSAL §7.6) over the admin API: admin
/// only, the instance tree listed with its marks, a settings section opened,
/// planned and saved, a save over a change made since it was opened refused
/// as stale, and none of it offered to the MCP projection.
#[tokio::test]
async fn the_registry_lists_opens_plans_and_saves() -> sc_error::Result<()> {
    let (mut client, _db, catalog) = setup_with_catalog().await?;
    sc_config::set_config(
        &catalog,
        sc_config::email::SMTP_HOST,
        json!("mail.example.org"),
    )
    .await?;
    sc_config::set_config(
        &catalog,
        sc_config::email::SMTP_PASSWORD,
        json!("SMTP-SECRET-3"),
    )
    .await?;
    // What the SPA asks first, which hands it the CSRF cookie.
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    sc_auth::bootstrap_roles(&catalog).await?;
    sc_auth::create_user(&catalog, "staff@example.com", "staffpass1", Role::PUBLIC).await?;
    client.send("POST", "/api/logout", None).await;

    // Nobody signed in, and a non-admin, are refused.
    let (status, _) = client.send("GET", "/api/registry", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "staff@example.com", "password": "staffpass1" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = client
        .send("GET", "/api/registry/entity?path=settings/email.json", None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = client.send("GET", "/api/registry", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let email = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == "settings/email.json")
        .cloned()
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(email["kind"], "setting");
    assert_eq!(email["instance_only"]["scope"], "whole");

    let (status, opened) = client
        .send("GET", "/api/registry/entity?path=settings/email.json", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    assert_eq!(opened["value"]["smtp_host"], "mail.example.org");
    assert_eq!(opened["value"]["smtp_password"], SECRET_SENTINEL);
    assert!(!opened.to_string().contains("SMTP-SECRET-3"), "{opened}");
    // The editor's schema is the section's declared keys, each typed.
    assert_eq!(
        opened["schema"]["properties"]["smtp_port"]["type"], "integer",
        "{opened}"
    );
    assert_eq!(opened["schema"]["additionalProperties"], false);

    let mut value = opened["value"].clone();
    value["smtp_host"] = json!("relay.example.org");
    let edit = json!({
        "path": "settings/email.json",
        "value": value,
        "version": opened["version"],
    });
    let (status, plan) = client
        .send("POST", "/api/registry/plan", Some(edit.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    let ops: Vec<&str> = plan["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["id"].as_str().unwrap())
        .collect();
    assert_eq!(ops, ["update:setting:email.smtp_host"], "{plan}");
    assert_eq!(plan["blockers"], json!([]));

    let mut save = edit.clone();
    save["value"] = plan["value"].clone();
    save["plan_digest"] = plan["digest"].clone();
    save["ticked"] = json!(ops);
    let (status, receipt) = client
        .send("POST", "/api/registry/save", Some(save.clone()))
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["result"], "converged", "{receipt}");
    assert_eq!(receipt["stored"]["smtp_host"], "relay.example.org");
    assert_eq!(receipt["changes"], json!([]), "{receipt}");
    assert_eq!(
        sc_config::stored_config(&catalog, sc_config::email::SMTP_PASSWORD).await?,
        Some(json!("SMTP-SECRET-3"))
    );

    // The same save again names the version from before it: stale.
    let (status, body) = client.send("POST", "/api/registry/save", Some(save)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap_or("").contains("stale"),
        "{body}"
    );

    // Not one of them is offered to an agent.
    let set = sc_api::admin_endpoints();
    for name in [
        "listRegistry",
        "getRegistryEntity",
        "planRegistrySave",
        "registrySave",
    ] {
        let endpoint = set.iter().find(|e| e.name == name).unwrap();
        assert!(endpoint.mcp.is_none(), "{name} is offered to MCP");
    }
    Ok(())
}

use crate::support::data_dir;

/// Every structure table a server's start bootstraps (`feldspar serve`'s
/// `connect_catalog`): what an export reads, a deploy writes and a safety
/// backup holds.
async fn bootstrap_structure(catalog: &Catalog) -> sc_error::Result<()> {
    sc_catalog::bootstrap_table_meta(catalog).await?;
    sc_catalog::bootstrap_field_meta(catalog).await?;
    sc_catalog::bootstrap_file_stores(catalog).await?;
    sc_catalog::bootstrap_db_connections(catalog).await?;
    sc_catalog::bootstrap_code_libraries(catalog).await?;
    sc_app::bootstrap(catalog).await?;
    sc_app::bootstrap_translations(catalog).await?;
    sc_viewpattern::bootstrap(catalog).await?;
    sc_action::bootstrap_triggers(catalog).await?;
    sc_workflow::bootstrap_workflow_versions(catalog).await?;
    sc_stream::bootstrap_streams(catalog).await?;
    sc_agent::bootstrap_agents(catalog).await?;
    sc_llm::bootstrap_llm_providers(catalog).await?;
    sc_llm::bootstrap_llm_models(catalog).await?;
    sc_mcp::bootstrap_mcp_servers(catalog).await?;
    sc_module::bootstrap_modules(catalog).await?;
    sc_dataset::bootstrap_datasets(catalog).await?;
    sc_model::bootstrap_models(catalog).await?;
    sc_analytics::bootstrap_workspaces(catalog).await?;
    sc_check::bootstrap_checklists(catalog).await?;
    sc_sync::bootstrap(catalog).await?;
    catalog.reload().await
}

/// Run git in `dir`, asserting it succeeded.
#[allow(
    clippy::disallowed_methods,
    reason = "test fixture: plays another instance's admin pushing to the repository"
)]
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=Bea", "-c", "user.email=bea@example.com"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`git {}` failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Source control (APP_SYNC_PROPOSAL §7.2) over the admin API, against a bare
/// repository on this machine: admin only, link, Status with its diff, Commit,
/// branches, an incoming change from another clone pulled, planned and
/// deployed, a discard reviewed and applied, the ledger, the registry's view of
/// the base and the tip — and none of it offered to the MCP projection.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn source_control_links_commits_pulls_deploys_and_discards() -> sc_error::Result<()> {
    let scratch = data_dir().join(format!("repos-{}", uuid::Uuid::new_v4().simple()));
    let remote = scratch.join("structure.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "--initial-branch=main"]);
    let url = format!("file://{}", remote.display());

    let (mut client, _db, catalog) = setup_with_catalog().await?;
    bootstrap_structure(&catalog).await?;
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    sc_auth::bootstrap_roles(&catalog).await?;
    sc_auth::create_user(&catalog, "staff@example.com", "staffpass1", Role::PUBLIC).await?;
    client.send("POST", "/api/logout", None).await;

    // Nobody signed in, and a non-admin, are refused.
    let (status, _) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "staff@example.com", "password": "staffpass1" })),
        )
        .await;
    for (method, path) in [
        ("GET", "/api/source-control/link"),
        ("POST", "/api/source-control/deploy/plan"),
        ("GET", "/api/source-control/deployments"),
    ] {
        let (status, _) = client.send(method, path, Some(json!({}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    let (status, body) = client
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Unlinked: Status says so, and has nothing.
    let (status, body) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["linked"], false);
    assert_eq!(body["entries"], json!([]));

    // A link to a repository that is not there is refused, and not kept.
    let (status, body) = client
        .send(
            "PUT",
            "/api/source-control/link",
            Some(json!({ "url": format!("file://{}/nowhere.git", scratch.display()), "branch": "main" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, body) = client.send("GET", "/api/source-control/link", None).await;
    assert_eq!(body["link"], Value::Null, "{body}");

    let (status, body) = client
        .send(
            "POST",
            "/api/source-control/link/test",
            Some(json!({ "url": url, "branch": "main" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["branches"], json!([]));
    let (status, body) = client
        .send(
            "PUT",
            "/api/source-control/link",
            Some(json!({
                "url": url,
                "branch": "main",
                "credential": { "kind": "none" },
                "policy": { "full_backup": "optional" },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["base_taken"], true);
    let (_, body) = client.send("GET", "/api/source-control/link", None).await;
    assert_eq!(body["link"]["url"], url.as_str(), "{body}");
    assert_eq!(body["link"]["policy"]["full_backup"], "optional");
    assert_eq!(body["credential"]["kind"], "none");

    // A table made here is a local change, which stops a deploy.
    catalog
        .create_table(
            "tickets",
            &[
                sc_catalog::DataField::plain(
                    "id",
                    sc_types::TypeRef::Basic(sc_types::BasicType::Int),
                )
                .required()
                .primary_key()
                .generated(sc_db::ColumnGenerator::Identity),
                sc_catalog::DataField::plain(
                    "title",
                    sc_types::TypeRef::Basic(sc_types::BasicType::Text),
                ),
            ],
        )
        .await?;
    let (status, body) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["linked"], true);
    let tickets = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["key"] == "table:tickets")
        .cloned()
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(tickets["class"], "local_new");
    assert_eq!(tickets["path"], "tables/tickets.json");
    assert_eq!(tickets["in_registry"], true);
    assert!(
        body["deploy_blockers"]
            .as_array()
            .unwrap()
            .contains(&json!("table:tickets")),
        "{body}"
    );
    let (status, diff) = client
        .send("GET", "/api/source-control/diff?key=table:tickets", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{diff}");
    assert_eq!(diff["class"], "local_new");
    assert_eq!(diff["local"]["changes"][0]["path"], "", "{diff}");
    assert_eq!(diff["local"]["changes"][0]["after"]["name"], "tickets");
    let (status, plan) = client
        .send("POST", "/api/source-control/deploy/plan", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert!(
        plan["blockers"][0]["message"]
            .as_str()
            .unwrap_or("")
            .contains("uncommitted"),
        "{plan}"
    );

    // Commit: needs a message, then pushes everything local.
    let (status, _) = client
        .send(
            "POST",
            "/api/source-control/commit",
            Some(json!({ "message": "  " })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, committed) = client
        .send(
            "POST",
            "/api/source-control/commit",
            Some(json!({ "message": "Add tickets" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{committed}");
    assert!(
        committed["entities"]
            .as_array()
            .unwrap()
            .contains(&json!("table:tickets")),
        "{committed}"
    );
    let (_, body) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(body["entries"], json!([]), "{body}");
    assert_eq!(body["tip"], committed["commit"]);

    // Branches: create one from the tip, see it, and switch back.
    let (status, body) = client
        .send(
            "POST",
            "/api/source-control/branch",
            Some(json!({ "name": "feature", "create": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = client
        .send("GET", "/api/source-control/branches", None)
        .await;
    assert_eq!(body["current"], "feature");
    assert_eq!(body["branches"], json!(["feature", "main"]), "{body}");
    let (status, body) = client
        .send(
            "POST",
            "/api/source-control/branch",
            Some(json!({ "name": "main" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Another instance's admin describes the table and pushes.
    let work = scratch.join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["clone", "-q", &url, "."]);
    let file = work.join("tables/tickets.json");
    let mut table: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    table["description"] = json!("Support tickets");
    std::fs::write(&file, sc_sync::render_json(&table)).unwrap();
    git(&work, &["commit", "-q", "-am", "Describe tickets"]);
    git(&work, &["push", "-q", "origin", "main"]);

    // Status is of the last pull until Pull.
    let (_, body) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(body["entries"], json!([]), "{body}");
    let (status, pulled) = client.send("POST", "/api/source-control/pull", None).await;
    assert_eq!(status, StatusCode::OK, "{pulled}");
    assert_eq!(pulled["before"], committed["commit"]);
    assert_eq!(pulled["forced"], false);
    let (_, body) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(body["entries"][0]["key"], "table:tickets", "{body}");
    assert_eq!(body["entries"][0]["class"], "incoming");
    assert_eq!(body["deploy_blockers"], json!([]));
    let (_, diff) = client
        .send("GET", "/api/source-control/diff?key=table:tickets", None)
        .await;
    assert_eq!(
        diff["incoming"]["changes"][0]["path"], "description",
        "{diff}"
    );
    assert_eq!(diff["incoming"]["changes"][0]["after"], "Support tickets");

    // The registry editor sees the base and the tip beside live.
    let (status, opened) = client
        .send("GET", "/api/registry/entity?path=tables/tickets.json", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    assert_eq!(opened["sync"]["class"], "incoming", "{opened}");
    assert_eq!(opened["sync"]["key"], "table:tickets");
    assert_eq!(opened["sync"]["tip"]["description"], "Support tickets");
    assert_eq!(opened["sync"]["base"]["description"], "");

    // Deploy: the review, a confirmation of another digest refused as stale,
    // then the reviewed one applied.
    let (status, plan) = client
        .send("POST", "/api/source-control/deploy/plan", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert_eq!(plan["blockers"], json!([]), "{plan}");
    assert_eq!(plan["commit"], pulled["tip"]);
    assert_eq!(plan["policy"]["destructive"], "confirm");
    let ticked: Vec<Value> = plan["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|op| op["ticked"] == true)
        .map(|op| op["id"].clone())
        .collect();
    assert!(!ticked.is_empty(), "{plan}");
    let (status, body) = client
        .send(
            "POST",
            "/api/source-control/deploy",
            Some(json!({ "commit": plan["commit"], "plan_digest": "not-the-plan", "ticked": ticked })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap_or("").contains("stale"),
        "{body}"
    );
    let (status, receipt) = client
        .send(
            "POST",
            "/api/source-control/deploy",
            Some(json!({ "commit": plan["commit"], "plan_digest": plan["digest"], "ticked": ticked })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["result"], "converged", "{receipt}");
    let (_, body) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(body["entries"], json!([]), "{body}");

    // A table made and then discarded: reviewed, ticked, gone.
    catalog
        .create_table(
            "scratch",
            &[sc_catalog::DataField::plain(
                "id",
                sc_types::TypeRef::Basic(sc_types::BasicType::Int),
            )
            .required()
            .primary_key()
            .generated(sc_db::ColumnGenerator::Identity)],
        )
        .await?;
    let keys = json!(["table:scratch"]);
    let (status, plan) = client
        .send(
            "POST",
            "/api/source-control/discard/plan",
            Some(json!({ "keys": keys })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    let all: Vec<Value> = plan["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["id"].clone())
        .collect();
    assert!(!all.is_empty(), "{plan}");
    let (status, receipt) = client
        .send(
            "POST",
            "/api/source-control/discard",
            Some(json!({ "keys": keys, "plan_digest": plan["digest"], "ticked": all })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_ne!(receipt["result"], "failed", "{receipt}");
    let (_, body) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(body["entries"], json!([]), "{body}");

    // The ledger has each of them, newest first.
    let (status, body) = client
        .send("GET", "/api/source-control/deployments?limit=10", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let kinds: Vec<&str> = body["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["discard", "deploy", "commit"], "{body}");
    assert_eq!(body["deployments"][2]["summary"]["message"], "Add tickets");

    // Unlinking forgets the link; Status is unlinked again.
    let (status, _) = client
        .send("DELETE", "/api/source-control/link", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = client.send("GET", "/api/source-control/status", None).await;
    assert_eq!(body["linked"], false);

    // Not one of them is offered to an agent.
    let set = sc_api::admin_endpoints();
    for name in [
        "getSyncLink",
        "setSyncLink",
        "deleteSyncLink",
        "generateSyncKey",
        "testSyncLink",
        "getSyncStatus",
        "getSyncEntityDiff",
        "syncPull",
        "syncCommit",
        "planSyncDiscard",
        "syncDiscard",
        "listSyncBranches",
        "switchSyncBranch",
        "planSyncDeploy",
        "syncDeploy",
        "listDeployments",
    ] {
        let endpoint = set
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("no endpoint {name}"));
        assert!(endpoint.mcp.is_none(), "{name} is offered to MCP");
    }
    std::fs::remove_dir_all(&scratch).ok();
    Ok(())
}

/// A file store's synced contents (APP_SYNC_PROPOSAL §3.5) over the admin
/// API: what `auto` resolves to for a store nothing uses (nothing, with the
/// warning Status shows), a preview of `include`, setting it, and refusing a
/// value that is none of the three — admin only.
#[tokio::test]
async fn a_file_stores_synced_contents_are_resolved_previewed_and_set() -> sc_error::Result<()> {
    let (mut client, _db, catalog) = setup_with_catalog().await?;
    bootstrap_structure(&catalog).await?;
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let dir = data_dir().join(format!("assets-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let (status, store) = client
        .send(
            "POST",
            "/api/file-stores",
            Some(json!({
                "name": "assets", "description": "", "backend": "local",
                "config": { "path": dir.to_string_lossy() }, "min_role": Value::Null,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{store}");
    let path = format!("/api/file-stores/{}/sync", store["id"].as_str().unwrap());

    let (status, body) = client.send("GET", &path, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applies"], true);
    assert_eq!(body["sync_contents"], "auto");
    assert_eq!(body["synced"], json!([]));
    assert!(
        body["warnings"][0]
            .as_str()
            .unwrap_or("")
            .contains("`assets` is not synced"),
        "{body}"
    );

    // A preview changes nothing.
    let (_, body) = client
        .send("GET", &format!("{path}?contents=include"), None)
        .await;
    assert_eq!(body["sync_contents"], "include");
    assert_eq!(body["synced"], json!([""]), "{body}");
    let (_, body) = client.send("GET", &path, None).await;
    assert_eq!(body["sync_contents"], "auto");

    let (status, body) = client
        .send("PUT", &path, Some(json!({ "sync_contents": "include" })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["synced"], json!([""]));
    let (_, body) = client.send("GET", &path, None).await;
    assert_eq!(body["sync_contents"], "include");
    assert_eq!(body["warnings"], json!([]));
    let (status, _) = client
        .send("PUT", &path, Some(json!({ "sync_contents": "everything" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    client.send("POST", "/api/logout", None).await;
    let (status, _) = client.send("GET", &path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for name in ["getFileStoreSync", "setFileStoreSync"] {
        let set = sc_api::admin_endpoints();
        assert!(set.iter().find(|e| e.name == name).unwrap().mcp.is_none());
    }
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
