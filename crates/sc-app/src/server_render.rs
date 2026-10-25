//! **Server-rendered code frameworks**: a built bundle that is static files
//! *and* a code body the server runs per request (the Astro module's
//! framework, `plugins/astro`).
//!
//! A [`CodeFramework`] serves what a bundler wrote and nothing else, which is
//! right for an SPA and wrong for a website: a page's content should be in the
//! HTML the server sends, and a blog's front page should show the posts that
//! are in the database now. So a framework may declare a **server entry** —
//! `build.server: "_server/entry.js"`, a path inside its output directory — and
//! its build writes there one JavaScript code body that answers a request.
//!
//! # The code body is a code action's
//!
//! The entry runs through [`TriggerDispatcher::run_code`] on `run_js_code`, the
//! path a JavaScript custom query takes. So what a page's frontmatter can reach
//! is exactly what a code action can — `db`, `fetch`, `fs`, `trigger`, `Table`,
//! `user`, `lib` — with the same bounds and the same (admin) authority, and
//! there is no second JavaScript surface to keep in step with the first. The
//! body is compiled once per isolate and cached by content, so a request costs
//! a call, not a compile.
//!
//! The one binding added is `request` ([`request_binding`]): the method, path,
//! query, parsed body, a few headers, the origin, the locale and the CSRF token.
//! What it answers is `{ status, headers: [[name, value]…], body }`.
//!
//! # Static first
//!
//! A `GET` for a file the build wrote — a stylesheet, an image, a page
//! prerendered at build time ([`prerender`]) — is served from the bundle without
//! running anything. Only what no file answers reaches the code body, which also
//! renders the not-found page. There is no SPA fallback: a website's unknown
//! path is a 404. `_server/` itself is never served: it is the site's source.
//!
//! # Speed
//!
//! - A rendered page is kept for the next anonymous visitor until a table it
//!   read is written ([`cache`], TODO-astro W2.1).
//! - A prerendered page that says `export const refresh = true` is served as a
//!   file until a table it read at build time is written, and then rendered
//!   again on its next request (W2.6).
//! - `<Image>`'s variants are resized here, once each ([`images`], W2.4).
//!
//! # Failures
//!
//! A render that fails is answered with the site's `500.astro`, when it has
//! one, after the failure is raised as an `error` event exactly as the router
//! would have raised it (W2.3). With no such page, the failure goes to the
//! router as before.

pub mod cache;
pub mod images;

use std::collections::{BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use sc_action::{Event, TriggerDispatcher};
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{Attrs, FormField};
use serde_json::{Value as Json, json};

use crate::application::Application;
use crate::build::BuildReport;
use crate::framework::{
    Asset, AssetBundle, BuildSpec, CodeFramework, Framework, Method, RequestBody,
};
use crate::{AppRequest, AppResponse};

/// The action a server entry runs on — the JavaScript code body's.
const RUN_JS: &str = "run_js_code";

/// Assets under this prefix are named by content hash, so a browser may keep
/// them forever.
const IMMUTABLE_PREFIX: &str = "_astro/";

/// What the build writes beside the entry: the prerendered pages that are
/// rendered again when their data changes. Read before `_server/` is dropped.
const PRERENDERED_MANIFEST: &str = "prerendered.json";

/// Check a declared server entry: a relative path inside the output directory,
/// with no way out of it. Answers the path normalised (no leading `./`).
pub fn check_server_entry(path: &str) -> Result<String> {
    let trimmed = path.trim().trim_start_matches("./");
    let bad = trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.contains('\\')
        || trimmed
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        return Err(Error::invalid(format!(
            "its build `server` is `{path}`, which is not a relative path inside the output \
             directory (`_server/entry.js`)"
        )));
    }
    Ok(trimmed.to_owned())
}

