//! Phase 5 end-to-end integration test: ownership formulas doing observable
//! work through a real application (§7.3).
//!
//! The access rule under test, everywhere: **allowed = the caller meets the
//! operation's `min_role` OR the formula grants the row**. Every table here
//! keeps its floors at admin-only, so for every non-admin caller the formula
//! is the *only* path to the data — which is exactly what makes each assertion
//! about the formula and nothing else. What is asserted, at three-plus roles
//! through the real router:
//!
//! - the classic owner formula grants exactly the caller's rows, for reads and
//!   writes separately, with denial **indistinguishable from absence** (the
//!   same 404 a missing row gets);
//! - an update cannot move a row out of the caller's ownership (the runtime
//!   WITH CHECK);
//! - an operation-split formula (`_read || …`) opens reads wide while writes
//!   stay owned;
//! - a Ⱶ-join formula grants through the Key link — on the symbolic path *and*
//!   on the reified path (an untranslatable spelling of the same rule behaves
//!   identically, which is the parity property doing production work);
//! - the documented anonymous corner: `owner === user.email` matches
//!   null-owner rows for anonymous callers, and the `user && …` guard closes
//!   it;
//! - the per-`File`-field endpoints apply the same rule;
//! - and every formula is set through the admin API **after** the app is
//!   mounted, so each test also exercises the live re-projection.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_app::{
    ApiConfig, Application, CodeFramework, FrameworkRef, app_source_from_config, build_application,
};
use sc_auth::{ROLE_ADMIN, Role, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, DataFieldKind, FieldMeta, FileStoreId, TableId, save_field_meta};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";
const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-ownership-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A cookie-carrying client addressing one host, echoing CSRF on mutations,
/// able to carry raw bytes for the file endpoints.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: Router, host: &str) -> Client {
        Client {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
        }
    }

    async fn raw(
        &mut self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: Option<Vec<u8>>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
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
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
        }
        if let Some(ct) = content_type {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        let request = builder.body(Body::from(body.unwrap_or_default())).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        for raw in headers.get_all(header::SET_COOKIE) {
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
        (status, headers, bytes.to_vec())
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let payload = body
            .as_ref()
            .map(|b| serde_json::to_vec(b).unwrap())
            .map(|bytes| (Some("application/json"), Some(bytes)))
            .unwrap_or((None, None));
        let (status, _, bytes) = self.raw(method, path, payload.0, payload.1).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// Fetch the CSRF cookie (as a browser loading the SPA would), then log in.
    async fn login(&mut self, email: &str, password: &str) -> StatusCode {
        self.raw("GET", "/", None, None).await;
        let (status, _) = self
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": email, "password": password })),
            )
            .await;
        status
    }

    /// An anonymous client still needs the CSRF cookie before it may mutate.
    async fn prime(&mut self) {
        self.raw("GET", "/", None, None).await;
    }
}

fn write_app_source(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let web = root.join("web");
    std::fs::create_dir_all(web.join("src")).unwrap();
    let script = web.join("build.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         set -e\n\
         test -f src/client.ts\n\
         mkdir -p dist/assets\n\
         printf '<!doctype html><div id=root></div>' > dist/index.html\n\
         cp src/client.ts dist/assets/client.js\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn code_framework() -> FrameworkRef {
    FrameworkRef::new("code")
        .with("store", "apps")
        .with("source", "web")
        .with("output", "web/dist")
        .with("command", "sh build.sh")
        .with("client", "web/src/client.ts")
}

fn blog_app() -> Application {
    Application::new("Blog", "blog", code_framework())
        .with_table(TableId("posts".to_owned()))
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"))
}

/// Bootstrap everything, create the schema (posts owned by email, projects
/// they may point at), build and mount the app **with the JS evaluator**, seed
/// the users, and return the router.
async fn setup(tmp: &TempDir) -> sc_error::Result<(Router, Arc<Catalog>, TestDb)> {
    let db = TestDb::new().await?;
    db.client()
        .await?
        .batch_execute(
            "DO $$ DECLARE r record; BEGIN \
               FOR r IN SELECT table_schema FROM information_schema.tables \
               WHERE table_name = 'users' AND table_type = 'BASE TABLE' LOOP \
                 EXECUTE format('DROP TABLE IF EXISTS %I.users CASCADE', r.table_schema); \
               END LOOP; END $$; \
             CREATE TABLE projects (\
               id bigint generated by default as identity primary key, \
               owner text); \
             CREATE TABLE posts (\
               id bigint generated by default as identity primary key, \
               title text not null, \
               owner text, \
               project bigint references projects(id)); \
             CREATE TABLE comments (\
               id bigint generated by default as identity primary key, \
               post bigint references posts(id), \
               author text)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;
    // The store the file-endpoint test's `File` field points at.
    let uploads_dir = tmp.path().join("uploads-store");
    std::fs::create_dir_all(&uploads_dir).map_err(sc_error::Error::from)?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("uploads", &uploads_dir)?))?;

    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &blog_app(), &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    // The real boot wiring: the registry carries the engine, and the mount is
    // built with it — a formula's reified path has something to run on, and
    // every re-projection (`refresh_table`) keeps it.
    let apps = Arc::new(AppMounts::new(catalog.clone()).with_evaluator(default_js_evaluator()));
    apps.mount(MountedApp::new_with(
        blog_app(),
        framework,
        &catalog,
        apps.evaluator(),
        None,
    )?)?;

    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps,
    )?;

    save_role(&catalog, &Role::new(80, "Member")).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    create_user(&catalog, ALICE, "alice-pw", 80).await?;
    create_user(&catalog, BOB, "bob-pw", 80).await?;
    Ok((router, catalog, db))
}

