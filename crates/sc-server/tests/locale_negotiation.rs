//! End-to-end: which language a request is served in (design §16.1, decision
//! D8), driven through the assembled router against a real Postgres database.
//!
//! Four claims, and each is one the unit tests below the router cannot make:
//!
//! - **A monolingual server does no work.** With one enabled locale, a French
//!   browser with a French cookie asking for `?lang=fr` gets no
//!   `Content-Language` and no `Vary` — decision D11, asserted rather than
//!   hoped.
//! - **Turning it on is a save.** Enabling French through the settings endpoint
//!   makes the very next request negotiable, with no restart.
//! - **The sources are tried in D8's order** over real HTTP: `?lang=` beats the
//!   signed-in user's `language` column, which beats the `lang` cookie, which
//!   beats `Accept-Language`.
//! - **The response says what it did.** `Content-Language` is the locale that
//!   was *negotiated*, not the one that was asked for, and `Vary` names what the
//!   answer depended on.
//!
//! The test restores the monolingual default before it returns, because the
//! enabled set is process-wide (it is an installation's, not a request's) and
//! every other test in this binary runs in the same process.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AppMounts, CSRF_COOKIE, CSRF_HEADER, ServerConfig, admin_handlers, build_router};
use sc_test_harness::TestDb;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A browser against the router: a cookie jar, plus the request headers and the
/// response headers this test is actually about.
struct Client {
    router: Router,
    cookies: HashMap<String, String>,
}

/// One answer: the status, the body, and the headers that say what language it
/// is in.
struct Answer {
    status: StatusCode,
    body: Value,
    headers: HeaderMap,
}

impl Answer {
    fn content_language(&self) -> Option<&str> {
        self.headers
            .get(header::CONTENT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
    }

    /// Every `Vary` header's names, joined — except `accept-encoding`, which
    /// the compression layer adds to every compressible response and which is
    /// not what these tests are about.
    fn vary(&self) -> Option<String> {
        let names: Vec<&str> = self
            .headers
            .get_all(header::VARY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|n| !n.eq_ignore_ascii_case("accept-encoding"))
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    }
}

impl Client {
    fn new(router: Router) -> Client {
        Client {
            router,
            cookies: HashMap::new(),
        }
    }

    async fn send(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
        extra: &[(&str, &str)],
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(path);
        if !self.cookies.is_empty() {
            let cookie_header = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie_header);
        }
        if method != "GET" && method != "HEAD" {
            if let Some(csrf) = self.cookies.get(CSRF_COOKIE) {
                builder = builder.header(CSRF_HEADER, csrf);
            }
        }
        for (name, value) in extra {
            builder = builder.header(*name, *value);
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
        let headers = response.headers().clone();
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
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Answer {
            status,
            body,
            headers,
        }
    }

