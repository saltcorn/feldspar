//! The builder's routes (TODO "The builder" §2, §4, §12, Phase 8): v1's
//! drag-and-drop layout editor, served by the admin server as a document of its
//! own, the way the file-store IDE is.
//!
//! - `GET /builder/applications/:app/views/:view?step=n`: the layout of one of
//!   a view's builder steps, counting from 0 (default 0);
//! - `GET /builder/applications/:app/pages/:page`: a page's layout;
//! - `GET /builder/static/:tag/*`: the builder bundle (`ui/builder/dist`),
//!   meaning its module and chunks, its CSS, Monaco's workers and CKEditor;
//! - `GET /builder/saltcorn-ui/:tag/*`: Saltcorn UI's `public/`, the stylesheets
//!   and v1 page scripts the canvas renders with, as the subdomain serves them;
//! - `GET /files/serve/*` on the admin origin, redirected to the application's
//!   (8.5).
//!
//! The router applies the admin check before any of these. Every answer here,
//! refusals included, carries the builder's Content-Security-Policy (8.3).
//!
//! **Why the assets are on the admin origin.** The canvas must render with the
//! subdomain's stylesheets, but the document is the admin's: its session, its
//! typed client and its CSRF cookie are this origin's. Loading the stylesheets
//! and scripts from the application's origin would put another origin in
//! `script-src`, so the same files are served here, under their own prefix.
//!
//! **The document is v1's `saltcorn-markup/builder.ts` output**, rendered here
//! around the chrome v1's `viewedit` and `pageedit` routes supplied: the
//! application, the view or page, the step, and the way back. Two things differ:
//! - v1's inline `builder.renderBuilder(...)` call is JSON boot data, which
//!   `ui/builder/src/boot.ts` reads;
//! - `#scbuildform` has no `action`, because `save-form.ts` saves it through the
//!   typed client (8.4).
//!
//! **What Rust reads of the options** is `mode`, and nothing else (§5 keeps
//! Rust out of the options). §12's allow-list is about which toolbox the builder
//! draws, and `mode` is the key that decides it.

use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::UNIX_EPOCH;

use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use sc_app::{AppId, Application, load_application};
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, Repr};
use sc_viewpattern::{
    ASSET_VERSION_TAG, Configurer, SALTCORN_UI_FRAMEWORK, view_runtime, view_sets,
};
use serde_json::{Value as Json, json};

use crate::apps::AppMounts;
use crate::handler::{HandlerRegistry, HandlerResponse};
use crate::security::{BUILDER_CONTENT_SECURITY_POLICY, builder_content_security_policy};

/// The path prefix the builder is served under.
pub const BUILDER_PREFIX: &str = "/builder";

/// The view modes the builder routes build (§12). A plugin pattern whose
/// workflow has a builder step answers some other mode, and is refused naming it.
pub const BUILDER_VIEW_MODES: [&str; 4] = ["show", "edit", "list", "filter"];

/// The one page mode.
pub const BUILDER_PAGE_MODE: &str = "page";

/// The id of the document's boot data (`ui/builder/src/boot.ts`).
pub const BUILDER_BOOT_ID: &str = "builder-boot";

/// The bundle's entry module, which is also how a built bundle is recognised.
const BUILDER_ENTRY: &str = "builder.js";

/// The bundle directory, if it holds a built bundle.
fn built_bundle(dir: Option<&Path>) -> Option<&Path> {
    dir.filter(|dir| dir.join(BUILDER_ENTRY).is_file())
}

/// Register `builderStatus`: whether the builder routes have a bundle to serve.
///
/// Registered by the router rather than in `admin_handlers`, because the bundle
/// directory is the server's configuration, which the admin handlers never see.
/// It is checked per request, the way the routes check it, so the answer and
/// what **Open in builder** then opens cannot disagree.
pub(crate) fn register_status_handler(
    handlers: &mut HandlerRegistry,
    bundle: Option<std::path::PathBuf>,
) {
    let bundle = std::sync::Arc::new(bundle);
    handlers.register("builderStatus", move |_ctx| {
        let bundle = bundle.clone();
        async move {
            let available = built_bundle(bundle.as_deref()).is_some();
            Ok(HandlerResponse::ok(json!({ "available": available })))
        }
    });
}

