//! The raw-route registry (security review 2026-11 §8, E4;
//! TODO-type-hardening H6.1).
//!
//! The typed endpoints declare who may call them in their type; the routes
//! outside them are listed in `sc_server::raw_routes()`, each with how it
//! authenticates. Three checks:
//!
//! - the list is the pinned one below, so a new raw route fails here until it
//!   says how it authenticates;
//! - no route reaches the router except through that list (a source scan);
//! - against a real server, every route that needs a caller refuses an
//!   anonymous request, and every route answers only the methods it lists.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use sc_api::auth::{CSRF_COOKIE, CSRF_HEADER};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, SecretKey};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, RouteAuth, ServerConfig, admin_handlers, build_router_with_apps, raw_routes,
};
use sc_test_harness::TestDb;
use tower::ServiceExt;

use RouteAuth::*;

/// Every route outside the typed endpoint set, with its methods and how it
/// authenticates. Adding a raw route means adding it here, which is the point:
/// a route that names no authentication does not get past this list.
const EXPECTED: &[(&str, &[&str], RouteAuth)] = &[
    ("/health", &["GET"], Public),
    ("/upload/{store}/{*path}", &["POST"], AdminSession),
    ("/download/{store}/{*path}", &["GET"], AdminSession),
    ("/backup/create", &["POST"], AdminSession),
    ("/backup/upload", &["POST"], AdminSession),
    ("/ide/lsp/{store}", &["GET"], AdminSession),
    ("/ide/terminal/{store}", &["GET"], AdminSession),
    ("/admin/agent-chat", &["GET"], AdminSession),
    ("/api/streams/{id}/observe", &["GET"], AdminSession),
    ("/api/model-instances/{id}/progress", &["GET"], AdminSession),
    ("/mcp", &["POST"], McpToken),
    ("/ide/mcp-proxy/{server}", &["*"], TerminalToken),
    ("/build-agent/heartbeat", &["POST"], BuildHostToken),
    ("/build-agent/claim", &["POST"], BuildHostToken),
    ("/build-agent/jobs/{id}/project", &["GET"], BuildHostToken),
    ("/build-agent/jobs/{id}/log", &["POST"], BuildHostToken),
    ("/build-agent/jobs/{id}/artifact", &["POST"], BuildHostToken),
    ("/build-agent/jobs/{id}/failed", &["POST"], BuildHostToken),
    ("/_feldspar/admin-handoff", &["GET"], HandoffToken),
    ("/_feldspar/oauth/providers", &["GET"], Public),
    ("/_feldspar/oauth/{provider}/start", &["GET"], Public),
    ("/_feldspar/oauth/callback", &["GET"], OAuthFlow),
    ("/_feldspar/oauth/complete", &["GET"], OAuthFlow),
    ("/_feldspar/second-factor", &["GET"], OwnSession),
    (
        "/_feldspar/second-factor/setup",
        &["POST"],
        SessionOrChallenge,
    ),
    (
        "/_feldspar/second-factor/enable",
        &["POST"],
        SessionOrChallenge,
    ),
    ("/_feldspar/second-factor/verify", &["POST"], Challenge),
    (
        "/_feldspar/second-factor/disable",
        &["POST"],
        SessionOrChallenge,
    ),
    (
        "/_feldspar/second-factor/recovery-codes",
        &["POST"],
        SessionOrChallenge,
    ),
    ("/_feldspar/second-factor/sign-in", &["GET"], Challenge),
    (
        "/_feldspar/hooks/{module}/{name}",
        &["GET", "POST"],
        ModuleVerifies,
    ),
    (
        "/_feldspar/hooks/{module}/{name}/{*rest}",
        &["GET", "POST"],
        ModuleVerifies,
    ),
    (
        "/_feldspar/routes/{module}/{*path}",
        &["GET", "POST"],
        ScopedSession,
    ),
    ("/_feldspar/oauth/consent/{id}", &["GET"], HostSession),
];