    async fn get(&mut self, path: &str, extra: &[(&str, &str)]) -> Answer {
        self.send("GET", path, None, extra).await
    }
}

/// A router over the admin endpoints, signed in as the first admin, with the
/// settings tables bootstrapped so Localisation can be saved.
async fn setup() -> sc_error::Result<(Client, TestDb)> {
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
    sc_auth::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;

    let sessions = Arc::new(SessionStore::default());
    let apps = Arc::new(AppMounts::new(catalog.clone()));
    let router = build_router(
        &sc_api::admin_endpoints(),
        admin_handlers(catalog, apps),
        sessions,
        &ServerConfig::default(),
    )?;

    let mut client = Client::new(router);
    client.get("/api/auth/status", &[]).await;
    let answer = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": "admin@example.com", "password": "hunter2pass" })),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    let answer = client
        .send(
            "POST",
            "/api/roles",
            Some(json!({ "role": 40, "name": "Staff", "description": "" })),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::CREATED);
    Ok((client, db))
}

/// One test at a time may hold the enabled set.
///
/// The enabled set is the **installation's**, so it lives in a process-wide slot
/// (`sc_i18n::set_active`), and every test in this binary shares the process.
/// Two tests each enabling their own locales would each see the other's, which
/// is not a race in the server so much as a race in the fixture — so they queue.
/// A `tokio` mutex rather than a `std` one because the guard is held across
/// every `await` in the test it guards — which is the whole test.
static LOCALES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Save the Localisation section.
async fn set_locales(client: &mut Client, default: &str, enabled: &str) {
    let answer = client
        .send(
            "POST",
            "/api/settings",
            Some(json!({
                "values": { "default_locale": default, "enabled_locales": enabled }
            })),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
}

#[tokio::test]
async fn a_request_is_served_in_the_language_it_negotiated() -> sc_error::Result<()> {
    let _held = LOCALES.lock().await;
    let (mut admin, _db) = setup().await?;

    // --- Monolingual: nothing is negotiated and nothing is said (D11) --------
    let answer = admin
        .get(
            "/api/auth/status?lang=fr",
            &[(header::ACCEPT_LANGUAGE.as_str(), "fr, de;q=0.9")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.content_language(),
        None,
        "a server with one enabled locale has no variation to declare"
    );
    assert_eq!(answer.vary(), None);
    assert_eq!(
        answer.body["locales"],
        json!({ "default": "en", "current": "en", "enabled": ["en"] }),
        "and it says so to the SPA, which is how the picker knows not to appear \
         — and `current` is still answered, because the SPA needs a locale to \
         load a catalogue for whether or not there is a choice"
    );

    // --- Turning it on is a save, not a restart -----------------------------
    set_locales(&mut admin, "en", "en, fr, de").await;

    let answer = admin
        .get(
            "/api/auth/status",
            &[(header::ACCEPT_LANGUAGE.as_str(), "fr;q=0.9, de;q=0.5")],
        )
        .await;
    assert_eq!(answer.content_language(), Some("fr"));
    assert_eq!(answer.vary().as_deref(), Some("Accept-Language, Cookie"));
    assert_eq!(
        answer.body["locales"],
        json!({ "default": "en", "current": "fr", "enabled": ["en", "fr", "de"] })
    );

    // A tag nobody enabled never escapes the enabled set: the default answers.
    let answer = admin
        .get(
            "/api/auth/status",
            &[(header::ACCEPT_LANGUAGE.as_str(), "ja")],
        )
        .await;
    assert_eq!(answer.content_language(), Some("en"));

    // `?lang=` is the loudest source there is, and beats the header.
    let answer = admin
        .get(
            "/api/auth/status?lang=de",
            &[(header::ACCEPT_LANGUAGE.as_str(), "fr")],
        )
        .await;
    assert_eq!(answer.content_language(), Some("de"));

    // --- The user's own column beats the browser's header -------------------
    let answer = admin
        .send(
            "POST",
            "/api/users",
            Some(json!({
                "email": "sam@example.com",
                "password": "correct-horse-battery",
                // An admin, because the admin API's `login` admits nobody else
                // — this test is about the language of a request, not about who
                // may make one.
                "role": 1,
                // Typed upper case, stored canonical: a tag is parsed, not
                // trusted.
                "language": "FR",
            })),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.body);
    assert_eq!(answer.body["user"]["language"], json!("fr"));

    let mut sam = Client::new(admin.router.clone());
    sam.get("/api/auth/status", &[]).await;
    let answer = sam
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "sam@example.com", "password": "correct-horse-battery" })),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);

    let answer = sam
        .get(
            "/api/auth/status",
            &[(header::ACCEPT_LANGUAGE.as_str(), "de")],
        )
        .await;
    assert_eq!(
        answer.content_language(),
        Some("fr"),
        "a stated preference outranks a browser's guess at one"
    );

    // …and `?lang=` still outranks the column, because it is this request's.
    let answer = sam
        .get(
            "/api/auth/status?lang=de",
            &[(header::ACCEPT_LANGUAGE.as_str(), "fr")],
        )
        .await;
    assert_eq!(answer.content_language(), Some("de"));

    // --- The cookie is how somebody with no account chooses -----------------
    let mut visitor = Client::new(admin.router.clone());
    visitor.get("/api/auth/status", &[]).await;
    visitor.cookies.insert("lang".to_owned(), "de".to_owned());
    let answer = visitor
        .get(
            "/api/auth/status",
            &[(header::ACCEPT_LANGUAGE.as_str(), "fr")],
        )
        .await;
    assert_eq!(answer.content_language(), Some("de"));

    // --- Clearing the language is a change, not an omission -----------------
    let listed = admin.get("/api/users", &[]).await.body;
    let sam_id = listed
        .as_array()
        .expect("a list of users")
        .iter()
        .find(|u| u["email"] == json!("sam@example.com"))
        .expect("sam")["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let answer = admin
        .send(
            "PUT",
            &format!("/api/users/{sam_id}"),
            Some(json!({
                "email": "sam@example.com",
                "role": 1,
                "language": Value::Null,
            })),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body["language"], Value::Null);

    // Signed in afresh, because the locale is negotiated from the `User` the
    // session store holds and a memory-backed session holds the one it was
    // opened with: what is being asserted here is the negotiation's order, not
    // how long a session cache keeps a row.
    let mut sam_again = Client::new(admin.router.clone());
    sam_again.get("/api/auth/status", &[]).await;
    let answer = sam_again
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "sam@example.com", "password": "correct-horse-battery" })),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let answer = sam_again
        .get(
            "/api/auth/status",
            &[(header::ACCEPT_LANGUAGE.as_str(), "de")],
        )
        .await;
    assert_eq!(
        answer.content_language(),
        Some("de"),
        "with the column cleared the browser's header is the next source"
    );

    // --- A tag that is not one is refused where it was typed ----------------
    let answer = admin
        .send(
            "POST",
            "/api/settings",
            Some(json!({ "values": { "enabled_locales": "en, notalanguagetag" } })),
            &[],
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(
        answer.body.to_string().contains("notalanguagetag"),
        "the refusal should name what was typed: {}",
        answer.body
    );

    // Put the process back where the rest of this binary expects it.
    set_locales(&mut admin, "en", "").await;
    let answer = admin
        .get(
            "/api/auth/status",
            &[(header::ACCEPT_LANGUAGE.as_str(), "fr")],
        )
        .await;
    assert_eq!(answer.content_language(), None);
    Ok(())
}

