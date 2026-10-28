//! Role grants end to end (TODO-payments P3.1, P3.2): a grant changes what a
//! signed-in user can read through an application's REST API, and an admin
//! manages grants from the users screen's endpoints.
//!
//! - **A grant lands within the session freshness TTL**, with no fresh sign-in:
//!   the session reads the user at their effective role on its next miss.
//! - **An expired grant ends access with no write** — the row is as it was, and
//!   the session notices at the expiry.
//! - **Formulas and RLS see the effective role**: `user.role` in an ownership
//!   formula, and the `sc.user`/`sc.role` settings a policy reads.
//! - The admin endpoints list the base and effective roles and the grants, add
//!   a time-limited one by hand, and revoke it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use sc_app::{
    ApiConfig, Application, CodeFramework, FrameworkRef, app_source_from_config, build_application,
};
use sc_auth::{
    CACHE_CAPACITY, NewRoleGrant, ROLE_ADMIN, Role, SessionStore, create_user, find_role_grant,
    grant_role, save_role,
};
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// How long a resolved session is believed.
const CACHE_TTL_MS: i64 = 200;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-grants-{}-{tag}-{:?}",
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

/// A cookie-carrying client addressing one host, echoing CSRF on mutations.
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
        body: Option<Value>,
    ) -> (StatusCode, Vec<u8>) {
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
        (status, bytes.to_vec())
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, bytes) = self.raw(method, path, body).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// Load the SPA (a safe GET) to obtain the CSRF cookie, then log in — the
    /// order a browser follows before it may mutate anything.
    async fn login(&mut self, email: &str, password: &str) -> StatusCode {
        self.raw("GET", "/", None).await;
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

/// The app's source tree: a git repo whose stand-in bundler emits an SPA.
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

/// Build and mount the app, bootstrap every platform table, seed a first admin,
/// and return a router serving both the admin (base domain) and the app.
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
             CREATE TABLE posts (\
               id bigint generated by default as identity primary key, \
               title text not null); \
             INSERT INTO posts (title) VALUES ('members only')",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    // The overlay table has to exist for the admin to configure through it, just
    // as `connect_catalog` bootstraps it on the real boot path.
    sc_catalog::bootstrap_table_meta(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;

    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &blog_app(), &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    apps.mount(MountedApp::new(blog_app(), framework, &catalog)?)?;

    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        // The database store, as a real server runs, with a short freshness TTL
        // so a grant made between two requests lands within the test.
        Arc::new(SessionStore::database_with(
            catalog.clone(),
            chrono::Duration::hours(24),
            chrono::Duration::milliseconds(CACHE_TTL_MS),
            CACHE_CAPACITY,
        )),
        &config,
        apps,
    )?;
    Ok((router, catalog, db))
}

/// Wait out the session cache's freshness TTL.
async fn past_the_ttl() {
    tokio::time::sleep(std::time::Duration::from_millis(CACHE_TTL_MS as u64 + 150)).await;
}