/// The framework an application's built bundle is served by: a plain
/// [`CodeFramework`], or — when the build declares a server entry — a
/// [`ServerRendered`] one over the same bundle.
///
/// What every mount path (build, boot, reload, preview) calls, so a
/// server-rendered application is mounted the same way whoever built it.
pub fn mount_code_framework(
    app: &Application,
    bundle: AssetBundle,
    build: BuildSpec,
    dispatcher: Option<Arc<TriggerDispatcher>>,
) -> Result<Arc<dyn Framework>> {
    let name = app.framework.name.clone();
    let Some(entry) = build.server_entry.clone() else {
        return Ok(Arc::new(CodeFramework::new(name, bundle).with_build(build)));
    };
    Ok(Arc::new(ServerRendered::new(
        app, bundle, build, &entry, dispatcher,
    )?))
}

/// A built site: its static files, and the code body that renders the rest.
pub struct ServerRendered {
    statics: CodeFramework,
    entry: Arc<str>,
    etags: HashMap<String, String>,
    app_id: uuid::Uuid,
    /// The application's name, which the page is bound as `request.site`: what
    /// an A/B test's rows are kept under (TODO-astro W3.3).
    site: String,
    run_name: String,
    dispatcher: Option<Arc<TriggerDispatcher>>,
    cache: Arc<cache::SiteCache>,
    images: images::SiteImages,
    /// The bundle keys of the prerendered pages that read data when they were
    /// rendered: pages, not files, so not for a caller the application does
    /// not admit (security review #25).
    data_pages: std::collections::HashSet<String>,
}

impl ServerRendered {
    /// Take the entry out of `bundle` — it is never served — and keep the rest
    /// as the site's static files, with no SPA fallback.
    pub fn new(
        app: &Application,
        mut bundle: AssetBundle,
        build: BuildSpec,
        entry: &str,
        dispatcher: Option<Arc<TriggerDispatcher>>,
    ) -> Result<ServerRendered> {
        let source = bundle.get(entry).ok_or_else(|| {
            Error::config(format!(
                "the build of `{}` wrote no server entry at {}/{entry}; build the \
                 application again",
                app.name, build.output_dir
            ))
        })?;
        let source = String::from_utf8(source.bytes.to_vec())
            .map_err(|_| Error::config(format!("the server entry {entry} is not UTF-8 text")))?;
        let manifest = bundle
            .get(&format!("{}/{PRERENDERED_MANIFEST}", server_dir(entry)))
            .and_then(|asset| serde_json::from_slice::<Json>(&asset.bytes).ok())
            .unwrap_or(Json::Null);
        let refresh = refresh_pages(&manifest);
        let data_pages = data_pages(&manifest);
        bundle.remove_prefix(server_dir(entry));
        bundle.clear_fallback();
        let etags = bundle
            .keys()
            .filter_map(|key| {
                bundle
                    .get(key)
                    .map(|asset| (key.to_owned(), etag(&asset.bytes)))
            })
            .collect();
        Ok(ServerRendered {
            statics: CodeFramework::new(app.framework.name.clone(), bundle).with_build(build),
            entry: Arc::from(source),
            etags,
            app_id: app.id.0,
            site: app.name.clone(),
            run_name: format!("page of {}", app.subdomain.trim()),
            dispatcher,
            cache: Arc::new(cache::SiteCache::new(refresh)),
            images: images::SiteImages::default(),
            data_pages,
        })
    }

    /// The site's render cache — for a test, or a caller that wants to know
    /// what is kept.
    pub fn render_cache(&self) -> &Arc<cache::SiteCache> {
        &self.cache
    }

    fn dispatcher(&self) -> Result<&Arc<TriggerDispatcher>> {
        self.dispatcher.as_ref().ok_or_else(|| {
            Error::config(
                "this server-rendered application was mounted without a trigger dispatcher, \
                 so it cannot run its pages",
            )
        })
    }

    /// The file a `GET` for `path` is answered with, if the build wrote one:
    /// the path itself, or the `index.html` of the directory it names (where a
    /// prerendered page is written).
    fn static_asset(&self, path: &str) -> Option<(String, &Asset)> {
        let key = crate::framework::normalize_asset_key(path);
        let candidates = if key.is_empty() {
            vec!["index.html".to_owned()]
        } else {
            vec![key.clone(), format!("{key}/index.html")]
        };
        candidates
            .into_iter()
            .find_map(|k| self.statics.asset(&k).map(|asset| (k, asset)))
    }

