//! Saltcorn UI, rendering (TODO "Saltcorn UI" 5.8): v1's own view patterns
//! serving a restored Saltcorn 1 backup's views over HTTP, through the assembled
//! router, on the application's subdomain.
//!
//! The data is the real `saltcorn-v1-BooksDB.zip`, restored through the two
//! requests the Backup screen makes; the views are that backup's own, with their
//! v1 configurations unchanged (the import itself is Phase 8). Two claims:
//!
//! - **Live**: *List Books* renders as a document — the layout, the menu, the
//!   assets, a row per book with its author joined in — under Saltcorn UI's
//!   CSP; the page embedding the Filter renders; `/` names what there is; a
//!   viewer below a view's role is refused; the static assets are served.
//! - **Posting** (Phase 6): an insert and an update through *Edit Books*'s form,
//!   the action column's trigger through the `run_action` route, the Filter's
//!   dropdown and range narrowing the list, the Delete action, and a post with
//!   no CSRF token refused.
//! - **Who is looking** (Phase 7): an anonymous viewer sent to sign in and back
//!   to the view they asked for, signing out, sign-up where it is offered, two
//!   members seeing their own rows through one view, and a view whose table
//!   the application no longer has failing with that sentence.
//! - **Golden**: each of the six patterns (List, Show, Edit, Feed, Filter,
//!   ListShowList) renders, as an ajax request gets it, exactly the committed
//!   HTML. This is what catches a shim returning something plausible and wrong,
//!   and what lets the vendored copy be refreshed with confidence.
//!   `SC_UPDATE_GOLDEN=1` rewrites the expected files.
//!
//! Both need the built Saltcorn UI bundle, and skip without it.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use sc_api::admin_endpoints;
use sc_app::{Application, save_application};
use sc_auth::{SessionStore, create_user};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_files::FileStoreDef;
use sc_server::{
    AppMounts, CSRF_COOKIE, CSRF_HEADER, ModuleServices, SESSION_COOKIE, ServerConfig,
    admin_handlers, build_and_mount, build_router_with_apps, default_js_evaluator, install_agents,
    install_triggers,
};
use sc_test_harness::TestDb;
use sc_viewpattern::{LibraryItem, Page, View};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE_DOMAIN: &str = "example.com";
const APP_HOST: &str = "booksdb.example.com";
const ADMIN: &str = "admin@example.com";
const PASSWORD: &str = "hunter2pass";
const V1_BACKUP: &[u8] = include_bytes!("fixtures/saltcorn-v1-BooksDB.zip");
/// BooksDB with a v1 library and a legacy page; see `fixtures/README.md`.
const V1_LIBRARY_BACKUP: &[u8] = include_bytes!("fixtures/saltcorn-v1-BooksDB-library.zip");

/// `TrimPages`, as this server's trigger: v1's `modify_row` of
/// `{ pages: Math.round(pages*0.9) }`, on the row the action column is on —
/// which a `none` trigger's body is given as its `payload`.
const TRIM_PAGES: &str = "const [book] = await db.Books.where({ id: payload.id }).rows();\n\
     await db.Books.where({ id: payload.id }).update({ pages: Math.round(book.pages * 0.9) });\n\
     return { notify: `Trimmed ${book.title}` };";

/// The built bundle's directory, if there is one.
pub(crate) fn bundle_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(sc_viewpattern::BUNDLE_DIR_IN_CHECKOUT);
    dir.join(sc_viewpattern::VIEW_RUNTIME_FILE)
        .is_file()
        .then_some(dir)
}

/// A cookie-carrying client over the router, which can also speak to an
/// application's subdomain.
pub(crate) struct Client {
    router: Router,
    pub(crate) cookies: HashMap<String, String>,
}

pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) headers: axum::http::HeaderMap,
    pub(crate) body: String,
}