/// Where the builder's canvas finds an application's files: v1's serve URL.
const FILES_SERVE_PREFIX: &str = "/files/serve/";

/// The cache lifetime of a file under a versioned prefix.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// What the builder routes need from the server.
pub(crate) struct BuilderEnv<'a> {
    /// The built `ui/builder` bundle, if this server has one.
    pub bundle: Option<&'a Path>,
    /// The mounted applications, and the catalog and triggers they run over.
    pub apps: &'a AppMounts,
    /// The domain applications are served under.
    pub base_domain: Option<&'a str>,
    /// Whether this server is reached over TLS, as far as it knows.
    pub secure: bool,
}

/// Whether `path` is the builder's.
pub(crate) fn is_builder_path(path: &str) -> bool {
    path.strip_prefix(BUILDER_PREFIX)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether `path` is v1's file serve URL, which the canvas asks the admin origin
/// for.
pub(crate) fn is_files_serve_path(path: &str) -> bool {
    path.len() > FILES_SERVE_PREFIX.len() && path.starts_with(FILES_SERVE_PREFIX)
}

/// Answer a request under [`BUILDER_PREFIX`], made by the admin `user`, whose
/// CSRF token is `csrf`.
pub(crate) async fn serve_builder(
    env: &BuilderEnv<'_>,
    uri: &Uri,
    headers: &HeaderMap,
    user: &User,
    csrf: &str,
) -> Response {
    let rest = uri.path().get(BUILDER_PREFIX.len()..).unwrap_or("");
    let segments: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
    let mut response = match segments.as_slice() {
        ["static", _tag, file @ ..] if !file.is_empty() && !file.contains(&"") => {
            asset(env.bundle, file, "the builder bundle is not built").await
        }
        ["saltcorn-ui", _tag, file @ ..] if !file.is_empty() && !file.contains(&"") => {
            let public = env.apps.saltcorn_ui_dir().map(|dir| dir.join("public"));
            asset(
                public.as_deref(),
                file,
                "the Saltcorn UI bundle is not built",
            )
            .await
        }
        ["applications", app, kind @ ("views" | "pages"), name]
            if !app.is_empty() && !name.is_empty() =>
        {
            let target = if *kind == "views" {
                match step_of(uri) {
                    Ok(step) => Target::View {
                        name: percent_decode(name),
                        step,
                    },
                    Err(sentence) => return refusal(StatusCode::NOT_FOUND, &sentence, None),
                }
            } else {
                Target::Page {
                    name: percent_decode(name),
                }
            };
            match document(env, headers, user, csrf, &percent_decode(app), target).await {
                Ok(response) | Err(response) => response,
            }
        }
        _ => refusal(
            StatusCode::NOT_FOUND,
            "There is no builder at this address.",
            None,
        ),
    };
    if !response
        .headers()
        .contains_key(header::CONTENT_SECURITY_POLICY)
    {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(BUILDER_CONTENT_SECURITY_POLICY),
        );
    }
    response
}

/// `GET /files/serve/*` on the admin origin: the file on the application's
/// origin, for an image `src` or CSS `url()` the builder's canvas renders (8.5).
///
/// Neither passes through the link listener (`ui/builder/src/links.ts`), and
/// the path alone does not say which application's file it is. The referring
/// document does: the builder document is served with
/// `Referrer-Policy: same-origin`, so the requests it makes for its own images
/// carry its URL, and only the builder's own documents are believed.
pub(crate) async fn redirect_file(
    env: &BuilderEnv<'_>,
    uri: &Uri,
    headers: &HeaderMap,
) -> Response {
    let not_here = |sentence: &str| {
        let mut response = (StatusCode::NOT_FOUND, sentence.to_owned()).into_response();
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(BUILDER_CONTENT_SECURITY_POLICY),
        );
        response
    };
    let Some(raw_app) = referring_application(headers) else {
        return not_here(
            "/files/serve/ is an application's address; the admin server answers it only for \
             the builder's page",
        );
    };
    let Some(catalog) = env.apps.catalog() else {
        return not_here("this server serves no applications");
    };
    let app = match require_saltcorn_ui_app(catalog, &raw_app).await {
        Ok(app) => app,
        Err(sentence) => return not_here(&sentence),
    };
    let Some(origin) = application_origin(env, headers, &app) else {
        return not_here("this server has no base domain, so the application has no address");
    };
    let path = uri.path_and_query().map_or(uri.path(), |p| p.as_str());
    Redirect::temporary(&format!("{origin}{path}")).into_response()
}