    /// One static file, with the caching a website wants: hashed assets kept
    /// forever, everything else revalidated by its ETag.
    fn serve_static(&self, key: &str, asset: &Asset, req: &AppRequest) -> AppResponse {
        let tag = self.etags.get(key).cloned().unwrap_or_default();
        let cache = if key.starts_with(IMMUTABLE_PREFIX) {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=0, must-revalidate"
        };
        if !tag.is_empty() && req.header("if-none-match") == Some(tag.as_str()) {
            return AppResponse::with_status(304, asset.content_type.clone(), Bytes::new())
                .header("etag", tag)
                .header("cache-control", cache);
        }
        AppResponse::ok(asset.content_type.clone(), asset.bytes.clone())
            .header("etag", tag)
            .header("cache-control", cache)
    }

    /// One file served from memory: a refreshed prerendered page.
    fn serve_rendered(page: &cache::RenderedPage, req: &AppRequest) -> AppResponse {
        let cache = "public, max-age=0, must-revalidate";
        if req.header("if-none-match") == Some(page.etag.as_str()) {
            return AppResponse::with_status(304, page.content_type.clone(), Bytes::new())
                .header("etag", page.etag.clone())
                .header("cache-control", cache);
        }
        AppResponse::ok(page.content_type.clone(), page.body.clone())
            .header("etag", page.etag.clone())
            .header("cache-control", cache)
    }

    /// A refreshable prerendered page whose data changed: render it again, as
    /// the build did, and serve that from now on. A render that fails leaves
    /// the file the build wrote being served, and says so in the log.
    async fn refresh(
        &self,
        key: &str,
        path: &str,
        asset: &Asset,
        req: &AppRequest,
        cat: &Catalog,
    ) -> AppResponse {
        let rendered = async {
            let dispatcher = self.dispatcher()?;
            self.cache.listen(cat);
            let epoch = self.cache.epoch();
            let trace = sc_expr::trace_sink();
            let mut page_req = AppRequest::get(path);
            page_req.base_url = String::new();
            let answer = run_entry(
                dispatcher,
                cat,
                &self.entry,
                &self.run_name,
                self.app_id,
                request_binding(&page_req, "prerender", &self.site),
                None,
                Some(trace.clone()),
            )
            .await?;
            let page = response_from(&answer)?;
            if !matches!(page.status, 200 | 404) {
                return Err(Error::invalid(format!(
                    "refreshing {path} answered {} rather than a page",
                    page.status
                )));
            }
            let tables = cache::watched_tables(cat, &sc_expr::read_trace(&trace).tables);
            let rendered = cache::RenderedPage {
                etag: etag(&page.body),
                content_type: page.content_type,
                body: page.body,
            };
            self.cache.refreshed(key, rendered.clone(), tables, epoch);
            Ok::<_, Error>(rendered)
        }
        .await;
        match rendered {
            Ok(page) => Self::serve_rendered(&page, req),
            Err(e) => {
                eprintln!(
                    "feldspar: refreshing the prerendered page {path}: {}",
                    e.chain()
                );
                self.serve_static(key, asset, req)
            }
        }
    }

    /// What a request is kept under in the render cache, when it may be: an
    /// anonymous `GET`.
    fn cache_key(req: &AppRequest) -> Option<cache::CacheKey> {
        if !matches!(req.method, Method::Get) || req.user.is_some() {
            return None;
        }
        Some(cache::CacheKey {
            origin: req.base_url.clone(),
            path: req.path.clone(),
            query: req
                .query
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            locale: req.locale.to_string(),
        })
    }

