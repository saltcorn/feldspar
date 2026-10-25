//! **Server-rendered applications** (`sc_app::server_render`, the Astro
//! module's framework): a build whose output holds a code body the server runs
//! per request — through the real router, with a real dispatcher and database
//! (SQLite, a file per test).
//!
//! The body here is written by hand, in the shape `plugins/astro`'s build
//! writes, so the test needs no npm; `plugins/astro/test/` covers the compiler
//! half.
//!
//! - a file the build wrote is served as a file, with an ETag and a 304;
//! - anything else runs the body, which reads the table with `db` — the code
//!   action's surface — and sees the request it is answering;
//! - `_server/` is never served, and an unknown path is the body's 404, not an
//!   SPA fallback;
//! - a form posts back with its CSRF field and gets the body's redirect;
//! - prerendering writes the listed pages into the output and the bundle, and
//!   a refreshable one is rendered again after a table it read is written;
//! - a rendered page is kept for the next visitor until a table it read is
//!   written, and a page that is a visitor's own, or wrote, is not kept;
//! - the request's own cookies reach the body, and the server's do not;
//! - a failed render is the site's 500 page, raised as an `error` event;
//! - `/_image` resizes an image the build wrote;
//! - the router hands the body a visitor id, and keeps it as a cookie only when
//!   the page used it (an A/B test); the A/B queries the runtime makes run on
//!   the real host, and the Analytics workspace reports on the two tables.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_action::TriggerDispatcher;
use sc_app::{Application, AssetBundle, BuildReport, BuildSpec, FrameworkRef};
use sc_auth::SessionStore;
use sc_catalog::{Catalog, DataField};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, MountedApp, ServerConfig, VISITOR_COOKIE, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_triggers,
};
use sc_types::{Attrs, BasicType, TypeRef};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const HOST: &str = "site.example.com";

/// The code body, as `plugins/astro`'s build writes it: one function body that
/// routes on `request` and answers `{ status, headers, body }`.
const ENTRY: &str = r#"
if (request.mode === "prerender-list") {
  return { paths: ["/about", "/team", "/signin"], refresh: ["/about"] };
}
if (request.mode === "error") {
  return { status: 500, headers: [], body: "<h1>sorry: " + request.error + "</h1>" };
}
// `cache` is the runtime's word on whether another visitor may get this answer.
const html = (body, status = 200, cache = true) =>
  ({ status, headers: [["content-type", "text/html; charset=utf-8"]], body, cache });
if (request.path === "/cookies") return html(JSON.stringify(request.cookies), 200, false);
if (request.path === "/ab") {
  // What the runtime's <Optimise> asks of the host, in its own words.
  const site = request.site, visitor = request.visitor;
  let seen = await db.astro_exposures.where({ site, experiment: "hero", visitor }).first();
  if (!seen) {
    await db.astro_exposures.insert({ site, experiment: "hero", variant: "speed", visitor,
                                      path: request.path, converted: 0,
                                      created_at: new Date().toISOString() });
    seen = await db.astro_exposures.where({ site, experiment: "hero", visitor }).first();
  }
  const counts = await db.astro_exposures.where({ site, experiment: "hero" })
    .groupBy("variant").aggregate({ n: "count()", won: "sum(converted)" }).rows();
  await db.astro_exposures.where({ id: seen.id }).update({ converted: 1 });
  return { ...html(JSON.stringify({ site, visitor, variant: seen.variant, counts }), 200, false),
           visitor: true };
}
if (request.path === "/boom") throw new Error("the page fell over");
if (request.path === "/visit") {
  await db.visits.insert({ id: String(Math.random()) });
  return html("<p>" + (await db.visits.count()) + " visits</p>");
}
if (request.path === "/posts") {
  const posts = await db.posts.orderBy("id").rows();
  return html("<ul>" + posts.map((p) => "<li>" + p.title + "</li>").join("") + "</ul>");
}
if (request.path === "/team") return html("<h1>" + (await db.posts.count()) + " authors</h1>");
if (request.path === "/signin") return html("<form>sign in</form>");
if (request.path === "/about") {
  return html("<h1>about " + (await db.posts.count()) + " posts, " + request.mode + "</h1>");
}
if (request.path === "/contact" && request.method === "POST") {
  const email = request.form.find(([k]) => k === "email")[1];
  return { status: 303, headers: [["location", "/contact?from=" + email]], body: "" };
}
if (request.path === "/contact") {
  return html('<form method="post"><input name="_csrf" value="' + request.csrfToken + '"></form>', 200, false);
}
return html("<h1>not found</h1>", 404);
"#;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-render-{}-{tag}-{:?}",
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

