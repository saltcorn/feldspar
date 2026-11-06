//! Phase 6 end-to-end integration test: the *same* ownership formula enforced by
//! Postgres row-level security instead of the runtime checks (§7.3, §6).
//!
//! Flipping `rls_enabled` swaps the enforcement mechanism, never the outcome, so
//! the core assertions here are §5's — an owner formula grants exactly the
//! caller's rows, a join formula grants through the Key link, denial is
//! indistinguishable from absence — re-run with the database doing the work.
//! What is *new* to RLS and asserted on top:
//!
//! - **fail closed**: a connection that runs a query without setting the caller
//!   GUCs sees no rows, because that is the shape of the generated policy — not
//!   a runtime guard that could be forgotten;
//! - **lifecycle**: turning RLS off restores the runtime path and leaves no
//!   policy debris (`pg_policies` empty for the table); a formula edit while RLS
//!   stays on regenerates the policies live;
//! - **the admin still sees everything** (role 1 clears the policies' role
//!   floor) even though the table is `FORCE`'d.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support::{blog_app, code_framework};
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_types::Role;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_app::{CodeFramework, app_source_from_config, build_application};
use sc_auth::{RoleDef, SessionStore, create_user, save_role};
use sc_catalog::Catalog;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, MountedApp, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator,
};
use sc_test_harness::{TestDb, TestServer};
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
            "sc-server-rls-{}-{tag}-{:?}",
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

    async fn login(&mut self, email: &str, password: &str) -> StatusCode {
        let mut builder = Request::builder()
            .method("GET")
            .uri("/")
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
        let response = self
            .router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str() {
                let pair = text.split(';').next().unwrap_or("");
                if let Some((name, value)) = pair.split_once('=') {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let (status, _) = self
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": email, "password": password })),
            )
            .await;
        status
    }
}

