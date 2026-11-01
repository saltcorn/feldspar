//! The builder's routes (TODO "The builder" 8.6): v1's layout builder served by
//! the admin server, over HTTP, against a real Postgres and the restored BooksDB
//! backup.
//!
//! The claims:
//!
//! - **The document.** Each of the four view patterns the builder builds (Show,
//!   Edit, List, Filter) and a page open as v1's builder page: its container, its
//!   form and its header-actions element, v1's page scripts in v1's order, then
//!   CKEditor and the bundle. What the bundle needs is JSON boot data: the
//!   worker's options, the stored layout, the mode, the target and where a save
//!   goes. There is no inline script. Every asset it names is served under the
//!   builder's policy.
//! - **The policy.** The document gets `BUILDER_CONTENT_SECURITY_POLICY` with the
//!   application's origin in `img-src`, the assets the constant itself, and the
//!   refusals too. `the_builder_policy_is_what_the_bundle_needs` holds the facts
//!   each absent relaxation rests on.
//! - **The refusals** are 404s naming what failed: the application id, the
//!   application, the view, the page, the step (a number, in range, a layout),
//!   and a page that is an HTML file. The refused *mode* is a unit test in
//!   `src/builder.rs`, because no pattern this server has answers another mode.
//! - **The canvas's files** on the admin origin are redirected to the
//!   application's, for the builder's own documents only.
//! - **Admin only**, and a server **without the bundle** says so.

use sc_api::auth::CSRF_COOKIE;
use sc_types::Role;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::EndpointSet;
use sc_auth::{SessionStore, User};
use sc_server::{
    BUILDER_BOOT_ID, BUILDER_CONTENT_SECURITY_POLICY, HandlerRegistry, SESSION_COOKIE,
    ServerConfig, build_router, builder_content_security_policy,
};
use sc_viewpattern::Page;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use crate::saltcorn_ui_render::{Answer, Client, booksdb, bundle_dir, setup, visitor};

/// The admin server's host; the application is `booksdb.` under it.
const HOST: &str = "example.com";
const APP_ORIGIN: &str = "http://booksdb.example.com";

/// The built builder bundle, if there is one.
pub(crate) fn builder_bundle_dir() -> Option<PathBuf> {
    let dir = ui_builder().join("dist");
    dir.join("builder.js").is_file().then_some(dir)
}

fn ui_builder() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/builder")
}

/// A browser navigation to `path` on the admin server.
async fn navigate(client: &mut Client, path: &str) -> Answer {
    client
        .request("GET", Some(HOST), path, &[("accept", "text/html")], None)
        .await
}

/// The document's boot data.
fn boot_of(html: &str) -> Value {
    let open = format!("<script type=\"application/json\" id=\"{BUILDER_BOOT_ID}\">");
    let start = html.find(&open).expect("the boot data") + open.len();
    let end = start + html[start..].find("</script>").expect("its end");
    serde_json::from_str(&html[start..end]).expect("the boot data is JSON")
}

/// Every `<script>`'s `src`, in order. A script without one must be the JSON
/// boot data: an inline script would need `'unsafe-inline'`.
fn scripts_of(html: &str) -> Vec<String> {
    html.split("<script")
        .skip(1)
        .filter_map(|tag| {
            let tag = &tag[..tag.find('>').expect("a script tag")];
            match tag.split_once("src=\"") {
                Some((_, rest)) => Some(rest[..rest.find('"').unwrap()].to_owned()),
                None => {
                    assert!(
                        tag.contains("type=\"application/json\""),
                        "an inline script: <script{tag}>"
                    );
                    None
                }
            }
        })
        .collect()
}

fn stylesheets_of(html: &str) -> Vec<String> {
    html.split("<link rel=\"stylesheet\" href=\"")
        .skip(1)
        .map(|rest| rest[..rest.find('"').unwrap()].to_owned())
        .collect()
}

