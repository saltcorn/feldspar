//! A module's consent flow (TODO-imap M10.3) end to end through the assembled
//! router, against a fake OAuth 2.0 provider on a real port: an action's
//! `oauth.begin(…)` → `/_feldspar/oauth/consent/{id}` in the caller's browser →
//! the provider → the shared callback → the completion, which writes the
//! tokens into the module's row, sealed, and sends the browser back.
//!
//! And the refusals: a flow taken up by anybody but the caller, and one
//! finished in another browser, write nothing.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use sc_action::{ActionContext, Event, EventKind};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ModuleServices, OAUTH_BINDING_COOKIE, ServerConfig,
    admin_handlers, build_router_with_apps, default_js_evaluator, install_agents, install_triggers,
};
use sc_test_harness::TestDb;
use sc_types::Attrs;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const HOST: &str = "localhost";
const ADMIN_EMAIL: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sc-module/tests/fixtures")
        .join(name)
}

fn have_npm() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One browser: its cookies, and the last response's `Location`.
struct Browser {
    router: Router,
    cookies: HashMap<String, String>,
    location: Option<String>,
}

impl Browser {
    fn new(router: Router) -> Browser {
        Browser {
            router,
            cookies: HashMap::new(),
            location: None,
        }
    }

    async fn send(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, HOST);
        if !self.cookies.is_empty() {
            let jar = (self.cookies.iter())
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, jar);
        }
        if method != "GET"
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some(b) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        self.location = response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        for line in response.headers().get_all(header::SET_COOKIE) {
            let pair = line.to_str().unwrap().split(';').next().unwrap_or("");
            if let Some((name, value)) = pair.split_once('=') {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn query_of(url: &str) -> HashMap<String, String> {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

/// A fake provider's token endpoint: checks the client secret and PKCE
/// against the challenge the test saw, and grants tokens named after the code.
async fn fake_provider(challenge: Arc<Mutex<Option<String>>>) -> String {
    let token = move |body: String| {
        let challenge = challenge.clone();
        async move {
            let form = query_of(&format!("?{body}"));
            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
            let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes()));
            if form.get("client_secret").map(String::as_str) != Some("csecret")
                || challenge.lock().unwrap().as_deref() != Some(computed.as_str())
            {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "invalid_grant" })),
                );
            }
            let code = form.get("code").cloned().unwrap_or_default();
            (
                StatusCode::OK,
                axum::Json(json!({
                    "access_token": format!("at-{code}"),
                    "refresh_token": format!("rt-{code}"),
                    "expires_in": 3600,
                    "token_type": "Bearer",
                })),
            )
        }
    };
    let app = Router::new().route("/token", axum::routing::post(token));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