    /// Run the entry for one request — or answer it from the render cache.
    async fn render(&self, req: &AppRequest, cat: &Catalog) -> Result<AppResponse> {
        let dispatcher = self.dispatcher()?;
        let key = Self::cache_key(req);
        if let Some(key) = &key
            && let Some(hit) = self.cache.get(key)
        {
            return Ok(hit.header("x-render-cache", "hit"));
        }
        self.cache.listen(cat);
        let epoch = self.cache.epoch();
        let trace = sc_expr::trace_sink();
        let caller = sc_api::caller_context(req.user.as_ref());
        let answer = match run_entry(
            dispatcher,
            cat,
            &self.entry,
            &self.run_name,
            self.app_id,
            request_binding(req, "render", &self.site),
            Some(&caller),
            Some(trace.clone()),
        )
        .await
        {
            Ok(answer) => answer,
            Err(e) => return self.server_error(req, cat, &caller, e).await,
        };
        let shareable = answer.get("cache") == Some(&Json::Bool(true));
        let mut out = response_from(&answer)?;
        // An A/B test chose by the visitor id, so it must outlast this request.
        out.keeps_visitor = answer.get("visitor") == Some(&Json::Bool(true));
        if !out.headers.iter().any(|(k, _)| k == "cache-control") {
            // A rendered page may carry this visitor's CSRF token or data, so a
            // shared cache must not keep it unless the page says it may.
            out.headers
                .push(("cache-control".to_owned(), "private, no-cache".to_owned()));
        }
        if let Some(key) = key
            && shareable
            && matches!(out.status, 200 | 404)
        {
            let touched = sc_expr::read_trace(&trace);
            if !touched.side_effects {
                let tables: BTreeSet<String> = cache::watched_tables(cat, &touched.tables);
                self.cache.insert(key, out.clone(), tables, epoch);
            }
        }
        Ok(out)
    }

    /// A render failed with `err`: answer the site's `500.astro`, after raising
    /// the failure as the `error` event the router would have raised. A
    /// deliberate `httpError`, a site with no such page, or a 500 page that
    /// fails too, hands `err` back to the router, which raises the event and
    /// answers as it does for any application.
    async fn server_error(
        &self,
        req: &AppRequest,
        cat: &Catalog,
        caller: &sc_catalog::CallerContext,
        err: Error,
    ) -> Result<AppResponse> {
        if err.http_reply().is_some() {
            return Err(err);
        }
        let Ok(dispatcher) = self.dispatcher() else {
            return Err(err);
        };
        let mut binding = request_binding(req, "error", &self.site);
        binding["error"] = Json::String(err.to_string());
        let answer = run_entry(
            dispatcher,
            cat,
            &self.entry,
            &self.run_name,
            self.app_id,
            binding,
            Some(caller),
            None,
        )
        .await;
        let page = match answer {
            Ok(answer) if answer.is_object() => match response_from(&answer) {
                Ok(page) => page,
                Err(_) => return Err(err),
            },
            Ok(_) => return Err(err),
            Err(page_err) => {
                eprintln!(
                    "feldspar: rendering 500.astro failed too: {}",
                    page_err.chain()
                );
                return Err(err);
            }
        };
        eprintln!("feldspar: internal error: {}", err.chain());
        let event = Event::error(err.kind(), err.to_string(), req.method.as_str(), &req.path)
            .caused_by(caller);
        dispatcher.fire(cat, &event).await;
        let mut page = page;
        page.status = 500;
        page.headers.retain(|(k, _)| k != "cache-control");
        page.headers
            .push(("cache-control".to_owned(), "no-store".to_owned()));
        Ok(page)
    }
}

/// What a caller the application does not let in gets for a page.
fn forbidden() -> AppResponse {
    AppResponse::html(
        403,
        "<!DOCTYPE html><title>Forbidden</title><h1>You may not see this page</h1>",
    )
}

/// The bundle keys of the prerendered pages a manifest says read data.
fn data_pages(manifest: &Json) -> std::collections::HashSet<String> {
    manifest
        .get("data")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(prerendered_file)
        .collect()
}

