//! **Interactive server-rendered sites** (TODO W4): signing in from an Astro
//! site's own forms, fragments behind the CSRF check, and — when npm and
//! Chromium are here — the real Astro build in a real browser.
//!
//! Through the real router, with a real dispatcher and database (SQLite, a file
//! per test). The site's body is written by hand, in the shape
//! `plugins/astro`'s build writes, for everything the *host* decides:
//!
//! - **W4.2** `/_auth/sign-in`, `/_auth/sign-up` and `/_auth/sign-out` go
//!   through the one session path: a right password signs in and goes on to
//!   `next`; a wrong one, an unknown address and a disabled account are one
//!   answer, back to the form; an account the application refuses (F9) gets
//!   no session; an account with a second factor is sent to the server's own
//!   page for its code; `next` off this host is `/`; a form posted from another
//!   site is refused before anything runs; signing out ends the session; and
//!   sign-up follows the REST API's settings.
//! - **W4.3** a fragment `POST` without the CSRF token is refused like a page's.
//!
//! With `SC_TEST_NPM=1` (or `SC_ASTRO_NODE_MODULES`) and Chromium, the
//! tutorial's site is scaffolded by the module's own generators, given the
//! generated client, built with npm and served on a port, and two signed-in
//! browsers use it (W4.1, W4.3, W4.4): the API called from a `<script>` under
//! the framework's CSP, a like and "load more" through fragments, and a post by
//! one visitor appearing for the other through `<Live>` — but a row the second
//! may not read never appearing, an update replacing and a delete removing.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::disallowed_methods,
    reason = "test fixture: starts the programs a test drives (npm, a browser) directly"
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::auth::CSRF_COOKIE;
use sc_app::{ApiConfig, Application, AssetBundle, BuildSpec, FrameworkRef};
use sc_auth::{RoleDef, SessionStore, create_user, save_role};
use sc_catalog::{Catalog, SecretKey};
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_server::{
    AppMounts, MountedApp, SESSION_COOKIE, ServerConfig, admin_handlers, build_router_with_apps,
    default_js_evaluator, install_triggers,
};
use sc_test_harness::Teardown;
use sc_types::Role;
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const HOST: &str = "club.example.com";
const PASSWORD: &str = "correct-horse";
const MEMBER: &str = "member@example.com";
const TWO_FACTOR: &str = "careful@example.com";
const DISABLED: &str = "gone@example.com";
const OUTSIDER: &str = "outsider@example.com";
const ROLE_MEMBER: Role = Role::of(40);
const ROLE_GUEST: Role = Role::of(80);

/// The site's body: who is looking, and whether sign-up is offered — what
/// `<SignIn>` and `<SignUp>` read.
const ENTRY: &str = r#"
const who = (typeof user !== "undefined" && user) ? user.email : "nobody";
return {
  status: 200,
  headers: [["content-type", "text/html; charset=utf-8"]],
  body: "<p class=\"who\">" + who + "</p><p class=\"signup\">" + request.signUp + "</p>",
  cache: false,
};
"#;

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf, Teardown);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-astro-interactive-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir, Teardown::new())
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

/// The club: members only (role 40 or better), sign-up on, giving members.
fn club() -> Application {
    let mut rest = ApiConfig::new("rest", "/api");
    rest.config
        .insert(sc_api::REST_CFG_ALLOW_SIGNUP.to_owned(), json!(true));
    rest.config.insert(
        sc_api::REST_CFG_NEW_USER_ROLE.to_owned(),
        json!(i64::from(ROLE_MEMBER)),
    );
    let mut app = Application::new("Club", "club", FrameworkRef::new("astro")).with_api(rest);
    app.min_role = ROLE_MEMBER;
    app
}

struct Fixture {
    router: Router,
    catalog: Arc<Catalog>,
    _tmp: TempDir,
}