/// What the negotiated language actually *changes* (tasks 3.1 and 3.2).
///
/// The companion to the test above: that one asserts which locale a request was
/// served in, this one asserts that the server then said something different.
/// Four surfaces, and each is a different way a sentence reaches a person:
///
/// - **A refusal an admin reads** — a wrong password, and a request with no
///   session at all. `t!` at the call site, against the request's locale (3.1).
/// - **A settings section** — its heading, its one sentence, a field's label and
///   that field's help text. All four are `sc-config` *data*, translated at the
///   API edge because the server translates everything the server says (3.2, D5).
/// - **A declared `config_spec`** — a framework's, which is the shape every
///   extension point's settings arrive in. If this one moved, they all do: they
///   go through one function.
/// - **A declared description** — a framework's one-line pitch, which is a
///   `&'static str` in a registry rather than a `FormField`.
///
/// The French comes from `crates/sc-i18n/locales/fr.json`, which is generated
/// from these very call sites by `feldspar i18n translate` — so a key that moved
/// makes this test fail loudly rather than silently serving English.
#[tokio::test]
async fn what_the_server_says_is_said_in_the_request_s_language() -> sc_error::Result<()> {
    let _held = LOCALES.lock().await;
    let (mut admin, _db) = setup().await?;

    // --- English first, so every assertion below is a *change* --------------
    let english = admin.get("/api/settings", &[]).await;
    let localisation = section(&english.body, "localisation");
    assert_eq!(localisation["label"], json!("Localisation"));
    assert_eq!(
        field(localisation, "enabled_locales")["label"],
        json!("Enabled languages")
    );

    set_locales(&mut admin, "en", "en, fr").await;

    // --- 3.1: a refusal, in the language of the person refused ---------------
    let mut stranger = Client::new(admin.router.clone());
    stranger.get("/api/auth/status", &[]).await;
    let answer = stranger
        .send(
            "POST",
            "/api/login",
            Some(json!({ "email": "admin@example.com", "password": "not-the-password" })),
            &[(header::ACCEPT_LANGUAGE.as_str(), "fr")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.content_language(), Some("fr"));
    assert!(
        answer.body.to_string().contains("identifiants invalides"),
        "a wrong password should be refused in French: {}",
        answer.body
    );

    // The authorization refusal, which is decided before a handler runs and
    // from the user alone — here, from no user at all, so the installation
    // default answers and the sentence is English.
    let mut nobody = Client::new(admin.router.clone());
    let answer = nobody.get("/api/tables", &[]).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert!(
        answer.body.to_string().contains("authentication required"),
        "{}",
        answer.body
    );

    // --- 3.2: the settings screen, all four kinds of English on it -----------
    let french = admin
        .get("/api/settings", &[(header::ACCEPT_LANGUAGE.as_str(), "fr")])
        .await;
    assert_eq!(french.content_language(), Some("fr"));
    let localisation = section(&french.body, "localisation");
    assert_eq!(localisation["label"], json!("Localisation"));
    assert_eq!(
        localisation["description"],
        json!("Les langues que cette installation sert, et celle vers laquelle elle se replie"),
        "the section's own sentence is the server's too"
    );
    let enabled = field(localisation, "enabled_locales");
    assert_eq!(enabled["label"], json!("Langues activées"));
    assert!(
        enabled["help"]
            .as_str()
            .is_some_and(|h| h.starts_with("Étiquettes BCP-47")),
        "the help text hangs off ConfigDef rather than FormField, and is \
         translated where it is serialised: {}",
        enabled["help"]
    );
    // A name is an identifier, not a label: translating it would rename the
    // setting, and a save keyed by `Langues activées` would store nothing.
    assert_eq!(enabled["name"], json!("enabled_locales"));

    // --- 3.2: a declared spec, and a declared sentence ----------------------
    let frameworks = admin
        .get(
            "/api/frameworks",
            &[(header::ACCEPT_LANGUAGE.as_str(), "fr")],
        )
        .await;
    assert_eq!(frameworks.status, StatusCode::OK, "{}", frameworks.body);
    let saltcorn_ui = frameworks
        .body
        .as_array()
        .expect("a list of frameworks")
        .iter()
        .find(|f| f["name"] == json!("saltcorn-ui"))
        .expect("the Saltcorn UI framework")
        .clone();
    assert_eq!(saltcorn_ui["label"], json!("Interface Saltcorn"));
    assert!(
        saltcorn_ui["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("Des vues et des pages")),
        "a framework's one-line pitch is a declared string and is translated \
         at the edge: {}",
        saltcorn_ui["description"]
    );
    let labels: Vec<&str> = saltcorn_ui["config_spec"]
        .as_array()
        .expect("a config spec")
        .iter()
        .filter_map(|f| f["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"Nom du site"),
        "every declared spec goes through one function, and this is it: {labels:?}"
    );

    // …and English, asked for, is still English: the catalogue is a lookup and
    // not a transformation.
    let english_again = admin.get("/api/frameworks?lang=en", &[]).await;
    let saltcorn_ui = english_again
        .body
        .as_array()
        .expect("a list of frameworks")
        .iter()
        .find(|f| f["name"] == json!("saltcorn-ui"))
        .expect("the Saltcorn UI framework")
        .clone();
    assert_eq!(saltcorn_ui["label"], json!("Saltcorn UI"));

    set_locales(&mut admin, "en", "").await;
    Ok(())
}

/// One section of the settings payload, by name.
fn section<'a>(settings: &'a Value, name: &str) -> &'a Value {
    settings["sections"]
        .as_array()
        .expect("the settings sections")
        .iter()
        .find(|s| s["name"] == json!(name))
        .unwrap_or_else(|| panic!("no `{name}` section"))
}

/// One field of a section, by name.
fn field<'a>(section: &'a Value, name: &str) -> &'a Value {
    section["fields"]
        .as_array()
        .expect("the section's fields")
        .iter()
        .find(|f| f["name"] == json!(name))
        .unwrap_or_else(|| panic!("no `{name}` field"))
}
