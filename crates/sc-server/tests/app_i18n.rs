//! An application's catalogue over HTTP: `GET {mount}/i18n/{locale}.json`
//! (§16.1, D7), through the assembled router against a real database and a real
//! file store.
//!
//! D7's claim is that an application's translations are **served, not bundled**
//! — so an admin who fixes a mistranslation does not wait for a bundler, and
//! "translate this application into Spanish" is not a deploy. That claim is only
//! worth anything if four things hold, and they are what this asserts:
//!
//! 1. The catalogue is at the path the generated runtime computes, beside the
//!    endpoint set rather than inside it, and comes back as JSON with the
//!    `Content-Language` the caller asked for.
//! 2. It is cacheable — an ETag, and a 304 on the second request — because
//!    otherwise "served" means "fetched on every page load".
//! 3. **A change is live.** Saving a translation and invalidating the mount is
//!    enough; no build runs, no process restarts.
//! 4. An application with no locales pays nothing (D11): 404 without a file
//!    store or a database being touched.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_app::{
    ApiConfig, Application, AssetBundle, CatalogStore, CodeFramework, FrameworkRef,
    app_catalog_store, save_application, set_app_locales,
};
use sc_auth::SessionStore;
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_error::Result;
use sc_files::LocalFileStore;
use sc_i18n::{Catalog as MessageCatalog, Locale, Message};
use sc_test_harness::TestDb;
use serde_json::Value;
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "tasks.example.com";

/// A scratch directory removed when the guard drops — the app's file store.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Result<TempDir> {
        let dir = std::env::temp_dir().join(format!(
            "sc-server-i18n-{}-{tag}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
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

/// One response, reduced to what this test has claims about.
struct Answer {
    status: StatusCode,
    etag: Option<String>,
    content_language: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
    }
}

struct Server {
    router: Router,
    catalog: Arc<Catalog>,
    apps: Arc<sc_server::AppMounts>,
    store: TempDir,
    _db: TestDb,
}

impl Server {
    async fn get(&self, path: &str, if_none_match: Option<&str>) -> Answer {
        let mut builder = Request::builder()
            .method("GET")
            .uri(path)
            .header(header::HOST, APP_HOST);
        if let Some(tag) = if_none_match {
            builder = builder.header(header::IF_NONE_MATCH, tag);
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let header = |name: header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let status = response.status();
        let etag = header(header::ETAG);
        let content_language = header(header::CONTENT_LANGUAGE);
        let content_type = header(header::CONTENT_TYPE);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        Answer {
            status,
            etag,
            content_language,
            content_type,
            body,
        }
    }
}

/// The application: a `code` app with its API at `/api`, so the catalogue sits
/// *under* a provider's mount and the provider's own "no such endpoint" is what
/// would answer if the route were not beside it.
fn tasks_app(locales: &[&str]) -> Application {
    let mut app = Application::new(
        "Tasks",
        "tasks",
        FrameworkRef::new("code")
            .with("store", "apps")
            .with("source", "web")
            .with("output", "web/dist")
            .with("command", "npm run build"),
    )
    .with_api(ApiConfig::new("rest", "/api"));
    let locales: Vec<Locale> = locales.iter().map(|t| Locale::parse(t).unwrap()).collect();
    set_app_locales(&mut app, &locales, locales.first());
    app
}

fn french() -> MessageCatalog {
    let mut cat = MessageCatalog::new(Locale::parse("fr").unwrap());
    cat.insert(
        "Add a task",
        Message::Simple("Ajouter une tâche".to_owned()),
    );
    cat.insert(
        "Delete {name}?",
        Message::Simple("Supprimer {name} ?".to_owned()),
    );
    cat
}

async fn setup(app: Application) -> Result<Server> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    sc_auth::bootstrap(&catalog).await?;
    sc_app::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;

    let store = TempDir::new("store")?;
    sc_catalog::save_file_store(
        &catalog,
        &sc_files::FileStoreDef::local("apps", store.path().to_string_lossy()),
    )
    .await?;
    catalog.connect_file_store(Arc::new(LocalFileStore::new("apps", store.path())?))?;

    let app = save_application(&catalog, &app).await?;

    let apps = Arc::new(sc_server::AppMounts::new(catalog.clone()));
    let framework = Arc::new(CodeFramework::new(
        "code",
        AssetBundle::new().with("index.html", "<!doctype html>"),
    ));
    apps.mount(sc_server::MountedApp::new(app, framework, &catalog)?)?;

    let config = sc_server::ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        ..sc_server::ServerConfig::default()
    };
    let router = sc_server::build_router_with_apps(
        &sc_api::admin_endpoints(),
        sc_server::admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;

    Ok(Server {
        router,
        catalog,
        apps,
        store,
        _db: db,
    })
}

#[tokio::test]
async fn an_applications_catalogue_is_served_beside_its_api_with_an_etag() -> Result<()> {
    let server = setup(tasks_app(&["fr"])).await?;
    let app = sc_app::list_applications(&server.catalog).await?.remove(0);

    // The catalogue as the admin's repository holds it: a file under the
    // project, written through the same seam the admin API will use.
    let store = app_catalog_store(&app)?;
    store.save(&server.catalog, &french()).await?;
    assert!(server.store.path().join("web/locales/fr.json").is_file());

    let answer = server.get("/api/i18n/fr.json", None).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.content_language.as_deref(), Some("fr"));
    assert_eq!(answer.content_type.as_deref(), Some("application/json"));
    assert_eq!(answer.json()["Add a task"], "Ajouter une tâche");
    // The message id is the English source text (D1), so the served file is
    // keyed by the sentence the application's code contains.
    assert_eq!(answer.json()["Delete {name}?"], "Supprimer {name} ?");
    let etag = answer.etag.clone().expect("an ETag");

    // The second load is a 304 with no body: served, and still cacheable.
    let again = server.get("/api/i18n/fr.json", Some(&etag)).await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty());
    assert_eq!(again.etag.as_deref(), Some(etag.as_str()));

    // A stale tag is not a match, and the body comes back.
    let stale = server.get("/api/i18n/fr.json", Some("\"0000\"")).await;
    assert_eq!(stale.status, StatusCode::OK);
    assert!(!stale.body.is_empty());

    Ok(())
}

