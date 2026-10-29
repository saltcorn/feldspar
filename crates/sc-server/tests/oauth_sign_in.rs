//! Signing in with an external identity provider (Settings → Authentication),
//! end to end through the assembled router, against a fake OAuth 2.0 provider
//! listening on a real port.
//!
//! What is pinned here:
//!
//! - an admin creates, lists (secret redacted), edits and deletes providers;
//! - an application's sign-in runs start → provider → callback (on the admin
//!   UI's host) → complete (on the application's), checks PKCE and the client
//!   secret at the token endpoint, creates the account when the provider says
//!   to, and leaves the browser with a session for it;
//! - the flow must finish in the browser that started it;
//! - on the admin UI's host only an administrator is signed in;
//! - Settings → Authentication's session lifetime and shared cookie apply to
//!   these sessions as they do to a password's.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use sc_app::{
    ApiConfig, Application, CodeFramework, FrameworkRef, app_source_from_config, build_application,
};
use sc_auth::{SessionStore, load_user_by_email};
use sc_catalog::{Catalog, FileStoreId, TableId};
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::LocalFileStore;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, MountedApp, OAUTH_BINDING_COOKIE, SESSION_COOKIE,
    ServerConfig, admin_handlers, build_router_with_apps,
};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "blog.example.com";
const ADMIN_EMAIL: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";
const CLIENT_SECRET: &str = "very-secret";

/// A scratch directory removed when the guard drops.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-oauth-{}-{tag}-{:?}",
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

/// One browser on one host: its cookies by name, and the last response's
/// `Location` and `Set-Cookie` lines.
struct Browser {
    router: Router,
    host: String,
    cookies: HashMap<String, String>,
    location: Option<String>,
    set_cookie: Vec<String>,
    headers: Vec<(String, String)>,
}

impl Browser {
    fn new(router: Router, host: &str) -> Browser {
        Browser {
            router,
            host: host.to_owned(),
            cookies: HashMap::new(),
            location: None,
            set_cookie: Vec::new(),
            headers: Vec::new(),
        }
    }

    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (status, bytes) = self.raw(method, path, body).await;
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
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
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
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
        self.set_cookie = (response.headers().get_all(header::SET_COOKIE).iter())
            .map(|raw| raw.to_str().unwrap().to_owned())
            .collect();
        for line in &self.set_cookie {
            let pair = line.split(';').next().unwrap_or("");
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
        (status, bytes.to_vec())
    }

    /// The `Set-Cookie` line of the last response that set the session.
    fn session_cookie_line(&self) -> Option<&String> {
        self.set_cookie
            .iter()
            .find(|l| l.starts_with(&format!("{SESSION_COOKIE}=")) && !l.contains("=;"))
    }

    async fn whoami(&mut self) -> Option<String> {
        let (status, body) = self.send("GET", "/api/whoami", None).await;
        (status == StatusCode::OK).then(|| body["email"].as_str().unwrap().to_owned())
    }

    async fn admin_user(&mut self) -> Option<String> {
        let (_, body) = self.send("GET", "/api/auth/status", None).await;
        body["current_user"]["email"].as_str().map(str::to_owned)
    }
}

/// The query of a URL, decoded.
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

/// What the fake provider saw at its token endpoint.
#[derive(Default)]
struct Seen {
    /// The PKCE challenge the authorization request carried, set by the test.
    challenge: Option<String>,
    /// Every token request's form.
    token_requests: Vec<HashMap<String, String>>,
}

/// A fake OAuth 2.0 provider: the code *is* the email it signs in, the token
/// endpoint checks the client secret and PKCE, and userinfo answers the email.
async fn fake_provider(seen: Arc<Mutex<Seen>>) -> String {
    let token = {
        let seen = seen.clone();
        move |body: String| {
            let seen = seen.clone();
            async move {
                let form = query_of(&format!("?{body}"));
                let mut seen = seen.lock().unwrap();
                seen.token_requests.push(form.clone());
                let verifier = form.get("code_verifier").cloned().unwrap_or_default();
                let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(Sha256::digest(verifier.as_bytes()));
                if form.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
                    || seen.challenge.as_deref() != Some(challenge.as_str())
                {
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({ "error": "invalid_grant" })),
                    );
                }
                let code = form.get("code").cloned().unwrap_or_default();
                (
                    StatusCode::OK,
                    axum::Json(
                        json!({ "access_token": format!("tok:{code}"), "token_type": "bearer" }),
                    ),
                )
            }
        }
    };
    let userinfo = |headers: axum::http::HeaderMap| async move {
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let email = auth
            .strip_prefix("Bearer tok:")
            .unwrap_or_default()
            .to_owned();
        // `silent…` gets no `email_verified` at all, as Microsoft's userinfo.
        if email.starts_with("silent") {
            return axum::Json(json!({ "sub": "1", "email": email }));
        }
        let verified = !email.starts_with("unverified");
        axum::Json(json!({ "sub": "1", "email": email, "email_verified": verified }))
    };
    let app = Router::new()
        .route("/token", axum::routing::post(token))
        .route("/userinfo", axum::routing::get(userinfo));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
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
         mkdir -p dist/assets\n\
         printf '<!doctype html><div id=root></div>' > dist/index.html\n",
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