/// Set the posts table's ownership formula through the admin API — after the
/// mount, so the live re-projection is what every assertion runs against.
/// Floors stay admin-only: the formula is the only path for non-admins.
async fn set_formula(admin: &mut Client, formula: &str) {
    let (status, body) = admin
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": 1,
                "min_role_write": 1,
                "ownership_formula": formula,
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "set formula {formula:?}: {body}");
}

/// The `title`s in a list response, sorted — order-independent row identity.
fn titles(list: &Value) -> Vec<String> {
    let mut out: Vec<String> = list
        .as_array()
        .unwrap_or_else(|| panic!("expected an array, got {list}"))
        .iter()
        .map(|row| row["title"].as_str().unwrap_or_default().to_owned())
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn an_owner_formula_grants_exactly_the_callers_rows() -> sc_error::Result<()> {
    let tmp = TempDir::new("owner");
    let (router, _catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO posts (id, title, owner) VALUES \
             (1, 'alices', 'alice@example.com'), \
             (2, 'bobs', 'bob@example.com'), \
             (3, 'orphaned', NULL); \
             SELECT setval(pg_get_serial_sequence('posts', 'id'), 100)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );

    // Before any formula: the floors are admin-only and there is nothing else.
    let mut alice = Client::new(router.clone(), APP_HOST);
    assert_eq!(alice.login(ALICE, "alice-pw").await, StatusCode::OK);
    let (status, _) = alice.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    set_formula(&mut admin, "owner === user.email").await;

    // --- reads: exactly the caller's rows --------------------------------
    let (status, list) = alice.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&list), vec!["alices"]);

    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login(BOB, "bob-pw").await, StatusCode::OK);
    let (_, list) = bob.send("GET", "/api/posts", None).await;
    assert_eq!(titles(&list), vec!["bobs"]);

    // A caller at the floor is unaffected throughout: the admin sees all.
    let mut app_admin = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        app_admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let (_, list) = app_admin.send("GET", "/api/posts", None).await;
    assert_eq!(titles(&list), vec!["alices", "bobs", "orphaned"]);

    // --- writes: denial is indistinguishable from absence ----------------
    let (status, _) = alice
        .send("PUT", "/api/posts/1", Some(json!({ "title": "alices!" })))
        .await;
    assert_eq!(status, StatusCode::OK, "alice updates her own row");
    let (status, denied) = alice
        .send("PUT", "/api/posts/2", Some(json!({ "title": "stolen" })))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "bob's row does not exist for alice"
    );
    let (status, missing) = alice
        .send("PUT", "/api/posts/999", Some(json!({ "title": "x" })))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Probe-free: a denied row and an absent row answer identically apart from
    // echoing the id the caller themselves named.
    assert_eq!(
        denied["error"]
            .as_str()
            .unwrap()
            .replace("id = 2", "id = <id>"),
        missing["error"]
            .as_str()
            .unwrap()
            .replace("id = 999", "id = <id>"),
        "denial must not be probeable"
    );

    // An update may not move the row out of the caller's ownership.
    let (status, body) = alice
        .send("PUT", "/api/posts/1", Some(json!({ "owner": BOB })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Inserts are checked against the proposed row.
    let (status, _) = alice
        .send(
            "POST",
            "/api/posts",
            Some(json!({ "title": "new", "owner": ALICE })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = alice
        .send(
            "POST",
            "/api/posts",
            Some(json!({ "title": "planted", "owner": BOB })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Deletes follow the same rule.
    let (status, _) = alice.send("DELETE", "/api/posts/2", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = alice.send("DELETE", "/api/posts/1", None).await;
    assert_eq!(status, StatusCode::OK);

    // --- the documented anonymous corner, and its guard ------------------
    // Anonymous `user.email` is null, and `null === null`: the null-owner row
    // is granted. Then `user && …` closes it — both behaviours by design.
    let mut anon = Client::new(router.clone(), APP_HOST);
    anon.prime().await;
    let (status, list) = anon.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&list), vec!["orphaned"]);

    set_formula(&mut admin, "user && owner === user.email").await;
    let (_, list) = anon.send("GET", "/api/posts", None).await;
    assert_eq!(titles(&list), Vec::<String>::new());
    let (_, list) = bob.send("GET", "/api/posts", None).await;
    assert_eq!(
        titles(&list),
        vec!["bobs"],
        "the guard costs logged-in users nothing"
    );
    Ok(())
}

#[tokio::test]
async fn an_operation_split_formula_opens_reads_and_keeps_writes_owned() -> sc_error::Result<()> {
    let tmp = TempDir::new("split");
    let (router, _catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO posts (id, title, owner) VALUES \
             (1, 'alices', 'alice@example.com'), \
             (2, 'bobs', 'bob@example.com')",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    set_formula(&mut admin, "_read || owner === user.email").await;

    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login(BOB, "bob-pw").await, StatusCode::OK);

    // Reads are open to everyone the endpoint admits…
    let (_, list) = bob.send("GET", "/api/posts", None).await;
    assert_eq!(titles(&list), vec!["alices", "bobs"]);

    // …while writes stay owned: bob edits his row, not alice's.
    let (status, _) = bob
        .send("PUT", "/api/posts/2", Some(json!({ "title": "bobs!" })))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = bob
        .send("PUT", "/api/posts/1", Some(json!({ "title": "hijack" })))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn an_aggregation_formula_grants_over_a_child_relation() -> sc_error::Result<()> {
    // A post is reachable if the caller authored a comment on it — an
    // ownership rule over an *incoming* key (Phase 7), enforced through the
    // runtime WHERE injection (the aggregation translates to a correlated
    // subquery). Floors stay admin-only, so the aggregation is the only path.
    let tmp = TempDir::new("agg");
    let (router, _catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO posts (id, title, owner) VALUES \
             (1, 'alice-commented', NULL), (2, 'bob-commented', NULL); \
             SELECT setval(pg_get_serial_sequence('posts', 'id'), 100); \
             INSERT INTO comments (post, author) VALUES \
             (1, 'alice@example.com'), (2, 'bob@example.com')",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let mut alice = Client::new(router.clone(), APP_HOST);
    assert_eq!(alice.login(ALICE, "alice-pw").await, StatusCode::OK);
    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login(BOB, "bob-pw").await, StatusCode::OK);

    set_formula(
        &mut admin,
        "commentsↃpost.some(c => c.author === user.email)",
    )
    .await;

    // Each caller reaches exactly the post they commented on.
    let (status, list) = alice.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&list), vec!["alice-commented"]);
    let (_, list) = bob.send("GET", "/api/posts", None).await;
    assert_eq!(titles(&list), vec!["bob-commented"]);

    // A write is guarded by the same aggregation: alice may edit her post,
    // bob may not (denial indistinguishable from absence).
    let (status, _) = alice
        .send(
            "PUT",
            "/api/posts/1",
            Some(json!({ "title": "alice-commented" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = bob
        .send("PUT", "/api/posts/1", Some(json!({ "title": "hijack" })))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn an_ownership_formula_grants_through_a_calc_field() -> sc_error::Result<()> {
    // A calc field `is_published = project !== null` has no column; the
    // ownership formula `owner === user.email || is_published` inlines its
    // definition (Phase 8), so a post is reachable if the caller owns it *or* it
    // is published — enforced through the runtime WHERE injection.
    let tmp = TempDir::new("calc");
    let (router, catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO projects (id, owner) VALUES (1, 'admin@example.com'); \
             INSERT INTO posts (id, title, owner, project) VALUES \
             (1, 'alice-only', 'alice@example.com', NULL), \
             (2, 'published', 'bob@example.com', 1), \
             (3, 'bob-only', 'bob@example.com', NULL)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    // The calc field is created directly through the catalog (there is no admin
    // calc-field endpoint yet); the ownership formula is set through the API,
    // which re-projects the mount over the now-current catalog.
    save_field_meta(
        &catalog,
        &FieldMeta::new("posts", "is_published").kind(DataFieldKind::Calc {
            expression: "project !== null".into(),
        }),
    )
    .await?;
    catalog.reload().await?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let mut alice = Client::new(router.clone(), APP_HOST);
    assert_eq!(alice.login(ALICE, "alice-pw").await, StatusCode::OK);
    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login(BOB, "bob-pw").await, StatusCode::OK);

    set_formula(&mut admin, "owner === user.email || is_published").await;

    // alice: her own post + the published one.
    let (status, list) = alice.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&list), vec!["alice-only", "published"]);
    // bob: his two posts (one of which is also the published one).
    let (_, list) = bob.send("GET", "/api/posts", None).await;
    assert_eq!(titles(&list), vec!["bob-only", "published"]);

    // The computed calc field also rides out on the read.
    let published = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["title"] == json!("published"))
        .unwrap();
    assert_eq!(published["is_published"], json!(true));
    Ok(())
}

#[tokio::test]
async fn a_join_formula_grants_through_keys_on_both_evaluators() -> sc_error::Result<()> {
    let tmp = TempDir::new("join");
    let (router, _catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO projects (id, owner) VALUES (1, 'alice@example.com'); \
             INSERT INTO posts (id, title, project) VALUES \
             (1, 'in-project', 1), \
             (2, 'unassigned', NULL)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let mut alice = Client::new(router.clone(), APP_HOST);
    assert_eq!(alice.login(ALICE, "alice-pw").await, StatusCode::OK);
    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login(BOB, "bob-pw").await, StatusCode::OK);

    // The same ownership rule, spelled twice: once translatable (the symbolic
    // path — the database filters) and once not (a method call — the reified
    // path in V8). The behaviour must be identical; this is the parity
    // property doing production work.
    for formula in [
        "projectⱵowner === user.email",
        "[projectⱵowner].some(o => o === user.email)",
    ] {
        set_formula(&mut admin, formula).await;

        let (status, list) = alice.send("GET", "/api/posts", None).await;
        assert_eq!(status, StatusCode::OK, "{formula}");
        assert_eq!(titles(&list), vec!["in-project"], "{formula}");
        let (_, list) = bob.send("GET", "/api/posts", None).await;
        assert_eq!(titles(&list), Vec::<String>::new(), "{formula}");

        // Writes go through the join too.
        let (status, _) = alice
            .send(
                "PUT",
                "/api/posts/1",
                Some(json!({ "title": "in-project" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{formula}");
        let (status, _) = bob
            .send("PUT", "/api/posts/1", Some(json!({ "title": "hijack" })))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{formula}");
    }
    Ok(())
}

#[tokio::test]
async fn file_endpoints_apply_the_ownership_rule() -> sc_error::Result<()> {
    let tmp = TempDir::new("files");
    let (router, _catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO posts (id, title, owner) VALUES \
             (1, 'alices', 'alice@example.com')",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );

    // An `attachment` File field, then the ownership formula — both through
    // the admin API against the mounted app.
    let (status, body) = admin
        .send(
            "POST",
            "/api/tables/posts/fields",
            Some(json!({
                "name": "attachment",
                "type": "text",
                "kind": { "type": "file", "store": "uploads", "folder": "docs",
                          "mime_allow": ["text/plain"] }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    set_formula(&mut admin, "owner === user.email").await;

    let mut alice = Client::new(router.clone(), APP_HOST);
    assert_eq!(alice.login(ALICE, "alice-pw").await, StatusCode::OK);
    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login(BOB, "bob-pw").await, StatusCode::OK);

    // Upload: an update of the row, so the row must be the caller's.
    let (status, _, denied) = bob
        .raw(
            "POST",
            "/api/posts/1/attachment/notes.txt",
            Some("text/plain"),
            Some(b"bobs notes".to_vec()),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "{}",
        String::from_utf8_lossy(&denied)
    );
    let (status, _, body) = alice
        .raw(
            "POST",
            "/api/posts/1/attachment/notes.txt",
            Some("text/plain"),
            Some(b"alices notes".to_vec()),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );

    // Download: a read of the row, same rule.
    let (status, _, bytes) = alice
        .raw("GET", "/api/posts/1/attachment", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"alices notes");
    let (status, _, _) = bob.raw("GET", "/api/posts/1/attachment", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let mut anon = Client::new(router.clone(), APP_HOST);
    anon.prime().await;
    let (status, _, _) = anon.raw("GET", "/api/posts/1/attachment", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}