/// What a builder document builds.
enum Target {
    View { name: String, step: usize },
    Page { name: String },
}

/// The `step` query parameter: a step number counting from 0, else 0.
fn step_of(uri: &Uri) -> std::result::Result<usize, String> {
    let step = uri
        .query()
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key == "step").then(|| percent_decode(value))
        });
    match step {
        None => Ok(0),
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| format!("`{value}` is not a step number; steps count from 0.")),
    }
}

/// One file of a bundle directory, or the 404 naming what is missing.
async fn asset(dir: Option<&Path>, file: &[&str], missing: &str) -> Response {
    let Some(dir) = dir else {
        return (StatusCode::NOT_FOUND, missing.to_owned()).into_response();
    };
    match crate::router::serve_file(dir, &format!("/{}", file.join("/"))).await {
        Some(mut response) => {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
            response
        }
        None => (StatusCode::NOT_FOUND, "no such file").into_response(),
    }
}

/// The builder document for `target` in the application whose id is `raw_app`,
/// or the refusal naming the first condition that fails, in the order a reader
/// would check them: the bundle, the application, its framework, the view or
/// page, the step, the options, the mode.
async fn document(
    env: &BuilderEnv<'_>,
    headers: &HeaderMap,
    user: &User,
    csrf: &str,
    raw_app: &str,
    target: Target,
) -> std::result::Result<Response, Response> {
    let not_found = |sentence: String| refusal(StatusCode::NOT_FOUND, &sentence, None);
    let Some(bundle) = built_bundle(env.bundle) else {
        return Err(not_found(
            "This server was built without the builder (ui/builder), so layouts can be edited \
             only as JSON in the admin UI. Build it with `npm ci && npm run build` in \
             ui/builder, and rebuild sc-cli."
                .to_owned(),
        ));
    };
    let Some(catalog) = env.apps.catalog() else {
        return Err(not_found("This server serves no applications.".to_owned()));
    };
    let app = require_saltcorn_ui_app(catalog, raw_app)
        .await
        .map_err(&not_found)?;
    let set = view_sets()
        .get(catalog, app.id)
        .await
        .map_err(|e| failure(&e))?;
    let runtime = view_runtime().map_err(|e| failure(&e))?;
    let triggers = env.apps.triggers().map(|d| d.as_ref());
    let configurer = Configurer::new(runtime, catalog, &app, Some(user), triggers)
        .await
        .map_err(|e| failure(&e))?;
    let origin = application_origin(env, headers, &app);
    let enc = encode_component;
    let admin_app = format!("/#/applications/{}", enc(&app.id.0.to_string()));

    let page = match target {
        Target::View { name, step } => {
            let Some(view) = set.view(&name) else {
                return Err(not_found(format!(
                    "The application {} has no view named {name}.",
                    app.name
                )));
            };
            let context = Json::Object(view.configuration.clone());
            let config_step = configurer
                .step(
                    &view.viewpattern,
                    view.table_name.as_deref(),
                    Some(&view.name),
                    step,
                    &context,
                )
                .await
                .map_err(|e| {
                    not_found(format!(
                        "The view {name} has no step {} to build: {}.",
                        step + 1,
                        sc_error::format_chain(&e)
                    ))
                })?;
            let step_label = format!(
                "Step {} of {} ({})",
                step + 1,
                config_step.count,
                config_step.name
            );
            if !config_step.builder {
                return Err(not_found(format!(
                    "{step_label} of the view {name} is a form, not a layout. Configure it in \
                     the admin UI."
                )));
            }
            if config_step.skip {
                return Err(not_found(format!(
                    "{step_label} of the view {name} is skipped for its configuration, so there \
                     is no layout to build."
                )));
            }
            let options = config_step.builder_options.ok_or_else(|| {
                refusal(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!(
                        "The view runtime answered no builder options for {step_label} of {name}."
                    ),
                    None,
                )
            })?;
            let mode = builder_mode(&options, &BUILDER_VIEW_MODES).map_err(&not_found)?;
            // Where the step's values land (v1's `contextField`), and so where
            // its layout is.
            let scope = match &config_step.context_field {
                Some(field) => context.get(field).cloned().unwrap_or(Json::Null),
                None => context,
            };
            let view_url = format!("{admin_app}/views/{}", enc(&name));
            let after_save = if step + 1 < config_step.count {
                format!("{view_url}?step={}", step + 1)
            } else {
                format!("{admin_app}/views")
            };
            Page {
                heading: format!("View {name}"),
                detail: Some(step_label),
                links: vec![("Back to configuration", format!("{view_url}?step={step}"))],
                step_name: config_step.name.clone(),
                boot: json!({
                    "target": { "kind": "view", "name": name, "step": step },
                    "stepCount": config_step.count,
                    "stepName": config_step.name,
                    "options": options,
                    "layout": scope.get("layout").cloned().unwrap_or(Json::Null),
                    "mode": mode,
                    "afterSave": after_save,
                }),
            }
        }
        Target::Page { name } => {
            let Some(page) = set.page(&name) else {
                return Err(not_found(format!(
                    "The application {} has no page named {name}.",
                    app.name
                )));
            };
            if let Some(file) = page.layout.get("html_file").and_then(Json::as_str) {
                return Err(not_found(format!(
                    "The page {name} is an HTML file ({file}), which has no layout to build."
                )));
            }
            let options = configurer
                .page_builder_options(page)
                .await
                .map_err(|e| failure(&e))?;
            let mode = builder_mode(&options, &[BUILDER_PAGE_MODE]).map_err(&not_found)?;
            let pages_url = format!("{admin_app}/pages");
            Page {
                heading: format!("Page {name}"),
                detail: None,
                links: vec![
                    (
                        "Page properties",
                        format!("{pages_url}/{}/properties", enc(&name)),
                    ),
                    ("Back to pages", pages_url.clone()),
                ],
                step_name: String::new(),
                boot: json!({
                    "target": { "kind": "page", "name": name },
                    "options": options,
                    "layout": page.layout,
                    "mode": mode,
                    "afterSave": pages_url,
                }),
            }
        }
    };
    drop(configurer);

    let mut boot = page.boot.clone();
    if let Some(obj) = boot.as_object_mut() {
        obj.insert("application".into(), json!(app.id.0.to_string()));
        obj.insert("applicationName".into(), json!(app.name));
        obj.insert(
            "applicationOrigin".into(),
            json!(origin.clone().unwrap_or_default()),
        );
        obj.insert("csrfToken".into(), json!(csrf));
        obj.insert("lightmode".into(), json!("light"));
        // The locale for the `builder` domain (§16.1, task 3.5). The builder is
        // admin-only and its document is not the SPA's, so the signal is the
        // signed-in admin's `language` column — the same column the SPA's own
        // picker writes, so choosing French there opens a French builder.
        obj.insert(
            "locale".into(),
            json!(crate::i18n::locale_for_user(Some(user)).as_str()),
        );
    }
    let html = render_document(&app, &page, &boot, csrf, &bundle_tag(bundle));
    let mut response = Html(html).into_response();
    let csp = builder_content_security_policy(origin.as_deref());
    if let Ok(value) = HeaderValue::from_str(&csp) {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// What differs between a view's builder document and a page's.
struct Page {
    heading: String,
    detail: Option<String>,
    links: Vec<(&'static str, String)>,
    step_name: String,
    boot: Json,
}

/// The Saltcorn UI application `raw_id` names, or the sentence saying why not.
async fn require_saltcorn_ui_app(
    catalog: &Catalog,
    raw_id: &str,
) -> std::result::Result<Application, String> {
    let id = uuid::Uuid::parse_str(raw_id)
        .map(AppId)
        .map_err(|_| format!("`{raw_id}` is not an application id."))?;
    let app = load_application(catalog, id)
        .await
        .map_err(|e| sc_error::format_chain(&e))?
        .ok_or_else(|| format!("There is no application with id {id}."))?;
    if app.framework.name != SALTCORN_UI_FRAMEWORK {
        return Err(format!(
            "The application {} uses the {} framework; only a Saltcorn UI application has views \
             and pages to build.",
            app.name, app.framework.name
        ));
    }
    Ok(app)
}

/// The builder mode `options` names, if it is one of `allowed` (§12).
fn builder_mode(options: &Json, allowed: &[&str]) -> std::result::Result<String, String> {
    let all = allowed.join(", ");
    match options.get("mode").and_then(Json::as_str) {
        Some(mode) if allowed.contains(&mode) => Ok(mode.to_owned()),
        Some(mode) => Err(format!(
            "The builder does not build a {mode} layout here; it builds {all}."
        )),
        None => Err(format!(
            "The builder options name no mode; the builder builds {all}."
        )),
    }
}

/// Where the application is served: the catalog's public origin, else this
/// request's host's port under the base domain. `None` without a base domain.
fn application_origin(
    env: &BuilderEnv<'_>,
    headers: &HeaderMap,
    app: &Application,
) -> Option<String> {
    if let Some(origin) = env.apps.catalog().and_then(|c| c.public_origin()) {
        return Some(origin.url_for(&app.subdomain));
    }
    let base = env.base_domain?;
    let scheme = if env.secure { "https" } else { "http" };
    let port = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|host| host.rsplit_once(':'))
        .map(|(_, port)| port)
        .filter(|port| port.parse::<u16>().is_ok());
    Some(match port {
        Some(port) => format!("{scheme}://{}.{base}:{port}", app.subdomain),
        None => format!("{scheme}://{}.{base}", app.subdomain),
    })
}

/// The application id in the referring builder document's URL, if the request
/// came from one on this host.
fn referring_application(headers: &HeaderMap) -> Option<String> {
    let referer = headers.get(header::REFERER)?.to_str().ok()?;
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let (_, rest) = referer.split_once("://")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !authority.eq_ignore_ascii_case(host) {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["builder", "applications", app, "views" | "pages", _] if !app.is_empty() => {
            Some(percent_decode(app))
        }
        _ => None,
    }
}

/// The tag in the bundle's asset URLs: the server's version and the build of the
/// bundle, so a rebuilt bundle is a new URL even when the version did not move.
fn bundle_tag(bundle: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(meta) = std::fs::metadata(bundle.join(BUILDER_ENTRY)) {
        meta.len().hash(&mut hasher);
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .hash(&mut hasher);
    }
    format!("{ASSET_VERSION_TAG}-{:x}", hasher.finish())
}

/// The document: v1's builder page (`saltcorn-markup/builder.ts`) in Saltcorn
/// UI's head (`sc-viewpattern`'s `framework.rs`), with the chrome around it.
///
/// Order, as v1's:
/// - the subdomain's stylesheets, then the builder's (`builder.css` holds
///   `saltcorn-builder.css`, `fonticonpicker.react.css` and Monaco's);
/// - jQuery, Bootstrap, `saltcorn-common.js` and `saltcorn.js`, as classic
///   scripts;
/// - CKEditor, as v1's builder page loads it;
/// - the boot data;
/// - the bundle, a module script, which runs after all of them.
fn render_document(app: &Application, page: &Page, boot: &Json, csrf: &str, tag: &str) -> String {
    let ui = |file: &str| format!("{BUILDER_PREFIX}/saltcorn-ui/{ASSET_VERSION_TAG}/{file}");
    let bundle = |file: &str| format!("{BUILDER_PREFIX}/static/{tag}/{file}");
    let links: String = page
        .links
        .iter()
        .map(|(label, href)| {
            format!(
                "<a class=\"btn btn-sm btn-outline-secondary\" href=\"{}\">{}</a>\n",
                escape(href),
                escape(label)
            )
        })
        .collect();
    let detail = page
        .detail
        .as_deref()
        .map(|d| format!("<span class=\"text-muted\">{}</span>\n", escape(d)))
        .unwrap_or_default();
    format!(
        "<!doctype html>\n\
         <html lang=\"en\" data-bs-theme=\"light\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1, shrink-to-fit=no\">\n\
         <title>{title}</title>\n\
         <link rel=\"stylesheet\" href=\"{bootstrap_css}\">\n\
         <link rel=\"stylesheet\" href=\"{fontawesome}\">\n\
         <link rel=\"stylesheet\" href=\"{saltcorn_css}\">\n\
         <link rel=\"stylesheet\" href=\"{builder_css}\">\n\
         <script src=\"{jquery}\"></script>\n\
         <script src=\"{bootstrap_js}\"></script>\n\
         <script src=\"{common}\"></script>\n\
         <script src=\"{saltcorn}\"></script>\n\
         <script src=\"{ckeditor}\"></script>\n\
         <script type=\"application/json\" id=\"{boot_id}\">{boot}</script>\n\
         <script type=\"module\" src=\"{builder_js}\"></script>\n\
         </head>\n\
         <body id=\"page-top\">\n\
         <header class=\"d-flex flex-wrap align-items-center gap-3 px-3 py-2 border-bottom\">\n\
         <span class=\"text-muted\">{app_name}</span>\n\
         <strong>{heading}</strong>\n\
         {detail}\
         {links}\
         <div id=\"builder-header-actions\" class=\"ms-auto d-flex gap-2\"></div>\n\
         </header>\n\
         <div id=\"saltcorn-builder\"></div>\n\
         <form id=\"scbuildform\" method=\"post\">\n\
         <input type=\"hidden\" name=\"contextEnc\" value=\"\">\n\
         <input type=\"hidden\" name=\"stepName\" value=\"{step_name}\">\n\
         <input type=\"hidden\" name=\"columns\" value=\"\">\n\
         <input type=\"hidden\" name=\"layout\" value=\"\">\n\
         <input type=\"hidden\" name=\"_csrf\" value=\"{csrf}\">\n\
         </form>\n\
         </body>\n\
         </html>\n",
        title = escape(&format!("{} · {} · builder", page.heading, app.name)),
        bootstrap_css = ui("bootstrap.min.css"),
        fontawesome = ui("fontawesome-free/css/all.min.css"),
        saltcorn_css = ui("saltcorn.css"),
        builder_css = bundle("builder.css"),
        jquery = ui("jquery-3.6.0.min.js"),
        bootstrap_js = ui("bootstrap.bundle.min.js"),
        common = ui("saltcorn-common.js"),
        saltcorn = ui("saltcorn.js"),
        ckeditor = bundle("ckeditor/ckeditor.js"),
        boot_id = BUILDER_BOOT_ID,
        boot = script_json(boot),
        builder_js = bundle(BUILDER_ENTRY),
        app_name = escape(&app.name),
        heading = escape(&page.heading),
        step_name = escape(&page.step_name),
        csrf = escape(csrf),
    )
}

/// A refusal as a short document: the sentence, and the way back to the admin UI.
fn refusal(status: StatusCode, sentence: &str, _: Option<()>) -> Response {
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>Builder</title>\n</head>\n<body>\n<h1>The builder cannot open this</h1>\n\
         <p>{}</p>\n<p><a href=\"/\">Back to the admin UI</a></p>\n</body>\n</html>\n",
        escape(sentence)
    );
    let mut response = (status, Html(html)).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(BUILDER_CONTENT_SECURITY_POLICY),
    );
    response
}