/// The refreshable prerendered pages a manifest lists, as `(bundle key, site
/// path)`.
fn refresh_pages(manifest: &Json) -> Vec<(String, String)> {
    manifest
        .get("refresh")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|path| path.as_str())
        .map(|path| (prerendered_file(path), path.to_owned()))
        .collect()
}

#[async_trait]
impl Framework for ServerRendered {
    fn name(&self) -> &str {
        self.statics.name()
    }

    fn config_spec(&self) -> Vec<FormField> {
        self.statics.config_spec()
    }

    async fn handle(&self, req: AppRequest, cat: &Catalog) -> Result<AppResponse> {
        if matches!(req.method, Method::Get) && req.path == images::IMAGE_PATH {
            let source = req
                .query
                .get("src")
                .and_then(|src| self.statics.asset(src.trim_start_matches('/')))
                .map(|asset| asset.bytes.clone());
            return Ok(self.images.serve(&req, source).await);
        }
        if matches!(req.method, Method::Get)
            && let Some((key, asset)) = self.static_asset(&req.path)
        {
            let refreshable = self.cache.refresh_page(&key);
            // A prerendered page that read data is a rendered page, written
            // ahead of time: refused as one to a caller not let in.
            if !req.admitted && (refreshable.is_some() || self.data_pages.contains(&key)) {
                return Ok(forbidden());
            }
            if let Some((path, current)) = refreshable {
                return Ok(match current {
                    Some(page) => Self::serve_rendered(&page, &req),
                    None => self.refresh(&key, &path, asset, &req, cat).await,
                });
            }
            return Ok(self.serve_static(&key, asset, &req));
        }
        // Somebody the application does not let in gets its files — they are
        // what draws a sign-in — and none of its rendered pages.
        if !req.admitted {
            return Ok(forbidden());
        }
        self.render(&req, cat).await
    }

    fn build(&self) -> Option<BuildSpec> {
        self.statics.build()
    }
}

/// The directory a server entry is in — what is removed from the served
/// bundle — or the entry itself when it is at the top of the output.
fn server_dir(entry: &str) -> &str {
    match entry.split_once('/') {
        Some((dir, _)) => dir,
        None => entry,
    }
}

/// A strong ETag for `bytes`.
fn etag(bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("\"{:016x}-{:x}\"", hasher.finish(), bytes.len())
}

/// What the entry is bound as `request`. `site` is the application's name.
fn request_binding(req: &AppRequest, mode: &str, site: &str) -> Json {
    let (form, json_body) = match &req.body {
        RequestBody::Empty => (Json::Null, Json::Null),
        RequestBody::Form(pairs) => (
            Json::Array(pairs.iter().map(|(k, v)| json!([k, v])).collect()),
            Json::Null,
        ),
        RequestBody::Json(value) => (Json::Null, value.clone()),
    };
    let mut binding = json!({
        "mode": mode,
        "method": req.method.as_str(),
        "path": req.path,
        "query": req.query,
        "headers": req.headers,
        "origin": req.base_url,
        "locale": req.locale.to_string(),
        "csrfToken": req.csrf_token,
        "cookies": req.cookies,
        "visitor": req.visitor,
        "site": site,
    });
    if !form.is_null() {
        binding["form"] = form;
    }
    if !json_body.is_null() {
        binding["json"] = json_body;
    }
    binding
}

/// Run `entry` once with `request` bound, recording what it touched into
/// `trace` when one is given.
#[allow(clippy::too_many_arguments)]
async fn run_entry(
    dispatcher: &TriggerDispatcher,
    cat: &Catalog,
    entry: &str,
    name: &str,
    app: uuid::Uuid,
    request: Json,
    caller: Option<&sc_catalog::CallerContext>,
    trace: Option<sc_expr::TraceSink>,
) -> Result<Json> {
    let mut scope = Attrs::new();
    scope.insert("request".to_owned(), request);
    dispatcher
        .run_code_traced(
            cat,
            RUN_JS,
            name,
            entry,
            scope,
            caller,
            Some(app),
            None,
            trace,
        )
        .await
}

