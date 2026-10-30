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
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, SESSION_COOKIE, ServerConfig, admin_handlers, build_router,
};
use sc_test_harness::TestDb;
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
    let db = TestDb::new().await?;

    // The multi-tenant v1 test template carries `users` tables in several
    // schemas; drop them all so bootstrap creates the clean MVP table. No-op in
    // clean CI.
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
    sc_config::bootstrap(&catalog).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps),
        sessions,
        &ServerConfig::default(),
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