async fn setup(tag: &str) -> sc_error::Result<Fixture> {
    let tmp = TempDir::new(tag);
    let driver = Arc::new(SqliteDriver::open(tmp.0.join("club.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    tmp.1.add(catalog.teardown_hook());
    catalog.set_secret_key(SecretKey::generate()?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_code_libraries(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    save_role(&catalog, &RoleDef::new(ROLE_MEMBER, "Member")).await?;
    save_role(&catalog, &RoleDef::new(ROLE_GUEST, "Guest")).await?;
    create_user(&catalog, MEMBER, PASSWORD, ROLE_MEMBER).await?;
    create_user(&catalog, OUTSIDER, PASSWORD, ROLE_GUEST).await?;
    let gone = create_user(&catalog, DISABLED, PASSWORD, ROLE_MEMBER).await?;
    let _changed = sc_auth::set_user_disabled(&catalog, gone.id(), true).await?;
    // A member with a second factor on.
    let careful = create_user(&catalog, TWO_FACTOR, PASSWORD, ROLE_MEMBER).await?;
    let secret = sc_auth::begin_second_factor_setup(&catalog, careful.id()).await?;
    let code = sc_auth::totp_at(&secret, chrono::Utc::now().timestamp());
    let sc_auth::Attempt::Permitted(permit) =
        sc_auth::reserve_attempt(&catalog, careful.id()).await?
    else {
        panic!("a first guess");
    };
    let enabled = sc_auth::finish_second_factor_setup(&catalog, permit, careful.id(), &code)
        .await?
        .expect("the app's code");
    let _changed = enabled.changed;

    let evaluator = default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_evaluator(evaluator)
            .with_triggers(dispatcher.clone()),
    );
    let app = club();
    let framework = sc_app::mount_code_framework(
        &app,
        AssetBundle::new().with("_server/entry.js", ENTRY),
        spec(),
        Some(dispatcher),
    )?;
    apps.mount(MountedApp::new_with(
        app,
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
        apps,
    )?;
    Ok(Fixture {
        router,
        catalog,
        _tmp: tmp,
    })
}

/// A browser: a cookie jar, on the club's host.
struct Browser {
    router: Router,
    cookies: HashMap<String, String>,
}

struct Answer {
    status: StatusCode,
    location: Option<String>,
    body: String,
    set_cookies: Vec<String>,
}

impl Browser {
    async fn new(router: &Router) -> Browser {
        let mut browser = Browser {
            router: router.clone(),
            cookies: HashMap::new(),
        };
        // First contact hands out the CSRF cookie.
        browser.get("/_auth/sign-in").await;
        assert!(browser.cookies.contains_key(CSRF_COOKIE));
        browser
    }

    fn csrf(&self) -> String {
        self.cookies[CSRF_COOKIE].clone()
    }

    fn signed_in(&self) -> bool {
        self.cookies.contains_key(SESSION_COOKIE)
    }

    async fn send(&mut self, request: axum::http::request::Builder, body: Body) -> Answer {
        let mut request = request.header(header::HOST, HOST);
        if !self.cookies.is_empty() {
            let jar = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            request = request.header(header::COOKIE, jar);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let mut set_cookies = Vec::new();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            let text = raw.to_str().unwrap().to_owned();
            if let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=') {
                let expired = value.is_empty() || text.contains("Max-Age=0");
                if expired {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
            set_cookies.push(text);
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        Answer {
            status,
            location,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            set_cookies,
        }
    }

    async fn get(&mut self, path: &str) -> Answer {
        self.send(Request::builder().uri(path), Body::empty()).await
    }

    /// A form post, as a browser sends one: the fields as they are given.
    async fn post_form(&mut self, path: &str, fields: &[(&str, &str)]) -> Answer {
        let body = fields
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        self.send(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            Body::from(body),
        )
        .await
    }

    /// `<SignIn />`'s form, filled in and posted with this browser's token.
    async fn sign_in(&mut self, email: &str, password: &str, next: &str) -> Answer {
        let csrf = self.csrf();
        self.post_form(
            "/_auth/sign-in",
            &[
                ("_csrf", &csrf),
                ("next", next),
                ("back", "/login"),
                ("email", email),
                ("password", password),
                ("remember", "1"),
                ("remember_shown", "1"),
            ],
        )
        .await
    }
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[tokio::test]
async fn signing_in_from_the_sites_own_form_takes_the_one_session_path() -> sc_error::Result<()> {
    let fx = setup("sign-in").await?;
    let mut browser = Browser::new(&fx.router).await;

    // A members-only site shows nobody signed out a page — but says where to
    // sign in, and its sign-in page is served all the same.
    let refused = browser.get("/feed").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(
        refused
            .body
            .contains("href=\"/_auth/sign-in?next=%2Ffeed\""),
        "{}",
        refused.body
    );
    let page = browser.get("/_auth/sign-in?next=/feed").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains(&format!("name=\"_csrf\" value=\"{}\"", browser.csrf())),
        "{}",
        page.body
    );

    // A wrong password, an unknown address and a disabled account: one answer,
    // back to the form's page, and no session.
    for (email, password) in [
        (MEMBER, "wrong"),
        ("nobody@example.com", PASSWORD),
        (DISABLED, PASSWORD),
    ] {
        let failed = browser.sign_in(email, password, "/feed").await;
        assert_eq!(failed.status, StatusCode::SEE_OTHER, "{email}");
        assert_eq!(
            failed.location.as_deref(),
            Some("/login?sign_in=failed"),
            "{email}"
        );
        assert!(!browser.signed_in(), "{email}");
    }

    // An account the club does not let in (F9): refused before a session.
    let outsider = browser.sign_in(OUTSIDER, PASSWORD, "/feed").await;
    assert_eq!(outsider.status, StatusCode::FORBIDDEN, "{}", outsider.body);
    assert!(!browser.signed_in());

    // A second factor: no session yet; the server's page asks for the code
    // and comes back to `next`.
    let careful = browser.sign_in(TWO_FACTOR, PASSWORD, "/feed").await;
    assert!(careful.status.is_redirection(), "{}", careful.status);
    let to = careful.location.unwrap();
    assert!(
        to.starts_with(sc_server::SECOND_FACTOR_PAGE_ROUTE)
            && (to.contains("return_to=%2Ffeed") || to.contains("return_to=/feed")),
        "{to}"
    );
    assert!(!browser.signed_in(), "a password alone is half a sign-in");
    assert!(
        browser
            .cookies
            .contains_key(sc_server::SECOND_FACTOR_COOKIE)
    );

    // A member: signed in, and on to where they were going.
    let mut member = Browser::new(&fx.router).await;
    let signed = member.sign_in(MEMBER, PASSWORD, "/feed").await;
    assert_eq!(signed.status, StatusCode::SEE_OTHER);
    assert_eq!(signed.location.as_deref(), Some("/feed"));
    assert!(member.signed_in());
    assert!(
        signed
            .set_cookies
            .iter()
            .any(|c| c.starts_with(&format!("{SESSION_COOKIE}=")) && c.contains("HttpOnly")),
        "the host sets the session cookie: {:?}",
        signed.set_cookies
    );
    let feed = member.get("/feed").await;
    assert_eq!(feed.status, StatusCode::OK, "{}", feed.body);
    assert!(feed.body.contains(&format!(">{MEMBER}<")), "{}", feed.body);

    // `next` off this host is the home page.
    let mut elsewhere = Browser::new(&fx.router).await;
    let off = elsewhere
        .sign_in(MEMBER, PASSWORD, "https://evil.example/")
        .await;
    assert_eq!(off.location.as_deref(), Some("/"));
    let slashes = elsewhere.sign_in(MEMBER, PASSWORD, "//evil.example").await;
    assert_eq!(slashes.location.as_deref(), Some("/"));

    // Signing out ends the session.
    let csrf = member.csrf();
    let out = member
        .post_form("/_auth/sign-out", &[("_csrf", &csrf), ("next", "/bye")])
        .await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);
    assert_eq!(out.location.as_deref(), Some("/bye"));
    assert!(!member.signed_in());
    assert_eq!(member.get("/feed").await.status, StatusCode::FORBIDDEN);
    // Signing out is a form post: a link cannot do it.
    assert_eq!(
        member.get("/_auth/sign-out").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    Ok(())
}

#[tokio::test]
async fn a_form_posted_from_another_site_is_refused_before_anything_runs() -> sc_error::Result<()> {
    let fx = setup("cross-site").await?;
    let mut browser = Browser::new(&fx.router).await;
    // The browser sends its cookies; the other site cannot know the token.
    for path in [
        "/_auth/sign-in",
        "/_auth/sign-up",
        "/_fragment/PostCard?id=1",
    ] {
        let forged = browser
            .post_form(path, &[("email", MEMBER), ("password", PASSWORD)])
            .await;
        assert_eq!(forged.status, StatusCode::FORBIDDEN, "{path}");
        assert!(forged.body.contains("CSRF"), "{path}: {}", forged.body);
        assert!(!browser.signed_in());
    }
    let forged = browser
        .post_form(
            "/_auth/sign-in",
            &[
                ("_csrf", "guessed"),
                ("email", MEMBER),
                ("password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn sign_up_follows_the_rest_apis_settings() -> sc_error::Result<()> {
    let fx = setup("sign-up").await?;
    let mut browser = Browser::new(&fx.router).await;
    let csrf = browser.csrf();
    let up = |email: &'static str, password: &'static str, again: &'static str| {
        vec![
            ("_csrf", csrf.clone()),
            ("next", "/welcome".to_owned()),
            ("back", "/join".to_owned()),
            ("email", email.to_owned()),
            ("password", password.to_owned()),
            ("password_repeat", again.to_owned()),
        ]
    };
    let post = async |browser: &mut Browser, fields: Vec<(&str, String)>| {
        let fields: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
        browser.post_form("/_auth/sign-up", &fields).await
    };

    let exists = post(&mut browser, up(MEMBER, "x", "x")).await;
    assert_eq!(exists.location.as_deref(), Some("/join?sign_up=exists"));
    let mismatch = post(&mut browser, up("new@example.com", "one", "two")).await;
    assert_eq!(mismatch.location.as_deref(), Some("/join?sign_up=mismatch"));
    let blank = post(&mut browser, up("new@example.com", "", "")).await;
    assert_eq!(blank.location.as_deref(), Some("/join?sign_up=missing"));
    assert!(!browser.signed_in());

    let made = post(&mut browser, up("new@example.com", "fresh-pw", "fresh-pw")).await;
    assert_eq!(made.status, StatusCode::SEE_OTHER, "{}", made.body);
    assert_eq!(made.location.as_deref(), Some("/welcome"));
    assert!(browser.signed_in(), "signed in as the new account");
    let page = browser.get("/welcome").await;
    assert!(page.body.contains(">new@example.com<"), "{}", page.body);
    assert!(
        page.body.contains("<p class=\"signup\">true</p>"),
        "the page is told sign-up is offered: {}",
        page.body
    );
    let made = sc_auth::load_user_by_email(&fx.catalog, "new@example.com")
        .await?
        .unwrap();
    assert_eq!(made.role(), ROLE_MEMBER, "the role the REST settings give");
    Ok(())
}

// ---------------------------------------------------------------------------
// The real build, in a real browser
// ---------------------------------------------------------------------------

/// The node_modules the real build uses: `SC_ASTRO_NODE_MODULES`, or `None` to
/// install them (with `SC_TEST_NPM=1`), or skip.
fn node_modules() -> Option<Option<PathBuf>> {
    if let Ok(shared) = std::env::var("SC_ASTRO_NODE_MODULES") {
        return Some(Some(PathBuf::from(shared)));
    }
    (std::env::var("SC_TEST_NPM").as_deref() == Ok("1")).then_some(None)
}

/// Run `program` in `dir`, failing the test with its output if it fails.
fn run(dir: &Path, program: &str, args: &[&str]) {
    let out = std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn two_visitors_use_the_tutorials_site_in_chromium() -> sc_error::Result<()> {
    let Some(modules) = node_modules() else {
        eprintln!(
            "skipping: set SC_TEST_NPM=1 (or SC_ASTRO_NODE_MODULES) to build the site with npm"
        );
        return Ok(());
    };
    let Ok(chromium) = sc_server::detect_browser(None) else {
        eprintln!("skipping: no Chromium on PATH");
        return Ok(());
    };
    walk(modules, chromium).await
}

const CHIRP_HOST: &str = "chirp.example.com";
const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";

/// The `posts` read rule from the tutorial's Step 1.
const POSTS_RULE: &str = r#"audience === "everyone" || author === user.id"#;

/// The first ```astro block after `#### `{file}`` in the Chirp tutorial.
fn tutorial_file(file: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let doc = std::fs::read_to_string(root.join("docs/tutorial-astro-chirp.md")).unwrap();
    let heading = format!("#### `{file}`");
    let section = doc
        .split_once(&heading)
        .unwrap_or_else(|| panic!("the tutorial has `{heading}`"))
        .1;
    let block = section
        .split_once("```astro\n")
        .unwrap_or_else(|| panic!("`{heading}` shows an astro block"))
        .1;
    block.split_once("\n```").unwrap().0.to_owned() + "\n"
}

/// `fut`, or a failure naming the step if it takes over 20 seconds.
async fn bounded<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    tokio::time::timeout(std::time::Duration::from_secs(20), fut)
        .await
        .ok()
}

/// Say which step of the walk is running, and fail by name if it hung.
async fn step<T>(name: &str, fut: impl std::future::Future<Output = Option<T>>) -> T {
    eprintln!("chirp walk: {name}");
    fut.await
        .unwrap_or_else(|| panic!("chirp walk: `{name}` took over 20 seconds"))
}

/// The CSP the Astro module declares, as a new application gets it.
fn astro_csp(plugin: &Path) -> sc_app::CspPolicy {
    let out = std::process::Command::new("node")
        .args([
            "-e",
            "console.log(JSON.stringify(require(process.argv[1]).frameworks.astro.csp))",
            plugin.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let declared: serde_json::Map<String, Value> =
        serde_json::from_slice(&out.stdout).expect("the module's CSP");
    let mut csp = sc_app::CspPolicy::strict();
    for (directive, sources) in declared {
        let sources: Vec<String> = sources
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect();
        csp = csp.directive(directive, sources);
    }
    csp
}

/// The tutorial's world, built and served on a port: the two tables with
/// Step 1's rule, the stream, the application, the site from the tutorial's
/// files, and Alice's first five posts (the fifth only for her).
#[allow(clippy::too_many_lines)]
async fn walk(modules: Option<PathBuf>, chromium: PathBuf) -> sc_error::Result<()> {
    use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
    use chromiumoxide::cdp::browser_protocol::target::{
        CreateBrowserContextParams, CreateTargetParams,
    };
    use chromiumoxide::{Browser, BrowserConfig};
    use futures::StreamExt;
    use sc_catalog::{AccessRules, DataField, FileStoreId, TableMeta};
    use sc_stream::{Stream, StreamProvider, StreamRegistry, TableChanges, save_stream};
    use sc_types::{BasicType, TypeRef};
    use std::time::{Duration, Instant};

    let tmp = TempDir::new("chirp");
    let store_root = tmp.0.join("apps");
    std::fs::create_dir_all(&store_root).unwrap();
    let driver = Arc::new(SqliteDriver::open(tmp.0.join("chirp.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    tmp.1.add(catalog.teardown_hook());
    catalog.set_secret_key(SecretKey::generate()?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_code_libraries(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    catalog.connect_file_store(Arc::new(sc_files::LocalFileStore::new(
        "apps",
        &store_root,
    )?))?;
    save_role(&catalog, &RoleDef::new(ROLE_MEMBER, "Member")).await?;
    let alice = create_user(&catalog, ALICE, PASSWORD, ROLE_MEMBER)
        .await?
        .id();
    create_user(&catalog, BOB, PASSWORD, ROLE_MEMBER).await?;

    // Step 1: the tables, their rules, the stream.
    let text = |name: &str| DataField::plain(name, TypeRef::Basic(BasicType::Text));
    let id = || {
        DataField::plain("id", TypeRef::Basic(BasicType::Int))
            .required()
            .primary_key()
            .generated(sc_db::ColumnGenerator::Identity)
    };
    catalog
        .create_table(
            "posts",
            &[
                id(),
                text("title").required(),
                text("author"),
                text("audience"),
            ],
        )
        .await?;
    catalog
        .create_table(
            "likes",
            &[
                id(),
                DataField::plain("post", TypeRef::Basic(BasicType::Int)),
            ],
        )
        .await?;
    let mut posts = TableMeta::new("posts");
    posts.access = AccessRules {
        min_role_read: Role::ADMIN,
        min_role_write: ROLE_MEMBER,
    };
    posts.attributes.insert(
        sc_catalog::ATTR_OWNERSHIP_FORMULA.to_owned(),
        json!(POSTS_RULE),
    );
    sc_catalog::save_table_meta(&catalog, &posts).await?;
    let mut likes = TableMeta::new("likes");
    likes.access = AccessRules {
        min_role_read: ROLE_MEMBER,
        min_role_write: ROLE_MEMBER,
    };
    sc_catalog::save_table_meta(&catalog, &likes).await?;

    let evaluator = default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    let mut registry = StreamRegistry::new();
    registry.register(Arc::new(TableChanges) as Arc<dyn StreamProvider>)?;
    let registry = Arc::new(registry);
    let streams = sc_server::install_streams_with(
        &catalog,
        &dispatcher,
        registry.clone(),
        sc_stream::StreamConfig::default(),
    )
    .await?;
    let mut stream = Stream::new("posts_live", "table_changes").min_role(ROLE_MEMBER);
    stream.configuration = json!({ "table": "posts" }).as_object().cloned().unwrap();
    save_stream(&catalog, &registry, &stream).await?;
    streams.reload(&catalog).await?;
    let code = async |js: &str| {
        dispatcher
            .run_code(
                &catalog,
                "run_js_code",
                "test",
                js,
                sc_types::Attrs::new(),
                sc_catalog::Authority::system("test"),
                None,
                None,
            )
            .await
            .unwrap()
    };
    for n in 1..=4 {
        code(&format!(
            r#"await db.posts.insert({{ id: {n}, title: "Post {n}", author: "{alice}", audience: "everyone" }});"#
        ))
        .await;
    }
    code(&format!(
        r#"await db.posts.insert({{ id: 5, title: "Alice only", author: "{alice}", audience: "me" }});"#
    ))
    .await;

    // Step 2: the application, with the CSP the module declares.
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/astro/index.js");
    let mut rest = ApiConfig::new("rest", "/api");
    rest.config
        .insert(sc_api::REST_CFG_ALLOW_SIGNUP.to_owned(), json!(true));
    rest.config.insert(
        sc_api::REST_CFG_NEW_USER_ROLE.to_owned(),
        json!(i64::from(ROLE_MEMBER)),
    );
    let mut app = Application::new(
        "Chirp",
        "chirp",
        FrameworkRef::new("astro")
            .with("store", "apps")
            .with("project", "site"),
    )
    .with_file_store(FileStoreId("apps".to_owned()))
    .with_api(rest)
    .with_table("posts".to_owned())
    .with_table("likes".to_owned())
    .with_stream(sc_app::StreamRef::new("posts_live"));
    app.csp = astro_csp(&plugin);

    // The project: the module's scaffold, then the tutorial's files.
    let project = store_root.join("site");
    std::fs::create_dir_all(&project).unwrap();
    let script = r#"
const fs = require("fs"), path = require("path");
const [plugin, root] = process.argv.slice(1);
const ctx = {
  project: "site", name: "site", runtime: "src/feldspar", client: "src/feldspar/client.ts",
  auth: true, app: { name: "Chirp", subdomain: "chirp", description: "", url: "" },
  settings: { site: "" }, tables: [],
};
for (const f of require(plugin).frameworks.astro.scaffold(ctx)) {
  fs.mkdirSync(path.dirname(path.join(root, f.path)), { recursive: true });
  fs.writeFileSync(path.join(root, f.path), f.contents);
}"#;
    run(
        &project,
        "node",
        &[
            "-e",
            script,
            plugin.to_str().unwrap(),
            project.to_str().unwrap(),
        ],
    );
    for file in [
        "src/pages/login.astro",
        "src/pages/index.astro",
        "src/fragments/PostCard.astro",
        "src/fragments/Timeline.astro",
    ] {
        let target = project.join(file);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, tutorial_file(file)).unwrap();
    }
    // What a build writes before it runs npm: the typed client and its helper.
    let source = sc_app::AppSource::new(FileStoreId("apps".to_owned()), spec())
        .with_client("site/src/feldspar/client.ts");
    let endpoints = sc_app::app_endpoints_with(&app, &catalog, Some(&dispatcher))?;
    sc_app::emit_client(&catalog, &app, &source, &endpoints).await?;
    match modules {
        Some(shared) => std::os::unix::fs::symlink(shared, project.join("node_modules")).unwrap(),
        None => run(&project, "npm", &["install", "--no-audit", "--no-fund"]),
    }
    run(&project, "npm", &["run", "build"]);

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_base_domain(Some(BASE_DOMAIN.to_owned()))
            .with_evaluator(evaluator)
            .with_triggers(dispatcher.clone())
            .with_models(models)
            .with_streams(streams),
    );
    let framework = sc_app::mount_code_framework(
        &app,
        AssetBundle::from_dir(project.join("dist"))?,
        spec(),
        Some(dispatcher.clone()),
    )?;
    apps.mount(MountedApp::new_with(
        app,
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = router.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    let origin = format!("http://{CHIRP_HOST}:{}", addr.port());

    // The page carries the framework's policy, which the browser enforces.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/login")
                .header(header::HOST, CHIRP_HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let policy = response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(policy.contains("connect-src 'self'"), "{policy}");
    assert!(
        !policy.contains("script-src"),
        "scripts stay 'self': {policy}"
    );

    let config = BrowserConfig::builder()
        .chrome_executable(chromium)
        .no_sandbox()
        .user_data_dir(tmp.0.join("chromium"))
        .arg(("host-resolver-rules", "MAP *.example.com 127.0.0.1"))
        .arg("no-proxy-server")
        .arg("disable-gpu")
        .launch_timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let (browser, mut handler) = Browser::launch(config).await.expect("Chromium starts");
    let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let eval = async |page: &chromiumoxide::Page, js: &str| -> Value {
        page.evaluate(js)
            .await
            .ok()
            .and_then(|r| r.into_value::<Value>().ok())
            .unwrap_or(Value::Null)
    };
    let until = async |page: &chromiumoxide::Page, js: &str, want: Value| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let got = eval(page, js).await;
            if got == want {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "`{js}` never became {want}, last {got}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    let titles = "[...document.querySelectorAll('#timeline .post .title')].map(e => e.textContent)";
    // Every policy violation the page meets, from before its first script.
    const WATCH: &str = "window.__violations = []; document.addEventListener('securitypolicyviolation', \
        (e) => window.__violations.push(e.violatedDirective + ' ' + e.blockedURI));";

    // Each visitor signs in through the site's own <SignIn /> form.
    let mut pages = Vec::new();
    for email in [ALICE, BOB] {
        let context = browser
            .create_browser_context(CreateBrowserContextParams::default())
            .await
            .unwrap();
        let page = browser
            .new_page(
                CreateTargetParams::builder()
                    .url("about:blank")
                    .browser_context_id(context)
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap();
        page.evaluate_on_new_document(AddScriptToEvaluateOnNewDocumentParams::new(WATCH))
            .await
            .unwrap();
        step("sign-in page", bounded(page.goto(format!("{origin}/"))))
            .await
            .unwrap();
        until(&page, "location.pathname", json!("/login")).await;
        eval(
            &page,
            &format!(
                "(() => {{ const f = document.querySelector('form[action=\"/_auth/sign-in\"]'); \
                 f.email.value = {email:?}; f.password.value = {PASSWORD:?}; \
                 HTMLFormElement.prototype.submit.call(f); return true; }})()"
            ),
        )
        .await;
        until(&page, "location.pathname", json!("/")).await;
        until(
            &page,
            "document.querySelector('#timeline')?.dataset.liveStatus",
            json!("live"),
        )
        .await;
        pages.push(page);
    }
    let (alice_page, bob_page) = (&pages[0], &pages[1]);
    until(
        alice_page,
        titles,
        json!(["Alice only", "Post 4", "Post 3"]),
    )
    .await;
    until(bob_page, titles, json!(["Post 4", "Post 3", "Post 2"])).await;

    eprintln!("chirp walk: load more");
    // "Load more", through the Timeline fragment.
    eval(bob_page, "document.querySelector('#more').click()").await;
    until(
        bob_page,
        titles,
        json!(["Post 4", "Post 3", "Post 2", "Post 1"]),
    )
    .await;
    until(
        bob_page,
        "document.querySelector('#more') === null",
        json!(true),
    )
    .await;

    eprintln!("chirp walk: a like");
    // A like: the card is replaced by the fragment's answer.
    eval(bob_page, "document.querySelector('#post-4 .like').click()").await;
    until(
        bob_page,
        "document.querySelector('#post-4 .like').textContent",
        json!("Like (1)"),
    )
    .await;

    eprintln!("chirp walk: a post reaches the other visitor");
    // W4.1: Alice posts through the page's <script> and the generated client;
    // the new post reaches Bob's timeline through <Live>, and his script's own
    // subscription hears it too.
    let compose = async |page: &chromiumoxide::Page, title: &str, audience: &str| {
        eval(
            page,
            &format!(
                "(() => {{ const f = document.querySelector('#compose'); f.title.value = {title:?}; \
                 f.audience.value = {audience:?}; f.requestSubmit(); return true; }})()"
            ),
        )
        .await;
    };
    compose(alice_page, "Hello from Alice", "everyone").await;
    until(bob_page, &format!("{titles}[0]"), json!("Hello from Alice")).await;
    until(
        alice_page,
        &format!("{titles}[0]"),
        json!("Hello from Alice"),
    )
    .await;
    until(bob_page, "document.body.dataset.lastPost", json!("6")).await;

    eprintln!("chirp walk: a private post");
    // A post Bob may not read never reaches his page: Alice's next public post
    // does, after it, and the private one is nowhere.
    compose(alice_page, "Just for me", "me").await;
    until(alice_page, &format!("{titles}[0]"), json!("Just for me")).await;
    compose(alice_page, "Back to everyone", "everyone").await;
    until(bob_page, &format!("{titles}[0]"), json!("Back to everyone")).await;
    assert_eq!(
        eval(bob_page, &format!("{titles}.includes('Just for me')")).await,
        json!(false)
    );
    assert_eq!(
        eval(bob_page, "document.body.innerHTML.includes('Just for me')").await,
        json!(false)
    );

    eprintln!("chirp walk: update and delete");
    // An update replaces the card in place; a delete removes it.
    let count = format!("{titles}.length");
    let before = eval(bob_page, &count).await;
    code(r#"await db.posts.where({ id: 6 }).update({ title: "Edited by Alice" });"#).await;
    until(
        bob_page,
        &format!("{titles}.includes('Edited by Alice')"),
        json!(true),
    )
    .await;
    assert_eq!(eval(bob_page, &count).await, before, "replaced, not added");
    code("await db.posts.where({ id: 6 }).delete();").await;
    until(
        bob_page,
        &format!("{titles}.includes('Edited by Alice')"),
        json!(false),
    )
    .await;

    eprintln!("chirp walk: the policy");
    // The strict policy stood throughout: no violation, and the socket is live.
    for page in &pages {
        assert_eq!(eval(page, "window.__violations").await, json!([]));
        assert_eq!(
            eval(
                page,
                "document.querySelector('#timeline').dataset.liveStatus"
            )
            .await,
            json!("live")
        );
    }

    eprintln!("chirp walk: JavaScript off");
    // With JavaScript off, the same page works as forms and links. A browser
    // with no JavaScript does two things here: it follows a link's `href`, and
    // it posts a form as it is. Done as plain requests carrying Bob's browser
    // cookies: Chromium with scripts disabled creates no script context, which
    // the CDP calls for clicking and reading a page wait on for ever.
    let jar = bob_page
        .get_cookies()
        .await
        .unwrap()
        .into_iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ");
    let csrf = bob_page
        .get_cookies()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.name == CSRF_COOKIE)
        .expect("Bob's CSRF cookie")
        .value;
    let plain = async |method: &str, path: &str, form: Option<String>| {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, CHIRP_HOST)
            .header(header::COOKIE, jar.clone())
            .header(header::REFERER, format!("http://{CHIRP_HOST}/"));
        if form.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(form.unwrap_or_default())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            location,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    };
    let (status, _, home) = plain("GET", "/", None).await;
    assert_eq!(status, StatusCode::OK, "{home}");
    // The "load more" link is an ordinary link to the next page.
    let href = home
        .split("id=\"more\" href=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("a 'load more' link: {home}"))
        .replace("&amp;", "&");
    assert!(href.starts_with("/?after="), "{href}");
    let (status, _, next) = plain("GET", &href, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(next.contains(">Post 1<"), "{next}");
    // The like is an ordinary form post to the fragment: the work is done and
    // the browser is sent back to the page, which shows it.
    let action = next
        .split("id=\"post-1\"")
        .nth(1)
        .and_then(|rest| rest.split("action=\"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("post 1's like form: {next}"))
        .replace("&amp;", "&");
    let (status, location, _) = plain(
        "POST",
        &action,
        Some(format!(
            "_csrf={csrf}&_back={}",
            href.replace('?', "%3F").replace('=', "%3D")
        )),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some(href.as_str()), "back to the page");
    let (_, _, again) = plain("GET", &href, None).await;
    let card = again.split("id=\"post-1\"").nth(1).unwrap();
    assert!(card.contains("Like (1)"), "{card}");
    let liked = code("return await db.likes.where({ post: 1 }).count();").await;
    assert_eq!(liked, json!(1));

    drop(pages);
    drop(browser);
    pump.abort();
    server.abort();
    Ok(())
}