/// What every builder document has, whatever it builds.
async fn assert_builder_document(client: &mut Client, label: &str, html: &str) {
    for id in ["saltcorn-builder", "scbuildform", "builder-header-actions"] {
        assert!(html.contains(&format!("id=\"{id}\"")), "{label}: #{id}");
    }
    let scripts = scripts_of(html);
    let files: Vec<&str> = scripts
        .iter()
        .map(|s| s.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        files,
        [
            "jquery-3.6.0.min.js",
            "bootstrap.bundle.min.js",
            "saltcorn-common.js",
            "saltcorn.js",
            "ckeditor.js",
            "builder.js"
        ],
        "{label}"
    );
    assert!(
        html.contains(&format!(
            "<script type=\"module\" src=\"{}\"></script>",
            scripts[5]
        )),
        "{label}: the bundle is a module script"
    );
    for asset in scripts.iter().chain(&stylesheets_of(html)) {
        let answer = client.request("GET", Some(HOST), asset, &[], None).await;
        assert_eq!(answer.status, StatusCode::OK, "{label}: {asset}");
        assert_eq!(
            answer.headers[header::CONTENT_SECURITY_POLICY],
            BUILDER_CONTENT_SECURITY_POLICY,
            "{label}: {asset}"
        );
        assert!(
            answer.headers[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("immutable"),
            "{label}: {asset} is under a versioned prefix"
        );
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn the_builder_opens_each_pattern_and_a_page_and_refuses_the_rest() -> sc_error::Result<()> {
    let (Some(bundle), Some(_)) = (bundle_dir(), builder_bundle_dir()) else {
        eprintln!("skipping: the Saltcorn UI and builder bundles are not built");
        return Ok(());
    };
    let mut server = setup("builder-route", bundle).await?;
    let catalog = server.apps.catalog().expect("a catalog").clone();
    let app = booksdb(&catalog).await;
    let id = app.id.0.to_string();
    let csrf = server.client.cookies[CSRF_COOKIE].clone();
    let document_csp = builder_content_security_policy(Some(APP_ORIGIN));
    assert!(document_csp.contains(&format!("img-src 'self' data: {APP_ORIGIN};")));

    // --- the four patterns ---------------------------------------------------
    for (name, mode) in [
        ("Show Books", "show"),
        ("Edit Books", "edit"),
        ("List Books", "list"),
        ("Filter books", "filter"),
    ] {
        let view = sc_viewpattern::load_view(&catalog, app.id, name)
            .await?
            .expect("the import has it");
        let encoded = name.replace(' ', "%20");
        let answer = navigate(
            &mut server.client,
            &format!("/builder/applications/{id}/views/{encoded}"),
        )
        .await;
        assert_eq!(answer.status, StatusCode::OK, "{name}: {}", answer.body);
        assert_eq!(
            answer.headers[header::CONTENT_SECURITY_POLICY],
            document_csp.as_str(),
            "{name}"
        );
        assert_eq!(answer.headers[header::REFERRER_POLICY], "same-origin");

        let boot = boot_of(&answer.body);
        assert_eq!(boot["mode"], mode, "{name}");
        assert_eq!(
            boot["options"]["mode"], mode,
            "{name}: the worker's options"
        );
        assert_eq!(
            boot["options"]["view_id"],
            json!(view.id.0.to_string()),
            "{name}"
        );
        assert_eq!(
            boot["target"],
            json!({ "kind": "view", "name": name, "step": 0 })
        );
        assert_eq!(boot["application"], json!(id));
        assert_eq!(boot["applicationName"], json!(app.name));
        assert_eq!(boot["applicationOrigin"], APP_ORIGIN);
        assert_eq!(boot["csrfToken"], json!(csrf));
        assert_eq!(
            boot["layout"],
            view.configuration
                .get("layout")
                .cloned()
                .unwrap_or(Value::Null),
            "{name}: the stored layout"
        );
        let count = boot["stepCount"].as_u64().expect("a step count");
        let view_url = format!("/#/applications/{id}/views/{encoded}");
        let after_save = if count > 1 {
            format!("{view_url}?step=1")
        } else {
            format!("/#/applications/{id}/views")
        };
        assert_eq!(boot["afterSave"], json!(after_save), "{name}");
        assert!(
            answer.body.contains(&format!(
                "href=\"{view_url}?step=0\">Back to configuration</a>"
            )),
            "{name}"
        );
        assert!(
            answer.body.contains(&format!("of {count} (")),
            "{name}: step 1 of {count}"
        );
        assert_builder_document(&mut server.client, name, &answer.body).await;
    }

    // --- a page --------------------------------------------------------------
    let page = sc_viewpattern::load_page(&catalog, app.id, "BooksOverview")
        .await?
        .expect("the import has it");
    let answer = navigate(
        &mut server.client,
        &format!("/builder/applications/{id}/pages/BooksOverview"),
    )
    .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(
        answer.headers[header::CONTENT_SECURITY_POLICY],
        document_csp.as_str()
    );
    let boot = boot_of(&answer.body);
    assert_eq!(boot["mode"], "page");
    assert_eq!(boot["options"]["page_id"], json!(page.id.0.to_string()));
    assert_eq!(
        boot["target"],
        json!({ "kind": "page", "name": "BooksOverview" })
    );
    assert_eq!(boot["layout"], page.layout);
    assert_eq!(
        boot["afterSave"],
        json!(format!("/#/applications/{id}/pages"))
    );
    assert!(answer.body.contains(&format!(
        "href=\"/#/applications/{id}/pages\">Back to pages</a>"
    )));
    assert!(answer.body.contains("Page properties</a>"));
    assert_builder_document(&mut server.client, "BooksOverview", &answer.body).await;

    // --- the refusals ----------------------------------------------------------
    sc_viewpattern::view_sets()
        .save_page(
            &catalog,
            &Page::new(app.id, "Handmade").layout(json!({ "html_file": "handmade.html" })),
        )
        .await?;
    let view_path = format!("/builder/applications/{id}/views/Show%20Books");
    for (path, sentence) in [
        (
            "/builder/applications/not-an-id/views/Show%20Books".to_owned(),
            "`not-an-id` is not an application id".to_owned(),
        ),
        (
            format!("/builder/applications/{}/views/Show%20Books", Uuid::nil()),
            format!("There is no application with id {}", Uuid::nil()),
        ),
        (
            format!("/builder/applications/{id}/views/Nope"),
            "has no view named Nope".to_owned(),
        ),
        (
            format!("/builder/applications/{id}/pages/Nope"),
            "has no page named Nope".to_owned(),
        ),
        (
            format!("{view_path}?step=two"),
            "`two` is not a step number".to_owned(),
        ),
        (
            format!("{view_path}?step=40"),
            "The view Show Books has no step 41 to build".to_owned(),
        ),
        (
            format!("/builder/applications/{id}/pages/Handmade"),
            "The page Handmade is an HTML file (handmade.html)".to_owned(),
        ),
        (
            format!("/builder/applications/{id}/tables/Books"),
            "There is no builder at this address".to_owned(),
        ),
    ] {
        let answer = navigate(&mut server.client, &path).await;
        assert_eq!(
            answer.status,
            StatusCode::NOT_FOUND,
            "{path}: {}",
            answer.body
        );
        assert!(answer.body.contains(&sentence), "{path}: {}", answer.body);
        assert_eq!(
            answer.headers[header::CONTENT_SECURITY_POLICY],
            BUILDER_CONTENT_SECURITY_POLICY,
            "{path}"
        );
    }
    // List's steps after its layout: a form, or skipped for this configuration.
    // Either way, not a layout, and the refusal names the step.
    let answer = navigate(
        &mut server.client,
        &format!("/builder/applications/{id}/views/List%20Books?step=1"),
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND, "{}", answer.body);
    assert!(answer.body.contains("Step 2 of "), "{}", answer.body);
    assert!(
        answer.body.contains("is a form, not a layout") || answer.body.contains("is skipped"),
        "{}",
        answer.body
    );

    // --- the canvas's files ------------------------------------------------------
    let referer = format!("http://{HOST}/builder/applications/{id}/pages/BooksOverview");
    let answer = server
        .client
        .request(
            "GET",
            Some(HOST),
            "/files/serve/BooksDB/cover.png?w=1",
            &[("referer", &referer)],
            None,
        )
        .await;
    assert_eq!(
        answer.status,
        StatusCode::TEMPORARY_REDIRECT,
        "{}",
        answer.body
    );
    assert_eq!(
        answer.headers[header::LOCATION],
        format!("{APP_ORIGIN}/files/serve/BooksDB/cover.png?w=1").as_str()
    );
    for referer in [
        None,
        Some(format!("http://{HOST}/#/applications/{id}/views")),
        Some(format!(
            "http://elsewhere.test/builder/applications/{id}/views/x"
        )),
    ] {
        let headers: Vec<(&str, &str)> = referer.iter().map(|r| ("referer", r.as_str())).collect();
        let answer = server
            .client
            .request(
                "GET",
                Some(HOST),
                "/files/serve/BooksDB/cover.png",
                &headers,
                None,
            )
            .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{referer:?}");
    }

    // --- admin only ----------------------------------------------------------------
    let mut stranger = visitor(&server.client);
    let answer = navigate(&mut stranger, &view_path).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.headers[header::LOCATION], "/");
    let asset = scripts_of(&navigate(&mut server.client, &view_path).await.body)
        .pop()
        .unwrap();
    let answer = stranger.request("GET", Some(HOST), &asset, &[], None).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{asset}");
    Ok(())
}

#[tokio::test]
async fn without_the_builder_bundle_the_routes_say_so() {
    let sessions = Arc::new(SessionStore::default());
    let config = ServerConfig {
        builder_dir: None,
        ..ServerConfig::default()
    };
    let router = build_router(
        &EndpointSet::new(),
        HandlerRegistry::new(),
        sessions.clone(),
        &config,
    )
    .expect("build router");
    let admin = sessions
        .login(User::new(Uuid::new_v4(), Role::ADMIN))
        .await
        .unwrap();
    for path in [
        format!(
            "/builder/applications/{}/views/Show%20Books",
            Uuid::new_v4()
        ),
        format!("/builder/applications/{}/pages/Home", Uuid::new_v4()),
    ] {
        let request = Request::get(&path)
            .header(header::COOKIE, format!("{SESSION_COOKIE}={admin}"))
            .header(header::ACCEPT, "text/html")
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            BUILDER_CONTENT_SECURITY_POLICY
        );
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("built without the builder") && body.contains("only as JSON"),
            "{body}"
        );
        assert!(!body.contains("<script"), "{body}");
    }
    let request = Request::get("/builder/static/tag/builder.js")
        .header(header::COOKIE, format!("{SESSION_COOKIE}={admin}"))
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// `builderStatus` says whether the builder routes have a bundle to serve, so
/// the admin UI offers **Open in builder** only when it opens a builder: `false`
/// with no bundle directory, or one without a built entry module, and `true`
/// with one.
#[tokio::test]
async fn builder_status_says_whether_the_bundle_is_built() {
    let root = std::env::temp_dir().join(format!("sc-builder-status-{}", Uuid::new_v4()));
    let empty = root.join("empty");
    let built = root.join("built");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::create_dir_all(&built).unwrap();
    std::fs::write(built.join("builder.js"), "export {};").unwrap();
    for (dir, expected) in [
        (None, false),
        (Some(empty.clone()), false),
        (Some(built.clone()), true),
    ] {
        let sessions = Arc::new(SessionStore::default());
        let config = ServerConfig {
            builder_dir: dir.clone(),
            ..ServerConfig::default()
        };
        let router = build_router(
            &sc_api::admin_endpoints(),
            HandlerRegistry::new(),
            sessions.clone(),
            &config,
        )
        .expect("build router");
        let admin = sessions
            .login(User::new(Uuid::new_v4(), Role::ADMIN))
            .await
            .unwrap();
        let request = Request::get("/api/builder")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={admin}"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{dir:?}");
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body, json!({ "available": expected }), "{dir:?}");

        // Admin only, like the routes it describes.
        let request = Request::get("/api/builder").body(Body::empty()).unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{dir:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Every file under `dir`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// The facts `BUILDER_CONTENT_SECURITY_POLICY`'s comment rests each absent
/// relaxation on, held against the builder as vendored and built.
#[test]
fn the_builder_policy_is_what_the_bundle_needs() {
    let policy = BUILDER_CONTENT_SECURITY_POLICY;
    let directive = |name: &str| {
        policy
            .split(';')
            .map(str::trim)
            .find(|d| d.split(' ').next() == Some(name))
            .unwrap_or_else(|| panic!("no {name}"))
            .to_owned()
    };
    assert_eq!(directive("script-src"), "script-src 'self'");
    assert_eq!(directive("worker-src"), "worker-src 'self'");
    assert_eq!(directive("font-src"), "font-src 'self'");
    assert_eq!(directive("img-src"), "img-src 'self' data:");
    for absent in [
        "'unsafe-eval'",
        "blob:",
        "https:",
        "*",
        "frame-src",
        "child-src",
    ] {
        assert!(!policy.contains(absent), "{absent}");
    }

    let vendor = ui_builder().join("vendor/saltcorn-builder");
    if !vendor.is_dir() {
        eprintln!("skipping the bundle's half: ui/builder/vendor is not here");
        return;
    }
    // No `frame-src` and no inline script: every CKEditor the builder mounts is
    // an inline editor, not the iframe editor whose frame runs `cke_actscrpt`.
    let mut mounts = 0;
    for file in files_under(&vendor) {
        let source = std::fs::read_to_string(&file).unwrap_or_default();
        for mount in source.split("<CKEditor").skip(1) {
            let element = &mount[..mount.find("/>").expect("a self-closing element")];
            assert!(
                element.contains("type=\"inline\""),
                "{} mounts a CKEditor that is not inline, whose frame needs an inline script",
                file.display()
            );
            mounts += 1;
        }
    }
    assert!(mounts > 0, "the builder mounts CKEditor");

    // `worker-src 'self'`: Monaco's workers are files beside the bundle.
    let monaco = std::fs::read_to_string(ui_builder().join("src/shims/monaco.ts")).unwrap();
    assert!(monaco.contains("new Worker(new URL("), "{monaco}");
    // No worker is made from a blob. The shim's comment says `blob:`, which is
    // why the code is what is checked here.
    for blob_worker in ["new Blob(", "createObjectURL("] {
        assert!(!monaco.contains(blob_worker), "{blob_worker}");
    }

    // `font-src 'self'`: the CSS's `data:` URLs are images, and its fonts files.
    if let Ok(css) = std::fs::read_to_string(ui_builder().join("dist/builder.css")) {
        for data in css.split("url(data:").skip(1) {
            assert!(
                data.starts_with("image/"),
                "a data: URL that is not an image: {}",
                &data[..40.min(data.len())]
            );
        }
        assert!(
            css.contains(".ttf") || css.contains(".woff"),
            "the fonts are files"
        );
    }
}