#[test]
fn every_raw_route_is_registered_with_how_it_authenticates() {
    let registered: BTreeSet<(&str, Vec<&str>, String)> = raw_routes()
        .map(|(path, methods, auth)| (path, methods.to_vec(), format!("{auth:?}")))
        .collect();
    let expected: BTreeSet<(&str, Vec<&str>, String)> = EXPECTED
        .iter()
        .map(|(path, methods, auth)| (*path, methods.to_vec(), format!("{auth:?}")))
        .collect();
    let unlisted: Vec<_> = registered.difference(&expected).collect();
    let missing: Vec<_> = expected.difference(&registered).collect();
    assert!(
        unlisted.is_empty() && missing.is_empty(),
        "raw routes not in this test's list (say how each authenticates): {unlisted:?}; \
         listed here but not registered: {missing:?}"
    );
    assert_eq!(
        raw_routes().count(),
        EXPECTED.len(),
        "a path registered twice"
    );
}

/// The rust files under `dir`.
fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// A route added to the axum router anywhere but `raw_routes::mount` would be
/// a route no list knows of. So the source says `.route(` with a path in one
/// place only (tests excepted: they build routers of their own).
#[test]
fn no_route_is_mounted_outside_the_registry() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut offenders = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        // The code before the file's tests.
        let code = text.split("#[cfg(test)]").next().unwrap_or("");
        for call in [".route(", ".route_service(", ".nest(", ".nest_service("] {
            for (at, _) in code.match_indices(call) {
                // A path is a literal or a `…_ROUTE` constant; the registry's
                // own call passes `raw.path`, and `ModuleRoutes::route` takes
                // a module name.
                let rest = code[at + call.len()..].trim_start();
                let is_path = rest.starts_with('"')
                    || rest.starts_with("crate::")
                    || rest.starts_with(|c: char| c.is_ascii_uppercase());
                if is_path {
                    let line = code[..at].lines().count();
                    offenders.push(format!("{}:{line}", file.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "routes mounted outside `raw_routes::RAW_ROUTES`: {offenders:#?}"
    );
}

/// `pattern` with every parameter filled in.
fn concrete(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|segment| match segment.strip_prefix('{') {
            Some(rest) if rest.starts_with('*') => "a/b".to_owned(),
            Some(_) => "x1".to_owned(),
            None => segment.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// An anonymous request: no session, no bearer token, but a CSRF token pair,
/// so what answers is the route rather than the CSRF check.
fn anonymous(method: &Method, path: &str) -> Request<Body> {
    Request::builder()
        .method(method.clone())
        .uri(path)
        .header("cookie", format!("{CSRF_COOKIE}=raw-route-test"))
        .header(CSRF_HEADER, "raw-route-test")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

#[tokio::test]
async fn a_route_that_needs_a_caller_refuses_an_anonymous_one() -> sc_error::Result<()> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    catalog.set_secret_key(SecretKey::generate()?);
    sc_auth::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_config::bootstrap_config(&catalog).await?;
    // The MCP server is off by default, and then `/mcp` is a 404 for
    // everybody; on, and open to remote clients, so what answers an
    // anonymous request is the bearer-token check itself.
    sc_config::set_config(&catalog, sc_config::MCP_ENABLED, serde_json::json!(true)).await?;
    sc_config::set_config(
        &catalog,
        sc_config::MCP_LOOPBACK_ONLY,
        serde_json::json!(false),
    )
    .await?;
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;

    let all = [
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
    ];
    let mut wrong = Vec::new();
    for (pattern, methods, auth) in raw_routes() {
        let path = concrete(pattern);
        for method in &all {
            let declared = methods.contains(&"*") || methods.contains(&method.as_str());
            let response = router
                .clone()
                .oneshot(anonymous(method, &path))
                .await
                .unwrap();
            let status = response.status();
            if !declared {
                // Only the methods it lists: anything else is the method
                // router's 405, before any handler.
                if status != StatusCode::METHOD_NOT_ALLOWED {
                    wrong.push(format!("{method} {path}: {status}, not 405 ({auth:?})"));
                }
            } else if auth.refuses_anonymous()
                && !matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
            {
                wrong.push(format!(
                    "{method} {path}: {status} for an anonymous caller ({auth:?})"
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    Ok(())
}