#[tokio::test]
async fn a_translation_is_live_without_a_rebuild() -> Result<()> {
    let server = setup(tasks_app(&["fr"])).await?;
    let app = sc_app::list_applications(&server.catalog).await?.remove(0);
    let store = app_catalog_store(&app)?;
    store.save(&server.catalog, &french()).await?;

    let before = server.get("/api/i18n/fr.json", None).await;
    assert_eq!(before.json()["Add a task"], "Ajouter une tâche");
    let first_etag = before.etag.clone().unwrap();

    // The admin fixes a mistranslation. Nothing is built and nothing restarts:
    // the store is written and the mount's cache is dropped.
    let mut fixed = french();
    fixed.insert("Add a task", Message::Simple("Créer une tâche".to_owned()));
    store.save(&server.catalog, &fixed).await?;
    server.apps.invalidate_catalogs(app.id);

    let after = server.get("/api/i18n/fr.json", None).await;
    assert_eq!(after.json()["Add a task"], "Créer une tâche");
    // A different catalogue is a different tag, so a browser holding the old
    // one is told rather than left with it.
    assert_ne!(after.etag.as_deref(), Some(first_etag.as_str()));

    // Without the invalidation the cache would still be answering — which is
    // what makes the invalidation the thing the admin API has to remember.
    let cached = server.get("/api/i18n/fr.json", None).await;
    assert_eq!(cached.etag, after.etag);

    Ok(())
}

#[tokio::test]
async fn an_enabled_locale_with_nothing_translated_yet_is_an_empty_catalogue() -> Result<()> {
    let server = setup(tasks_app(&["fr", "de"])).await?;

    // German is enabled and has no file: the answer is an empty catalogue, not
    // a 404. The locale is one the application serves, and the runtime that
    // asked has a well-formed answer to cache — English renders either way.
    let answer = server.get("/api/i18n/de.json", None).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.json(), serde_json::json!({}));
    assert!(answer.etag.is_some());

    Ok(())
}