/// The entry's answer as a response.
fn response_from(answer: &Json) -> Result<AppResponse> {
    let shape = || {
        Error::invalid(format!(
            "the server entry answered {answer}, not {{ status, headers, body }}"
        ))
    };
    let status = answer
        .get("status")
        .and_then(Json::as_u64)
        .ok_or_else(shape)?;
    let status = u16::try_from(status)
        .ok()
        .filter(|s| (200..=599).contains(s))
        .ok_or_else(shape)?;
    let body = answer
        .get("body")
        .and_then(Json::as_str)
        .unwrap_or_default();
    let mut content_type = "text/html; charset=utf-8".to_owned();
    let mut headers = Vec::new();
    for pair in answer
        .get("headers")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(value)) = (
            pair.get(0).and_then(Json::as_str),
            pair.get(1).and_then(Json::as_str),
        ) else {
            return Err(shape());
        };
        let name = name.to_ascii_lowercase();
        if name == "content-type" {
            content_type = value.to_owned();
        } else {
            headers.push((name, value.to_owned()));
        }
    }
    let mut out = AppResponse::with_status(status, content_type, Bytes::from(body.to_owned()));
    out.headers = headers;
    Ok(out)
}

/// **Prerender** a server-rendered build: ask the entry which pages say
/// `prerender = true`, render each through the same code body, and write it into
/// the output directory and the report's bundle — so the mount serves it as a
/// file, and a reload from disk finds it there.
///
/// Answers how many pages were written. A build without a server entry has
/// none, and one with no dispatcher (a build from the command line, with no
/// server around it) writes none and says so in the report: those pages are
/// then rendered per request, which is slower and otherwise the same.
pub async fn prerender(
    cat: &Catalog,
    app: &Application,
    build: &BuildSpec,
    report: &mut BuildReport,
    dispatcher: Option<&Arc<TriggerDispatcher>>,
) -> Result<usize> {
    let Some(entry_path) = &build.server_entry else {
        return Ok(0);
    };
    let Some(dispatcher) = dispatcher else {
        report.stdout.push_str(
            "\nprerendering skipped: no server to run the pages on; they are rendered per request\n",
        );
        return Ok(0);
    };
    let entry = report
        .bundle
        .get(entry_path)
        .map(|asset| String::from_utf8_lossy(&asset.bytes).into_owned())
        .ok_or_else(|| {
            Error::config(format!(
                "the build wrote no server entry at {}/{entry_path}",
                build.output_dir
            ))
        })?;
    let name = format!("prerender of {}", app.subdomain.trim());
    let mut base = AppRequest::get("/");
    base.base_url = String::new();
    let list = run_entry(
        dispatcher,
        cat,
        &entry,
        &name,
        app.id.0,
        request_binding(&base, "prerender-list", &app.name),
        None,
        None,
    )
    .await
    .map_err(|e| Error::invalid(format!("prerendering {}: {e}", app.name)))?;
    let paths: Vec<String> = list
        .get("paths")
        .and_then(Json::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(|p| p.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let mut written = 0;
    // The pages that read a table: refused, like a rendered page, to a caller
    // the application does not let in.
    let mut data: Vec<String> = Vec::new();
    for path in paths {
        let mut req = AppRequest::get(path.clone());
        req.base_url = String::new();
        let trace = sc_expr::trace_sink();
        let answer = run_entry(
            dispatcher,
            cat,
            &entry,
            &name,
            app.id.0,
            request_binding(&req, "prerender", &app.name),
            None,
            Some(trace.clone()),
        )
        .await
        .map_err(|e| Error::invalid(format!("prerendering {path} of {}: {e}", app.name)))?;
        let page = response_from(&answer)?;
        let expected = if path == "/404" { 404 } else { 200 };
        if page.status != expected {
            return Err(Error::invalid(format!(
                "prerendering {path} of {} answered {} rather than a page; a prerendered page \
                 cannot redirect or depend on the request",
                app.name, page.status
            )));
        }
        let touched = sc_expr::read_trace(&trace);
        if !touched.tables.is_empty() || touched.side_effects {
            data.push(path.clone());
        }
        let file = prerendered_file(&path);
        write_output(&report.output_dir, &file, &page.body)?;
        report.bundle.insert(&file, page.body.clone());
        written += 1;
    }
    // The pages to render again when their data changes (`refresh = true`),
    // beside the entry: the mount reads it, and nothing serves it.
    let refresh: Vec<&str> = list
        .get("refresh")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .collect();
    let manifest = format!("{}/{PRERENDERED_MANIFEST}", server_dir(entry_path));
    let body = Bytes::from(json!({ "refresh": refresh, "data": data }).to_string());
    write_output(&report.output_dir, &manifest, &body)?;
    report.bundle.insert(&manifest, body);
    report
        .stdout
        .push_str(&format!("\nprerendered {written} page(s)\n"));
    Ok(written)
}

/// Where a prerendered path is written: `/` is `index.html`, `/about` is
/// `about/index.html`, `/404` is `404.html`, and a path whose last segment has
/// an extension (`/rss.xml`, an endpoint) is written as it is.
fn prerendered_file(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return "index.html".to_owned();
    }
    if trimmed == "404" {
        return "404.html".to_owned();
    }
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if last.contains('.') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/index.html")
    }
}