/// A failure the builder route did not expect, as a refusal: a not-found or
/// invalid error is a 404 naming what was not found, anything else a 500.
fn failure(error: &Error) -> Response {
    let status = match error.repr() {
        Repr::NotFound(_) | Repr::Invalid(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    refusal(status, &sc_error::format_chain(error), None)
}

/// JSON safe inside a `<script>` element: nothing in it can close the element
/// or open a comment.
fn script_json(value: &Json) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "null".to_owned())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Text for an HTML element or a quoted attribute.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// `encodeURIComponent`, which is how the admin UI's hash routes spell a name
/// (and how a map layer's tile URL carries the layer).
pub(crate) fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A path segment, percent-decoded; a malformed escape is kept as it is.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = segment.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_five_modes_are_built() {
        let options = |mode: &str| json!({ "mode": mode });
        for mode in BUILDER_VIEW_MODES {
            assert_eq!(
                builder_mode(&options(mode), &BUILDER_VIEW_MODES).as_deref(),
                Ok(mode)
            );
        }
        assert_eq!(
            builder_mode(&options("page"), &[BUILDER_PAGE_MODE]).as_deref(),
            Ok("page")
        );
        // A plugin pattern's own mode, and a page's mode where a view's belongs.
        let refused = builder_mode(&options("kanban"), &BUILDER_VIEW_MODES).unwrap_err();
        assert!(refused.contains("kanban"), "{refused}");
        assert!(refused.contains("show, edit, list, filter"), "{refused}");
        assert!(builder_mode(&options("page"), &BUILDER_VIEW_MODES).is_err());
        assert!(builder_mode(&options("show"), &[BUILDER_PAGE_MODE]).is_err());
        assert!(
            builder_mode(&json!({}), &BUILDER_VIEW_MODES)
                .unwrap_err()
                .contains("no mode")
        );
    }

    #[test]
    fn boot_data_cannot_close_its_script_element() {
        let text = script_json(&json!({ "text": "</script><!-- & \u{2028}" }));
        assert!(!text.contains('<') && !text.contains('>') && !text.contains('&'));
        let back: Json = serde_json::from_str(&text).unwrap();
        assert_eq!(back["text"], "</script><!-- & \u{2028}");
    }

    #[test]
    fn a_name_is_spelled_as_the_admin_ui_spells_it() {
        assert_eq!(encode_component("Show Books"), "Show%20Books");
        assert_eq!(encode_component("a/b?c#d"), "a%2Fb%3Fc%23d");
        assert_eq!(percent_decode("Show%20Books"), "Show Books");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn only_a_builder_document_on_this_host_names_the_files_application() {
        let headers = |referer: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_static("example.com:3032"));
            headers.insert(header::REFERER, HeaderValue::from_str(referer).unwrap());
            headers
        };
        let app = "7f1b3f0e-8f4e-4d7a-9a51-6f3c2b1d0e9a";
        for kind in ["views", "pages"] {
            assert_eq!(
                referring_application(&headers(&format!(
                    "http://example.com:3032/builder/applications/{app}/{kind}/Show%20Books?step=0"
                ))),
                Some(app.to_owned())
            );
        }
        // Another host, the admin UI itself, and no referrer at all.
        assert_eq!(
            referring_application(&headers(&format!(
                "http://evil.example/builder/applications/{app}/views/x"
            ))),
            None
        );
        assert_eq!(
            referring_application(&headers("http://example.com:3032/#/tables")),
            None
        );
        assert_eq!(referring_application(&HeaderMap::new()), None);
    }

    #[test]
    fn the_paths_are_the_builders_and_nothing_that_merely_starts_like_them() {
        assert!(is_builder_path("/builder"));
        assert!(is_builder_path("/builder/applications/x/views/y"));
        assert!(!is_builder_path("/builders"));
        assert!(is_files_serve_path("/files/serve/BooksDB/a.png"));
        assert!(!is_files_serve_path("/files/serve/"));
        assert!(!is_files_serve_path("/files/list"));
    }

    #[test]
    fn a_step_counts_from_zero() {
        let uri = |s: &str| s.parse::<Uri>().unwrap();
        assert_eq!(step_of(&uri("/builder/applications/a/views/v")), Ok(0));
        assert_eq!(
            step_of(&uri("/builder/applications/a/views/v?x=1&step=2")),
            Ok(2)
        );
        assert!(step_of(&uri("/b?step=two")).unwrap_err().contains("`two`"));
        assert!(step_of(&uri("/b?step=-1")).is_err());
    }
}