#[tokio::test]
async fn a_locale_the_application_does_not_serve_is_not_there() -> Result<()> {
    let server = setup(tasks_app(&["fr"])).await?;

    // Enabled locales are the whole surface: Spanish is not one, so there is
    // nothing at its path — the same answer an unexposed stream gets, and for
    // the same reason.
    assert_eq!(
        server.get("/api/i18n/es.json", None).await.status,
        StatusCode::NOT_FOUND
    );
    // A tag that is not a locale gets the identical refusal: a stranger learns
    // nothing about which locales this application has.
    assert_eq!(
        server
            .get("/api/i18n/not-a-locale!.json", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    Ok(())
}

#[tokio::test]
async fn an_untranslated_application_costs_nothing() -> Result<()> {
    // No `locales` attribute at all — the state of every application that has
    // not turned i18n on, and what D11 is about.
    let server = setup(tasks_app(&[])).await?;

    let answer = server.get("/api/i18n/fr.json", None).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    // The guard is before the store, so nothing was read and nothing was
    // created on disk.
    assert!(!server.store.path().join("web/locales").exists());

    Ok(())
}

// ---------------------------------------------------------------------------
// The milestone's end to end, against a scripted translator (task 4.6)
// ---------------------------------------------------------------------------

/// A translator that answers from a table, and — for one key — answers
/// *wrongly*, by renaming a placeholder.
///
/// It stands in for the LLM so this runs in `cargo test` with no API key and no
/// money spent. D9 is not "the LLM is careful": it is "the machine checks", and
/// the only way to assert a check is to hand it something that fails.
struct Scripted(std::collections::BTreeMap<String, Value>);

#[async_trait::async_trait]
impl sc_i18n::Translator for Scripted {
    async fn translate_batch(
        &self,
        _source: &Locale,
        _target: &Locale,
        keys: &[String],
    ) -> Result<std::collections::BTreeMap<String, Value>> {
        Ok(keys
            .iter()
            .filter_map(|k| self.0.get(k).map(|v| (k.clone(), v.clone())))
            .collect())
    }
}

/// The project an admin's coding agent wrote: three `t()` call sites, one of
/// them with a placeholder, and one bare literal nobody wrapped.
fn write_project(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root.join("web/src/pages"))?;
    std::fs::write(
        root.join("web/src/App.tsx"),
        "import { useT } from \"./feldspar/i18n\";\n\
         \n\
         export default function App() {\n\
        \x20 const { t } = useT();\n\
        \x20 return (\n\
        \x20   <main>\n\
        \x20     <h1>{t(\"Tasks\")}</h1>\n\
        \x20     <button>{t(\"Add a task\")}</button>\n\
        \x20     <p>This one nobody wrapped.</p>\n\
        \x20   </main>\n\
        \x20 );\n\
         }\n",
    )?;
    std::fs::write(
        root.join("web/src/pages/Detail.tsx"),
        "import { t } from \"../feldspar/i18n\";\n\
         \n\
         export function confirmDelete(name: string) {\n\
        \x20 return t(\"Delete {name}?\", { name });\n\
         }\n",
    )?;
    // The generated runtime itself is never scanned: it is where `t` is
    // written, so its one `t(text, args)` has a variable message and has to.
    std::fs::create_dir_all(root.join("web/src/feldspar"))?;
    std::fs::write(
        root.join("web/src/feldspar/i18n.tsx"),
        "export function t(text: string, args?: Record<string, string>) {\n\
        \x20 return text;\n\
         }\n",
    )?;
    // And neither is a dependency tree.
    std::fs::create_dir_all(root.join("web/node_modules/react"))?;
    std::fs::write(
        root.join("web/node_modules/react/index.js"),
        "export const t = () => t(\"Never mine to translate\");\n",
    )?;
    Ok(())
}

#[tokio::test]
async fn an_application_is_extracted_translated_and_served_end_to_end() -> Result<()> {
    let server = setup(tasks_app(&["fr"])).await?;
    write_project(server.store.path())?;
    let app = sc_app::list_applications(&server.catalog).await?.remove(0);

    // --- What does this application say? The same tree-sitter pass `feldspar
    // i18n extract` runs, over the application's own file store.
    let found = sc_server::translations::application_strings(&server.catalog, &app).await?;
    let keys = found.keys();
    assert_eq!(
        keys,
        vec![
            "Add a task".to_owned(),
            "Delete {name}?".to_owned(),
            "Tasks".to_owned(),
        ],
        "the runtime and node_modules are not scanned"
    );
    // The other half of the same parse: a literal nobody wrapped.
    assert!(
        found
            .findings
            .iter()
            .any(|f| f.text.contains("This one nobody wrapped")),
        "{:?}",
        found.findings
    );

    // --- Translate missing, against the scripted translator. One answer is
    // sabotaged: `{nom}` is not `{name}`.
    let translator = Scripted(
        [
            ("Tasks".to_owned(), serde_json::json!("Tâches")),
            (
                "Add a task".to_owned(),
                serde_json::json!("Ajouter une tâche"),
            ),
            (
                "Delete {name}?".to_owned(),
                serde_json::json!("Supprimer {nom} ?"),
            ),
        ]
        .into_iter()
        .collect(),
    );
    let fr = Locale::parse("fr")?;
    let report = sc_server::translations::fill_missing(
        &server.catalog,
        &server.apps,
        &app,
        &fr,
        &translator,
    )
    .await?;
    assert_eq!(report["filled"], 2);
    // The machine checks the placeholders whatever the prompt said (D9): the
    // third is rejected, named, and left in English rather than rendering
    // `{nom}` to a customer.
    let rejected = report["rejected"].as_array().unwrap();
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0]["key"], "Delete {name}?");

    // --- It landed in the admin's repository, as a file, where git will see it.
    let written = std::fs::read_to_string(server.store.path().join("web/locales/fr.json"))?;
    assert!(written.contains("Tâches"), "{written}");
    assert!(!written.contains("{nom}"), "{written}");

    // --- And the running application serves it, with no build in between.
    let answer = server.get("/api/i18n/fr.json", None).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.content_language.as_deref(), Some("fr"));
    assert_eq!(answer.json()["Add a task"], "Ajouter une tâche");
    // The rejected one is absent, so the application renders its English.
    assert!(answer.json().get("Delete {name}?").is_none());

    // --- Saving by hand goes through the same check, and the same store.
    let saved = sc_server::translations::save_catalogue(
        &server.catalog,
        &server.apps,
        &app,
        &fr,
        &serde_json::json!({ "Tasks": "Tâches", "Add a task": "Créer une tâche" }),
    )
    .await?;
    assert_eq!(saved["messages"], 2);
    assert_eq!(
        server.get("/api/i18n/fr.json", None).await.json()["Add a task"],
        "Créer une tâche"
    );

    // A translation that renamed a placeholder is refused naming the key,
    // whether a model or an admin wrote it.
    let refused = sc_server::translations::save_catalogue(
        &server.catalog,
        &server.apps,
        &app,
        &fr,
        &serde_json::json!({ "Delete {name}?": "Supprimer {nom} ?" }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(refused.contains("Delete {name}?"), "{refused}");

    Ok(())
}

#[tokio::test]
async fn an_orphan_survives_a_save_that_never_showed_it() -> Result<()> {
    let server = setup(tasks_app(&["fr"])).await?;
    write_project(server.store.path())?;
    let app = sc_app::list_applications(&server.catalog).await?.remove(0);
    let fr = Locale::parse("fr")?;

    // A translation of a string the source no longer says.
    sc_server::translations::save_catalogue(
        &server.catalog,
        &server.apps,
        &app,
        &fr,
        &serde_json::json!({ "Tasks": "Tâches", "Removed last week": "Retiré" }),
    )
    .await?;

    // The screen never shows it — it is not a key the extractor found — so a
    // save from the grid does not mention it…
    let screen = sc_server::translations::translations_json(&server.catalog, &app).await?;
    let shown: Vec<&str> = screen["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["key"].as_str().unwrap())
        .collect();
    assert!(!shown.contains(&"Removed last week"));
    // …but it is listed as an orphan, so the admin knows it is there.
    assert_eq!(
        screen["locales"][0]["orphans"],
        serde_json::json!(["Removed last week"])
    );

    // …and it is still there afterwards. Shown, never deleted.
    sc_server::translations::save_catalogue(
        &server.catalog,
        &server.apps,
        &app,
        &fr,
        &serde_json::json!({ "Tasks": "Tâches" }),
    )
    .await?;
    let served = server.get("/api/i18n/fr.json", None).await.json();
    assert_eq!(served["Removed last week"], "Retiré");

    Ok(())
}

/// The MCP tools an external coding agent translates with: enable a locale,
/// read what is missing, write its own translations, and correct one — over the
/// same handlers as the Translations screen, so a save is checked, merged and
/// live without a build.
#[tokio::test]
async fn a_coding_agent_translates_an_application_through_the_mcp_tools() -> Result<()> {
    use sc_api::mcp::{Areas, ToolContext};
    use sc_api::schema_edit::Grants;
    use serde_json::json;

    // No locales yet: "translate this app to German" starts from nothing.
    let server = setup(tasks_app(&[])).await?;
    write_project(server.store.path())?;
    // Enabling a locale refreshes the mount, which for this fixture's framework
    // (no prebuilt bundle) means a build: one that does nothing.
    let web = server.store.path().join("web");
    std::fs::create_dir_all(web.join("dist"))?;
    std::fs::write(web.join("dist/index.html"), "<!doctype html>")?;
    std::fs::write(
        web.join("package.json"),
        r#"{ "name": "tasks", "scripts": { "build": "true" } }"#,
    )?;
    let tools = sc_app::mcp::tool_set(Grants::all(), Areas::all());
    let ctx = ToolContext {
        catalog: &server.catalog,
        user: None,
        role: 1,
        triggers: None,
        actor: "the test agent",
    };

    let before = tools
        .call(
            "describe_translations",
            &json!({ "application": "tasks" }),
            &ctx,
        )
        .await?;
    assert_eq!(before["messages"].as_array().unwrap().len(), 3);
    assert_eq!(before["unwrapped_count"], 1, "{before}");
    assert!(
        before["notes"].to_string().contains("update_application"),
        "{before}"
    );

    let enabled = tools
        .call(
            "update_application",
            &json!({ "application": "tasks",
                     "locales": { "add": ["de"], "default_locale": "de" } }),
            &ctx,
        )
        .await?;
    assert_eq!(enabled["locales"]["locales"], json!(["de"]));
    assert_eq!(enabled["locales"]["default_locale"], "de");
    assert_eq!(enabled["was"]["locales"]["locales"], json!([]));

    let missing = tools
        .call(
            "describe_translations",
            &json!({ "application": "tasks", "locale": "de", "missing_only": true }),
            &ctx,
        )
        .await?;
    let keys: Vec<&str> = missing["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["Add a task", "Delete {name}?", "Tasks"]);

    tools
        .call(
            "save_translations",
            &json!({ "application": "tasks", "locale": "de", "messages": {
                "Tasks": "Aufgaben",
                "Add a task": "Aufgabe hinzufügen",
                "Delete {name}?": "{name} löschen?",
            }}),
            &ctx,
        )
        .await?;

    // "The translation of 'Add a task' is wrong": one key, and the others stay.
    let fixed = tools
        .call(
            "save_translations",
            &json!({ "application": "tasks", "locale": "de",
                     "messages": { "Add a task": "Neue Aufgabe" } }),
            &ctx,
        )
        .await?;
    assert_eq!(fixed["written"], 1);
    let served = server.get("/api/i18n/de.json", None).await.json();
    assert_eq!(served["Add a task"], "Neue Aufgabe");
    assert_eq!(served["Tasks"], "Aufgaben");
    assert_eq!(served["Delete {name}?"], "{name} löschen?");

    // A renamed placeholder is refused naming the key, and nothing changes.
    let refused = tools
        .call(
            "save_translations",
            &json!({ "application": "tasks", "locale": "de",
                     "messages": { "Delete {name}?": "{Name} löschen?" } }),
            &ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("Delete {name}?"), "{refused}");
    assert_eq!(
        server.get("/api/i18n/de.json", None).await.json()["Delete {name}?"],
        "{name} löschen?"
    );

    let after = tools
        .call(
            "describe_translations",
            &json!({ "application": "tasks", "locale": "de", "missing_only": true }),
            &ctx,
        )
        .await?;
    assert!(after["messages"].as_array().unwrap().is_empty(), "{after}");
    assert_eq!(after["locales"][0]["percent"], 100);
    Ok(())
}

// ---------------------------------------------------------------------------
// The instance catalogue under the app's own (TODO F6.2)
// ---------------------------------------------------------------------------

/// The shared German strings: two the apps do not translate, one they do.
fn shared_german() -> MessageCatalog {
    let mut cat = MessageCatalog::new(Locale::parse("de").unwrap());
    cat.insert("Sign in", Message::Simple("Anmelden".to_owned()));
    cat.insert("Password", Message::Simple("Passwort".to_owned()));
    cat.insert("Copy", Message::Simple("Kopieren".to_owned()));
    cat
}

/// An app's own German: `Copy` overridden, `Password` left blank (which counts
/// as missing), and one string of its own.
fn own_german() -> MessageCatalog {
    let mut cat = MessageCatalog::new(Locale::parse("de").unwrap());
    cat.insert("Copy", Message::Simple("Duplizieren".to_owned()));
    cat.insert("Password", Message::Simple(String::new()));
    cat.insert("Add a task", Message::Simple("Aufgabe anlegen".to_owned()));
    cat
}

async fn assert_merged(server: &Server, app: &Application) -> Result<()> {
    sc_app::instance_catalog_store()
        .save(&server.catalog, &shared_german())
        .await?;
    app_catalog_store(app)?
        .save(&server.catalog, &own_german())
        .await?;
    server.apps.invalidate_all_catalogs();

    let answer = server.get("/api/i18n/de.json", None).await;
    assert_eq!(answer.status, StatusCode::OK);
    let body = answer.json();
    // An app key overrides.
    assert_eq!(body["Copy"], "Duplizieren");
    // An instance-only key is served.
    assert_eq!(body["Sign in"], "Anmelden");
    // An empty app value is missing, so the shared one shows through.
    assert_eq!(body["Password"], "Passwort");
    assert_eq!(body["Add a task"], "Aufgabe anlegen");

    // Changing the shared catalogue is live in the app after the sweep.
    let mut changed = shared_german();
    changed.insert("Sign in", Message::Simple("Einloggen".to_owned()));
    sc_app::instance_catalog_store()
        .save(&server.catalog, &changed)
        .await?;
    server.apps.invalidate_all_catalogs();
    let after = server.get("/api/i18n/de.json", None).await;
    assert_eq!(after.json()["Sign in"], "Einloggen");
    assert_ne!(after.etag, answer.etag);

    // A locale the app does not enable is still a 404, even when the instance
    // has a catalogue for it.
    let mut french_shared = french();
    french_shared.insert("Sign in", Message::Simple("Se connecter".to_owned()));
    sc_app::instance_catalog_store()
        .save(&server.catalog, &french_shared)
        .await?;
    assert_eq!(
        server.get("/api/i18n/fr.json", None).await.status,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test]
async fn a_file_backed_app_serves_the_instance_catalogue_under_its_own() -> Result<()> {
    let server = setup(tasks_app(&["de"])).await?;
    let app = sc_app::list_applications(&server.catalog).await?.remove(0);
    assert_merged(&server, &app).await?;
    // The app's own file holds only its own strings: the merge is served, not
    // written back.
    let file = std::fs::read_to_string(server.store.path().join("web/locales/de.json"))?;
    assert!(!file.contains("Anmelden"), "{file}");
    Ok(())
}

#[tokio::test]
async fn a_row_backed_app_serves_the_instance_catalogue_under_its_own() -> Result<()> {
    // `none` has no build step, so its catalogue is `_fd_translations` rows —
    // the Saltcorn UI half of the split.
    let mut app = Application::new(
        "Tasks",
        "tasks",
        FrameworkRef::new("none").with("store", "apps"),
    )
    .with_api(ApiConfig::new("rest", "/api"));
    set_app_locales(&mut app, &[Locale::parse("de")?], None);
    let server = setup(app).await?;
    let app = sc_app::list_applications(&server.catalog).await?.remove(0);
    assert!(
        app_catalog_store(&app)?
            .describe()
            .contains("_fd_translations")
    );
    assert_merged(&server, &app).await
}

#[tokio::test]
async fn an_app_with_no_catalogue_of_its_own_serves_the_shared_one() -> Result<()> {
    let server = setup(tasks_app(&["de"])).await?;
    sc_app::instance_catalog_store()
        .save(&server.catalog, &shared_german())
        .await?;
    let answer = server.get("/api/i18n/de.json", None).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.json()["Sign in"], "Anmelden");
    Ok(())
}

// ---------------------------------------------------------------------------
// The shared Translations screen's server half (TODO F6.3)
// ---------------------------------------------------------------------------

/// Turn on German and French for the installation (Settings → Localisation).
async fn enable_instance_locales(catalog: &Catalog, list: &str) -> Result<()> {
    sc_config::bootstrap_config(catalog).await?;
    sc_config::set_config(catalog, "enabled_locales", serde_json::json!(list)).await
}

#[tokio::test]
async fn the_shared_catalogue_is_edited_and_filled_like_an_applications() -> Result<()> {
    use serde_json::json;
    let server = setup(tasks_app(&["de"])).await?;
    write_project(server.store.path())?;
    let app = sc_app::list_applications(&server.catalog).await?.remove(0);
    let de = Locale::parse("de")?;
    let fr = Locale::parse("fr")?;
    sc_config::bootstrap_config(&server.catalog).await?;

    // Not an enabled language yet: refused, naming where to enable it.
    let refused = sc_server::translations::save_instance_catalogue(
        &server.catalog,
        Some(&server.apps),
        &de,
        &json!({ "Tasks": "Aufgaben" }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(refused.contains("Localisation"), "{refused}");

    enable_instance_locales(&server.catalog, "en, de, fr").await?;
    // The source language is the keys, so it is not a column.
    let screen = sc_server::translations::instance_translations_json(&server.catalog).await?;
    let tags: Vec<&str> = screen["locales"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["locale"].as_str().unwrap())
        .collect();
    assert_eq!(tags, ["de", "fr"]);
    assert_eq!(screen["messages"], json!([]));

    // An admin adds two strings in German. The app's first request for German
    // has already been cached, so this also checks the save sweeps every mount.
    assert!(
        server
            .get("/api/i18n/de.json", None)
            .await
            .json()
            .get("Tasks")
            .is_none()
    );
    sc_server::translations::save_instance_catalogue(
        &server.catalog,
        Some(&server.apps),
        &de,
        &json!({ "Tasks": "Aufgaben", "Delete {name}?": "{name} löschen?" }),
    )
    .await?;
    assert_eq!(
        server.get("/api/i18n/de.json", None).await.json()["Tasks"],
        "Aufgaben"
    );

    // Placeholders are checked as an application's are.
    let refused = sc_server::translations::save_instance_catalogue(
        &server.catalog,
        Some(&server.apps),
        &de,
        &json!({ "Delete {name}?": "{nom} löschen?" }),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(refused.contains("Delete {name}?"), "{refused}");

    // The keys are the union of the locales; French has none of them yet.
    let screen = sc_server::translations::instance_translations_json(&server.catalog).await?;
    assert_eq!(screen["messages"].as_array().unwrap().len(), 2);
    assert_eq!(screen["locales"][1]["locale"], "fr");
    assert_eq!(screen["locales"][1]["translated"], 0);

    // Translate missing fills French from those keys.
    let translator = Scripted(
        [
            ("Tasks".to_owned(), json!("Tâches")),
            ("Delete {name}?".to_owned(), json!("Supprimer {name} ?")),
        ]
        .into_iter()
        .collect(),
    );
    let report = sc_server::translations::fill_instance_missing(
        &server.catalog,
        Some(&server.apps),
        &fr,
        &translator,
    )
    .await?;
    assert_eq!(report["filled"], 2, "{report}");

    // --- The application's screen shows the shared strings it inherits, and
    // counts them as translated.
    let app_screen = sc_server::translations::translations_json(&server.catalog, &app).await?;
    let row = |key: &str| {
        app_screen["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["key"] == key)
            .cloned()
            .unwrap()
    };
    assert_eq!(row("Tasks")["inherited"]["de"], "Aufgaben");
    assert_eq!(row("Tasks")["translations"], json!({}));
    assert_eq!(row("Add a task")["inherited"], json!({}));
    assert_eq!(app_screen["locales"][0]["translated"], 2);
    assert_eq!(app_screen["locales"][0]["total"], 3);

    // The app's Translate missing asks only for what nothing covers.
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    struct Recording(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl sc_i18n::Translator for Recording {
        async fn translate_batch(
            &self,
            _source: &Locale,
            _target: &Locale,
            keys: &[String],
        ) -> Result<std::collections::BTreeMap<String, Value>> {
            self.0.lock().unwrap().extend(keys.iter().cloned());
            Ok(keys
                .iter()
                .map(|k| (k.clone(), Value::String(format!("[de] {k}"))))
                .collect())
        }
    }
    sc_server::translations::fill_missing(
        &server.catalog,
        &server.apps,
        &app,
        &de,
        &Recording(asked.clone()),
    )
    .await?;
    assert_eq!(*asked.lock().unwrap(), vec!["Add a task".to_owned()]);
    let served = server.get("/api/i18n/de.json", None).await.json();
    assert_eq!(served["Add a task"], "[de] Add a task");
    assert_eq!(served["Tasks"], "Aufgaben");
    Ok(())
}

/// The MCP tools with `scope: "instance"` (TODO F6.4): an agent fills the
/// shared catalogue once, and an application's description shows the shared
/// strings as inherited rather than missing.
#[tokio::test]
async fn a_coding_agent_fills_the_shared_catalogue_through_the_mcp_tools() -> Result<()> {
    use sc_api::mcp::{Areas, ToolContext};
    use sc_api::schema_edit::Grants;
    use serde_json::json;

    let server = setup(tasks_app(&["de"])).await?;
    write_project(server.store.path())?;
    sc_config::bootstrap_config(&server.catalog).await?;
    enable_instance_locales(&server.catalog, "en, de").await?;
    let tools = sc_app::mcp::tool_set(Grants::all(), Areas::all());
    let ctx = ToolContext {
        catalog: &server.catalog,
        user: None,
        role: 1,
        triggers: None,
        actor: "the test agent",
    };

    let empty = tools
        .call(
            "describe_translations",
            &json!({ "scope": "instance" }),
            &ctx,
        )
        .await?;
    assert_eq!(empty["scope"], "instance");
    assert!(
        empty["notes"].to_string().contains("save_translations"),
        "{empty}"
    );

    // `scope: instance` with an application is a contradiction, said so.
    let refused = tools
        .call(
            "describe_translations",
            &json!({ "scope": "instance", "application": "tasks" }),
            &ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("application"), "{refused}");

    // Any key may be added to the shared catalogue: it has no source.
    let saved = tools
        .call(
            "save_translations",
            &json!({ "scope": "instance", "locale": "de",
                     "messages": { "Tasks": "Aufgaben", "Sign in": "Anmelden" } }),
            &ctx,
        )
        .await?;
    assert_eq!(saved["written"], 2);
    assert!(saved.get("not_in_source").is_none(), "{saved}");
    // A save merges here too: one key changes, the other stays.
    tools
        .call(
            "save_translations",
            &json!({ "scope": "instance", "locale": "de",
                     "messages": { "Sign in": "Einloggen" } }),
            &ctx,
        )
        .await?;
    let shared = tools
        .call(
            "describe_translations",
            &json!({ "scope": "instance", "locale": "de" }),
            &ctx,
        )
        .await?;
    let keys: Vec<(&str, &str)> = shared["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["key"].as_str().unwrap(),
                m["translation"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(keys, [("Sign in", "Einloggen"), ("Tasks", "Aufgaben")]);

    // A locale the installation does not enable is refused.
    let refused = tools
        .call(
            "save_translations",
            &json!({ "scope": "instance", "locale": "fr", "messages": { "Tasks": "Tâches" } }),
            &ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("Localisation"), "{refused}");

    // The application lists `Tasks` as inherited, and not as missing.
    let app = tools
        .call(
            "describe_translations",
            &json!({ "application": "tasks", "locale": "de" }),
            &ctx,
        )
        .await?;
    let tasks = app["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["key"] == "Tasks")
        .unwrap();
    assert_eq!(tasks["inherited"], "Aufgaben");
    assert!(tasks["translation"].is_null());
    let missing = tools
        .call(
            "describe_translations",
            &json!({ "application": "tasks", "locale": "de", "missing_only": true }),
            &ctx,
        )
        .await?;
    let keys: Vec<&str> = missing["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["Add a task", "Delete {name}?"]);
    assert_eq!(
        server.get("/api/i18n/de.json", None).await.json()["Tasks"],
        "Aufgaben"
    );

    // Writing without `allow_edit` is refused naming the grant.
    let read_only = sc_app::mcp::tool_set(Grants::none(), Areas::all());
    let refused = read_only
        .call(
            "save_translations",
            &json!({ "scope": "instance", "locale": "de", "messages": { "Tasks": "X" } }),
            &ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("allow_edit"), "{refused}");
    Ok(())
}
