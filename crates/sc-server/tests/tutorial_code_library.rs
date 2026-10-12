//! `docs/tutorial-code-library.md`, run (TODO F3.5): the library and the two
//! queries the tutorial prints, **read out of the tutorial itself**, served by
//! a real router over a real database — so the page cannot drift from what the
//! server does.
//!
//! What it walks: an instance library `auth` saved through the admin API (step
//! 1), imported by a query in each of two applications (step 2) — 401 with the
//! library's message signed out, the caller's email and an argon2id hash signed
//! in — the "used by" the screen shows, and step 3's one edit changing what
//! both applications answer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::{CustomQuery, Method, QueryLanguage, set_custom_queries};
use sc_app::{ApiConfig, Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_auth::{ROLE_ADMIN, ROLE_PUBLIC, SessionStore, create_user};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_sqlite::SqliteDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, ServerConfig, admin_handlers,
    build_router_with_apps, default_js_evaluator, install_triggers,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const PASSWORD: &str = "correct-horse";
const BEFORE: &str = "Sign in to continue";
const AFTER: &str = "Please sign in first";

const TUTORIAL: &str = include_str!("../../../docs/tutorial-code-library.md");

/// The `js` blocks of one step, in order.
fn blocks(step: &str) -> Vec<String> {
    let section = TUTORIAL
        .split(&format!("## {step}"))
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap_or_else(|| panic!("the tutorial has `{step}`"));
    section
        .split("```js\n")
        .skip(1)
        .map(|b| b.split("\n```").next().expect("a closed block").to_owned())
        .collect()
}

/// A browser on one host: cookies, and the CSRF header a page sends.
struct Client {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
}

impl Client {
    fn new(router: &Router, host: &str) -> Client {
        Client {
            router: router.clone(),
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
        if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let body = match body {
            Some(json) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&json).unwrap())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Ok(text) = raw.to_str()
                && let Some((name, value)) = text.split(';').next().unwrap_or("").split_once('=')
            {
                self.cookies.insert(name.to_owned(), value.to_owned());
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn login(&mut self, email: &str) {
        self.send("GET", "/", None).await;
        let (status, body) = self
            .send(
                "POST",
                "/api/login",
                Some(json!({ "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

fn app(name: &str, subdomain: &str, query: CustomQuery) -> Application {
    let mut api = ApiConfig::new("rest", "/api");
    set_custom_queries(&mut api.config, &[query]).unwrap();
    let framework = FrameworkRef::new("code")
        .with("store", "files")
        .with("source", subdomain)
        .with("output", format!("{subdomain}/dist"))
        .with("command", "true");
    Application::new(name, subdomain, framework)
        .with_file_store(sc_catalog::FileStoreId("files".to_owned()))
        .with_api(api)
}

async fn setup(dir: &std::path::Path) -> sc_error::Result<Router> {
    let driver = Arc::new(SqliteDriver::open(dir.join("tutorial.db"))?);
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_catalog::bootstrap_code_libraries(&catalog).await?;
    sc_catalog::save_file_store(
        &catalog,
        &sc_files::FileStoreDef::local("files", dir.to_string_lossy()),
    )
    .await?;
    create_user(&catalog, "admin@example.com", PASSWORD, ROLE_ADMIN).await?;
    sc_auth::save_role(&catalog, &sc_auth::Role::new(80, "Reader")).await?;
    create_user(&catalog, "you@example.com", PASSWORD, 80).await?;

    let step2 = blocks("Step 2");
    let query = |name: &str, method: Method, path: &str, code: &str| {
        CustomQuery::new(name, method, path, code)
            .language(QueryLanguage::Javascript)
            .min_role(ROLE_PUBLIC)
    };
    let todo = sc_app::save_application(
        &catalog,
        &app("Todo", "todo", query("me", Method::Get, "/me", &step2[0])),
    )
    .await?;
    let blog = sc_app::save_application(
        &catalog,
        &app(
            "Blog",
            "blog",
            query("newShare", Method::Post, "/shares", &step2[1]),
        ),
    )
    .await?;

    let evaluator = default_js_evaluator();
    let agents = sc_server::install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, evaluator.clone(), &agents, &models).await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(evaluator)
            .with_triggers(dispatcher),
    );
    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    for application in [todo, blog] {
        apps.mount(MountedApp::new_with(
            application,
            framework.clone(),
            &catalog,
            apps.evaluator(),
            apps.triggers(),
        )?)?;
    }
    build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig {
            base_domain: Some("example.com".to_owned()),
            ..ServerConfig::default()
        },
        apps,
    )
}

/// Step 2's call on `host`, signed out.
async fn signed_out(router: &Router, host: &str) -> (StatusCode, Value) {
    let mut anon = Client::new(router, host);
    if host.starts_with("todo.") {
        anon.send("GET", "/api/me", None).await
    } else {
        anon.send(
            "POST",
            "/api/shares",
            Some(json!({ "password": "long enough" })),
        )
        .await
    }
}

#[tokio::test]
async fn the_code_library_tutorial_runs_as_printed() -> sc_error::Result<()> {
    let dir = std::env::temp_dir().join(format!("sc-tutorial-code-library-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).map_err(sc_error::Error::from)?;
    let router = setup(&dir).await?;

    // Step 1: the library, saved as the screen saves it.
    let library = blocks("Step 1").remove(0);
    assert!(library.contains(BEFORE), "step 3 edits this message");
    assert!(TUTORIAL.contains(&format!("`\"{BEFORE}\"` to `\"{AFTER}\"`")));
    let mut admin = Client::new(&router, "example.com");
    admin.login("admin@example.com").await;
    let (status, created) = admin
        .send(
            "POST",
            "/api/code-libraries",
            Some(json!({ "name": "auth", "code": library })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    // A typo is refused with its line, and nothing is saved.
    let (status, refused) = admin
        .send(
            "POST",
            "/api/code-libraries",
            Some(json!({ "name": "typo", "code": "exports.a = 1;\nexports.b = (;" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(refused.to_string().contains("line 2"), "{refused}");

    // Step 2: signed out, both applications answer the library's 401.
    for host in ["todo.example.com", "blog.example.com"] {
        let (status, body) = signed_out(&router, host).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{host}: {body}");
        assert_eq!(body["error"], json!(BEFORE), "{host}");
    }
    // Signed in: the caller's own email, and a hash.
    let mut you = Client::new(&router, "todo.example.com");
    you.login("you@example.com").await;
    let (status, body) = you.send("GET", "/api/me", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["email"], json!("you@example.com"));
    let mut you = Client::new(&router, "blog.example.com");
    you.login("you@example.com").await;
    let (status, body) = you
        .send(
            "POST",
            "/api/shares",
            Some(json!({ "password": "long enough" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["hash"].as_str().unwrap().starts_with("$argon2id$"),
        "{body}"
    );
    let (status, body) = you
        .send("POST", "/api/shares", Some(json!({ "password": "short" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The screen's "used by".
    let (_, listed) = admin.send("GET", "/api/code-libraries", None).await;
    let used: Vec<String> = listed[0]["used_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            format!(
                "{} in {}",
                u["name"].as_str().unwrap(),
                u["application"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(used, vec!["newShare in blog", "me in todo"]);

    // Step 3: one edit, and both applications answer the new message.
    let edited = library.replace(BEFORE, AFTER);
    let (status, body) = admin
        .send(
            "PUT",
            &format!("/api/code-libraries/{id}"),
            Some(json!({ "name": "auth", "code": edited })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for host in ["todo.example.com", "blog.example.com"] {
        let (status, body) = signed_out(&router, host).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{host}");
        assert_eq!(body["error"], json!(AFTER), "{host}");
    }
    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