fn write_app_source(root: &Path) {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let web = root.join("web");
    std::fs::create_dir_all(web.join("src")).unwrap();
    let script = web.join("build.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nset -e\ntest -f src/client.ts\nmkdir -p dist/assets\n\
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

async fn setup(tmp: &TempDir) -> sc_error::Result<(Router, Arc<Catalog>, TestDb)> {
    let TestServer { db, catalog, .. } = TestServer::builder()
        .sql(
            "CREATE TABLE projects (id bigint generated by default as identity primary key, \
               owner text); \
             CREATE TABLE posts (id bigint generated by default as identity primary key, \
               title text not null, owner text, project bigint references projects(id), \
               published_at timestamptz, status text)",
        )
        .build()
        .await?;
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &blog_app(), &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
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

    save_role(&catalog, &RoleDef::new(Role::of(80), "Member")).await?;
    create_user(&catalog, "admin@example.com", "admin-pw", Role::ADMIN).await?;
    create_user(&catalog, ALICE, "alice-pw", Role::of(80)).await?;
    create_user(&catalog, BOB, "bob-pw", Role::of(80)).await?;
    Ok((router, catalog, db))
}

/// Set posts' ownership formula and RLS flag through the admin API (which drives
/// `enable_rls`/`disable_rls`), floors staying admin-only.
async fn set_rls(admin: &mut Client, formula: &str, rls: bool) {
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
                "rls_enabled": rls,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "set rls {formula:?}: {body}");
}

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
async fn rls_enforces_the_same_owner_rule_as_the_runtime_path() -> sc_error::Result<()> {
    let tmp = TempDir::new("owner");
    let (router, _catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO posts (id, title, owner) VALUES \
             (1, 'alices', 'alice@example.com'), (2, 'bobs', 'bob@example.com'); \
             SELECT setval(pg_get_serial_sequence('posts', 'id'), 100)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    set_rls(&mut admin, "owner === user.email", true).await;

    // Reads: exactly the caller's rows — enforced by the SELECT policy.
    let mut alice = Client::new(router.clone(), APP_HOST);
    assert_eq!(alice.login(ALICE, "alice-pw").await, StatusCode::OK);
    let (status, list) = alice.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&list), vec!["alices"]);

    let mut bob = Client::new(router.clone(), APP_HOST);
    assert_eq!(bob.login(BOB, "bob-pw").await, StatusCode::OK);
    let (_, list) = bob.send("GET", "/api/posts", None).await;
    assert_eq!(titles(&list), vec!["bobs"]);

    // The admin sees everything even though the table is FORCE'd: role 1 clears
    // the policies' role floor. (Through the admin API's own row viewer.)
    let (_, all) = admin.send("GET", "/api/tables/posts/rows", None).await;
    assert_eq!(titles(&all), vec!["alices", "bobs"]);

    // Writes: owned, with denial indistinguishable from absence (the WITH CHECK
    // / USING policies, and the 42501 → not-found mapping).
    let (status, _) = alice
        .send("PUT", "/api/posts/1", Some(json!({ "title": "alices!" })))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = alice
        .send("PUT", "/api/posts/2", Some(json!({ "title": "stolen" })))
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "bob's row is invisible to alice"
    );
    // An insert outside ownership is refused by the INSERT policy's WITH CHECK.
    let (status, _) = alice
        .send(
            "POST",
            "/api/posts",
            Some(json!({ "title": "planted", "owner": BOB })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = alice
        .send(
            "POST",
            "/api/posts",
            Some(json!({ "title": "mine", "owner": ALICE })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Delete follows the USING policy.
    let (status, _) = alice.send("DELETE", "/api/posts/2", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    Ok(())
}

/// TODO-type-hardening H7.2: time in a formula means the same under RLS (the
/// policy reads the `sc.now` GUC that `set_caller_context` sets) as on the
/// runtime path (the statement's instant, bound): an embargo on reads and a
/// 15-minute edit window on updates.
#[tokio::test]
async fn an_embargo_and_an_edit_window_hold_with_rls_and_without() -> sc_error::Result<()> {
    let tmp = TempDir::new("time");
    let (router, _catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO posts (id, title, owner, published_at) VALUES \
             (1, 'published', 'bob@example.com', now() - interval '1 hour'), \
             (2, 'embargoed', 'bob@example.com', now() + interval '1 hour'), \
             (3, 'fresh', 'alice@example.com', now() - interval '5 minutes'), \
             (4, 'stale', 'alice@example.com', now() - interval '20 minutes'); \
             SELECT setval(pg_get_serial_sequence('posts', 'id'), 100)",
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
    let formula = "_read && published_at <= new Date() \
                   || _update && owner === user.email \
                      && published_at.getTime() > Date.now() - 15*60e3";
    for rls in [true, false] {
        set_rls(&mut admin, formula, rls).await;
        let (status, list) = alice.send("GET", "/api/posts", None).await;
        assert_eq!(status, StatusCode::OK, "rls={rls}: {list}");
        assert_eq!(
            titles(&list),
            vec!["fresh", "published", "stale"],
            "rls={rls}: the embargoed row is not yet readable"
        );
        let (status, body) = alice
            .send("PUT", "/api/posts/3", Some(json!({ "title": "fresh" })))
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "rls={rls}: inside the window: {body}"
        );
        let (status, _) = alice
            .send("PUT", "/api/posts/4", Some(json!({ "title": "edited" })))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "rls={rls}: past the window");
    }
    Ok(())
}

/// The number of `old` triggers on `posts` (TODO-type-hardening H7.4).
async fn old_triggers(db: &TestDb) -> i64 {
    db.client()
        .await
        .unwrap()
        .query_one(
            "SELECT count(*) FROM pg_trigger \
             WHERE tgrelid = 'posts'::regclass AND tgname = 'sc_owner_update_old'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

/// TODO-type-hardening H7.4: with RLS on, a formula that reads `old` is
/// enforced by Postgres — the `UPDATE` policy checks the stored row, and a
/// generated `BEFORE UPDATE` trigger the proposed row against `OLD` — with the
/// verdicts the server's own check gives with RLS off.
#[tokio::test]
async fn old_is_enforced_by_a_trigger_under_rls() -> sc_error::Result<()> {
    let tmp = TempDir::new("old");
    let (router, _catalog, db) = setup(&tmp).await?;
    let reset = || async {
        db.client()
            .await
            .unwrap()
            .batch_execute(
                "SELECT set_config('sc.role', '1', false); \
                 DELETE FROM posts; INSERT INTO posts (id, title, owner, status) VALUES \
                 (1, 'report', 'alice@example.com', 'draft'), \
                 (2, 'memo', 'alice@example.com', 'submitted'), \
                 (3, 'bobs', 'bob@example.com', 'draft')",
            )
            .await
            .unwrap();
    };
    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let mut alice = Client::new(router.clone(), APP_HOST);
    assert_eq!(alice.login(ALICE, "alice-pw").await, StatusCode::OK);
    // The string literal is a dollar-quote tag: it must not end the trigger
    // function's text.
    let formula = "owner === user.email && (_read || _update && title === old.title \
                   && status !== \"$fd$\" \
                   && (status === old.status || old.status === \"draft\" && status === \"submitted\"))";
    for rls in [true, false] {
        reset().await;
        set_rls(&mut admin, formula, rls).await;
        assert_eq!(old_triggers(&db).await, i64::from(rls), "rls={rls}");
        // Refused: RLS reports it as the not-found a policy violation is, the
        // server's own check as a 403.
        let refused = if rls {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::FORBIDDEN
        };
        let (status, body) = alice
            .send("PUT", "/api/posts/2", Some(json!({ "status": "draft" })))
            .await;
        assert_eq!(status, refused, "rls={rls}: submitted → draft: {body}");
        let (status, _) = alice
            .send("PUT", "/api/posts/1", Some(json!({ "title": "renamed" })))
            .await;
        assert_eq!(status, refused, "rls={rls}: the title is frozen");
        let (status, body) = alice
            .send(
                "PUT",
                "/api/posts/1",
                Some(json!({ "status": "submitted" })),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "rls={rls}: draft → submitted: {body}"
        );
        // Somebody else's row fails the existing-row check, `old` or not.
        let (status, _) = alice
            .send(
                "PUT",
                "/api/posts/3",
                Some(json!({ "status": "submitted" })),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "rls={rls}: bob's row");
        let (_, list) = alice.send("GET", "/api/posts", None).await;
        assert_eq!(titles(&list), vec!["memo", "report"], "rls={rls}");
    }
    // A formula without `old` takes the trigger away again.
    set_rls(&mut admin, formula, true).await;
    assert_eq!(old_triggers(&db).await, 1);
    set_rls(&mut admin, "owner === user.email", true).await;
    assert_eq!(old_triggers(&db).await, 0);
    let functions: i64 = db
        .client()
        .await?
        .query_one(
            "SELECT count(*) FROM pg_proc WHERE proname = '_fd_owner_old_posts'",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?
        .get(0);
    assert_eq!(functions, 0, "the function goes with the trigger");
    Ok(())
}

#[tokio::test]
async fn a_forgotten_caller_context_sees_no_rows() -> sc_error::Result<()> {
    let tmp = TempDir::new("failclosed");
    let (router, catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO posts (id, title, owner) VALUES \
             (1, 'alices', 'alice@example.com'), (2, 'bobs', 'bob@example.com')",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    set_rls(&mut admin, "owner === user.email", true).await;

    // A query on a pooled connection with **no** GUCs set — exactly what a code
    // path that forgot the caller context would do. The server connects as the
    // table owner, so only `FORCE ROW LEVEL SECURITY` makes RLS apply here at
    // all; with it, the role and user settings read NULL and every policy
    // denies. Fail closed is a property of the policy, not a guard.
    let rows: Vec<sc_db::Row> = catalog
        .primary(sc_catalog::Unchecked::system("test"))
        .query(&sc_query::Statement::Select(Box::new(
            sc_query::Select::from(sc_query::Source::table("posts"))
                .columns(vec![sc_query::Projection::all()]),
        )))
        .await?
        .try_collect()
        .await?;
    assert!(
        rows.is_empty(),
        "a query with no caller context must see no rows under RLS, saw {}",
        rows.len()
    );

    Ok(())
}

#[tokio::test]
async fn toggling_rls_off_restores_the_runtime_path_and_leaves_no_policies() -> sc_error::Result<()>
{
    let tmp = TempDir::new("lifecycle");
    let (router, _catalog, db) = setup(&tmp).await?;
    db.client()
        .await?
        .batch_execute(
            "INSERT INTO projects (id, owner) VALUES (1, 'alice@example.com'); \
             INSERT INTO posts (id, title, owner, project) VALUES \
             (1, 'alices', 'alice@example.com', 1), (2, 'bobs', 'bob@example.com', NULL)",
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

    // Enable, and see the policies exist.
    set_rls(&mut admin, "owner === user.email", true).await;
    assert_eq!(policy_count(&db, "posts").await, 4);
    let (_, list) = alice.send("GET", "/api/posts", None).await;
    assert_eq!(titles(&list), vec!["alices"]);

    // A formula edit while RLS stays on regenerates the policies live — the
    // rule now grants through the project join instead of the owner column.
    set_rls(&mut admin, "projectⱵowner === user.email", true).await;
    assert_eq!(policy_count(&db, "posts").await, 4);
    let (_, list) = alice.send("GET", "/api/posts", None).await;
    assert_eq!(
        titles(&list),
        vec!["alices"],
        "still alice's, now via the join"
    );

    // Turn RLS off: the runtime path is back (same verdict), and no policy
    // debris remains on the table.
    set_rls(&mut admin, "projectⱵowner === user.email", false).await;
    assert_eq!(
        policy_count(&db, "posts").await,
        0,
        "no policy debris after disable"
    );
    let (status, list) = alice.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        titles(&list),
        vec!["alices"],
        "runtime enforcement restored"
    );

    Ok(())
}

/// The number of RLS policies on a table, straight from `pg_policies`.
async fn policy_count(db: &TestDb, table: &str) -> i64 {
    let client = db.client().await.unwrap();
    let row = client
        .query_one(
            "SELECT count(*) FROM pg_policies WHERE tablename = $1",
            &[&table],
        )
        .await
        .unwrap();
    row.get(0)
}