/// Set `posts`' access through the admin API.
async fn set_access(admin: &mut Client, min_role_read: u8, formula: &str, rls: bool) {
    let (status, body) = admin
        .send(
            "PUT",
            "/api/tables/posts",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": min_role_read,
                "min_role_write": 1,
                "ownership_formula": formula,
                "rls_enabled": rls,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn people(catalog: &Catalog) -> sc_error::Result<uuid::Uuid> {
    create_user(catalog, "admin@example.com", "admin-pw", ROLE_ADMIN).await?;
    save_role(catalog, &Role::new(60, "Pro")).await?;
    save_role(catalog, &Role::new(80, "Member")).await?;
    Ok(create_user(catalog, "reader@example.com", "reader-pw", 80)
        .await?
        .id)
}

#[tokio::test]
async fn a_grant_opens_a_table_within_the_ttl_and_its_end_closes_it_with_no_write()
-> sc_error::Result<()> {
    let tmp = TempDir::new("floor");
    let (router, catalog, _db) = setup(&tmp).await?;
    let reader_id = people(&catalog).await?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    set_access(&mut admin, 60, "", false).await;

    let mut reader = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        reader.login("reader@example.com", "reader-pw").await,
        StatusCode::OK
    );
    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "role 80 is below the floor of 60"
    );

    // The admin grants role 60 for a week, by hand.
    let path = format!("/api/users/{reader_id}/role-grants");
    let (status, grant) = admin
        .send(
            "POST",
            &path,
            Some(json!({ "role": 60, "expires_in_days": 7, "note": "comp" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    assert_eq!(grant["source"], json!("admin"));
    assert_eq!(grant["in_force"], json!(true));
    assert_eq!(grant["note"], json!("comp"));

    // No new sign-in: the session reads the user again once its entry is stale.
    past_the_ttl().await;
    let (status, rows) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK, "the grant reached the session");
    assert_eq!(rows.as_array().map(Vec::len), Some(1));

    let (status, listed) = admin.send("GET", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["base_role"], json!(80), "users.role is untouched");
    assert_eq!(listed["effective_role"], json!(60));
    assert_eq!(listed["grants"].as_array().map(Vec::len), Some(1));

    // Revoked by the admin: closed again, and the grant is kept, revoked.
    let id = grant["id"].as_str().unwrap();
    let (status, revoked) = admin
        .send("POST", &format!("{path}/{id}/revoke"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["in_force"], json!(false));
    assert!(revoked["revoked_at"].is_string());
    past_the_ttl().await;
    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A grant about to end, as a subscription's period does.
    grant_role(
        &catalog,
        NewRoleGrant {
            user_id: reader_id,
            role: 60,
            source: "test".into(),
            source_ref: "sub_1".into(),
            starts_at: None,
            expires_at: Some(Utc::now() + chrono::Duration::milliseconds(1200)),
            note: None,
        },
    )
    .await?;
    let before = find_role_grant(&catalog, "test", "sub_1").await?;
    past_the_ttl().await;
    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let (status, _) = reader.send("GET", "/api/posts", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "access ended at the expiry");
    assert_eq!(
        find_role_grant(&catalog, "test", "sub_1").await?,
        before,
        "and nothing was written to end it"
    );

    // Somebody else's grant is not this user's to revoke through their path.
    let other = create_user(&catalog, "other@example.com", "pw", 80).await?;
    let (status, _) = admin
        .send(
            "POST",
            &format!("/api/users/{}/role-grants/{id}/revoke", other.id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // And a role nobody defined is refused by name.
    let (status, body) = admin.send("POST", &path, Some(json!({ "role": 55 }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    Ok(())
}

#[tokio::test]
async fn formulas_and_row_level_security_see_the_effective_role() -> sc_error::Result<()> {
    let tmp = TempDir::new("rls");
    let (router, catalog, _db) = setup(&tmp).await?;
    let reader_id = people(&catalog).await?;

    let mut admin = Client::new(router.clone(), BASE_DOMAIN);
    assert_eq!(
        admin.login("admin@example.com", "admin-pw").await,
        StatusCode::OK
    );
    let mut reader = Client::new(router.clone(), APP_HOST);
    assert_eq!(
        reader.login("reader@example.com", "reader-pw").await,
        StatusCode::OK
    );

    let grant = |source_ref: &str| NewRoleGrant {
        user_id: reader_id,
        role: 60,
        source: "test".into(),
        source_ref: source_ref.into(),
        starts_at: None,
        expires_at: None,
        note: None,
    };
    for rls in [false, true] {
        set_access(&mut admin, 1, "_read && user.role <= 60", rls).await;
        let (status, rows) = reader.send("GET", "/api/posts", None).await;
        assert_eq!(status, StatusCode::OK, "rls {rls}: {rows}");
        assert_eq!(rows, json!([]), "rls {rls}: role 80 reads nothing");

        grant_role(&catalog, grant("pro")).await?;
        past_the_ttl().await;
        let (status, rows) = reader.send("GET", "/api/posts", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            rows.as_array().map(Vec::len),
            Some(1),
            "rls {rls}: `user.role` is the effective role"
        );
        sc_auth::revoke_role_grant(&catalog, "test", "pro").await?;
        past_the_ttl().await;
    }
    Ok(())
}