/// Write one prerendered file under the output directory, refusing a path that
/// would leave it.
fn write_output(output_dir: &Path, file: &str, body: &[u8]) -> Result<()> {
    if file.split('/').any(|part| part == ".." || part.is_empty()) {
        return Err(Error::invalid(format!(
            "a prerendered page's path `{file}` leaves the output directory"
        )));
    }
    let target = output_dir.join(file);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrameworkRef;

    fn app() -> Application {
        Application::new("Site", "site", FrameworkRef::new("astro"))
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

    fn bundle() -> AssetBundle {
        AssetBundle::new()
            .with("index.html", "<h1>home</h1>")
            .with("about/index.html", "<h1>about</h1>")
            .with("_astro/site.abc.css", "body{}")
            .with(
                "_server/entry.js",
                "return { status: 200, headers: [], body: 'x' };",
            )
            .fallback("index.html")
    }

    #[test]
    fn a_server_entry_must_stay_inside_the_output() {
        assert_eq!(
            check_server_entry("./_server/entry.js").unwrap(),
            "_server/entry.js"
        );
        for bad in ["", "/etc/x.js", "../x.js", "_server/../../x.js", "a//b.js"] {
            assert!(check_server_entry(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_entry_is_never_served_and_there_is_no_spa_fallback() {
        let fw = ServerRendered::new(&app(), bundle(), spec(), "_server/entry.js", None).unwrap();
        assert!(fw.entry.contains("status: 200"));
        assert!(fw.static_asset("/_server/entry.js").is_none());
        assert!(
            fw.static_asset("/no/such/page").is_none(),
            "no SPA fallback"
        );
        assert_eq!(fw.static_asset("/").unwrap().0, "index.html");
        assert_eq!(fw.static_asset("/about").unwrap().0, "about/index.html");
        assert_eq!(fw.static_asset("/about/").unwrap().0, "about/index.html");
    }

    #[test]
    fn hashed_assets_are_immutable_and_pages_revalidate() {
        let fw = ServerRendered::new(&app(), bundle(), spec(), "_server/entry.js", None).unwrap();
        let req = AppRequest::get("/_astro/site.abc.css");
        let (key, asset) = fw.static_asset(&req.path).unwrap();
        let css = fw.serve_static(&key, asset, &req);
        assert_eq!(css.status, 200);
        assert!(
            css.headers
                .iter()
                .any(|(k, v)| k == "cache-control" && v.contains("immutable"))
        );

        let (key, asset) = fw.static_asset("/").unwrap();
        let page = fw.serve_static(&key, asset, &AppRequest::get("/"));
        let tag = page
            .headers
            .iter()
            .find(|(k, _)| k == "etag")
            .unwrap()
            .1
            .clone();
        let mut again = AppRequest::get("/");
        again.headers.insert("if-none-match".to_owned(), tag);
        assert_eq!(fw.serve_static(&key, asset, &again).status, 304);
    }

    #[test]
    fn a_build_without_its_entry_is_refused_by_name() {
        let msg = ServerRendered::new(&app(), AssetBundle::new(), spec(), "_server/entry.js", None)
            .err()
            .unwrap()
            .to_string();
        assert!(msg.contains("_server/entry.js"), "{msg}");
    }

    #[test]
    fn the_entry_answer_becomes_a_response() {
        let out = response_from(&json!({
            "status": 303,
            "headers": [["Location", "/thanks"], ["content-type", "text/plain"]],
            "body": "moved"
        }))
        .unwrap();
        assert_eq!(out.status, 303);
        assert_eq!(out.content_type, "text/plain");
        assert_eq!(
            out.headers,
            vec![("location".to_owned(), "/thanks".to_owned())]
        );
        assert_eq!(&out.body[..], b"moved");
        assert!(response_from(&json!({ "status": 99, "body": "" })).is_err());
        assert!(response_from(&json!("html")).is_err());
    }

    #[test]
    fn prerendered_paths_are_written_where_the_static_lookup_finds_them() {
        assert_eq!(prerendered_file("/"), "index.html");
        assert_eq!(prerendered_file("/about"), "about/index.html");
        assert_eq!(prerendered_file("/blog/hello/"), "blog/hello/index.html");
        assert_eq!(prerendered_file("/rss.xml"), "rss.xml");
        assert_eq!(prerendered_file("/404"), "404.html");
    }

    #[test]
    fn only_an_anonymous_get_is_kept_and_its_query_is_part_of_the_key() {
        let mut get = AppRequest::get("/blog");
        get.query.insert("page".to_owned(), "2".to_owned());
        let key = ServerRendered::cache_key(&get).unwrap();
        assert_eq!(key.query, vec![("page".to_owned(), "2".to_owned())]);
        assert!(ServerRendered::cache_key(&AppRequest::new(Method::Post, "/blog")).is_none());
        let mut signed_in = AppRequest::get("/blog");
        signed_in.user = Some(sc_auth::User::new(uuid::Uuid::nil(), 40).unwrap());
        assert!(ServerRendered::cache_key(&signed_in).is_none());
    }

    #[test]
    fn the_manifest_names_the_refreshable_pages_where_they_are_served() {
        let manifest = json!({ "refresh": ["/", "/blog/hello"] });
        assert_eq!(
            refresh_pages(&manifest),
            vec![
                ("index.html".to_owned(), "/".to_owned()),
                ("blog/hello/index.html".to_owned(), "/blog/hello".to_owned()),
            ]
        );
        let built = bundle().with("_server/prerendered.json", manifest.to_string());
        let fw = ServerRendered::new(&app(), built, spec(), "_server/entry.js", None).unwrap();
        assert!(fw.cache.refresh_page("index.html").is_some());
        assert!(fw.cache.refresh_page("about/index.html").is_none());
        assert!(
            fw.static_asset("/_server/prerendered.json").is_none(),
            "the manifest is not served"
        );
    }

    #[test]
    fn the_request_binding_carries_the_parsed_body_and_the_token() {
        let mut req = AppRequest::new(Method::Post, "/contact");
        req.body = RequestBody::Form(vec![("email".to_owned(), "a@b.c".to_owned())]);
        req.csrf_token = "tok".to_owned();
        req.cookies.insert("theme".to_owned(), "dark".to_owned());
        req.visitor = "v1".to_owned();
        let binding = request_binding(&req, "render", "Site");
        assert_eq!(binding["cookies"], json!({ "theme": "dark" }));
        assert_eq!(binding["method"], "POST");
        assert_eq!(binding["form"], json!([["email", "a@b.c"]]));
        assert_eq!(binding["csrfToken"], "tok");
        assert_eq!(binding["visitor"], "v1");
        assert_eq!(binding["site"], "Site");
        assert!(binding.get("json").is_none());
    }
}