impl Client {
    async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let answer = self
            .request(
                method,
                None,
                path,
                &[],
                body.map(|b| (serde_json::to_vec(&b).unwrap(), "application/json")),
            )
            .await;
        (
            answer.status,
            serde_json::from_str(&answer.body).unwrap_or(Value::Null),
        )
    }

    /// A GET of `path` on the application's subdomain.
    async fn app_get(&mut self, path: &str, headers: &[(&str, &str)]) -> Answer {
        self.request("GET", Some(APP_HOST), path, headers, None)
            .await
    }

    /// A POST on the application's subdomain **as a browser sends one**: no
    /// CSRF header unless `headers` has one, so the token is wherever the
    /// caller put it — a form's `_csrf` field, v1's `CSRF-Token` header, or
    /// nowhere.
    async fn app_post(
        &mut self,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Answer {
        self.request_as(
            "POST",
            Some(APP_HOST),
            path,
            headers,
            Some((body, content_type)),
            false,
        )
        .await
    }

    pub(crate) async fn request(
        &mut self,
        method: &str,
        host: Option<&str>,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<(Vec<u8>, &str)>,
    ) -> Answer {
        self.request_as(method, host, path, headers, body, true)
            .await
    }

    async fn request_as(
        &mut self,
        method: &str,
        host: Option<&str>,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<(Vec<u8>, &str)>,
        csrf_header: bool,
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if !self.cookies.is_empty() {
            let cookie = self
                .cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            builder = builder.header(header::COOKIE, cookie);
        }
        if method != "GET"
            && csrf_header
            && let Some(csrf) = self.cookies.get(CSRF_COOKIE)
        {
            builder = builder.header(CSRF_HEADER, csrf);
        }
        let request = match body {
            Some((bytes, content_type)) => builder
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(bytes))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        for raw in response.headers().get_all(header::SET_COOKIE) {
            if let Some((name, value)) = raw
                .to_str()
                .ok()
                .and_then(|t| t.split(';').next())
                .and_then(|p| p.split_once('='))
            {
                if value.is_empty() {
                    self.cookies.remove(name);
                } else {
                    self.cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 32 * 1024 * 1024)
            .await
            .unwrap();
        Answer {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }
}

/// A server with the modules (and so the view runtime) running, BooksDB
/// restored, the admin signed in, and the BooksDB Saltcorn UI application
/// mounted with the backup's views, one Feed, one ListShowList and its page.
pub(crate) struct Server {
    pub(crate) client: Client,
    /// What `/api/backup/restore` answered: `{ restored, warnings }`.
    restore_report: Value,
    pub(crate) apps: Arc<AppMounts>,
    _catalog: Arc<Catalog>,
    _modules: Arc<ModuleServices>,
    _db: TestDb,
    _files: TempDir,
    _registries: tokio::sync::RwLockReadGuard<'static, ()>,
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) async fn setup(tag: &str, bundle: PathBuf) -> sc_error::Result<Server> {
    setup_with(tag, bundle, true).await
}

/// [`setup`], choosing whether to write `TrimPages` as this server's trigger.
/// Without it the application declares a trigger the restore refused, which is
/// what a real import of BooksDB leaves behind.
async fn setup_with(
    tag: &str,
    bundle: PathBuf,
    write_trim_pages: bool,
) -> sc_error::Result<Server> {
    setup_from(tag, bundle, V1_BACKUP, write_trim_pages).await
}

/// [`setup_with`], restoring `archive` rather than the plain BooksDB backup.
async fn setup_from(
    tag: &str,
    bundle: PathBuf,
    archive: &[u8],
    write_trim_pages: bool,
) -> sc_error::Result<Server> {
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
    sc_app::bootstrap(&catalog).await?;
    sc_config::bootstrap(&catalog).await?;
    sc_catalog::bootstrap_table_meta(&catalog).await?;
    sc_catalog::bootstrap_field_meta(&catalog).await?;
    sc_catalog::bootstrap_file_stores(&catalog).await?;
    sc_llm::bootstrap_llm_providers(&catalog).await?;
    sc_viewpattern::bootstrap(&catalog).await?;
    let agents = install_agents(&catalog).await?;
    let models = sc_server::install_models(&catalog, sc_model::DEFAULT_MAX_ROWS).await?;
    let dispatcher = install_triggers(&catalog, default_js_evaluator(), &agents, &models).await?;
    let root = std::env::temp_dir().join(format!(
        "sc-saltcorn-ui-render-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let registries = crate::module_registries().read().await;
    let modules = ModuleServices::install(
        &catalog,
        &dispatcher,
        &agents,
        &models,
        Some(root.join("modules")),
        None,
        1,
        sc_server::default_python_adapter(),
        Some(bundle.clone()),
    )
    .await?;

    let apps = Arc::new(
        AppMounts::new(catalog.clone())
            .with_agents(agents)
            .with_triggers(dispatcher)
            .with_saltcorn_ui_dir(Some(bundle)),
    );
    let config = ServerConfig {
        base_domain: Some(BASE_DOMAIN.to_owned()),
        builder_dir: crate::builder_route::builder_bundle_dir(),
        ..ServerConfig::default()
    };
    let router = build_router_with_apps(
        &admin_endpoints(),
        admin_handlers(catalog.clone(), apps.clone()),
        Arc::new(SessionStore::default()),
        &config,
        apps.clone(),
    )?;
    let mut client = Client {
        router,
        cookies: HashMap::new(),
    };
    client.send("GET", "/api/auth/status", None).await;
    let (status, body) = client
        .send(
            "POST",
            "/api/first-user",
            Some(json!({ "email": ADMIN, "password": PASSWORD })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // --- BooksDB's tables and rows, restored as the Backup screen restores them.
    let files = root.join("files");
    std::fs::create_dir_all(&files)?;
    sc_catalog::save_file_store(
        &catalog,
        &FileStoreDef::local("BooksDB", files.to_string_lossy()),
    )
    .await?;
    sc_catalog::connect_file_store_def(
        &catalog,
        &sc_catalog::load_file_store_by_name(&catalog, "BooksDB")
            .await?
            .expect("the store just saved"),
    )?;
    let uploaded = client
        .request(
            "POST",
            None,
            "/backup/upload",
            &[],
            Some((archive.to_vec(), "application/zip")),
        )
        .await;
    assert_eq!(uploaded.status, StatusCode::OK, "{}", uploaded.body);
    let uploaded: Value = serde_json::from_str(&uploaded.body).unwrap();
    let (status, report) = client
        .send(
            "POST",
            "/api/backup/restore",
            Some(json!({ "id": uploaded["id"], "include": uploaded["include"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");

    // --- The trigger List Books' action column runs. BooksDB's `TrimPages` is a
    // v1 `modify_row`, an action this server does not have, so the restore
    // leaves it out; this is the same trigger written as this server writes one,
    // so the view names a trigger its application declares (§12.3).
    if write_trim_pages {
        let (status, created) = client
            .send(
                "POST",
                "/api/triggers",
                Some(json!({
                    "name": "TrimPages",
                    "description": "",
                    "when": "none",
                    "channel": Value::Null,
                    "only_if": Value::Null,
                    "action": "run_js_code",
                    "configuration": { "code": TRIM_PAGES },
                    "min_role": Value::Null,
                    "enabled": true,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
    }

    // --- The application the restore made of the backup, its views and its page
    // (Phase 8) — with this test's own menu, and the two patterns BooksDB has no
    // view of.
    let mut app = sc_app::load_application_by_subdomain(&catalog, "booksdb")
        .await?
        .unwrap_or_else(|| panic!("the restore made BooksDB an application: {report}"));
    assert!(
        app.triggers.iter().any(|t| t.0 == "TrimPages"),
        "{:?}",
        app.triggers
    );
    app.framework.config.insert(
        "menu_items".to_owned(),
        json!([
            { "type": "Page", "label": "Overview", "pagename": "BooksOverview", "min_role": 1 },
            { "type": "View", "label": "Books", "viewname": "List Books", "min_role": 1 },
            { "type": "Admin Page", "label": "Tables", "admin_page": "Tables", "min_role": 1 },
        ]),
    );
    let app = save_application(&catalog, &app).await?;
    assert_eq!(sc_viewpattern::list_views(&catalog, app.id).await?.len(), 7);

    let feed = View::new(app.id, "Feed Books", "Feed", "Books")
        .min_role(1)
        .configuration(object(json!({
            "show_view": "Show Books", "order_field": "title", "descending": false,
            "cols_sm": 1, "cols_md": 1, "cols_lg": 1, "cols_xl": 1,
        })));
    sc_viewpattern::save_view(&catalog, &feed).await?;
    let lsl = View::new(app.id, "Books and details", "ListShowList", "Books")
        .min_role(1)
        .configuration(object(json!({
            "list_view": "List Books", "show_view": "Show Books", "list_width": 4, "subtables": {},
        })));
    sc_viewpattern::save_view(&catalog, &lsl).await?;

    // Saving the rows is not deploying them; the mount re-reads them.
    build_and_mount(&apps, app).await?;

    Ok(Server {
        client,
        restore_report: report,
        apps: apps.clone(),
        _catalog: catalog,
        _modules: modules,
        _db: db,
        _files: TempDir(root),
        _registries: registries,
    })
}

fn object(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn zip_entry(archive: &[u8], path: &str) -> String {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("a zip");
    let mut file = zip.by_name(path).expect("the entry");
    let mut text = String::new();
    file.read_to_string(&mut text).expect("text");
    text
}

/// 5.5–5.8: *List Books* over HTTP, and the routes around it.
#[tokio::test]
async fn list_books_renders_over_http_on_the_applications_subdomain() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("live", bundle).await?;
    let client = &mut server.client;

    let list = client.app_get("/view/List%20Books", &[]).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(
        list.headers[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    // §10: the framework's own policy, and nothing but `script-src` relaxed.
    let csp = list.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(csp.contains("script-src 'self' 'unsafe-inline'"), "{csp}");
    assert!(csp.contains("default-src 'self'"), "{csp}");
    assert!(!csp.contains("unsafe-eval"), "{csp}");

    let html = &list.body;
    // The document (5.6): the head, the assets, the title.
    assert!(html.starts_with("<!doctype html>"), "{html}");
    assert!(html.contains("<title>List Books</title>"), "{html}");
    for asset in [
        "bootstrap.min.css",
        "jquery-3.6.0.min.js",
        "saltcorn-common.js",
        "saltcorn.js",
    ] {
        assert!(
            html.contains(&format!(
                "/static_assets/{}/{asset}",
                sc_viewpattern::ASSET_VERSION_TAG
            )),
            "{asset}: {html}"
        );
    }
    // The layout: the navbar with the brand and the menu, minus the v1 admin page.
    assert!(html.contains("id=\"mainNav\""), "{html}");
    assert!(html.contains(">BooksDB<"), "{html}");
    assert!(html.contains("href=\"/page/BooksOverview\""), "{html}");
    assert!(html.contains("href=\"/view/List%20Books\""), "{html}");
    assert!(!html.contains(">Tables<"), "{html}");
    // The list: v1's own table, a row per book, the authors' last names joined in.
    let books = row_count(client, "Books").await;
    assert!(books > 0);
    let rows = html.matches("<tr").count();
    assert_eq!(rows, 1 + books, "a header and {books} books: {html}");
    for author in authors(client).await {
        assert!(html.contains(&format!(">{author}<")), "{author}: {html}");
    }
    assert!(html.contains("/view/Show%20Books?id="), "{html}");

    // The same view to an ajax reload: the HTML alone, no document.
    let fragment = client
        .app_get(
            "/view/List%20Books",
            &[("X-Requested-With", "XMLHttpRequest")],
        )
        .await;
    assert_eq!(fragment.status, StatusCode::OK);
    assert!(
        !fragment.body.contains("<!doctype html>"),
        "{}",
        fragment.body
    );
    assert_eq!(fragment.body.matches("<tr").count(), 1 + books);

    // The page: the Filter, and the list it embeds.
    let page = client.app_get("/page/BooksOverview", &[]).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("data-sc-view-source"), "{}", page.body);
    assert_eq!(page.body.matches("<tr").count(), 1 + books, "{}", page.body);

    // `/`: no root page for any role, so the document names what there is.
    let index = client.app_get("/", &[]).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(
        index.body.contains("href=\"/page/BooksOverview\""),
        "{}",
        index.body
    );
    assert!(
        index.body.contains("href=\"/view/List%20Books\""),
        "{}",
        index.body
    );

    // A static asset, from the bundle.
    let css = client
        .app_get(
            &format!(
                "/static_assets/{}/saltcorn.css",
                sc_viewpattern::ASSET_VERSION_TAG
            ),
            &[],
        )
        .await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
    let escape = client
        .app_get("/static_assets/x/..%2F..%2Fview-runtime.js", &[])
        .await;
    assert_eq!(escape.status, StatusCode::NOT_FOUND);

    // Nothing by that name, and a viewer below the view's role.
    let missing = client.app_get("/view/No%20such%20view", &[]).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert!(missing.body.contains("No such view"), "{}", missing.body);
    let mut anonymous = Client {
        router: client.router.clone(),
        cookies: HashMap::new(),
    };
    let refused = anonymous.app_get("/view/List%20Books", &[]).await;
    assert_eq!(refused.status, StatusCode::FOUND);
    assert_eq!(
        refused.headers[header::LOCATION],
        "/auth/login?dest=%2Fview%2FList%2520Books"
    );
    assert!(!refused.body.contains("<tr"), "{}", refused.body);
    Ok(())
}

const FORM: &str = "application/x-www-form-urlencoded";

/// A URL-encoded form body, as a browser encodes one.
fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    let encode = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    char::from(b).to_string()
                }
                b' ' => "+".to_owned(),
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

/// The book with `id`, read back through the admin API.
async fn book(client: &mut Client, id: &Value) -> Option<Value> {
    rows_of(client, "Books")
        .await
        .into_iter()
        .find(|b| &b["id"] == id)
}

/// Phase 6: what a browser does with the forms, buttons and links v1's patterns
/// render — through the assembled router, CSRF middleware and all.
#[tokio::test]
async fn the_views_save_forms_run_actions_filter_and_delete() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("post", bundle).await?;
    let client = &mut server.client;
    let csrf = client
        .cookies
        .get(CSRF_COOKIE)
        .cloned()
        .expect("the admin's requests were given a CSRF cookie");

    // 6.4: the rendered form carries the viewer's token, and so does the page
    // script `saltcorn.js` posts with.
    let edit = client.app_get("/view/Edit%20Books?id=1", &[]).await;
    assert_eq!(edit.status, StatusCode::OK, "{}", edit.body);
    assert!(
        edit.body
            .contains(&format!("name=\"_csrf\" value=\"{csrf}\"")),
        "{}",
        edit.body
    );
    assert!(
        edit.body
            .contains(&format!("var _sc_globalCsrf = \"{csrf}\"")), // v1's global
        "{}",
        edit.body
    );

    let before = row_count(client, "Books").await;
    let new_book = |title: &'static str, extra: &[(&'static str, String)]| {
        let mut pairs: Vec<(&str, String)> = vec![
            ("author", "2".to_owned()),
            ("pages", "321".to_owned()),
            ("published_on", "2021-03-04".to_owned()),
            ("publisher", "1".to_owned()),
            ("title", title.to_owned()),
        ];
        pairs.extend(extra.iter().cloned());
        let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        form(&borrowed)
    };

    // 6.5: a form posted with no token, or with the wrong one, is refused
    // before any pattern runs, and nothing is written.
    let refused = client
        .app_post("/view/Edit%20Books", FORM, new_book("Forged", &[]), &[])
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    let forged = client
        .app_post(
            "/view/Edit%20Books",
            FORM,
            new_book("Forged", &[("_csrf", "0".repeat(64))]),
            &[],
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN, "{}", forged.body);
    assert_eq!(row_count(client, "Books").await, before);

    // 6.1: an insert through Edit Books — the form as the browser submits it,
    // the token in its `_csrf` field — lands a row and redirects.
    let inserted = client
        .app_post(
            "/view/Edit%20Books",
            FORM,
            new_book("Anna Karenina", &[("_csrf", csrf.clone())]),
            &[],
        )
        .await;
    assert_eq!(inserted.status, StatusCode::FOUND, "{}", inserted.body);
    assert!(inserted.headers.contains_key(header::LOCATION));
    let books = rows_of(client, "Books").await;
    assert_eq!(books.len(), before + 1);
    let added = books
        .iter()
        .find(|b| b["title"] == "Anna Karenina")
        .cloned()
        .expect("the inserted book");
    assert_eq!(added["pages"], json!(321));
    assert_eq!(added["author"], json!(2));
    let id = added["id"].clone();

    // An update: the same form, with the row's id.
    let updated = client
        .app_post(
            "/view/Edit%20Books",
            FORM,
            new_book(
                "Anna Karenina (revised)",
                &[("id", id.to_string()), ("_csrf", csrf.clone())],
            ),
            &[],
        )
        .await;
    assert_eq!(updated.status, StatusCode::FOUND, "{}", updated.body);
    assert_eq!(row_count(client, "Books").await, before + 1);
    assert_eq!(
        book(client, &id).await.expect("still there")["title"],
        json!("Anna Karenina (revised)")
    );

    // 6.2 and 6.3: the action column runs the trigger the application
    // declares, through the `run_action` route, as `saltcorn.js`'s `view_post`
    // sends it — JSON, v1's `CSRF-Token` header — and answers JSON.
    let ran = client
        .app_post(
            "/view/List%20Books/run_action",
            "application/json",
            json!({ "rndid": "ce2dfa", "id": id.to_string(), "column_index": 8 })
                .to_string()
                .into_bytes(),
            &[
                ("CSRF-Token", csrf.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ],
        )
        .await;
    assert_eq!(ran.status, StatusCode::OK, "{}", ran.body);
    assert_eq!(ran.headers[header::CONTENT_TYPE], "application/json");
    let answer: Value = serde_json::from_str(&ran.body).unwrap();
    assert_eq!(answer["success"], json!("ok"), "{answer}");
    assert_eq!(
        book(client, &id).await.expect("still there")["pages"],
        json!(289),
        "TrimPages ran on the row the column is on"
    );

    // 6.5: the Filter's dropdown and range round-trip through the state and
    // narrow the list embedded under them. Moby Dick is Melville's and 500
    // pages; War and Peace and Anna Karenina are Tolstoy's.
    let everything = client.app_get("/page/BooksOverview", &[]).await;
    assert_eq!(everything.body.matches("<tr").count(), 1 + before + 1);
    let melville = client.app_get("/page/BooksOverview?author=1", &[]).await;
    assert_eq!(melville.status, StatusCode::OK, "{}", melville.body);
    assert_eq!(melville.body.matches("<tr").count(), 2, "{}", melville.body);
    assert!(melville.body.contains(">Moby Dick<"), "{}", melville.body);
    assert!(!melville.body.contains(">War and Peace<"));
    assert!(
        regex_lite::Regex::new(r#"<option value="1"[^>]*selected"#)
            .unwrap()
            .is_match(&melville.body),
        "the dropdown shows the state it was given: {}",
        melville.body
    );
    let long = client
        .app_get("/page/BooksOverview?_gte_pages=600", &[])
        .await;
    assert_eq!(long.status, StatusCode::OK, "{}", long.body);
    assert_eq!(long.body.matches("<tr").count(), 2, "{}", long.body);
    assert!(long.body.contains(">War and Peace<"), "{}", long.body);
    assert!(long.body.contains("value=\"600\""), "{}", long.body);
    // What the Filter's reload fetches when the dropdown changes: the list
    // alone, over the new state.
    let reloaded = client
        .app_get(
            "/view/List%20Books?author=2&_lte_pages=400",
            &[("X-Requested-With", "XMLHttpRequest")],
        )
        .await;
    assert_eq!(reloaded.body.matches("<tr").count(), 2, "{}", reloaded.body);
    assert!(
        reloaded.body.contains(">Anna Karenina (revised)<"),
        "{}",
        reloaded.body
    );

    // The Delete action: `ajax_post_btn` to `/delete/<table>/<id>`, which
    // answers the ajax call and deletes the row as the viewer.
    let deleted = client
        .app_post(
            &format!("/delete/Books/{id}?redirect=/view/List%20Books"),
            FORM,
            Vec::new(),
            &[
                ("CSRF-Token", csrf.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ],
        )
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(
        serde_json::from_str::<Value>(&deleted.body).unwrap(),
        json!({ "success": true })
    );
    assert!(book(client, &id).await.is_none());
    assert_eq!(row_count(client, "Books").await, before);
    // A form post is sent back where it came from — a path on the application,
    // and nowhere else.
    let again = client
        .app_post(
            &format!("/delete/Books/{id}?redirect=/view/List%20Books"),
            FORM,
            form(&[("_csrf", &csrf)]),
            &[],
        )
        .await;
    assert_eq!(again.status, StatusCode::FOUND, "{}", again.body);
    assert_eq!(again.headers[header::LOCATION], "/view/List%20Books");
    let offsite = client
        .app_post(
            "/delete/Books/1?redirect=https://evil.example/",
            FORM,
            form(&[("_csrf", &csrf)]),
            &[("X-Requested-With", "")],
        )
        .await;
    assert_eq!(offsite.headers[header::LOCATION], "/");
    // A table outside the application's subset is not one it deletes from.
    let outside = client
        .app_post(
            "/delete/users/1",
            FORM,
            Vec::new(),
            &[
                ("CSRF-Token", csrf.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ],
        )
        .await;
    let outside: Value = serde_json::from_str(&outside.body).unwrap();
    assert_eq!(outside["success"], json!(false), "{outside}");
    assert!(
        outside["error"].as_str().unwrap().contains("`users`"),
        "{outside}"
    );

    Ok(())
}

/// 6.3, §12.3: an action a view may not run. At save time, where the
/// configuration names it; at run time, when the application stops declaring
/// the trigger a stored view names.
#[tokio::test]
async fn an_action_that_is_not_the_applications_is_refused_by_name() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("refuse", bundle).await?;
    let catalog = server._catalog.clone();
    let app = sc_app::list_applications(&catalog)
        .await?
        .into_iter()
        .find(|a| a.subdomain == "booksdb")
        .expect("the application");

    // Save time: v1's state action by its own name, and a trigger the
    // application does not declare.
    for action in ["run_js_code", "AddBook"] {
        let view = View::new(app.id, "Runs something", "List", "Books")
            .min_role(1)
            .configuration(object(json!({
                "columns": [{ "type": "Action", "action_name": action, "rndid": "abc123" }],
            })));
        let msg = sc_viewpattern::save_view(&catalog, &view)
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains(&format!("`{action}`")), "{msg}");
        assert!(msg.contains("it declares TrimPages"), "{msg}");
    }

    // Run time: the application no longer declares TrimPages, and List Books
    // still names it. `Trigger.findOne` no longer finds it, so the action
    // column's route answers what v1's `run_action` answers for an action it
    // cannot find — `{ error }`, naming it, with a 200 — and the row is
    // untouched.
    let mut without = app.clone();
    without.triggers.clear();
    let without = save_application(&catalog, &without).await?;
    build_and_mount(&server.apps, without).await?;
    let client = &mut server.client;
    let csrf = client.cookies.get(CSRF_COOKIE).cloned().unwrap();
    let pages = book(client, &json!(1)).await.unwrap()["pages"].clone();
    let ran = client
        .app_post(
            "/view/List%20Books/run_action",
            "application/json",
            json!({ "rndid": "ce2dfa", "id": "1", "column_index": 8 })
                .to_string()
                .into_bytes(),
            &[
                ("CSRF-Token", csrf.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ],
        )
        .await;
    assert_eq!(ran.status, StatusCode::OK, "{}", ran.body);
    let answer: Value = serde_json::from_str(&ran.body).unwrap();
    assert!(
        answer["error"].as_str().unwrap().contains("TrimPages"),
        "{answer}"
    );
    assert_eq!(book(client, &json!(1)).await.unwrap()["pages"], pages);
    Ok(())
}

/// Found by running the definition of done by hand (12.4): BooksDB restored as it
/// comes — its `TrimPages` refused, because `modify_row` is not an action here,
/// and nobody writing it again — still mounts and serves. The application declares
/// the trigger (the import puts every v1 trigger in its subset, so List Books can
/// be saved), and an application with no API exposes no trigger, so the missing
/// one is found when the action column runs it, not on the mount.
#[tokio::test]
async fn a_restored_backup_serves_although_a_trigger_it_names_was_refused() -> sc_error::Result<()>
{
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup_with("refused-trigger", bundle, false).await?;
    let report = server.restore_report.clone();
    let lines = |key: &str| -> Vec<String> {
        report[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_owned())
            .collect()
    };
    assert!(
        lines("warnings")
            .iter()
            .any(|l| l.starts_with("trigger `TrimPages`") && l.contains("modify_row")),
        "{report}"
    );
    assert!(
        lines("restored")
            .iter()
            .any(|l| l == "application `booksdb` serving"),
        "the restore mounts the application: {report}"
    );
    assert!(
        !lines("warnings")
            .iter()
            .any(|l| l.contains("application `booksdb`")),
        "{report}"
    );
    let app = booksdb(&server._catalog).await;
    assert!(app.triggers.iter().any(|t| t.0 == "TrimPages"));

    // Every view renders, List Books with a row per book.
    let client = &mut server.client;
    let list = client.app_get("/view/List%20Books", &[]).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    for row in rows_of(client, "Books").await {
        let title = row["title"].as_str().unwrap();
        assert!(list.body.contains(title), "{title} in {}", list.body);
    }

    // Its TrimPages column answers naming the trigger, and changes nothing.
    let csrf = client.cookies.get(CSRF_COOKIE).cloned().unwrap();
    let pages = book(client, &json!(1)).await.unwrap()["pages"].clone();
    let ran = client
        .app_post(
            "/view/List%20Books/run_action",
            "application/json",
            json!({ "rndid": "ce2dfa", "id": "1", "column_index": 8 })
                .to_string()
                .into_bytes(),
            &[
                ("CSRF-Token", csrf.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ],
        )
        .await;
    let answer: Value = serde_json::from_str(&ran.body).unwrap_or(Value::Null);
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains("TrimPages")),
        "{}: {}",
        ran.status,
        ran.body
    );
    assert_eq!(book(client, &json!(1)).await.unwrap()["pages"], pages);
    Ok(())
}

/// The builder, 10.4: what the definition of done's public page asks of a List
/// it embeds, found by running it in a browser.
///
/// - **A search narrows it.** Filter's and List's search bars set v1's `_fts`
///   state, as `_fts_<table>` (`table.santized_name`). The list failed naming
///   `db.getTenantSchema`, because the worker's `db` answered
///   `supports_multiple_schemas` with a truthy stub, and the Table shim then
///   refused `_fts` outright.
/// - **A stranger is not drawn Delete.** A viewer allowed the view but not the
///   table's writes gets v1's owner check (`table.is_owner`) before each Delete
///   link, and the whole list failed with "is_owner is not a function".
#[tokio::test]
async fn a_search_narrows_a_list_and_a_stranger_is_not_drawn_delete() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("search", bundle).await?;
    let app = booksdb(&server._catalog).await;
    let client = &mut server.client;

    let titles: Vec<String> = rows_of(client, "Books")
        .await
        .iter()
        .map(|row| row["title"].as_str().unwrap().to_owned())
        .collect();
    let found = titles
        .iter()
        .find(|t| t.contains("Moby"))
        .expect("Moby Dick")
        .clone();
    let others: Vec<&String> = titles.iter().filter(|t| **t != found).collect();
    assert!(!others.is_empty(), "{titles:?}");

    // Both of v1's spellings: the table's own and the bare one, which also
    // shows the match is case-insensitive.
    for query in ["_fts_Books=Moby", "_fts=moby"] {
        let list = client
            .app_get(&format!("/view/List%20Books?{query}"), &[])
            .await;
        assert_eq!(list.status, StatusCode::OK, "{query}: {}", list.body);
        assert!(list.body.contains(&found), "{query}: {}", list.body);
        for other in &others {
            assert!(
                !list.body.contains(other.as_str()),
                "{query} kept {other}: {}",
                list.body
            );
        }
    }
    let nothing = client
        .app_get("/view/List%20Books?_fts_Books=no-such-words", &[])
        .await;
    assert_eq!(nothing.status, StatusCode::OK, "{}", nothing.body);
    for title in &titles {
        assert!(
            !nothing.body.contains(title.as_str()),
            "{title}: {}",
            nothing.body
        );
    }

    // The admin is drawn Delete.
    let admin_list = client.app_get("/view/List%20Books", &[]).await;
    assert_eq!(admin_list.status, StatusCode::OK, "{}", admin_list.body);
    assert!(
        admin_list.body.contains("/delete/Books/"),
        "{}",
        admin_list.body
    );

    // A public List over tables anybody may read and only an admin may write.
    for table in ["Books", "Authors", "Publishers"] {
        let (status, body) = client
            .send(
                "PUT",
                &format!("/api/tables/{table}"),
                Some(json!({
                    "label": "",
                    "description": "",
                    "min_role_read": 100,
                    "min_role_write": 1,
                    "ownership_formula": "",
                    "rls_enabled": false,
                    "sync_enabled": false,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{table}: {body}");
    }
    let view_path = format!("/api/applications/{}/views/List%20Books", app.id.0);
    let (status, mut view) = client.send("GET", &view_path, None).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    view["min_role"] = json!(100);
    let (status, body) = client.send("PUT", &view_path, Some(view)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mut stranger = visitor(client);
    let list = stranger.app_get("/view/List%20Books", &[]).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    for title in &titles {
        assert!(list.body.contains(title.as_str()), "{title}: {}", list.body);
    }
    assert!(!list.body.contains("/delete/Books/"), "{}", list.body);
    // And the search works for them too.
    let searched = stranger
        .app_get("/view/List%20Books?_fts_Books=Moby", &[])
        .await;
    assert_eq!(searched.status, StatusCode::OK, "{}", searched.body);
    assert!(searched.body.contains(&found), "{}", searched.body);
    Ok(())
}

/// The BooksDB application, as it is saved.
pub(crate) async fn booksdb(catalog: &Catalog) -> Application {
    sc_app::list_applications(catalog)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.subdomain == "booksdb")
        .expect("the application")
}

/// A browser with nothing in it yet, on the same server as `client`.
pub(crate) fn visitor(client: &Client) -> Client {
    Client {
        router: client.router.clone(),
        cookies: HashMap::new(),
    }
}

/// `Location`, as a string.
fn location(answer: &Answer) -> String {
    answer.headers[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned()
}

/// Sign `email` in through the application's own form, as a browser does: the
/// form first (which is what gives a new browser its CSRF cookie), then the post.
async fn sign_in(client: &Client, email: &str, password: &str) -> Client {
    let mut browser = visitor(client);
    let form_page = browser.app_get("/auth/login", &[]).await;
    assert_eq!(form_page.status, StatusCode::OK, "{}", form_page.body);
    let csrf = browser.cookies.get(CSRF_COOKIE).cloned().expect("a token");
    let signed = browser
        .app_post(
            "/auth/login",
            FORM,
            form(&[("_csrf", &csrf), ("email", email), ("password", password)]),
            &[],
        )
        .await;
    assert_eq!(signed.status, StatusCode::FOUND, "{email}: {}", signed.body);
    assert!(browser.cookies.contains_key(SESSION_COOKIE));
    browser
}

/// TODO F9: once the application's `access_if` refuses everybody, a signed-in
/// admin is sent to sign in rather than shown a view, the sign-in form is still
/// served, and signing in again is refused without a session.
#[tokio::test]
async fn an_application_that_refuses_the_caller_serves_only_its_sign_in() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("access", bundle).await?;
    let mut admin = sign_in(&server.client, ADMIN, PASSWORD).await;
    let shown = admin.app_get("/view/List%20Books", &[]).await;
    assert_eq!(shown.status, StatusCode::OK, "{}", shown.body);

    // Refuse everybody, through the admin API as the form saves.
    let id = booksdb(&server._catalog).await.id.to_string();
    let (_, list) = server.client.send("GET", "/api/applications", None).await;
    let mut body = list
        .as_array()
        .and_then(|apps| apps.iter().find(|a| a["id"] == json!(id)))
        .cloned()
        .expect("listed");
    body["access_if"] = json!("false");
    let (status, saved) = server
        .client
        .send("PUT", &format!("/api/applications/{id}"), Some(body))
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let refused = admin.app_get("/view/List%20Books", &[]).await;
    assert_eq!(refused.status, StatusCode::FOUND, "{}", refused.body);
    assert_eq!(
        location(&refused),
        "/auth/login?dest=%2Fview%2FList%2520Books"
    );
    assert!(!refused.body.contains("<tr"), "{}", refused.body);

    let mut browser = visitor(&server.client);
    let form_page = browser.app_get("/auth/login", &[]).await;
    assert_eq!(form_page.status, StatusCode::OK, "{}", form_page.body);
    let csrf = browser.cookies.get(CSRF_COOKIE).cloned().expect("a token");
    let signed = browser
        .app_post(
            "/auth/login",
            FORM,
            form(&[("_csrf", &csrf), ("email", ADMIN), ("password", PASSWORD)]),
            &[],
        )
        .await;
    assert_eq!(signed.status, StatusCode::FORBIDDEN, "{}", signed.body);
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    Ok(())
}

/// 7.1, 7.3, 7.4 and 7.5: somebody who is not signed in is sent to sign in and
/// comes back to what they asked for; signing out leaves; sign-up is offered
/// only when the settings say so; and a view whose table the application no
/// longer has fails naming it.
#[tokio::test]
async fn signing_in_lands_on_the_view_asked_for_and_signing_out_leaves() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("auth", bundle).await?;
    let catalog = server._catalog.clone();
    let mut browser = visitor(&server.client);

    // 7.1: every imported view is `min_role` 1, so a navigation is sent to sign
    // in with the way back, and the view is not run.
    let asked = browser.app_get("/view/List%20Books?author=1", &[]).await;
    assert_eq!(asked.status, StatusCode::FOUND, "{}", asked.body);
    let to_login = location(&asked);
    assert_eq!(
        to_login,
        "/auth/login?dest=%2Fview%2FList%2520Books%3Fauthor%3D1"
    );
    assert!(!asked.body.contains("<tr"), "{}", asked.body);
    let page = browser.app_get("/page/BooksOverview", &[]).await;
    assert_eq!(page.status, StatusCode::FOUND);
    assert_eq!(location(&page), "/auth/login?dest=%2Fpage%2FBooksOverview");
    // An ajax reload cannot follow a form: it is told 401.
    let ajax = browser
        .app_get(
            "/view/List%20Books",
            &[("X-Requested-With", "XMLHttpRequest")],
        )
        .await;
    assert_eq!(ajax.status, StatusCode::UNAUTHORIZED, "{}", ajax.body);

    // 7.3: the form, carrying the way back and the browser's token.
    let login = browser.app_get(&to_login, &[]).await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.body);
    let csrf = browser.cookies.get(CSRF_COOKIE).cloned().expect("a token");
    let dest = "/view/List%20Books?author=1";
    for part in [
        "action=\"/auth/login\"".to_owned(),
        format!("name=\"dest\" value=\"{dest}\""),
        format!("name=\"_csrf\" value=\"{csrf}\""),
    ] {
        assert!(login.body.contains(&part), "{part}: {}", login.body);
    }
    assert!(
        !login.body.contains("/auth/signup"),
        "sign-up is not offered: {}",
        login.body
    );

    // A wrong password is refused, the email kept, nobody signed in.
    let wrong = browser
        .app_post(
            "/auth/login",
            FORM,
            form(&[
                ("_csrf", &csrf),
                ("dest", dest),
                ("email", ADMIN),
                ("password", "not-the-password"),
            ]),
            &[],
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{}", wrong.body);
    assert!(
        wrong.body.contains("Incorrect email or password"),
        "{}",
        wrong.body
    );
    assert!(
        wrong.body.contains(&format!("value=\"{ADMIN}\"")),
        "{}",
        wrong.body
    );
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    // So is a sign-in with no token: a form on another site cannot sign this
    // browser in.
    let forged = browser
        .app_post(
            "/auth/login",
            FORM,
            form(&[("email", ADMIN), ("password", PASSWORD)]),
            &[],
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN, "{}", forged.body);
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));

    // The right one: signed in, and back where they were going.
    let signed = browser
        .app_post(
            "/auth/login",
            FORM,
            form(&[
                ("_csrf", &csrf),
                ("dest", dest),
                ("email", ADMIN),
                ("password", PASSWORD),
            ]),
            &[],
        )
        .await;
    assert_eq!(signed.status, StatusCode::FOUND, "{}", signed.body);
    assert_eq!(location(&signed), dest);
    assert!(browser.cookies.contains_key(SESSION_COOKIE));
    let landed = browser.app_get(dest, &[]).await;
    assert_eq!(landed.status, StatusCode::OK, "{}", landed.body);
    assert!(landed.body.contains("<tr"), "{}", landed.body);
    assert!(
        landed.body.contains("href=\"/auth/logout\""),
        "{}",
        landed.body
    );

    // Signing out: the session ends, and the view asks again.
    let out = browser.app_get("/auth/logout", &[]).await;
    assert_eq!(out.status, StatusCode::FOUND);
    assert_eq!(location(&out), "/");
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    let again = browser.app_get("/view/List%20Books", &[]).await;
    assert_eq!(again.status, StatusCode::FOUND, "{}", again.body);

    // Sign-up is not offered until the settings say so…
    let closed = browser.app_get("/auth/signup", &[]).await;
    assert_eq!(closed.status, StatusCode::NOT_FOUND);
    assert!(
        closed.body.contains("does not offer sign-up"),
        "{}",
        closed.body
    );
    // …and then it is, making an account of the role the settings give (80,
    // v1's `user`, which the restore brought with it).
    let mut open = booksdb(&catalog).await;
    open.framework = open.framework.clone().with("allow_signup", true);
    let open = save_application(&catalog, &open).await?;
    build_and_mount(&server.apps, open.clone()).await?;
    let offered = browser.app_get("/auth/login", &[]).await;
    assert!(offered.body.contains("/auth/signup"), "{}", offered.body);
    let signup = browser
        .app_get("/auth/signup?dest=%2Fview%2FList%2520Books", &[])
        .await;
    assert_eq!(signup.status, StatusCode::OK, "{}", signup.body);
    let csrf = browser.cookies.get(CSRF_COOKIE).cloned().expect("a token");
    let reader = "reader@example.com";
    let attempt = |email: &str, password: &str, repeat: &str| {
        form(&[
            ("_csrf", &csrf),
            ("dest", "/view/List%20Books"),
            ("email", email),
            ("password", password),
            ("passwordRepeat", repeat),
        ])
    };
    for (body, problem) in [
        (attempt(reader, "one-password", "another"), "not the same"),
        (
            attempt(ADMIN, "one-password", "one-password"),
            "already an account",
        ),
        (attempt(reader, "", ""), "password is required"),
    ] {
        let refused = browser.app_post("/auth/signup", FORM, body, &[]).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
        assert!(
            refused.body.contains(problem),
            "{problem}: {}",
            refused.body
        );
    }
    let made = browser
        .app_post(
            "/auth/signup",
            FORM,
            attempt(reader, "reader-pass", "reader-pass"),
            &[],
        )
        .await;
    assert_eq!(made.status, StatusCode::FOUND, "{}", made.body);
    assert_eq!(location(&made), "/view/List%20Books");
    assert!(browser.cookies.contains_key(SESSION_COOKIE));
    let account = sc_auth::load_user_by_email(&catalog, reader)
        .await?
        .expect("the account");
    assert_eq!(account.role, 80);
    // Signed in, and still not allowed: a role-80 user is told so, by name.
    let forbidden = browser.app_get("/view/List%20Books", &[]).await;
    assert_eq!(
        forbidden.status,
        StatusCode::FORBIDDEN,
        "{}",
        forbidden.body
    );
    assert!(
        forbidden.body.contains("may not see the view List Books"),
        "{}",
        forbidden.body
    );

    // 7.4: the application stops having Books, under views that name it. The
    // view and the page embedding one fail with that sentence, not an empty list.
    let mut narrowed = open;
    narrowed.tables.retain(|t| t.0 != "Books");
    let narrowed = save_application(&catalog, &narrowed).await?;
    build_and_mount(&server.apps, narrowed).await?;
    let sentence = "names the table Books, which the application BooksDB does not have";
    for path in ["/view/List%20Books", "/page/BooksOverview"] {
        let failed = server.client.app_get(path, &[]).await;
        assert_eq!(
            failed.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{path}: {}",
            failed.body
        );
        assert!(failed.body.contains(sentence), "{path}: {}", failed.body);
        assert!(!failed.body.contains("<tr"), "{path}: {}", failed.body);
    }
    Ok(())
}

/// 7.2: whose rows a view shows is the viewer's. Books gets an owner and the
/// ownership formula `owner === user.email`; two members open **one** List view
/// and each sees their own book, and the admin sees both.
#[tokio::test]
async fn two_viewers_see_their_own_rows_through_one_view() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("owners", bundle).await?;
    let catalog = server._catalog.clone();
    let client = &mut server.client;

    let (status, body) = client
        .send(
            "POST",
            "/api/tables/Books/fields",
            Some(json!({ "name": "owner", "type": "text" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = client
        .send(
            "PUT",
            "/api/tables/Books",
            Some(json!({
                "label": "",
                "description": "",
                "min_role_read": 1,
                "min_role_write": 1,
                "ownership_formula": "owner === user.email",
                "rls_enabled": false,
                "sync_enabled": false,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Role 80 is v1's `user`, restored with the backup.
    let (alice, bob) = ("alice@example.com", "bob@example.com");
    create_user(&catalog, alice, "alice-pass", 80).await?;
    create_user(&catalog, bob, "bob-pass", 80).await?;
    let books = rows_of(client, "Books").await;
    assert!(books.len() >= 2, "{books:?}");
    let mut titles = Vec::new();
    for (book, owner) in books.iter().zip([alice, bob]) {
        let (status, body) = client
            .send(
                "PUT",
                &format!("/api/tables/Books/rows/{}", book["id"]),
                Some(json!({ "owner": owner })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        titles.push(book["title"].as_str().unwrap().to_owned());
    }

    // One view for members: List Books' own configuration, cut down to the
    // columns of Books itself (a member may not read Authors to join it).
    let pack: Value = serde_json::from_str(&zip_entry(V1_BACKUP, "pack.json")).unwrap();
    let mut configuration = pack["views"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "List Books")
        .unwrap()["configuration"]
        .clone();
    let own_column =
        |c: &Value| ["title", "pages"].contains(&c["field_name"].as_str().unwrap_or(""));
    configuration["columns"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["type"] == "Field" && own_column(c));
    configuration["layout"]["besides"]
        .as_array_mut()
        .unwrap()
        .retain(|b| b["contents"]["type"] == "field" && own_column(&b["contents"]));
    let app = booksdb(&catalog).await;
    let view = View::new(app.id, "My Books", "List", "Books")
        .min_role(80)
        .configuration(object(configuration));
    sc_viewpattern::save_view(&catalog, &view).await?;
    build_and_mount(&server.apps, app).await?;

    let ajax = [("X-Requested-With", "XMLHttpRequest")];
    for (who, password, own) in [(alice, "alice-pass", 0), (bob, "bob-pass", 1)] {
        let mut member = sign_in(&server.client, who, password).await;
        let list = member.app_get("/view/My%20Books", &ajax).await;
        assert_eq!(list.status, StatusCode::OK, "{who}: {}", list.body);
        assert_eq!(
            list.body.matches("<tr").count(),
            2,
            "{who}: a header and their one book: {}",
            list.body
        );
        assert!(list.body.contains(&titles[own]), "{who}: {}", list.body);
        assert!(
            !list.body.contains(&titles[1 - own]),
            "{who}: {}",
            list.body
        );
    }
    // The admin meets the table's floor, so the formula does not narrow them.
    let everyone = server.client.app_get("/view/My%20Books", &ajax).await;
    assert_eq!(everyone.status, StatusCode::OK, "{}", everyone.body);
    for title in &titles {
        assert!(everyone.body.contains(title), "{title}: {}", everyone.body);
    }
    Ok(())
}

/// A table's rows, read back through the admin API.
async fn rows_of(client: &mut Client, table: &str) -> Vec<Value> {
    let (status, rows) = client
        .send("GET", &format!("/api/tables/{table}/rows"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let rows = rows.get("rows").cloned().unwrap_or(rows);
    rows.as_array().expect("rows").clone()
}

async fn row_count(client: &mut Client, table: &str) -> usize {
    rows_of(client, table).await.len()
}

/// The authors' last names, read back through the admin API.
async fn authors(client: &mut Client) -> Vec<String> {
    let names: Vec<String> = rows_of(client, "Authors")
        .await
        .iter()
        .filter_map(|r| r["last_name"].as_str().map(str::to_owned))
        .collect();
    assert!(!names.is_empty());
    names
}

/// 5.8: each of the six patterns renders the committed HTML.
#[tokio::test]
async fn the_six_patterns_render_their_golden_html() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("golden", bundle).await?;
    let golden =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/saltcorn-ui-golden");
    let update = std::env::var_os("SC_UPDATE_GOLDEN").is_some();
    let cases = [
        ("list", "/view/List%20Books"),
        ("show", "/view/Show%20Books?id=1"),
        ("edit", "/view/Edit%20Books?id=1"),
        ("feed", "/view/Feed%20Books"),
        ("filter", "/view/Filter%20books"),
        ("listshowlist", "/view/Books%20and%20details?id=1"),
    ];
    let mut wrong = Vec::new();
    for (name, path) in cases {
        let answer = server
            .client
            .app_get(path, &[("X-Requested-With", "XMLHttpRequest")])
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{name}: {}", answer.body);
        let rendered = normalise(&answer.body);
        let file = golden.join(format!("{name}.html"));
        if update {
            std::fs::create_dir_all(&golden)?;
            std::fs::write(&file, &rendered)?;
            continue;
        }
        let expected = std::fs::read_to_string(&file).unwrap_or_default();
        if rendered != expected {
            wrong.push(format!(
                "{name} ({path}) differs from {}:\n{rendered}",
                file.display()
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{}\n\n(SC_UPDATE_GOLDEN=1 rewrites the expected files)",
        wrong.join("\n\n")
    );
    Ok(())
}

/// The builder 6.4, 6.6: the builder canvas's calls over the BooksDB import —
/// a field previewed as text, through a join, as a key's select and at a given
/// row; *Show Books* and *BooksOverview* previewed; a fieldview's configuration
/// form; and the distinct values of a key and of a number — each exactly the
/// committed golden (`fixtures/saltcorn-ui-golden/builder-*`).
///
/// Then the subset: with `Publishers` dropped from the application, the join
/// through `publisher` previews nothing and the key has no values to offer.
#[tokio::test]
async fn the_builder_canvas_calls_answer_their_goldens() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("builder-canvas", bundle).await?;
    let catalog = server._catalog.clone();
    let mut app = booksdb(&catalog).await;
    let api = format!("/api/applications/{}/builder", app.id.0);
    let golden =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/saltcorn-ui-golden");
    let update = std::env::var_os("SC_UPDATE_GOLDEN").is_some();
    let field = |field: &str, fieldview: &str| json!({ "table": "Books", "field": field, "fieldview": fieldview });
    let cases: [(&str, &str, &str, Option<Value>); 9] = [
        (
            "field-title",
            "POST",
            "field-preview",
            Some(field("title", "as_text")),
        ),
        (
            "field-join",
            "POST",
            "field-preview",
            Some(field("publisher.name", "as_text")),
        ),
        (
            "field-key-select",
            "POST",
            "field-preview",
            Some(field("author", "select")),
        ),
        (
            "field-at-row",
            "POST",
            "field-preview",
            Some(
                json!({ "table": "Books", "field": "title", "fieldview": "as_text", "row_id": 2 }),
            ),
        ),
        (
            "view-show-books",
            "POST",
            "view-preview",
            Some(json!({ "view": "Show Books", "state": {} })),
        ),
        (
            "page-booksoverview",
            "POST",
            "page-preview",
            Some(json!({ "page": "BooksOverview" })),
        ),
        (
            "fieldview-config-date",
            "POST",
            "fieldview-config",
            Some(json!({
                "table": "Books", "type": "Field", "field_name": "published_on",
                "fieldview": "format", "mode": "show",
            })),
        ),
        ("distinct-author", "GET", "distinct/Books/author", None),
        ("distinct-pages", "GET", "distinct/Books/pages", None),
    ];
    let mut wrong = Vec::new();
    for (name, method, path, body) in cases {
        let (status, answer) = server
            .client
            .send(method, &format!("{api}/{path}"), body)
            .await;
        assert_eq!(status, StatusCode::OK, "{name}: {answer}");
        let (file, rendered) = match answer.get("html").and_then(Value::as_str) {
            Some(html) => (golden.join(format!("builder-{name}.html")), normalise(html)),
            None => (
                golden.join(format!("builder-{name}.json")),
                format!("{}\n", serde_json::to_string_pretty(&answer).unwrap()),
            ),
        };
        if update {
            std::fs::create_dir_all(&golden)?;
            std::fs::write(&file, &rendered)?;
            continue;
        }
        let expected = std::fs::read_to_string(&file).unwrap_or_default();
        if rendered != expected {
            wrong.push(format!(
                "{name} differs from {}:\n{rendered}",
                file.display()
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{}\n\n(SC_UPDATE_GOLDEN=1 rewrites the expected files)",
        wrong.join("\n\n")
    );

    // The table the builder names is the application's, and so is every table
    // it reaches through a key.
    app.tables.retain(|t| t.0 != "Publishers");
    save_application(&catalog, &app).await?;
    sc_viewpattern::view_sets().reload(&catalog, app.id).await?;
    let (status, preview) = server
        .client
        .send(
            "POST",
            &format!("{api}/field-preview"),
            Some(field("publisher.name", "as_text")),
        )
        .await;
    assert_eq!((status, &preview), (StatusCode::OK, &json!({ "html": "" })));
    let (status, values) = server
        .client
        .send("GET", &format!("{api}/distinct/Books/publisher"), None)
        .await;
    assert_eq!(
        (status, &values),
        (StatusCode::OK, &json!({ "success": [] }))
    );
    let (status, body) = server
        .client
        .send("GET", &format!("{api}/distinct/Publishers/name"), None)
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert!(body.to_string().contains("`Publishers`"), "{body}");
    Ok(())
}

/// The builder 5.7: the options the builder is opened with, computed in the
/// worker by v1's own code over the BooksDB import, are what a real Saltcorn 1
/// passes to `renderBuilder` over the same backup (`fixtures/builder-options/`,
/// recorded by `record-builder-options.sh`), key for key, apart from the
/// differences [`builder_option_differences`] lists with their reasons.
///
/// Then: a form step answers no options, and the options see only the
/// application's tables — a table dropped from its subset is not joined to,
/// listed or offered as a parent.
///
/// `SC_DUMP_BUILDER_OPTIONS=<dir>` writes what this server computed, before
/// anything is normalised, for comparing by hand.
#[tokio::test]
async fn the_builder_options_are_what_saltcorn_1_passes() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("builder-options", bundle).await?;
    let catalog = server._catalog.clone();
    let mut app = booksdb(&catalog).await;
    // The import's views only, as v1 had them: the setup's Feed and
    // ListShowList are this suite's own. Deleted as the admin deletes a view,
    // which is what moves the generation the worker's snapshot is keyed on.
    for extra in ["Feed%20Books", "Books%20and%20details"] {
        let (status, body) = server
            .client
            .send(
                "DELETE",
                &format!("/api/applications/{}/views/{extra}", app.id.0),
                None,
            )
            .await;
        assert!(status.is_success(), "{extra}: {status} {body}");
    }
    let recorded = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/builder-options");
    let dump = std::env::var_os("SC_DUMP_BUILDER_OPTIONS").map(PathBuf::from);
    let step_path = format!("/api/applications/{}/view-config-step", app.id.0);

    let mut ours: Vec<(&str, Value)> = Vec::new();
    for (file, name) in [
        ("show-books", "Show Books"),
        ("edit-books", "Edit Books"),
        ("list-books", "List Books"),
        ("filter-books", "Filter books"),
    ] {
        let view = sc_viewpattern::load_view(&catalog, app.id, name)
            .await?
            .unwrap_or_else(|| panic!("the import has {name}"));
        let (status, step) = server
            .client
            .send(
                "POST",
                &step_path,
                Some(json!({
                    "viewpattern": view.viewpattern,
                    "table_name": view.table_name,
                    "name": view.name,
                    "step": 0,
                    "context": Value::Object(view.configuration.clone()),
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{name}: {step}");
        assert_eq!(
            step["builder"], true,
            "{name}'s first step is its layout: {step}"
        );
        let options = &step["builder_options"];
        assert!(options.is_object(), "{name}: {step}");
        // What the differences below take out, checked for what it is here.
        assert_eq!(options["view_id"], json!(view.id.0.to_string()), "{name}");
        assert_eq!(options["triggerActions"], json!(["TrimPages"]), "{name}");
        ours.push((file, options.clone()));
    }
    let page = sc_viewpattern::load_page(&catalog, app.id, "BooksOverview")
        .await?
        .expect("the import has BooksOverview");
    let configurer = sc_viewpattern::Configurer::new(
        sc_viewpattern::view_runtime()?,
        &catalog,
        &app,
        None,
        None,
    )
    .await?;
    let page_options = configurer.page_builder_options(&page).await?;
    drop(configurer);
    assert_eq!(page_options["page_id"], json!(page.id.0.to_string()));
    assert_eq!(page_options["triggerActions"], json!(["TrimPages"]));
    // The image in the application's store, where its serve route finds it.
    assert_eq!(
        page_options["images"],
        json!([{
            "id": "BooksDB/Each+session+is+30+mins.png",
            "filename": "Each+session+is+30+mins.png",
            "location": "BooksDB/Each+session+is+30+mins.png",
        }])
    );
    ours.push(("page-booksoverview", page_options));

    if let Some(dir) = &dump {
        std::fs::create_dir_all(dir)?;
        for (file, options) in &ours {
            std::fs::write(
                dir.join(format!("{file}.json")),
                serde_json::to_string_pretty(options).unwrap(),
            )?;
        }
    }

    let mut wrong = Vec::new();
    for (file, options) in &ours {
        let path = recorded.join(format!("{file}.json"));
        let v1: Value = serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap();
        wrong.extend(builder_option_differences(file, v1, options.clone()));
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));

    // --- A form step has no builder options: List's *Options* step.
    let list = sc_viewpattern::load_view(&catalog, app.id, "List Books")
        .await?
        .expect("List Books");
    let mut index = 1;
    let form_step = loop {
        let (status, step) = server
            .client
            .send(
                "POST",
                &step_path,
                Some(json!({
                    "viewpattern": "List", "table_name": list.table_name, "name": list.name,
                    "step": index, "context": Value::Object(list.configuration.clone()),
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "step {index}: {step}");
        if step["skip"] == false {
            break step;
        }
        index += 1;
    };
    assert_eq!(form_step["builder"], false, "{form_step}");
    assert!(form_step["builder_options"].is_null(), "{form_step}");

    // --- The subset (5.2, 5.3): without Publishers, Books' key to it is no
    // join, no parent and no table the builder offers.
    app.tables.retain(|t| t.0 != "Publishers");
    let app = save_application(&catalog, &app).await?;
    // Saving the row is not deploying it; the mount is what the snapshot the
    // worker holds is rebuilt from (as 7.4's narrowing does).
    build_and_mount(&server.apps, app.clone()).await?;
    let show = sc_viewpattern::load_view(&catalog, app.id, "Show Books")
        .await?
        .expect("Show Books");
    let (status, step) = server
        .client
        .send(
            "POST",
            &step_path,
            Some(json!({
                "viewpattern": "Show", "table_name": show.table_name, "name": show.name,
                "step": 0, "context": Value::Object(show.configuration.clone()),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{step}");
    let options = &step["builder_options"];
    let joins: Vec<&str> = options["join_field_picker_data"]["join_field_options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|j| j["name"].as_str())
        .collect();
    assert_eq!(joins, ["author"], "{options}");
    let parents = options["parent_field_list"].as_array().unwrap();
    assert!(
        parents
            .iter()
            .all(|p| !p.as_str().unwrap().starts_with("publisher.")),
        "{parents:?}"
    );
    let tables: Vec<&str> = options["tables"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        !tables.contains(&"Publishers") && tables.contains(&"Books"),
        "{tables:?}"
    );
    Ok(())
}

/// How this server's builder options for `file` differ from what Saltcorn 1
/// recorded, after the intended differences are taken out of both
/// ([`normalise_builder_options`]): one line per top-level key that still
/// differs.
fn builder_option_differences(file: &str, mut v1: Value, mut ours: Value) -> Vec<String> {
    normalise_builder_options(Side::V1, &mut v1);
    normalise_builder_options(Side::Ours, &mut ours);
    let (Value::Object(v1), Value::Object(ours)) = (v1, ours) else {
        return vec![format!("{file}: the options are not both objects")];
    };
    let mut keys: Vec<&String> = v1.keys().chain(ours.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|key| v1.get(*key) != ours.get(*key))
        .map(|key| {
            let show = |v: Option<&Value>| {
                let text = v.map_or("(absent)".to_owned(), Value::to_string);
                text.chars().take(600).collect::<String>()
            };
            format!(
                "{file}: `{key}` differs\n  v1:   {}\n  ours: {}",
                show(v1.get(key)),
                show(ours.get(key))
            )
        })
        .collect()
}

/// Which recording a value came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    V1,
    Ours,
}

/// Saltcorn 1's serial ids for BooksDB's tables, as the recording's restore
/// numbered them.
const V1_TABLE_IDS: [(u64, &str); 4] = [
    (1, "users"),
    (2, "Books"),
    (3, "Authors"),
    (4, "Publishers"),
];

/// Take the **intended** differences out of one side's builder options. Each is
/// a fact about this server, written with its reason, not a tolerance; anything
/// else that differs fails the test.
///
/// - **Ids.** v1 numbers tables, fields, views and pages; this server names a
///   table and a field, and gives a view and a page a UUID. v1's `table_id`s and
///   field `id`s become the names they stand for, and `view_id`/`page_id` go
///   (the test checks ours are the view's and page's own ids).
/// - **Field order.** v1's field cache is ordered by name; a table here keeps its
///   column order. `fields`, each `agg_field_opts` list, each page view's
///   `fixed_state_fields`, the join picker's sub-fields and `parent_field_list`
///   are compared in name order.
/// - **v1's defaulted field attributes.** v1's field form writes every attribute
///   it has, empty (`regexp: ""`, `min: null`, `include_fts: false`,
///   `on_delete: "Fail"`); the import keeps the attributes that say something.
/// - **A primary key's `is_unique`.** v1 marks its serial id unique; here a
///   primary key's uniqueness is its being the key.
/// - **v1's state actions** (`run_js_code`, `send_email`, … and every plugin's):
///   this server's view actions are the patterns' own and the application's
///   triggers (§12), so v1's groups of them, and their config forms,
///   descriptions and constraints, are not here.
/// - **Triggers.** The application's triggers are this server's: BooksDB's
///   `TrimPages` rewritten as `run_js_code`, and its `AddBook` workflow not
///   imported. v1 also leaves a table's trigger off a page, and nothing here is
///   a table trigger. The trigger group and `triggerActions` go from both sides;
///   the test checks ours name the application's trigger.
/// - **`users`** in `tables`: v1's account table; accounts here are this
///   server's, not a table an application has.
/// - **Role names.** This server's built-in roles are `Admin` and `Public`.
/// - **Image locations.** An image is `<store>/<path>`, which the application's
///   `/files/serve/` reads as that store's file; v1 has one file area. The test
///   checks ours exactly.
/// - **A page's own record** in `pages`: v1's row carries `fixed_states` (folded
///   into the layout by the import) and `updated_at`; ours carries
///   `root_page_for_roles` in its attributes.
fn normalise_builder_options(side: Side, options: &mut Value) {
    normalise_ids(side, options);
    let Value::Object(map) = options else {
        return;
    };
    let by_name = |list: &mut Value| {
        if let Value::Array(items) = list {
            items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        }
    };
    if let Some(fields) = map.get_mut("fields") {
        by_name(fields);
    }
    for key in ["agg_field_opts", "fixed_state_fields"] {
        if let Some(Value::Object(lists)) = map.get_mut(key) {
            lists.values_mut().for_each(by_name);
        }
    }
    if let Some(Value::Array(parents)) = map.get_mut("parent_field_list") {
        parents.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
    if let Some(Value::Array(joins)) = map
        .get_mut("join_field_picker_data")
        .and_then(|j| j.get_mut("join_field_options"))
    {
        for join in joins {
            if let Some(sub) = join.get_mut("subFields") {
                by_name(sub);
            }
        }
    }
    if let Some(Value::Array(tables)) = map.get_mut("tables") {
        tables.retain(|t| t["name"] != "users");
        if side == Side::V1 {
            for table in tables.iter_mut() {
                table["id"] = table["name"].clone();
            }
        }
    }
    if let Some(Value::Array(roles)) = map.get_mut("roles") {
        for role in roles {
            if let Some(name) = role["role"].as_str() {
                role["role"] = json!(name.to_lowercase());
            }
        }
    }
    if side == Side::Ours
        && let Some(Value::Array(images)) = map.get_mut("images")
    {
        for image in images {
            for key in ["id", "location"] {
                if let Some(path) = image[key].as_str().and_then(|p| p.strip_prefix("BooksDB/")) {
                    image[key] = json!(path);
                }
            }
        }
    }
    if let Some(Value::Array(pages)) = map.get_mut("pages") {
        for page in pages.iter_mut().filter_map(Value::as_object_mut) {
            page.remove("id");
            page.remove("fixed_states");
            page.remove("updated_at");
            if let Some(Value::Object(attributes)) = page.get_mut("attributes") {
                attributes.remove("root_page_for_roles");
            }
        }
    }
    normalise_actions(side, map);
}

/// The ids, defaulted attributes and primary keys' `is_unique` of
/// [`normalise_builder_options`], wherever they are.
fn normalise_ids(side: Side, value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("view_id");
            map.remove("page_id");
            if side == Side::V1 {
                let table = map
                    .get("table_id")
                    .and_then(Value::as_u64)
                    .and_then(|id| V1_TABLE_IDS.iter().find(|(n, _)| *n == id));
                if let Some((_, name)) = table {
                    map.insert("table_id".to_owned(), json!(name));
                    if map.get("id").is_some_and(Value::is_number) {
                        let field = map.get("name").cloned().unwrap_or(Value::Null);
                        map.insert("id".to_owned(), field);
                    }
                }
            }
            if map.get("primary_key") == Some(&json!(true)) {
                map.remove("is_unique");
            }
            if let Some(Value::Object(attributes)) = map.get_mut("attributes") {
                attributes.retain(|key, v| {
                    !(v.is_null()
                        || *v == json!("")
                        || *v == json!(false)
                        || (key == "on_delete" && *v == json!("Fail")))
                });
            }
            map.values_mut().for_each(|v| normalise_ids(side, v));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| normalise_ids(side, v)),
        _ => {}
    }
}

/// The state actions and triggers of [`normalise_builder_options`]: the action
/// picker keeps its built-in group and `Other`'s multi-step action.
fn normalise_actions(side: Side, map: &mut serde_json::Map<String, Value>) {
    let mut gone: Vec<String> = map
        .remove("triggerActions")
        .and_then(|t| t.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect();
    let Some(Value::Array(groups)) = map.get_mut("actions") else {
        return;
    };
    let builtin = groups
        .first()
        .and_then(|g| g["label"].as_str())
        .map(str::to_owned);
    let mut kept = Vec::new();
    for mut group in std::mem::take(groups) {
        let label = group["label"].as_str().unwrap_or_default().to_owned();
        let names: Vec<String> = group["options"]
            .as_array()
            .map(|o| {
                o.iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if Some(&label) == builtin.as_ref() {
            kept.push(group);
        } else if label == "Triggers" {
            gone.extend(names);
        } else if label == "Other" {
            if side == Side::V1 {
                gone.extend(names.iter().filter(|n| *n != "Multi-step action").cloned());
            }
            group["options"] = json!(
                names
                    .iter()
                    .filter(|n| *n == "Multi-step action")
                    .collect::<Vec<_>>()
            );
            kept.push(group);
        } else if side == Side::V1 {
            gone.extend(names);
        } else {
            kept.push(group);
        }
    }
    *groups = kept;
    for key in [
        "actionConfigForms",
        "actionDescriptions",
        "actionConstraints",
    ] {
        if let Some(Value::Object(by_action)) = map.get_mut(key) {
            by_action.retain(|name, _| !gone.contains(name));
        }
    }
}

/// The builder 2.5: a `library` segment renders as its item's layout with the
/// slots filled, resolved by v1's own `Library.resolveSegment` over the
/// application's snapshot — **the same HTML as that layout written inline** —
/// in a Show, an Edit, a Filter and a page. A reference to an item that does not
/// exist, and an item's reference to itself, render blank where they were.
///
/// Each view keeps its restored layout and has one row of it moved into an item,
/// with a heading above it: the heading becomes a content slot and the row's
/// field a field slot. The page places an item with a content slot and a slot
/// its placement leaves unfilled (a field slot means nothing on a page, which has
/// no row), then a missing item, then an item containing itself, above the view
/// it already embeds.
#[tokio::test]
async fn a_placed_library_item_renders_as_its_inline_equivalent() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("library", bundle).await?;
    let catalog = server._catalog.clone();
    let app = booksdb(&catalog).await;

    for (name, path) in [
        ("Show Books", "/view/Show%20Books?id=1"),
        ("Edit Books", "/view/Edit%20Books?id=1"),
        ("Filter books", "/view/Filter%20books"),
    ] {
        let view = sc_viewpattern::load_view(&catalog, app.id, name)
            .await?
            .expect("the restored view");
        let layout = view.configuration["layout"].clone();
        let row = layout["above"]
            .as_array()
            .and_then(|above| {
                above
                    .iter()
                    .position(|seg| first_of(&mut seg.clone(), "field").is_some())
            })
            .unwrap_or_else(|| panic!("{name} has a row with a field: {layout}"));
        let part = json!({ "above": [
            { "type": "blank", "contents": "Wrapped row", "textStyle": "h5" },
            layout["above"][row].clone(),
        ]});
        let (item_layout, slots, resolved) = slotted(&part);
        assert_eq!(slots.as_array().map(Vec::len), Some(2), "{name}: {slots}");

        let mut inline = layout.clone();
        inline["above"][row] = resolved;
        let expected = render_with_layout(&mut server, &view, inline, path).await?;
        assert!(expected.contains("Wrapped row"), "{name}: {expected}");

        let item = sc_viewpattern::save_library_item(
            &catalog,
            &LibraryItem::new(app.id, format!("{name} row")).layout(item_layout),
        )
        .await?;
        let mut placed = layout.clone();
        placed["above"][row] = json!({
            "type": "library", "library_id": item.id.0.to_string(), "slots": slots,
        });
        let rendered = render_with_layout(&mut server, &view, placed, path).await?;
        assert_eq!(
            rendered, expected,
            "{name}: the placed item renders as the inline row"
        );
    }

    // --- A page.
    let page = sc_viewpattern::load_page(&catalog, app.id, "BooksOverview")
        .await?
        .expect("the restored page");
    let heading = sc_viewpattern::save_library_item(
        &catalog,
        &LibraryItem::new(app.id, "Overview heading").layout(json!({ "above": [
            { "type": "blank", "contents": "Overview", "textStyle": "h2" },
            { "type": "library-slot", "name": "intro" },
            { "type": "library-slot", "name": "unfilled" },
        ]})),
    )
    .await?;
    let itself = LibraryItem::new(app.id, "Contains itself");
    let itself_id = itself.id.0.to_string();
    let itself = sc_viewpattern::save_library_item(
        &catalog,
        &itself.layout(json!({ "above": [
            { "type": "blank", "contents": "Before itself" },
            { "type": "library", "library_id": itself_id },
        ]})),
    )
    .await?;
    let never_saved = LibraryItem::new(app.id, "Never saved").id.0.to_string();

    let inline = json!({ "above": [
        { "above": [
            { "type": "blank", "contents": "Overview", "textStyle": "h2" },
            { "type": "blank", "contents": "Find a book" },
            { "type": "blank", "contents": "" },
        ]},
        { "type": "blank", "contents": "" },
        { "above": [
            { "type": "blank", "contents": "Before itself" },
            { "type": "blank", "contents": "" },
        ]},
        page.layout.clone(),
    ]});
    let placed = json!({ "above": [
        { "type": "library", "library_id": heading.id.0.to_string(), "slots": [
            { "name": "intro", "kind": "content",
              "contents": { "type": "blank", "contents": "Find a book" } },
        ]},
        { "type": "library", "library_id": never_saved },
        { "type": "library", "library_id": itself.id.0.to_string() },
        page.layout.clone(),
    ]});
    let mut rendered = Vec::new();
    for layout in [inline, placed] {
        sc_viewpattern::save_page(&catalog, &page.clone().layout(layout)).await?;
        build_and_mount(&server.apps, booksdb(&catalog).await).await?;
        let answer = server
            .client
            .app_get(
                "/page/BooksOverview",
                &[("X-Requested-With", "XMLHttpRequest")],
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        rendered.push(normalise(&answer.body));
    }
    for text in ["Overview", "Find a book", "Before itself"] {
        assert!(rendered[0].contains(text), "{text}: {}", rendered[0]);
    }
    assert_eq!(
        rendered[1], rendered[0],
        "the page's placed items render as their inline layouts"
    );
    Ok(())
}

/// The builder 3.1 and 3.2: what a page built in the builder needs to run.
///
/// An action button on a page posts to `/page/:name/action/:rndid`, which runs
/// the segment's trigger — a button inside a placed library item included — and
/// answers v1's JSON: success, the action's error with 400, and 404 for an
/// action or a page that is not there. The post is refused without the CSRF
/// token and below the page's role. And a page's `no_menu` and
/// `request_fluid_layout` reach the document.
#[tokio::test]
async fn a_pages_action_buttons_run_and_its_properties_shape_the_document() -> sc_error::Result<()>
{
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("page-action", bundle).await?;
    let catalog = server._catalog.clone();

    // Two triggers a page's buttons run, neither needing a row: one answers,
    // one throws. `SayHello` also writes an author, so its running is seen.
    let say_hello = "await db.Authors.insert({ first_name: \"Page\", last_name: \"Button\" });\n\
                     return { notify: \"Hello from the page\" };";
    for (name, code) in [
        ("SayHello", say_hello),
        ("Refuses", "throw new Error(\"no books today\");"),
    ] {
        let (status, created) = server
            .client
            .send(
                "POST",
                "/api/triggers",
                Some(json!({
                    "name": name, "description": "", "when": "none", "channel": Value::Null,
                    "only_if": Value::Null, "action": "run_js_code",
                    "configuration": { "code": code }, "min_role": Value::Null, "enabled": true,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
    }
    let mut app = booksdb(&catalog).await;
    app.triggers.push(sc_app::TriggerRef::new("SayHello"));
    app.triggers.push(sc_app::TriggerRef::new("Refuses"));
    let app = save_application(&catalog, &app).await?;

    let button = |rndid: &str, action: &str| {
        json!({
            "type": "action", "action_name": action, "rndid": rndid,
            "action_style": "btn-primary", "action_label": format!("Run {rndid}"),
        })
    };
    let card = sc_viewpattern::save_library_item(
        &catalog,
        &LibraryItem::new(app.id, "A card with a button").layout(json!({ "above": [
            { "type": "blank", "contents": "In a card" },
            button("11b0a2", "SayHello"),
        ]})),
    )
    .await?;
    let actions = Page::new(app.id, "Actions").layout(json!({ "above": [
        button("a1b2c3", "SayHello"),
        button("fa11ed", "Refuses"),
        { "type": "library", "library_id": card.id.0.to_string() },
    ]}));
    sc_viewpattern::save_page(&catalog, &actions).await?;
    let mut quiet =
        Page::new(app.id, "Quiet").layout(json!({ "type": "blank", "contents": "No menu here" }));
    quiet.attributes.insert("no_menu".to_owned(), json!(true));
    sc_viewpattern::save_page(&catalog, &quiet).await?;
    let mut wide =
        Page::new(app.id, "Wide").layout(json!({ "type": "blank", "contents": "Wide open" }));
    wide.attributes
        .insert("request_fluid_layout".to_owned(), json!(true));
    sc_viewpattern::save_page(&catalog, &wide).await?;
    build_and_mount(&server.apps, booksdb(&catalog).await).await?;

    let client = &mut server.client;
    let csrf = client.cookies.get(CSRF_COOKIE).cloned().unwrap();

    // The buttons post where the route is, the one in the item included.
    let rendered = client.app_get("/page/Actions", &[]).await;
    assert_eq!(rendered.status, StatusCode::OK, "{}", rendered.body);
    for rndid in ["a1b2c3", "fa11ed", "11b0a2"] {
        assert!(
            rendered
                .body
                .contains(&format!("/page/Actions/action/{rndid}")),
            "{rndid}: {}",
            rendered.body
        );
    }

    // Success, as `page_post_action` posts it.
    let authors = row_count(client, "Authors").await;
    let ran = post_page_action(client, "/page/Actions/action/a1b2c3", Some(&csrf)).await;
    assert_eq!(ran.status, StatusCode::OK, "{}", ran.body);
    let answer: Value = serde_json::from_str(&ran.body).unwrap();
    assert_eq!(answer["success"], json!("ok"), "{answer}");
    assert_eq!(row_count(client, "Authors").await, authors + 1);

    // A button inside a placed library item.
    let in_item = post_page_action(client, "/page/Actions/action/11b0a2", Some(&csrf)).await;
    assert_eq!(in_item.status, StatusCode::OK, "{}", in_item.body);
    assert_eq!(row_count(client, "Authors").await, authors + 2);

    // The action's own failure, with 400.
    let failed = post_page_action(client, "/page/Actions/action/fa11ed", Some(&csrf)).await;
    assert_eq!(failed.status, StatusCode::BAD_REQUEST, "{}", failed.body);
    let answer: Value = serde_json::from_str(&failed.body).unwrap();
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains("no books today")),
        "{answer}"
    );

    // No such action, and no such page.
    for path in ["/page/Actions/action/000000", "/page/Nowhere/action/a1b2c3"] {
        let missing = post_page_action(client, path, Some(&csrf)).await;
        assert_eq!(
            missing.status,
            StatusCode::NOT_FOUND,
            "{path}: {}",
            missing.body
        );
        assert_eq!(
            serde_json::from_str::<Value>(&missing.body).unwrap(),
            json!({ "error": "Action not found" })
        );
    }

    // No CSRF token: refused before anything runs.
    let forged = post_page_action(client, "/page/Actions/action/a1b2c3", None).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN, "{}", forged.body);
    assert_eq!(row_count(client, "Authors").await, authors + 2);

    // The page's role: an admin-only page refuses a stranger and a member.
    sc_viewpattern::save_page(&catalog, &actions.clone().min_role(1)).await?;
    build_and_mount(&server.apps, booksdb(&catalog).await).await?;
    let mut stranger = visitor(&server.client);
    stranger.app_get("/auth/login", &[]).await;
    let token = stranger.cookies.get(CSRF_COOKIE).cloned().unwrap();
    let refused =
        post_page_action(&mut stranger, "/page/Actions/action/a1b2c3", Some(&token)).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{}", refused.body);
    create_user(&catalog, "member@example.com", PASSWORD, 80).await?;
    let mut member = sign_in(&server.client, "member@example.com", PASSWORD).await;
    let token = member.cookies.get(CSRF_COOKIE).cloned().unwrap();
    let refused = post_page_action(&mut member, "/page/Actions/action/a1b2c3", Some(&token)).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    let client = &mut server.client;
    assert_eq!(row_count(client, "Authors").await, authors + 2);

    // 3.2: the navbar, left out by `no_menu`, and fluid by `request_fluid_layout`.
    let normal = client.app_get("/page/Actions", &[]).await;
    assert!(normal.body.contains("id=\"mainNav\""), "{}", normal.body);
    assert!(!normal.body.contains("container-fluid"), "{}", normal.body);
    let quiet = client.app_get("/page/Quiet", &[]).await;
    assert_eq!(quiet.status, StatusCode::OK, "{}", quiet.body);
    assert!(quiet.body.contains("No menu here"), "{}", quiet.body);
    assert!(!quiet.body.contains("mainNav"), "{}", quiet.body);
    let wide = client.app_get("/page/Wide", &[]).await;
    assert!(wide.body.contains("Wide open"), "{}", wide.body);
    assert!(wide.body.contains("id=\"mainNav\""), "{}", wide.body);
    assert!(
        wide.body.contains("<div class=\"container-fluid\">"),
        "{}",
        wide.body
    );
    Ok(())
}

/// The builder 4.5: a v1 library and a legacy page, imported, render on the
/// subdomain.
///
/// *Show Books* places *Book header* (serial 1 in the v1 pack), which places
/// *Book note* (serial 2): both only render if the import's UUIDs reached the
/// placements, the nested one included. *Featured book* embeds *Show Books*
/// with v1's legacy `fixed_states` entry `{ id: 2 }`; the page shows *War and
/// Peace* only because the import folded that into the segment, since nothing
/// here reads `fixed_states`.
#[tokio::test]
async fn an_imported_library_and_legacy_page_render_on_the_subdomain() -> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup_from("v1-library", bundle, V1_LIBRARY_BACKUP, true).await?;
    let report = server.restore_report.to_string();
    assert!(
        report.contains("2 library items into application `booksdb`"),
        "{report}"
    );
    let client = &mut server.client;

    let show = client
        .app_get(
            "/view/Show%20Books?id=1",
            &[("X-Requested-With", "XMLHttpRequest")],
        )
        .await;
    assert_eq!(show.status, StatusCode::OK, "{}", show.body);
    let html = &show.body;
    let at = |text: &str| {
        html.find(text)
            .unwrap_or_else(|| panic!("`{text}` is rendered: {html}"))
    };
    // The header, its title slot filled with the row's title, and the nested
    // note with its content slot — all above the view's own first row.
    assert!(html.contains("<h3>Book</h3>"), "{html}");
    assert!(at("<h3>Book</h3>") < at("Moby Dick"), "{html}");
    assert!(at("Moby Dick") < at("Note:"), "{html}");
    assert!(at("Note:") < at("From the BooksDB library"), "{html}");
    assert!(at("From the BooksDB library") < at("Melville"), "{html}");

    let page = client.app_get("/page/Featured%20book", &[]).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let html = &page.body;
    assert!(html.contains("Featured this week"), "{html}");
    assert!(html.contains("War and Peace"), "{html}");
    assert!(html.contains("Tolstoy"), "{html}");
    assert!(!html.contains("Moby Dick"), "{html}");
    Ok(())
}

/// A post to a page's action route as `saltcorn.js`'s `page_post_action` makes
/// it: JSON, v1's `CSRF-Token` header, as ajax.
async fn post_page_action(client: &mut Client, path: &str, csrf: Option<&str>) -> Answer {
    let mut headers = vec![("X-Requested-With", "XMLHttpRequest")];
    if let Some(csrf) = csrf {
        headers.push(("CSRF-Token", csrf));
    }
    client
        .app_post(path, "application/json", b"{}".to_vec(), &headers)
        .await
}

/// Save `view` with `layout` as its layout, deploy, and render `path` as the
/// golden test does.
async fn render_with_layout(
    server: &mut Server,
    view: &View,
    layout: Value,
    path: &str,
) -> sc_error::Result<String> {
    let mut configuration = view.configuration.clone();
    configuration.insert("layout".to_owned(), layout);
    sc_viewpattern::save_view(&server._catalog, &view.clone().configuration(configuration)).await?;
    build_and_mount(&server.apps, booksdb(&server._catalog).await).await?;
    let answer = server
        .client
        .app_get(path, &[("X-Requested-With", "XMLHttpRequest")])
        .await;
    assert_eq!(answer.status, StatusCode::OK, "{path}: {}", answer.body);
    Ok(normalise(&answer.body))
}

/// The first segment of type `kind` in `value`, depth first.
fn first_of<'a>(value: &'a mut Value, kind: &str) -> Option<&'a mut Value> {
    if value.get("type").and_then(Value::as_str) == Some(kind) {
        return Some(value);
    }
    match value {
        Value::Array(items) => items.iter_mut().find_map(|v| first_of(v, kind)),
        Value::Object(map) => map.values_mut().find_map(|v| first_of(v, kind)),
        _ => None,
    }
}

/// `part` made into a library item: its first `field` segment becomes the slot
/// `value` and its first `blank` the slot `label`. Answers the item's layout,
/// the slots a placement fills them with, and `part` as it is once resolved —
/// v1's `resolveSegment` gives a field slot exactly
/// `{ type, field_name, fieldview, configuration: {} }` and a content slot its
/// contents unchanged.
fn slotted(part: &Value) -> (Value, Value, Value) {
    let mut item = part.clone();
    let mut resolved = part.clone();
    let mut slots = Vec::new();
    if let Some(field) = first_of(&mut item, "field") {
        let (field_name, fieldview) = (field["field_name"].clone(), field["fieldview"].clone());
        *field = json!({ "type": "library-slot", "name": "value" });
        *first_of(&mut resolved, "field").expect("the same field") = json!({
            "type": "field", "field_name": field_name, "fieldview": fieldview, "configuration": {},
        });
        slots.push(json!({
            "name": "value", "kind": "field", "field": field_name, "fieldview": fieldview,
        }));
    }
    if let Some(blank) = first_of(&mut item, "blank") {
        let contents = blank.clone();
        *blank = json!({ "type": "library-slot", "name": "label" });
        slots.push(json!({ "name": "label", "kind": "content", "contents": contents }));
    }
    (item, Value::Array(slots), resolved)
}

/// What legitimately differs between two renders of one view, replaced by a
/// placeholder so the rest is compared exactly:
///
/// - a UUID — the signed-in admin's id, new in every test database;
/// - v1's random form ids (`form3fa9c1`) — an action column's `rndid` is
///   *stored* in the view's configuration and stays;
/// - a form's `_csrf` value, which is the test client's own CSRF token;
/// - the text of a `<time>`, which v1 renders in the server's locale and
///   timezone and `saltcorn-common.js` re-renders in the browser's — its
///   `datetime` attribute is kept, and is the value.
fn normalise(html: &str) -> String {
    let uuid =
        regex_lite::Regex::new("[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
            .unwrap();
    // A form's id, wherever it is used: its `id`, and the script that finds it.
    // v1 draws it as `Math.floor(Math.random() * 16777215).toString(16)`, so it
    // is one to six hex digits, not always six.
    let form_id = regex_lite::Regex::new(r#"\bform[0-9a-f]{1,6}\b"#).unwrap();
    let time = regex_lite::Regex::new(r"(<time [^>]*>)[^<]*(</time>)").unwrap();
    let csrf = regex_lite::Regex::new(r#"(name="_csrf" value=")[^"]*(")"#).unwrap();
    let out = uuid.replace_all(html, "UUID");
    let out = csrf.replace_all(&out, "${1}CSRF${2}");
    let out = form_id.replace_all(&out, "formRNDID");
    let out = time.replace_all(&out, "${1}TIME${2}");
    let mut out = out.trim_end().to_owned();
    out.push('\n');
    out
}

/// The milestone's Saltcorn UI half (§16.1, task 4.6): the **same screen, the
/// same button, rows instead of files**.
///
/// A Saltcorn UI application's definition is rows, so its strings are values in
/// its views' configurations — a column's header, a link's text — and its
/// catalogue is an `_fd_translations` row rather than a file in a repository.
/// Everything above that is the code application's: the same
/// `getTranslations`, the same coverage, the same save.
///
/// What this asserts is the end of it: a header translated on that screen comes
/// back translated **in the rendered HTML**, on a request that asked for
/// French, with nothing rebuilt and nothing restarted.
#[tokio::test]
async fn a_saltcorn_ui_applications_strings_are_translated_from_the_same_screen()
-> sc_error::Result<()> {
    let Some(bundle) = bundle_dir() else {
        eprintln!(
            "skipping: the Saltcorn UI bundle is not built (npm ci && npm run build in ui/saltcorn-ui)"
        );
        return Ok(());
    };
    let mut server = setup("i18n", bundle).await?;

    let app_id = {
        let (status, apps) = server.client.send("GET", "/api/applications", None).await;
        assert_eq!(status, StatusCode::OK, "{apps}");
        apps.as_array()
            .unwrap()
            .iter()
            .find(|a| a["subdomain"] == "booksdb")
            .expect("BooksDB")["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    // --- What does this application say? The patterns' own `getStringsForI18n`,
    // over every view — the rows-side equivalent of the tree-sitter pass a code
    // application gets.
    let (status, screen) = server
        .client
        .send(
            "GET",
            &format!("/api/applications/{app_id}/translations"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{screen}");
    let keys: Vec<String> = screen["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["key"].as_str().unwrap().to_owned())
        .collect();
    assert!(!keys.is_empty(), "{screen}");
    // A label an admin typed when they configured *List Books*: the text of
    // its link to *Show Authors*. The pattern's own `getStringsForI18n` found
    // it, which is the rows-side equivalent of a `t()` call site.
    let header = keys
        .iter()
        .find(|k| k.as_str() == "Author")
        .unwrap_or_else(|| panic!("the `Author` link label among {keys:?}"));
    // The "file" a message is attributed to is the view it was configured on,
    // which is where an admin would go to change it.
    let sites = screen["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["key"] == header.as_str())
        .unwrap()["sites"]
        .as_array()
        .unwrap()
        .clone();
    assert!(sites.iter().any(|s| s["file"] == "List Books"), "{sites:?}");
    // Nothing to lint: there is no source to parse, because an admin typed
    // these into a form.
    assert_eq!(screen["unwrapped"], serde_json::json!([]));
    assert_eq!(screen["problems"], serde_json::json!([]));
    // And the catalogue's home says which half of the split this application is
    // on.
    assert!(
        screen["store"]
            .as_str()
            .unwrap()
            .contains("_fd_translations"),
        "{screen}"
    );

    // --- Turn French on, and translate the header. Same screen, same calls.
    let (status, body) = server
        .client
        .send(
            "PUT",
            &format!("/api/applications/{app_id}/locales"),
            // English *and* French, English the fallback: which is what an
            // admin adding a second language does, and what makes the two
            // requests below differ by the language they ask for rather than by
            // when they were made.
            Some(json!({ "locales": ["en", "fr"], "default_locale": "en" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = server
        .client
        .send(
            "PUT",
            &format!("/api/applications/{app_id}/translations/fr"),
            Some(json!({ "messages": { header.as_str(): "Auteur" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["messages"], 1);

    // The coverage figure moved, and it is the same figure a code application's
    // screen shows.
    let (_, screen) = server
        .client
        .send(
            "GET",
            &format!("/api/applications/{app_id}/translations"),
            None,
        )
        .await;
    let french_row = screen["locales"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["locale"] == "fr")
        .expect("the fr row");
    assert_eq!(french_row["translated"], 1);

    // --- The rendered HTML. English first, so the change below is the only
    // thing that could have produced the difference.
    let english = server.client.app_get("/view/List%20Books", &[]).await;
    assert_eq!(english.status, StatusCode::OK, "{}", english.body);
    assert!(english.body.contains(">Author</a>"), "{}", english.body);
    assert!(!english.body.contains("Auteur"));

    // …and French, negotiated on the request. No rebuild, no restart: the row
    // was saved a moment ago and the framework re-read it.
    let french = server
        .client
        .app_get("/view/List%20Books?lang=fr", &[])
        .await;
    assert_eq!(french.status, StatusCode::OK, "{}", french.body);
    assert!(french.body.contains("Auteur"), "{}", french.body);
    assert_eq!(french.headers[header::CONTENT_LANGUAGE], "fr");

    // `Accept-Language` gets there too, which is the header a browser actually
    // sends.
    let negotiated = server
        .client
        .app_get(
            "/view/List%20Books",
            &[("accept-language", "fr-CA,fr;q=0.9")],
        )
        .await;
    assert!(negotiated.body.contains("Auteur"), "{}", negotiated.body);

    Ok(())
}