struct Server {
    router: Router,
    catalog: Arc<Catalog>,
    dispatcher: Arc<sc_action::TriggerDispatcher>,
    root: PathBuf,
    db: TestDb,
    _registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn setup() -> sc_error::Result<(Server, Browser)> {
    let db = TestDb::new().await?;
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
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    // Where a field is recorded as Secret, as `feldspar serve` makes it.
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!("sc-module-consent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let registries = crate::module_registries().read().await;
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.clone()),
        None,
        1,
        sc_server::default_python_adapter(),
        None,
    )
    .await?;
    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_evaluator(default_js_evaluator())
            .with_triggers(dispatcher.clone())
            .with_agents(agents)
            .with_modules(modules.clone()),
    );
    let router = build_router_with_apps(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &ServerConfig::default(),
        apps,
    )?;
    let mut admin = Browser::new(router.clone());
    admin.send("GET", "/api/auth/status", None).await;
    let (status, body) = admin
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN_EMAIL, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = admin
        .send(
            "POST",
            "/api/modules",
            Some(json!({
                "source": "local",
                "location": fixture("oauth-module").display().to_string(),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Ok((
        Server {
            router,
            catalog,
            dispatcher,
            root,
            db,
            _registries: registries,
        },
        admin,
    ))
}

/// Run the fixture's `connect` action as `user`: the consent URL it answers.
async fn begin(server: &Server, user: &sc_auth::User, provider: &str) -> String {
    let event = Event::new(EventKind::None)
        .payload(json!({ "account_id": 1, "provider": provider }))
        .caller(1, Some(json!({ "id": user.id.to_string(), "role": 1 })));
    let config = Attrs::new();
    let registry = server.dispatcher.registry();
    let action = registry.require("connect").unwrap();
    let mut ctx = ActionContext::new(&server.catalog, &event, &config, "connect");
    let begun = action.run(&mut ctx).await.unwrap();
    begun["url"]
        .as_str()
        .unwrap_or_else(|| panic!("{begun}"))
        .to_owned()
}

/// Take up `url` in `browser` and answer the provider for it with `code`:
/// the address the callback sends the browser on to.
async fn consent_at_provider(
    browser: &mut Browser,
    url: &str,
    code: &str,
    challenge: &Arc<Mutex<Option<String>>>,
    provider: &str,
) -> String {
    let (status, _) = browser.send("GET", url, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let to_provider = browser.location.clone().unwrap();
    assert!(
        to_provider.starts_with(&format!("{provider}/authorize?")),
        "{to_provider}"
    );
    assert!(browser.cookies.contains_key(OAUTH_BINDING_COOKIE));
    let params = query_of(&to_provider);
    assert_eq!(params["client_id"], "cid");
    assert_eq!(params["scope"], "mail offline_access");
    assert_eq!(
        params["login_hint"], "ada@outlook.com",
        "the module's own parameters"
    );
    assert_eq!(params["prompt"], "select_account");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(
        params["redirect_uri"],
        format!("http://{HOST}/_feldspar/oauth/callback"),
        "the same callback a sign-in uses"
    );
    *challenge.lock().unwrap() = Some(params["code_challenge"].clone());
    let (status, _) = browser
        .send(
            "GET",
            &format!(
                "/_feldspar/oauth/callback?code={code}&state={}",
                params["state"]
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let back = browser.location.clone().unwrap();
    back.strip_prefix(&format!("http://{HOST}"))
        .unwrap_or_else(|| panic!("{back}"))
        .to_owned()
}

#[tokio::test]
async fn a_modules_consent_flow_stores_the_tokens_in_its_row() -> sc_error::Result<()> {
    if !have_npm() {
        eprintln!("skipping: npm is not on the PATH");
        return Ok(());
    }
    let (server, mut admin) = setup().await?;
    let user = sc_auth::load_user_by_email(&server.catalog, ADMIN_EMAIL)
        .await?
        .unwrap();
    server
        .db
        .client()
        .await?
        .batch_execute(&format!(
            "INSERT INTO oa_accounts (owner, email, auth) VALUES ('{}', 'ada@outlook.com', 'password')",
            user.id
        ))
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let challenge = Arc::new(Mutex::new(None));
    let provider = fake_provider(challenge.clone()).await;
    let stored = || async {
        server
            .db
            .client()
            .await
            .unwrap()
            .query_one(
                "SELECT access_token, refresh_token, expires_at IS NOT NULL, auth FROM oa_accounts",
                &[],
            )
            .await
            .map(|r| {
                (
                    r.get::<_, Option<String>>(0),
                    r.get::<_, Option<String>>(1),
                    r.get::<_, bool>(2),
                    r.get::<_, Option<String>>(3),
                )
            })
            .unwrap()
    };

    // Taken up by a browser that is not the caller's: refused, nothing kept.
    let url = begin(&server, &user, &provider).await;
    let mut stranger = Browser::new(server.router.clone());
    let (status, _) = stranger.send("GET", &url, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let refused = stranger.location.clone().unwrap();
    assert!(
        refused.starts_with("/settings/mail?oauth_error="),
        "{refused}"
    );
    assert!(
        query_of(&refused)["oauth_error"].contains("sign in as the user"),
        "{refused}"
    );
    assert!(!stranger.cookies.contains_key(OAUTH_BINDING_COOKIE));
    let (status, _) = admin.send("GET", &url, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        admin.location.as_deref().unwrap().contains("oauth_error="),
        "a flow is taken up once: {:?}",
        admin.location
    );

    // Finished in another browser than the one that started it: refused.
    let url = begin(&server, &user, &provider).await;
    let complete = consent_at_provider(&mut admin, &url, "one", &challenge, &provider).await;
    assert!(
        complete.starts_with("/_feldspar/oauth/complete?token="),
        "{complete}"
    );
    let (_, _) = stranger.send("GET", &complete, None).await;
    let refused = stranger.location.clone().unwrap();
    assert!(
        query_of(&refused)["oauth_error"].contains("started in another browser"),
        "{refused}"
    );
    assert_eq!(stored().await, (None, None, false, Some("password".into())));

    // The caller's own browser, start to finish.
    let url = begin(&server, &user, &provider).await;
    let complete = consent_at_provider(&mut admin, &url, "two", &challenge, &provider).await;
    let (status, page) = admin.send("GET", &complete, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains("url=/settings/mail\""),
        "back to the application, with no error: {page}"
    );
    assert!(
        !admin.cookies.contains_key(OAUTH_BINDING_COOKIE),
        "the binding is spent"
    );
    let (access, refresh, expires, auth) = stored().await;
    assert_eq!(auth.as_deref(), Some("oauth2"), "the module's `set`");
    assert!(expires, "the expiry from `expires_in`");
    // Sealed at rest, as every Secret field is…
    for sealed in [&access, &refresh] {
        let sealed = sealed.as_deref().unwrap();
        assert!(!sealed.contains("-two"), "{sealed}");
    }
    // …and revealed to the server's own reader.
    let table = server.catalog.require("oa_accounts")?;
    let rows = sc_api::rows::list_rows(&server.catalog, &table).await?;
    let mut revealed = rows[0].clone();
    sc_catalog::secrets::reveal_json(&server.catalog, &table, &mut revealed)?;
    assert_eq!(revealed["access_token"], json!("at-two"));
    assert_eq!(revealed["refresh_token"], json!("rt-two"));
    Ok(())
}