fn spec() -> BuildSpec {
    BuildSpec {
        command: "npm".to_owned(),
        args: vec!["run".to_owned(), "build".to_owned()],
        source_dir: "site".to_owned(),
        output_dir: "site/dist".to_owned(),
        install: None,
        server_entry: Some("_server/entry.js".to_owned()),
    }
}

/// An 8×4 PNG, red.
const PNG: [u8; 74] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 8, 0, 0, 0, 4, 8, 2, 0,
    0, 0, 60, 175, 233, 167, 0, 0, 0, 17, 73, 68, 65, 84, 120, 156, 99, 56, 33, 39, 135, 21, 49,
    80, 79, 2, 0, 110, 254, 32, 129, 64, 71, 42, 1, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn bundle() -> AssetBundle {
    AssetBundle::new()
        .with("index.html", "<!doctype html><h1>home</h1>")
        .with("_astro/site.abc123.css", "body{margin:0}")
        .with("_astro/pic.abc123.png", PNG.to_vec())
        .with("_server/entry.js", ENTRY)
}

struct Server {
    catalog: Arc<Catalog>,
    router: Router,
    dispatcher: Arc<TriggerDispatcher>,
    app: Application,
}

async fn setup(tmp: &TempDir) -> sc_error::Result<Server> {
    let driver = Arc::new(SqliteDriver::open(tmp.path().join("site.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_code_libraries(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    catalog
        .create_table(
            "posts",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int))
                    .required()
                    .primary_key(),
                DataField::plain("title", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    catalog
        .create_table(
            "visits",
            &[DataField::plain("id", TypeRef::Basic(BasicType::Text))
                .required()
                .primary_key()],
        )
        .await?;
    catalog
        .create_table(
            "journal",
            &[
                DataField::plain("id", TypeRef::Basic(BasicType::Int)).primary_key(),
                DataField::plain("message", TypeRef::Basic(BasicType::Text)),
                DataField::plain("route", TypeRef::Basic(BasicType::Text)),
            ],
        )
        .await?;
    let evaluator = default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    // The rows, written the way a page would write them.
    dispatcher
        .run_code(
            &catalog,
            "run_js_code",
            "seed",
            r#"await db.posts.insert({ id: 1, title: "Hello" });
               await db.posts.insert({ id: 2, title: "World" });"#,
            Attrs::new(),
            None,
            None,
            None,
        )
        .await?;

    // Not saved: saving checks the framework against the installed modules, and
    // this test mounts the framework directly rather than installing the module.
    let app = Application::new("Site", "site", FrameworkRef::new("astro"));
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(dispatcher.clone()),
    );
    let framework = sc_app::mount_code_framework(&app, bundle(), spec(), Some(dispatcher.clone()))?;
    apps.mount(MountedApp::new_with(
        app.clone(),
        framework,
        &catalog,
        apps.evaluator(),
        apps.triggers(),
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
        apps.clone(),
    )?;
    Ok(Server {
        catalog,
        router,
        dispatcher,
        app,
    })
}

impl Server {
    /// Run `code` as a code action: a write through the row layer, which is
    /// what a table's write event is raised from.
    async fn code(&self, code: &str) -> sc_error::Result<serde_json::Value> {
        self.dispatcher
            .run_code(
                &self.catalog,
                "run_js_code",
                "test",
                code,
                Attrs::new(),
                None,
                None,
                None,
            )
            .await
    }
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

async fn send(router: &Router, request: Request<Body>) -> Answer {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn get(router: &Router, path: &str) -> Answer {
    let request = Request::builder()
        .uri(path)
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    send(router, request).await
}

#[tokio::test]
async fn files_are_files_and_everything_else_runs_the_page_body() -> sc_error::Result<()> {
    let tmp = TempDir::new("serve");
    let server = setup(&tmp).await?;

    // A file the build wrote: no body is run for it.
    let home = get(&server.router, "/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert_eq!(home.body, "<!doctype html><h1>home</h1>");
    let tag = home.headers.get(header::ETAG).expect("an etag").clone();
    let again = send(
        &server.router,
        Request::builder()
            .uri("/")
            .header(header::HOST, HOST)
            .header(header::IF_NONE_MATCH, tag)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    let css = get(&server.router, "/_astro/site.abc123.css").await;
    assert!(
        css.headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );

    // A rendered page reads the table with the code action's `db`.
    let posts = get(&server.router, "/posts").await;
    assert_eq!(posts.status, StatusCode::OK, "{}", posts.body);
    assert_eq!(posts.body, "<ul><li>Hello</li><li>World</li></ul>");
    assert_eq!(
        posts.headers[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert_eq!(posts.headers[header::CACHE_CONTROL], "private, no-cache");

    // The site's source is never a file; an unknown path is the body's 404.
    let source = get(&server.router, "/_server/entry.js").await;
    assert_eq!(source.status, StatusCode::NOT_FOUND);
    assert!(!source.body.contains("db.posts"), "{}", source.body);
    let missing = get(&server.router, "/no/such/page").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.body, "<h1>not found</h1>");
    Ok(())
}

#[tokio::test]
async fn a_form_posts_back_with_its_csrf_field_and_is_redirected() -> sc_error::Result<()> {
    let tmp = TempDir::new("form");
    let server = setup(&tmp).await?;

    // The page carries this browser's token, which its cookie also holds.
    let page = get(&server.router, "/contact").await;
    assert_eq!(page.status, StatusCode::OK);
    let cookie = page
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|c| c.starts_with(&format!("{CSRF_COOKIE}=")))
        .expect("a CSRF cookie on first contact")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let token = cookie.split_once('=').unwrap().1.to_owned();
    assert!(
        page.body.contains(&format!("value=\"{token}\"")),
        "{}",
        page.body
    );

    let post = |body: String| {
        Request::builder()
            .method("POST")
            .uri("/contact")
            .header(header::HOST, HOST)
            .header(header::COOKIE, cookie.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap()
    };
    let sent = send(&server.router, post(format!("_csrf={token}&email=a%40b.c"))).await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.body);
    assert_eq!(sent.headers[header::LOCATION], "/contact?from=a@b.c");

    // Without the field, the CSRF check refuses it before any page runs.
    let forged = send(&server.router, post("email=a%40b.c".to_owned())).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn prerendering_writes_the_listed_pages_through_the_same_body() -> sc_error::Result<()> {
    let tmp = TempDir::new("prerender");
    let server = setup(&tmp).await?;
    let output_dir = tmp.path().join("dist");
    std::fs::create_dir_all(&output_dir).unwrap();
    let mut report = BuildReport {
        bundle: bundle(),
        output_dir: output_dir.clone(),
        git_repo: false,
        stdout: String::new(),
        stderr: String::new(),
        client_path: None,
        installed: false,
        install_log: None,
        reused: false,
    };
    let written = sc_app::prerender_site(
        &server.catalog,
        &server.app,
        &spec(),
        &mut report,
        Some(&server.dispatcher),
    )
    .await?;
    assert_eq!(written, 3);
    let page = "<h1>about 2 posts, prerender</h1>";
    assert_eq!(
        std::fs::read_to_string(output_dir.join("about/index.html")).unwrap(),
        page
    );
    assert_eq!(
        &report.bundle.get("about/index.html").unwrap().bytes[..],
        page.as_bytes()
    );
    assert!(report.stdout.contains("prerendered 3 page(s)"));

    // Mounted from that bundle, the page is a file: it says `prerender`, not
    // `render`, because no body ran for this request.
    let framework = sc_app::mount_code_framework(
        &server.app,
        report.bundle.clone(),
        spec(),
        Some(server.dispatcher.clone()),
    )?;
    let served = framework
        .handle(sc_app::AppRequest::get("/about"), &server.catalog)
        .await?;
    assert_eq!(&served.body[..], page.as_bytes());

    // It says `refresh`: a write to the table it read renders it again on the
    // next request, as the build did, and that is served until the next write.
    assert!(report.bundle.get("_server/prerendered.json").is_some());
    server
        .code("await db.posts.insert({ id: 3, title: 'Three' });")
        .await?;
    let refreshed = framework
        .handle(sc_app::AppRequest::get("/about"), &server.catalog)
        .await?;
    assert_eq!(
        String::from_utf8_lossy(&refreshed.body),
        "<h1>about 3 posts, prerender</h1>"
    );
    let tag = refreshed
        .headers
        .iter()
        .find(|(k, _)| k == "etag")
        .map(|(_, v)| v.clone())
        .expect("a refreshed page has an etag");
    let mut again = sc_app::AppRequest::get("/about");
    again.headers.insert("if-none-match".to_owned(), tag);
    assert_eq!(framework.handle(again, &server.catalog).await?.status, 304);

    // A build with no server to run the pages on skips the step and says so.
    let mut bare = report.clone();
    bare.stdout.clear();
    let none =
        sc_app::prerender_site(&server.catalog, &server.app, &spec(), &mut bare, None).await?;
    assert_eq!(none, 0);
    assert!(bare.stdout.contains("prerendering skipped"));

    // A caller the application does not let in gets the pages that read no
    // data — a sign-in page — and the 403 for those that did, refreshable or
    // not (security review #25).
    let refused = |path: &str| {
        let mut req = sc_app::AppRequest::get(path);
        req.admitted = false;
        req
    };
    for path in ["/about", "/team"] {
        let answer = framework.handle(refused(path), &server.catalog).await?;
        assert_eq!(answer.status, 403, "{path}");
    }
    let signin = framework
        .handle(refused("/signin"), &server.catalog)
        .await?;
    assert_eq!(signin.status, 200);
    assert_eq!(&signin.body[..], b"<form>sign in</form>");
    // Admitted, the data page is served.
    let team = framework
        .handle(sc_app::AppRequest::get("/team"), &server.catalog)
        .await?;
    assert_eq!(&team.body[..], b"<h1>2 authors</h1>");
    Ok(())
}

/// The whole path with the real module: `plugins/astro`'s own generators write
/// a project, npm builds it with the real compiler, and the code body it wrote
/// runs in the real isolate against the real table — the check that the
/// compiled output asks nothing of the isolate that it does not have (no
/// `URL`, the sandbox's own `Headers`). Opt-in, as the Vue build is: it needs
/// Node, npm and the registry.
#[tokio::test]
async fn the_bundled_astro_module_builds_a_site_this_server_renders() -> sc_error::Result<()> {
    if std::env::var("SC_TEST_NPM").as_deref() != Ok("1") {
        eprintln!("skipping: set SC_TEST_NPM=1 to run the real npm build");
        return Ok(());
    }
    let tmp = TempDir::new("astro-e2e");
    let server = setup(&tmp).await?;
    assert!(
        astro_tables(&server.catalog).await,
        "the npm build needs node"
    );
    let project = tmp.path().join("site");
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/astro/index.js");
    // What the module worker does on Scaffold, in Node: the same functions.
    let script = r#"
const fs = require("fs"), path = require("path");
const [plugin, root] = process.argv.slice(1);
const ctx = {
  project: "site", name: "site", runtime: "src/feldspar", client: "src/feldspar/client.ts",
  auth: false, app: { name: "Acme", subdomain: "site", description: "Rockets, fast.", url: "" },
  settings: { site: "" },
  tables: [{ name: "posts", pascal: "Posts", title: "Posts", client: "posts", pk: "id",
             fields: [{ name: "id", primary_key: true }, { name: "title", primary_key: false }] }],
};
for (const f of require(plugin).frameworks.astro.scaffold(ctx)) {
  fs.mkdirSync(path.dirname(path.join(root, f.path)), { recursive: true });
  fs.writeFileSync(path.join(root, f.path), f.contents);
}"#;
    let run = |program: &str, args: &[&str]| {
        let out = std::process::Command::new(program)
            .args(args)
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{program} {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    std::fs::create_dir_all(&project).unwrap();
    run(
        "node",
        &[
            "-e",
            script,
            plugin.to_str().unwrap(),
            project.to_str().unwrap(),
        ],
    );
    // An A/B test, as the tutorial writes one.
    std::fs::write(
        project.join("src/pages/landing.astro"),
        r#"---
import { Optimise, Variant, Goal } from "feldspar:components";
---
<h1><Optimise name="hero" allocate="thompson">
  <Variant name="best">The best tool for the job</Variant>
  <Variant name="speed">Be done in no time!</Variant>
</Optimise></h1>
<Goal name="signup"><a href="https://app.example.com/signup">Sign up</a></Goal>"#,
    )
    .unwrap();
    match std::env::var("SC_ASTRO_NODE_MODULES") {
        Ok(shared) => std::os::unix::fs::symlink(shared, project.join("node_modules")).unwrap(),
        Err(_) => run("npm", &["install", "--no-audit", "--no-fund"]),
    }
    run("npm", &["run", "build"]);

    let mut report = BuildReport {
        bundle: AssetBundle::from_dir(project.join("dist"))?,
        output_dir: project.join("dist"),
        git_repo: false,
        stdout: String::new(),
        stderr: String::new(),
        client_path: None,
        installed: false,
        install_log: None,
        reused: false,
    };
    let written = sc_app::prerender_site(
        &server.catalog,
        &server.app,
        &spec(),
        &mut report,
        Some(&server.dispatcher),
    )
    .await?;
    assert_eq!(written, 2, "the landing page and the 404 page");
    let framework = sc_app::mount_code_framework(
        &server.app,
        report.bundle.clone(),
        spec(),
        Some(server.dispatcher.clone()),
    )?;
    let page = |path: &str| {
        let framework = framework.clone();
        let catalog = server.catalog.clone();
        let mut req = sc_app::AppRequest::get(path);
        req.base_url = "http://site.example.com".to_owned();
        req.csrf_token = "tok".to_owned();
        async move { framework.handle(req, &catalog).await }
    };

    let home = page("/").await?;
    let home = String::from_utf8_lossy(&home.body).into_owned();
    assert!(home.contains("Rockets, fast."), "{home}");
    assert!(
        home.contains("<link rel=\"stylesheet\" href=\"/_astro/site."),
        "{home}"
    );

    let posts = page("/posts").await?;
    assert_eq!(
        posts.status,
        200,
        "{}",
        String::from_utf8_lossy(&posts.body)
    );
    let posts = String::from_utf8_lossy(&posts.body).into_owned();
    assert!(
        // Scoped styles put the component's class on each element.
        posts.contains(">Hello</span>") && posts.contains(">World</span>"),
        "{posts}"
    );
    assert!(
        posts.contains("<link rel=\"canonical\" href=\"http://site.example.com/posts\""),
        "{posts}"
    );

    let contact = page("/contact").await?;
    assert!(
        String::from_utf8_lossy(&contact.body)
            .contains("<input type=\"hidden\" name=\"_csrf\" value=\"tok\">")
    );
    let missing = page("/nope").await?;
    assert_eq!(missing.status, 404);
    assert!(String::from_utf8_lossy(&missing.body).contains("Page not found"));
    let sitemap = page("/sitemap.xml").await?;
    assert!(
        String::from_utf8_lossy(&sitemap.body).contains("<loc>http://site.example.com/posts</loc>")
    );

    // The A/B test: one variant, an exposure in the real table, the goal link
    // counted and redirected.
    let visit = |path: &str, query: &[(&str, &str)]| {
        let framework = framework.clone();
        let catalog = server.catalog.clone();
        let mut req = sc_app::AppRequest::get(path);
        req.visitor = "0123456789abcdef0123456789abcdef".to_owned();
        for (k, v) in query {
            req.query.insert((*k).to_owned(), (*v).to_owned());
        }
        async move { framework.handle(req, &catalog).await }
    };
    let landing = visit("/landing", &[]).await?;
    let html = String::from_utf8_lossy(&landing.body).into_owned();
    assert_eq!(landing.status, 200, "{html}");
    assert!(landing.keeps_visitor, "the router is told to keep the id");
    assert_eq!(
        html.matches("The best tool").count() + html.matches("no time").count(),
        1,
        "{html}"
    );
    let exposures = server
        .code("return await db.astro_exposures.rows();")
        .await?;
    assert_eq!(exposures.as_array().unwrap().len(), 1, "{exposures}");
    assert_eq!(exposures[0]["site"], "Site");
    let href = html
        .split("href=\"/_goal/signup?")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("no goal link in {html}"))
        .replace("&amp;", "&");
    let query: Vec<(String, String)> = href
        .split('&')
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap();
            (k.to_owned(), urlencoding_decode(v))
        })
        .collect();
    let query: Vec<(&str, &str)> = query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let followed = visit("/_goal/signup", &query).await?;
    assert_eq!(followed.status, 302);
    assert!(followed.headers.contains(&(
        "location".to_owned(),
        "https://app.example.com/signup".to_owned()
    )));
    let converted = server
        .code("return await db.astro_conversions.rows();")
        .await?;
    assert_eq!(
        converted[0]["variant"], exposures[0]["variant"],
        "{converted}"
    );
    Ok(())
}

/// Percent-decoding, for a query value the page wrote.
fn urlencoding_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn a_rendered_page_is_kept_until_a_table_it_read_is_written() -> sc_error::Result<()> {
    let tmp = TempDir::new("cache");
    let server = setup(&tmp).await?;
    let hit = |answer: &Answer| answer.headers.get("x-render-cache").is_some();

    let first = get(&server.router, "/posts").await;
    assert_eq!(first.body, "<ul><li>Hello</li><li>World</li></ul>");
    assert!(!hit(&first));
    let second = get(&server.router, "/posts").await;
    assert!(hit(&second), "the second visitor gets the kept page");
    assert_eq!(second.body, first.body);
    assert_eq!(second.headers[header::CACHE_CONTROL], "private, no-cache");

    // A write to a table the page read drops it; the next render shows it.
    server
        .code("await db.posts.insert({ id: 3, title: 'Again' });")
        .await?;
    let third = get(&server.router, "/posts").await;
    assert!(!hit(&third));
    assert_eq!(
        third.body,
        "<ul><li>Hello</li><li>World</li><li>Again</li></ul>"
    );
    // A write to a table it did not read leaves it.
    server
        .code("await db.journal.insert({ id: 9, message: 'x' });")
        .await?;
    assert!(hit(&get(&server.router, "/posts").await));

    // A query is part of what was asked.
    assert!(!hit(&get(&server.router, "/posts?page=2").await));

    // The visitor's own page (its CSRF token) is never kept.
    get(&server.router, "/contact").await;
    assert!(!hit(&get(&server.router, "/contact").await));

    // Nor is a render that wrote: each visit must run.
    let one = get(&server.router, "/visit").await;
    let two = get(&server.router, "/visit").await;
    assert!(!hit(&two));
    assert_ne!(one.body, two.body, "{} / {}", one.body, two.body);
    Ok(())
}

#[tokio::test]
async fn the_page_sees_the_requests_cookies_but_not_the_servers() -> sc_error::Result<()> {
    let tmp = TempDir::new("cookies");
    let server = setup(&tmp).await?;
    let request = Request::builder()
        .uri("/cookies")
        .header(header::HOST, HOST)
        .header(
            header::COOKIE,
            format!("theme=dark; sc_session=secret; {CSRF_COOKIE}=token; sc_guest=g"),
        )
        .body(Body::empty())
        .unwrap();
    let answer = send(&server.router, request).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body, r#"{"theme":"dark"}"#);
    Ok(())
}

#[tokio::test]
async fn a_failed_render_is_the_500_page_and_still_an_error_event() -> sc_error::Result<()> {
    let tmp = TempDir::new("error");
    let server = setup(&tmp).await?;
    sc_action::save_trigger(
        &server.catalog,
        &server.dispatcher.registry(),
        &sc_action::Trigger::new("on_error", sc_action::EventKind::Error, "insert_row")
            .config("table", "journal")
            .config(
                "values",
                serde_json::json!({ "id": "1", "message": "payload.message", "route": "payload.path" }),
            ),
    )
    .await?;
    server.dispatcher.reload(&server.catalog).await?;

    let answer = get(&server.router, "/boom").await;
    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        answer.body.starts_with("<h1>sorry: ") && answer.body.contains("the page fell over"),
        "{}",
        answer.body
    );
    assert_eq!(answer.headers[header::CACHE_CONTROL], "no-store");
    let journal = server.code("return await db.journal.rows();").await?;
    assert_eq!(journal[0]["route"], "/boom", "{journal}");
    assert!(
        journal[0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("the page fell over")),
        "{journal}"
    );
    Ok(())
}

#[tokio::test]
async fn an_image_the_build_wrote_is_resized_once_and_kept() -> sc_error::Result<()> {
    let tmp = TempDir::new("image");
    let server = setup(&tmp).await?;
    let answer = get(&server.router, "/_image?src=%2F_astro%2Fpic.abc123.png&w=4").await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.headers[header::CONTENT_TYPE], "image/png");
    assert!(
        answer.headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    // Not the bytes as text: a PNG header says its size, 4×2.
    let response = server
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_image?src=%2F_astro%2Fpic.abc123.png&w=4")
                .header(header::HOST, HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let side = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    assert_eq!((side(16), side(20)), (4, 2));

    let elsewhere = get(&server.router, "/_image?src=%2Ffavicon.png&w=4").await;
    assert_eq!(elsewhere.status, StatusCode::BAD_REQUEST);
    let missing = get(&server.router, "/_image?src=%2F_astro%2Fnope.png&w=4").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    Ok(())
}

/// Create the Astro module's A/B tables from its own declaration — node
/// evaluates `plugins/astro/index.js`'s `tables`, and the module table facility
/// creates them as it does at install. `false` (and the test skips) when there
/// is no `node`.
async fn astro_tables(catalog: &Catalog) -> bool {
    let index = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/astro/index.js");
    let output = match std::process::Command::new("node")
        .args([
            "-e",
            "console.log(JSON.stringify(require(process.argv[1]).tables))",
        ])
        .arg(&index)
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => panic!("{}", String::from_utf8_lossy(&output.stderr)),
        Err(_) => {
            eprintln!("skipping: there is no `node` to read plugins/astro's tables");
            return false;
        }
    };
    let raw: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    let (decls, issues) = sc_module::tables::parse_declarations(&raw);
    assert!(issues.is_empty(), "{issues:?}");
    let report = sc_module::tables::sync(
        catalog,
        "@feldspar/astro",
        &decls,
        &sc_module::tables::claims([("@feldspar/astro", &decls[..])]),
    )
    .await;
    assert!(report.issues.is_empty(), "{:?}", report.issues);
    assert_eq!(
        report.tables_created,
        ["astro_exposures", "astro_conversions"]
    );
    true
}

/// The `sc_visitor` cookie a response set, if any.
fn visitor_set(answer: &Answer) -> Option<String> {
    answer
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|c| c.starts_with(&format!("{VISITOR_COOKIE}=")))
        .map(str::to_owned)
}

#[tokio::test]
async fn the_visitor_id_is_handed_over_and_kept_only_when_used() -> sc_error::Result<()> {
    let tmp = TempDir::new("visitor");
    let server = setup(&tmp).await?;
    if !astro_tables(&server.catalog).await {
        return Ok(());
    }
    let with_cookie = |path: &str, cookie: &str| {
        Request::builder()
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::COOKIE, cookie.to_owned())
            .body(Body::empty())
            .unwrap()
    };

    // A page that does not use the id sets no cookie.
    assert_eq!(visitor_set(&get(&server.router, "/posts").await), None);

    // One that does gets a fresh id, kept for a year, sent on arrival from
    // another site, and not readable by the page's scripts.
    let first = get(&server.router, "/ab").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let cookie = visitor_set(&first).expect("an A/B page keeps the visitor");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    assert!(cookie.contains("Max-Age=31536000"), "{cookie}");
    let id = cookie
        .split(';')
        .next()
        .unwrap()
        .trim_start_matches(&format!("{VISITOR_COOKIE}="))
        .to_owned();
    assert_eq!(id.len(), 32);
    let body: serde_json::Value = serde_json::from_str(&first.body).unwrap();
    assert_eq!(body["visitor"], id.as_str(), "the body saw the id it set");
    assert_eq!(
        body["site"], "Site",
        "rows are kept under the application's name"
    );
    assert_eq!(
        body["counts"],
        serde_json::json!([{ "variant": "speed", "n": 1, "won": 0 }]),
        "grouped counts, as Thompson sampling reads them"
    );

    // The visitor comes back: the same id, the same exposure, no new cookie.
    let again = send(
        &server.router,
        with_cookie("/ab", &format!("{VISITOR_COOKIE}={id}")),
    )
    .await;
    assert_eq!(
        visitor_set(&again),
        None,
        "an id the browser has is not set again"
    );
    let body: serde_json::Value = serde_json::from_str(&again.body).unwrap();
    assert_eq!(body["visitor"], id.as_str());
    assert_eq!(
        body["counts"],
        serde_json::json!([{ "variant": "speed", "n": 1, "won": 1 }]),
        "one exposure, now converted"
    );

    // A value the server could not have minted is not passed on.
    let forged = send(
        &server.router,
        with_cookie("/ab", &format!("{VISITOR_COOKIE}=<script>")),
    )
    .await;
    let body: serde_json::Value = serde_json::from_str(&forged.body).unwrap();
    assert_ne!(body["visitor"], "<script>");
    assert!(visitor_set(&forged).is_some(), "and is replaced");
    // Nor is the id one of the application's own cookies.
    let cookies = send(
        &server.router,
        with_cookie("/cookies", &format!("{VISITOR_COOKIE}={id}; theme=dark")),
    )
    .await;
    assert_eq!(cookies.body, r#"{"theme":"dark"}"#);
    Ok(())
}

#[tokio::test]
async fn the_ab_workspace_reports_on_the_two_tables_for_its_site() -> sc_error::Result<()> {
    use sc_analytics::plot::{RenderedTable, render_table};
    let tmp = TempDir::new("ab-workspace");
    let server = setup(&tmp).await?;
    assert!(
        sc_analytics::ab::ensure_ab_workspace(&server.catalog, "Site")
            .await?
            .is_none(),
        "no A/B tables (the module is not installed), no workspace"
    );
    if !astro_tables(&server.catalog).await {
        return Ok(());
    }
    server
        .code(
            r#"let id = 0;
               const add = (site, variant, converted) => db.astro_exposures.insert({
                 site, experiment: "hero", variant, visitor: "v" + ++id, converted });
               for (let i = 0; i < 4; i++) await add("Site", "control", i < 1 ? 1 : 0);
               for (let i = 0; i < 4; i++) await add("Site", "speed", i < 3 ? 1 : 0);
               await add("Other", "speed", 1);
               await db.astro_conversions.insert({ site: "Site", goal: "signup",
                 experiment: "hero", variant: "speed", visitor: "v5" });"#,
        )
        .await?;

    let workspace = sc_analytics::ab::ensure_ab_workspace(&server.catalog, "Site")
        .await?
        .expect("the tables are there");
    assert_eq!(workspace.name, "A/B tests: Site");
    assert_eq!(workspace.kind, sc_analytics::WorkspaceKind::Dashboard);
    let again = sc_analytics::ab::ensure_ab_workspace(&server.catalog, "Site")
        .await?
        .unwrap();
    assert_eq!(again.id, workspace.id, "made once");
    assert_eq!(
        sc_analytics::list_workspaces(&server.catalog)
            .await?
            .iter()
            .filter(|w| w.name == workspace.name)
            .count(),
        1
    );

    // The rates table: this site's rows only, by variant.
    let panel = &workspace.state["tiles"][1]["panel"];
    let spec: sc_analytics::plot::TableSpec =
        serde_json::from_value(panel["content"]["spec"].clone()).unwrap();
    let RenderedTable::Table(table) =
        render_table(&server.catalog, &sc_dataset::Caller::admin(), &spec).await?
    else {
        panic!("the rates table did not render");
    };
    let rows: Vec<String> = table
        .body
        .rows
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows[0].starts_with(r#"["hero","control",4,"#), "{rows:?}");
    assert!(rows[0].ends_with(",0.25]"), "{rows:?}");
    assert!(rows[1].starts_with(r#"["hero","speed",4,"#), "{rows:?}");
    assert!(rows[1].ends_with(",0.75]"), "{rows:?}");
    Ok(())
}