/// A router serving the admin UI on the base domain and a blog application on
/// its subdomain, and an admin signed in on the admin UI.
async fn setup(
    tmp: &TempDir,
) -> sc_error::Result<(Router, Arc<Catalog>, Arc<AppMounts>, Browser, TestDb)> {
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
               title text not null)",
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;

    write_app_source(tmp.path());
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", tmp.path())?))?;
    let app = Application::new("Blog", "blog", code_framework())
        .with_table(TableId("posts".to_owned()))
        .with_file_store(FileStoreId("apps".to_owned()))
        .with_api(ApiConfig::new("rest", "/api"));
    let source = app_source_from_config(&code_framework())?;
    let report = build_application(&catalog, &app, &source, None).await?;
    let framework = Arc::new(CodeFramework::new("code", report.bundle));
    let apps =
        Arc::new(AppMounts::new(catalog.clone()).with_base_domain(Some(BASE_DOMAIN.to_owned())));
    apps.mount(MountedApp::new(app, framework, &catalog)?)?;

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
    let mut admin = Browser::new(router.clone(), BASE_DOMAIN);
    admin.send("GET", "/api/auth/status", None).await;
    let (status, body) = admin
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN_EMAIL, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Ok((router, catalog, apps, admin, db))
}

/// Run one sign-in for `email`, started (and finished) by `browser`, with the
/// provider's redirect handled by a fresh browser on the admin UI's host.
/// Returns the status of the completing request.
async fn sign_in(
    router: &Router,
    browser: &mut Browser,
    seen: &Arc<Mutex<Seen>>,
    email: &str,
    return_to: &str,
) -> (StatusCode, Option<String>) {
    let (status, _) = browser
        .raw(
            "GET",
            &format!("/_feldspar/oauth/corp/start?return_to={return_to}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let to_provider = browser.location.clone().unwrap();
    assert!(browser.cookies.contains_key(OAUTH_BINDING_COOKIE));
    let params = query_of(&to_provider);
    assert_eq!(params["client_id"], "cid");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(
        params["redirect_uri"],
        format!("http://{BASE_DOMAIN}/_feldspar/oauth/callback"),
        "one callback, on the admin UI's host"
    );
    seen.lock().unwrap().challenge = Some(params["code_challenge"].clone());

    let mut at_callback = Browser::new(router.clone(), BASE_DOMAIN);
    let (status, _) = at_callback
        .raw(
            "GET",
            &format!(
                "/_feldspar/oauth/callback?code={email}&state={}",
                params["state"]
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let back = at_callback.location.clone().unwrap();
    let prefix = format!("http://{}", browser.host);
    let Some(path) = back.strip_prefix(&prefix) else {
        panic!("{back} is not on {prefix}");
    };
    if !path.starts_with("/_feldspar/oauth/complete") {
        return (status, Some(path.to_owned()));
    }
    let (status, _) = browser.raw("GET", path, None).await;
    (status, browser.location.clone())
}

#[tokio::test]
async fn an_application_and_the_admin_ui_sign_in_with_an_identity_provider() -> sc_error::Result<()>
{
    let tmp = TempDir::new("flow");
    let (router, catalog, apps, mut admin, _db) = setup(&tmp).await?;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let provider_url = fake_provider(seen.clone()).await;

    // --- the admin's list ------------------------------------------------
    let (status, body) = admin
        .send(
            "POST",
            "/api/oauth-providers",
            Some(json!({ "config": {
                "preset": "custom", "name": "corp", "label": "Corp SSO",
                "client_id": "cid", "client_secret": CLIENT_SECRET,
                "authorization_url": format!("{provider_url}/authorize"),
                "token_url": format!("{provider_url}/token"),
                "userinfo_url": format!("{provider_url}/userinfo"),
                "create_users": true, "new_user_role": 100,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["config"]["client_secret"], sc_types::SECRET_SENTINEL);

    let (_, list) = admin.send("GET", "/api/oauth-providers", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["name"], "corp");
    assert_eq!(
        list[0]["config"]["client_secret"],
        sc_types::SECRET_SENTINEL
    );

    // An edit that sends the sentinel back keeps the secret.
    let mut config = list[0]["config"].clone();
    config["label"] = json!("Corp");
    let (status, body) = admin
        .send(
            "PUT",
            "/api/oauth-providers/corp",
            Some(json!({ "config": config })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["label"], "Corp");
    let stored = sc_config::oauth_provider(&catalog, "corp").await?.unwrap();
    assert_eq!(stored.client_secret, CLIENT_SECRET);

    // A second provider with the same name is refused.
    let (status, _) = admin
        .send(
            "POST",
            "/api/oauth-providers",
            Some(json!({ "config": config })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // --- an application's sign-in ----------------------------------------
    let mut blog = Browser::new(router.clone(), APP_HOST);
    let (status, offered) = blog.send("GET", "/_feldspar/oauth/providers", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        offered,
        json!([{ "name": "corp", "label": "Corp", "url": "/_feldspar/oauth/corp/start" }])
    );

    let (status, _) = sign_in(&router, &mut blog, &seen, "new@example.com", "/orders").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        blog.session_cookie_line().is_some(),
        "{:?}",
        blog.set_cookie
    );
    assert!(
        !blog.cookies.contains_key(OAUTH_BINDING_COOKIE),
        "the binding cookie is spent"
    );
    assert_eq!(blog.whoami().await.as_deref(), Some("new@example.com"));
    let created = load_user_by_email(&catalog, "new@example.com")
        .await?
        .unwrap();
    assert_eq!(created.role, 100);
    let token_request = seen.lock().unwrap().token_requests.last().cloned().unwrap();
    assert_eq!(token_request["grant_type"], "authorization_code");

    // An account a signup made for somebody else's address (security review
    // #41): the provider's sign-in for that address claims it. Whoever signed
    // up loses their password and their session, and the address is proven.
    let squatted = sc_auth::sign_up_user(&catalog, "victim@example.com", "squat-pw", 100).await?;
    assert!(squatted.email_unproven());
    let mut squatter = Browser::new(router.clone(), APP_HOST);
    squatter.send("GET", "/", None).await;
    let (status, body) = squatter
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "victim@example.com", "password": "squat-pw" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        squatter.whoami().await.as_deref(),
        Some("victim@example.com")
    );
    let mut holder = Browser::new(router.clone(), APP_HOST);
    let (status, _) = sign_in(&router, &mut holder, &seen, "victim@example.com", "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(holder.whoami().await.as_deref(), Some("victim@example.com"));
    assert_eq!(
        squatter.whoami().await,
        None,
        "the squatter's session is gone"
    );
    let (status, _) = squatter
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "victim@example.com", "password": "squat-pw" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "and so is their password");
    let claimed = load_user_by_email(&catalog, "victim@example.com")
        .await?
        .unwrap();
    assert_eq!(claimed.id, squatted.id);
    assert!(!claimed.email_unproven());

    // An unverified address is refused, back on the page it started from.
    let mut other = Browser::new(router.clone(), APP_HOST);
    let (_, back) = sign_in(&router, &mut other, &seen, "unverified@example.com", "/x").await;
    assert!(back.unwrap().starts_with("/x?oauth_error="));
    assert!(
        load_user_by_email(&catalog, "unverified@example.com")
            .await?
            .is_none()
    );
    // So is one the provider does not say it verified, unless the admin
    // trusts its addresses.
    let mut other = Browser::new(router.clone(), APP_HOST);
    let (_, back) = sign_in(&router, &mut other, &seen, "silent@example.com", "/x").await;
    assert!(back.unwrap().starts_with("/x?oauth_error="));
    assert!(
        load_user_by_email(&catalog, "silent@example.com")
            .await?
            .is_none()
    );

    // --- finished in another browser: refused -----------------------------
    let mut starter = Browser::new(router.clone(), APP_HOST);
    starter
        .raw("GET", "/_feldspar/oauth/corp/start?return_to=/", None)
        .await;
    let params = query_of(&starter.location.clone().unwrap());
    seen.lock().unwrap().challenge = Some(params["code_challenge"].clone());
    let mut at_callback = Browser::new(router.clone(), BASE_DOMAIN);
    at_callback
        .raw(
            "GET",
            &format!(
                "/_feldspar/oauth/callback?code=new@example.com&state={}",
                params["state"]
            ),
            None,
        )
        .await;
    let complete = at_callback.location.clone().unwrap();
    let path = complete
        .strip_prefix(&format!("http://{APP_HOST}"))
        .unwrap();
    let mut victim = Browser::new(router.clone(), APP_HOST);
    let (status, _) = victim.raw("GET", path, None).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(victim.location.clone().unwrap().contains("oauth_error="));
    assert_eq!(victim.whoami().await, None);
    // And the state cannot be replayed.
    let (_, _) = at_callback
        .raw(
            "GET",
            &format!(
                "/_feldspar/oauth/callback?code=new@example.com&state={}",
                params["state"]
            ),
            None,
        )
        .await;
    assert!(
        at_callback
            .location
            .clone()
            .unwrap()
            .starts_with("/?oauth_error=")
    );

    // --- the admin UI: administrators only --------------------------------
    let mut admin_ui = Browser::new(router.clone(), BASE_DOMAIN);
    let (_, back) = sign_in(&router, &mut admin_ui, &seen, "new@example.com", "/").await;
    assert!(
        back.unwrap().contains("oauth_error="),
        "not an administrator"
    );
    assert_eq!(admin_ui.admin_user().await, None);
    let (status, _) = sign_in(&router, &mut admin_ui, &seen, ADMIN_EMAIL, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(admin_ui.admin_user().await.as_deref(), Some(ADMIN_EMAIL));
    // A flow started on a host this server does not serve by name — which
    // routes to the admin, as any unknown host does — is refused: its
    // completion token would be sent back there, to whoever runs that host.
    for forged in ["attacker.example", "evil.com:8080"] {
        let mut attacker = Browser::new(router.clone(), forged);
        let (status, _) = attacker
            .raw("GET", "/_feldspar/oauth/corp/start?return_to=/", None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{forged}");
        assert!(!attacker.cookies.contains_key(OAUTH_BINDING_COOKIE));
    }
    // The admin host does not create accounts, whatever the provider says.
    let mut stranger = Browser::new(router.clone(), BASE_DOMAIN);
    sign_in(&router, &mut stranger, &seen, "stranger@example.com", "/").await;
    assert!(
        load_user_by_email(&catalog, "stranger@example.com")
            .await?
            .is_none()
    );

    // --- the session lifetime and the shared cookie apply -----------------
    let (status, body) = admin
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "session_ttl_hours": 2 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(apps.session_ttl(), chrono::Duration::hours(2));
    let (status, _) = admin
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "session_ttl_hours": 0 } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "zero hours is refused");
    // A native client's cookie lasts as long as its session.
    let mut phone = Browser::new(router.clone(), APP_HOST);
    phone.headers.push((
        sc_api::auth::CLIENT_KIND_HEADER.to_owned(),
        sc_api::auth::NATIVE_CLIENT.to_owned(),
    ));
    sign_in(&router, &mut phone, &seen, "new@example.com", "/").await;
    let line = phone.session_cookie_line().unwrap();
    assert!(line.contains("Max-Age=7200"), "{line}");

    let (status, _) = admin
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "shared_session_cookie": true } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut shared = Browser::new(router.clone(), APP_HOST);
    sign_in(&router, &mut shared, &seen, "new@example.com", "/").await;
    let line = shared.session_cookie_line().unwrap();
    assert!(line.contains(&format!("Domain={BASE_DOMAIN}")), "{line}");

    // --- deleted: no longer offered ----------------------------------------
    let mut admin = Browser::new(router.clone(), BASE_DOMAIN);
    admin.send("GET", "/api/auth/status", None).await;
    admin
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": ADMIN_EMAIL, "password": PASSWORD })),
        )
        .await;
    let (status, body) = admin
        .send("DELETE", "/api/oauth-providers/corp", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);
    let (_, offered) = blog.send("GET", "/_feldspar/oauth/providers", None).await;
    assert_eq!(offered, json!([]));
    let (status, _) = blog
        .raw("GET", "/_feldspar/oauth/corp/start?return_to=/", None)
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(blog.location.clone().unwrap().contains("oauth_error="));
    Ok(())
}
