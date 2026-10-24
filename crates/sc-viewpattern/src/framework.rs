//! Saltcorn UI as a [`Framework`] (TODO "Saltcorn UI" Phase 5): the fourth
//! framework, and the first with nothing to build.
//!
//! An application whose framework is `saltcorn-ui` owns views and pages rather
//! than a source tree, and this is what serves them on its subdomain:
//!
//! - `/` — the viewer's home page, else a document naming what there is;
//! - `/view/:name` and `/view/:name/*slug` — one view, through the
//!   [`ViewRuntime`];
//! - `/page/:name` — one page, with the views it embeds rendered in place;
//! - `/static_assets/:tag/*` — the browser half of v1 (Bootstrap, jQuery,
//!   `saltcorn.js`), out of the bundle's `public/`;
//! - `/files/serve/*` — the application's file stores, under their access rules;
//! - `/plugins/public/:name@:version/*` — an installed plugin's own `public/`,
//!   which its declared headers point into (Phase 11).
//!
//! And three things posted to (Phase 6):
//!
//! - `POST /view/:name` and `/view/:name/*slug` — the pattern's `runPost`: a
//!   form's fields as `req.body`, and the redirect or re-rendered form back out;
//! - `POST /view/:name/:route` — one of the pattern's `routes` (`run_action`,
//!   `update_matching_rows`), answering JSON;
//! - `POST /delete/:table/:id` — v1's Delete action, under the viewer's
//!   authority.
//!
//! And who is looking (Phase 7):
//!
//! - `/auth/login` and `/auth/signup` — the application's own forms, answered
//!   with a session change the router applies exactly as it applies an API
//!   provider's login; sign-up only where the settings offer it;
//! - `/auth/logout`;
//! - a view or page below the viewer's role is not run: an anonymous navigation
//!   is sent to `/auth/login` with the way back, anybody else is told no.
//!
//! **No build**: [`SaltcornUiFactory`] constructs the framework when the
//! application is mounted, and a mount re-reads the application's view set
//! under a new generation (§4). What a request renders is always the view set
//! as of the last write, because every write goes through [`view_sets`].
//!
//! **Whose authority**: every read a view makes is under the viewer's — the
//! surfaces a call carries are built from the request's user, exactly as a code
//! body's are built from its event's caller (§11).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use bytes::Bytes;
use sc_action::TriggerDispatcher;
use sc_api::SessionAction;
use sc_api::code_host::{FileStoreHost, TableHost, TriggerRunHost, schema_snapshot};
use sc_app::{
    AppRequest, AppResponse, Application, BuildSpec, CspPolicy, Framework, FrameworkFactory,
    FrameworkInfo, Method, MountContext, RequestBody, TriggerRef,
};
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, ErrorKind, Repr, Result};
use sc_expr::{CodeHosts, JsEvaluator, ModuleFnHost, TriggerHost};
use sc_i18n::t;
use sc_types::{Attrs, BasicType, FormField};
use serde_json::{Map, Value as Json};

use crate::bundle::{SALTCORN_UI_FRAMEWORK, require_view_runtime};
use crate::plugins::{installed_plugin_assets, plugin_header_tags, plugin_public_file};
use crate::runtime::{
    ViewContext, ViewOutput, ViewRequest, ViewRuntime, ViewUser, Wrap, view_runtime,
};
use crate::snapshot::{MENU_CONFIG_KEY, ViewSnapshot};
use crate::view::{Page, View};
use crate::view_set::{ViewSet, ViewSets};

/// The `site_name` setting: the brand in the navbar, and `getConfig("site_name")`.
pub const CFG_SITE_NAME: &str = "site_name";

/// The `root_pages` setting: which page `/` is for each role, as
/// `{ "<role id>": "<page name>" }`. A role it does not name falls back to the
/// pages whose `root_page_for_roles` attribute names it, which is where a
/// restored v1 backup keeps the answer.
pub const CFG_ROOT_PAGES: &str = "root_pages";

/// The `allow_signup` setting: whether `/auth/signup` is offered (7.3). v1's own
/// key; off unless an admin turns it on.
pub const CFG_ALLOW_SIGNUP: &str = "allow_signup";

/// The `new_user_role` setting: the role an account made at `/auth/signup` gets.
/// v1's own key and v1's default, 80; never the admin role.
pub const CFG_NEW_USER_ROLE: &str = "new_user_role";

/// The role [`CFG_NEW_USER_ROLE`] defaults to — v1's `user` role.
const DEFAULT_NEW_USER_ROLE: u8 = 80;

/// The `getConfig` keys the view runtime answers that are **not** settings: the
/// application knows them (`base_url`), or the server decides them
/// (`enable_dynamic_updates`, always off). Every other declared key is offered
/// by [`saltcorn_ui_config_spec`] — §7's rule, held by a test in `sc-module`
/// against the runtime's own list.
pub const DERIVED_CONFIG_KEYS: [&str; 2] = ["base_url", "enable_dynamic_updates"];

/// The version tag in `/static_assets/<tag>/…`. The server's version, so an
/// upgrade is a new URL and a year-long cache is safe.
pub const ASSET_VERSION_TAG: &str = env!("CARGO_PKG_VERSION");

/// The public role, which an anonymous viewer has.
const ROLE_PUBLIC: u8 = 100;

/// The admin role, which sign-up may never give.
const ROLE_ADMIN: u8 = 1;

/// The view sets every Saltcorn UI application renders from, one per
/// application, shared by the mount, the renders and every writer.
///
/// One per process for the reason [`view_runtime`] is: an application's views
/// are the application's wherever they are read from, and a write that reloaded
/// one cache while a render read another would be the stale read §4's
/// generation exists to rule out.
pub fn view_sets() -> &'static ViewSets {
    static SETS: OnceLock<ViewSets> = OnceLock::new();
    SETS.get_or_init(ViewSets::new)
}

/// Install Saltcorn UI into `sc-app`'s framework registry. Idempotent.
pub fn install_saltcorn_ui() -> Result<()> {
    sc_app::install_framework_factory(Arc::new(SaltcornUiFactory))
}

/// The settings a Saltcorn UI application has (5.4): the brand, the menu, the
/// home page per role, and every `getConfig` key the view runtime declares that
/// the application does not already know.
pub fn saltcorn_ui_config_spec() -> Vec<FormField> {
    vec![
        FormField::new(CFG_SITE_NAME, BasicType::Text)
            .label("Site name")
            .default_value(""),
        FormField::new(MENU_CONFIG_KEY, BasicType::Json)
            .label("Menu")
            .default_value(Json::Array(Vec::new())),
        FormField::new(CFG_ROOT_PAGES, BasicType::Json)
            .label("Home page per role")
            .default_value(Json::Object(Map::new())),
        FormField::new("default_locale", BasicType::Text)
            .label("Default locale")
            .default_value("en"),
        FormField::new("login_form", BasicType::Text)
            .label("Login form view")
            .default_value(""),
        FormField::new("exttables_min_role_read", BasicType::Json)
            .label("External tables: read role per table")
            .default_value(Json::Object(Map::new())),
        FormField::new("layout_by_role", BasicType::Json)
            .label("Layout per role")
            .default_value(Json::Object(Map::new())),
        FormField::new("push_policy_by_role", BasicType::Json)
            .label("Push policy per role")
            .default_value(Json::Object(Map::new())),
        FormField::new("localizer_languages", BasicType::Json)
            .label("Languages")
            .default_value(Json::Object(Map::new())),
        FormField::new("search_disable_fts", BasicType::Bool)
            .label("Search: no full-text search")
            .default_value(false),
        FormField::new("search_use_websearch", BasicType::Bool)
            .label("Search: web-style queries")
            .default_value(false),
        FormField::new(CFG_ALLOW_SIGNUP, BasicType::Bool)
            .label("Offer sign-up")
            .default_value(false),
        FormField::new(CFG_NEW_USER_ROLE, BasicType::Int)
            .label("Role of a new account")
            .default_value(i64::from(DEFAULT_NEW_USER_ROLE)),
    ]
}

/// What the spec's vocabulary cannot say about the settings: the menu is a list
/// of entries and the home pages a map of role to page name.
pub fn check_saltcorn_ui_config(config: &Attrs) -> Result<()> {
    fn check_menu(items: &Json, at: &str) -> Result<()> {
        let Json::Array(items) = items else {
            return Err(Error::invalid(format!(
                "framework `{SALTCORN_UI_FRAMEWORK}`: `{at}` must be a list of menu entries"
            )));
        };
        for (i, item) in items.iter().enumerate() {
            let entry = format!("{at}[{i}]");
            if !item.get("type").is_some_and(Json::is_string) {
                return Err(Error::invalid(format!(
                    "framework `{SALTCORN_UI_FRAMEWORK}`: menu entry `{entry}` has no `type` \
                     (View, Page, Link or Header)"
                )));
            }
            if let Some(sub) = item.get("subitems").filter(|s| !s.is_null()) {
                check_menu(sub, &format!("{entry}.subitems"))?;
            }
        }
        Ok(())
    }
    if let Some(menu) = config.get(MENU_CONFIG_KEY).filter(|m| !m.is_null()) {
        check_menu(menu, MENU_CONFIG_KEY)?;
    }
    if let Some(roots) = config.get(CFG_ROOT_PAGES).filter(|r| !r.is_null()) {
        let ok = roots.as_object().is_some_and(|m| {
            m.iter()
                .all(|(role, page)| role.parse::<u8>().is_ok() && page.is_string())
        });
        if !ok {
            return Err(Error::invalid(format!(
                "framework `{SALTCORN_UI_FRAMEWORK}`: `{CFG_ROOT_PAGES}` must map a role id to a \
                 page name, like {{\"1\": \"Dashboard\", \"100\": \"Welcome\"}}"
            )));
        }
    }
    // Sign-up is anybody at all making an account, so it may not make an
    // administrator.
    if let Some(role) = config.get(CFG_NEW_USER_ROLE).filter(|r| !r.is_null())
        && !role
            .as_u64()
            .is_some_and(|r| (u64::from(ROLE_ADMIN) + 1..=u64::from(ROLE_PUBLIC)).contains(&r))
    {
        return Err(Error::invalid(format!(
            "framework `{SALTCORN_UI_FRAMEWORK}`: `{CFG_NEW_USER_ROLE}` must be a role from 2 to \
             100; an account anybody can make at /auth/signup cannot be an administrator"
        )));
    }
    Ok(())
}

/// The policy a Saltcorn UI application gets unless the admin states one (§10):
/// the strict baseline with `script-src` widened by `'unsafe-inline'`, because
/// v1's markup puts JavaScript in `onclick` attributes, which cannot be nonced.
/// Nothing else moves — no `eval`, no `blob:`, no other origin.
pub fn saltcorn_ui_csp() -> CspPolicy {
    CspPolicy::strict().directive(
        "script-src",
        vec!["'self'".to_owned(), "'unsafe-inline'".to_owned()],
    )
}

/// Saltcorn UI in the framework registry: its picker entry, its settings, and
/// the mount that constructs it.
pub struct SaltcornUiFactory;

#[async_trait]
impl FrameworkFactory for SaltcornUiFactory {
    fn info(&self) -> FrameworkInfo {
        FrameworkInfo {
            name: SALTCORN_UI_FRAMEWORK.to_owned(),
            label: "Saltcorn UI".to_owned(),
            description: "Views and pages you configure here, rendered by the server with \
                          Saltcorn 1's own List, Show, Edit, Feed and Filter. Nothing to write \
                          and nothing to build: pick the tables it shows."
                .to_owned(),
            serves_ui: true,
        }
    }

    fn config_spec(&self) -> Vec<FormField> {
        saltcorn_ui_config_spec()
    }

    fn default_csp(&self) -> CspPolicy {
        saltcorn_ui_csp()
    }

    fn check_config(&self, config: &Attrs) -> Result<()> {
        check_saltcorn_ui_config(config)
    }

    /// Construct the framework — after checking, **once, here**, that this
    /// server can render anything at all (2.6): the bundle it was built with,
    /// and the view runtime started from it. The view set is re-read under a new
    /// generation, which is what makes a mount the reload of an application
    /// (5.7).
    async fn mount(&self, app: &Application, ctx: MountContext<'_>) -> Result<Arc<dyn Framework>> {
        let runtime_file = require_view_runtime(ctx.bundle_dir)?;
        let bundle = runtime_file
            .parent()
            .map_or_else(PathBuf::new, Path::to_path_buf);
        let runtime = view_runtime()?;
        view_sets().reload(ctx.catalog, app.id).await?;
        Ok(Arc::new(SaltcornUiFramework {
            app: app.clone(),
            evaluator: ctx.evaluator,
            triggers: ctx.triggers,
            public_dir: bundle.join("public"),
            runtime,
            snapshot: RwLock::new(None),
            catalogues: RwLock::new(std::collections::HashMap::new()),
        }))
    }
}

/// One mounted Saltcorn UI application.
pub struct SaltcornUiFramework {
    app: Application,
    evaluator: Option<Arc<dyn JsEvaluator>>,
    triggers: Option<Arc<TriggerDispatcher>>,
    /// The bundle's `public/`: what `/static_assets/` serves.
    public_dir: PathBuf,
    runtime: Arc<dyn ViewRuntime>,
    /// The snapshot of the view set's current generation, built once for it.
    snapshot: RwLock<Option<Arc<ViewSnapshot>>>,
    /// The application's catalogue per locale, flat (§16.1, task 4.5).
    ///
    /// This framework renders **server-side**, so the phrases are looked up as
    /// the HTML is built and the catalogue has to be here rather than in the
    /// browser. It is read once per locale and dropped by
    /// [`forget_catalogues`](Framework::forget_catalogues) when a translation
    /// is saved, which is what makes a fix live without a remount (D7).
    ///
    /// An application with no locales never populates it: the load is behind
    /// the same `app_is_translated` guard every other path uses (D11).
    catalogues: RwLock<std::collections::HashMap<String, Arc<BTreeMap<String, String>>>>,
}

impl SaltcornUiFramework {
    /// The catalogue for this request's locale, read once and remembered.
    ///
    /// Flat strings only: v1's `__` has no plural forms, only positional `%s`,
    /// so a plural entry is left out and its English renders — which is right,
    /// because a message with plural forms was not one of v1's to begin with.
    async fn catalogue(&self, cat: &Catalog, locale: &str) -> Arc<BTreeMap<String, String>> {
        // The zero-cost case, first and without a lock on the slow path (D11).
        if !sc_app::app_is_translated(&self.app) {
            return Arc::new(BTreeMap::new());
        }
        if let Ok(held) = self.catalogues.read()
            && let Some(found) = held.get(locale)
        {
            return found.clone();
        }
        let mut flat = BTreeMap::new();
        // The app's own catalogue over the instance's (TODO F6), the same
        // merge the `{mount}/i18n/{locale}.json` route serves.
        if let Ok(parsed) = sc_i18n::Locale::parse(locale)
            && let Ok(Some(catalogue)) = sc_app::merged_app_catalogue(cat, &self.app, &parsed).await
        {
            for (key, message) in catalogue.messages() {
                if let sc_i18n::Message::Simple(text) = message {
                    flat.insert(key.clone(), text.clone());
                }
            }
        }
        let held = Arc::new(flat);
        if let Ok(mut cache) = self.catalogues.write() {
            cache.insert(locale.to_owned(), held.clone());
        }
        held
    }
}

#[async_trait]
impl Framework for SaltcornUiFramework {
    fn name(&self) -> &str {
        SALTCORN_UI_FRAMEWORK
    }

    fn config_spec(&self) -> Vec<FormField> {
        saltcorn_ui_config_spec()
    }

    fn forget_catalogues(&self) {
        if let Ok(mut cache) = self.catalogues.write() {
            cache.clear();
        }
    }

    async fn handle(&self, req: AppRequest, cat: &Catalog) -> Result<AppResponse> {
        // The request's catalogue, read once here so the six places that build
        // a `ViewRequest` can pick it up synchronously (§16.1, task 4.5).
        let _ = self.catalogue(cat, req.locale.as_str()).await;
        let path = req.path.clone();
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if let ["auth", action] = segments.as_slice() {
            return self.auth(&req, cat, action).await;
        }
        // A caller the application does not let in (TODO F9) reaches the sign-in
        // forms above and the assets they are drawn with, and nothing else:
        // a page, a view, a post and a store file all go to the sign-in.
        if !req.admitted
            && !matches!(
                segments.as_slice(),
                ["static_assets", ..] | ["plugins", "public", ..]
            )
        {
            return Ok(self.not_admitted(&req));
        }
        match req.method {
            Method::Get => {}
            // What v1's `routes/view.ts` and `routes/delete.ts` answer. The CSRF
            // check has already run, in front of every application (6.4).
            Method::Post => {
                return match segments.as_slice() {
                    ["view", name, rest @ ..] if !name.is_empty() => {
                        self.post_view(&req, cat, &percent_decode(name), rest).await
                    }
                    ["page", name, "action", rndid] if !name.is_empty() && !rndid.is_empty() => {
                        self.page_action(&req, cat, &percent_decode(name), &percent_decode(rndid))
                            .await
                    }
                    ["delete", table, id] if !table.is_empty() && !id.is_empty() => {
                        self.delete(&req, cat, &percent_decode(table), &percent_decode(id))
                            .await
                    }
                    _ => Ok(self.message(
                        &req,
                        404,
                        &t!(req.locale, "Not found"),
                        &t!(req.locale, "There is nothing to post to at this address."),
                    )),
                };
            }
            _ => return Ok(AppResponse::method_not_allowed()),
        }
        match segments.as_slice() {
            [""] => self.index(&req, cat).await,
            ["view", name, slug @ ..] if !name.is_empty() => {
                self.view(&req, cat, &percent_decode(name), slug).await
            }
            ["page", name] if !name.is_empty() => self.page(&req, cat, &percent_decode(name)).await,
            ["static_assets", _tag, rest @ ..] if !rest.is_empty() => self.static_asset(rest),
            ["files", "serve", rest @ ..] if !rest.is_empty() => self.file(&req, cat, rest).await,
            ["plugins", "public", plugin, rest @ ..] if !rest.is_empty() => {
                Ok(plugin_asset(&percent_decode(plugin), rest))
            }
            _ => Ok(self.message(
                &req,
                404,
                &t!(req.locale, "Not found"),
                &t!(req.locale, "There is nothing at this address."),
            )),
        }
    }

    fn build(&self) -> Option<BuildSpec> {
        None
    }
}

impl SaltcornUiFramework {
    /// `/`: the viewer's home page, or a document naming the pages and views
    /// there are (DoD step 5).
    async fn index(&self, req: &AppRequest, cat: &Catalog) -> Result<AppResponse> {
        let set = view_sets().get(cat, self.app.id).await?;
        let role = role_of(req);
        if let Some(page) = self.home_page(cat, &set, role).await? {
            return self.render_page(req, cat, &set, page).await;
        }
        let site = escape(&self.site_name());
        let links = |kind: &str, names: Vec<&str>| -> String {
            names
                .iter()
                .map(|name| {
                    format!(
                        "<li><a href=\"/{kind}/{}\">{}</a></li>",
                        escape(&percent_encode(name)),
                        escape(name)
                    )
                })
                .collect::<String>()
        };
        let body = if set.pages.is_empty() && set.views.is_empty() {
            "<h1>Nothing here yet</h1><p>This application has no pages and no views.</p>".to_owned()
        } else {
            let mut body =
                format!("<h1>{site}</h1><p>There is no home page here for your role.</p>");
            if !set.pages.is_empty() {
                body.push_str("<h2>Pages</h2><ul>");
                body.push_str(&links(
                    "page",
                    set.pages.iter().map(|p| p.name.as_str()).collect(),
                ));
                body.push_str("</ul>");
            }
            if !set.views.is_empty() {
                body.push_str("<h2>Views</h2><ul>");
                body.push_str(&links(
                    "view",
                    set.views.iter().map(|v| v.name.as_str()).collect(),
                ));
                body.push_str("</ul>");
            }
            body
        };
        Ok(AppResponse::html(
            200,
            self.document(
                req,
                &self.site_name(),
                &format!("<main class=\"container py-4\">{body}</main>"),
                &[],
            ),
        ))
    }

    /// The page `/` is for `role`: the `root_pages` setting, then a page whose
    /// `root_page_for_roles` names the role by id or by name.
    async fn home_page<'s>(
        &self,
        cat: &Catalog,
        set: &'s ViewSet,
        role: u8,
    ) -> Result<Option<&'s Page>> {
        let configured = self
            .app
            .framework
            .config
            .get(CFG_ROOT_PAGES)
            .and_then(|m| m.get(role.to_string()))
            .and_then(Json::as_str);
        if let Some(name) = configured
            && let Some(page) = set.page(name)
        {
            return Ok(Some(page));
        }
        let claims = |p: &&Page| {
            p.attributes
                .get("root_page_for_roles")
                .and_then(Json::as_array)
                .is_some_and(|roles| !roles.is_empty())
        };
        if !set.pages.iter().any(|p| claims(&p)) {
            return Ok(None);
        }
        let role_name = sc_auth::list_roles(cat)
            .await?
            .into_iter()
            .find(|r| r.role == role)
            .map(|r| r.name);
        Ok(set.pages.iter().filter(claims).find(|p| {
            p.attributes["root_page_for_roles"]
                .as_array()
                .is_some_and(|roles| {
                    roles.iter().any(|r| {
                        r.as_u64() == Some(u64::from(role))
                            || r.as_str().is_some_and(|s| {
                                s == role.to_string() || Some(s) == role_name.as_deref()
                            })
                    })
                })
        }))
    }

    /// `/view/:name` and `/view/:name/*slug`: v1's view GET. An ajax request —
    /// the Filter's reload of the list under it — gets the view's HTML alone;
    /// a navigation gets it in the layout.
    async fn view(
        &self,
        req: &AppRequest,
        cat: &Catalog,
        name: &str,
        slug: &[&str],
    ) -> Result<AppResponse> {
        let set = view_sets().get(cat, self.app.id).await?;
        let Some(view) = set.view(name) else {
            return Ok(self.no_view(req, name));
        };
        if let Some(refused) = self.refused(
            req,
            &t!(req.locale, "the view {name}", name = name),
            view.min_role,
        ) {
            return Ok(refused);
        }
        let state = state_of(req, view, slug);
        let request = self.view_request(req, self.wrap_for(req, view));
        let snapshot = self.snapshot(cat, &set, &req.base_url).await?;
        let schema = schema_snapshot(cat)?;
        let hosts = self.viewer_hosts(cat, req);
        let ctx = ViewContext {
            snapshot: &snapshot,
            request: &request,
            hosts: hosts.surfaces(),
            schema: Some(&schema),
        };
        let out = self.runtime.render(view, &state, ctx).await;
        self.respond(req, out, &request)
    }

    /// `POST /view/:name/…`: one of the pattern's routes when the one segment
    /// after the name is a route the pattern declares, and otherwise the
    /// pattern's `runPost` with the rest as the slug.
    ///
    /// v1 decides by the number of segments alone (`/:viewname/:route` is
    /// matched first), which makes a one-part slug unpostable; asking the
    /// pattern's manifest keeps both.
    async fn post_view(
        &self,
        req: &AppRequest,
        cat: &Catalog,
        name: &str,
        rest: &[&str],
    ) -> Result<AppResponse> {
        let set = view_sets().get(cat, self.app.id).await?;
        let Some(view) = set.view(name) else {
            return Ok(self.no_view(req, name));
        };
        if let Some(refused) = self.refused(
            req,
            &t!(req.locale, "the view {name}", name = name),
            view.min_role,
        ) {
            return Ok(refused);
        }
        if let [route] = rest {
            let route = percent_decode(route);
            let declared = self
                .runtime
                .patterns()
                .await?
                .into_iter()
                .find(|p| p.name == view.viewpattern)
                .is_some_and(|p| p.routes.contains(&route));
            if declared {
                return self.route(req, cat, &set, view, &route).await;
            }
        }

        // v1's `rewrite_query_from_slug`: the slug's parts join the query, which
        // is the state a POST is run over.
        let mut request = self.view_request(req, self.wrap_for(req, view));
        if let Json::Object(state) = state_of(req, view, rest) {
            request.query = state
                .into_iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k, v.to_owned())))
                .collect();
        }
        let snapshot = self.snapshot(cat, &set, &req.base_url).await?;
        let schema = schema_snapshot(cat)?;
        let hosts = self.viewer_hosts(cat, req);
        let body = request.body.clone();
        let ctx = ViewContext {
            snapshot: &snapshot,
            request: &request,
            hosts: hosts.surfaces(),
            schema: Some(&schema),
        };
        let out = self.runtime.post(view, &body, ctx).await;
        self.respond(req, out, &request)
    }

    /// `POST /view/:name/:route`: v1's `runRoute`, which answers JSON — so a
    /// route that fails answers `{ "error": … }` too, which is what
    /// `saltcorn.js` shows the viewer, rather than a document it cannot.
    async fn route(
        &self,
        req: &AppRequest,
        cat: &Catalog,
        set: &ViewSet,
        view: &View,
        route: &str,
    ) -> Result<AppResponse> {
        let request = self.view_request(req, None);
        let snapshot = self.snapshot(cat, set, &req.base_url).await?;
        let schema = schema_snapshot(cat)?;
        let hosts = self.viewer_hosts(cat, req);
        let ctx = ViewContext {
            snapshot: &snapshot,
            request: &request,
            hosts: hosts.surfaces(),
            schema: Some(&schema),
        };
        match self.runtime.route(view, route, &request.body, ctx).await {
            Err(e) if e.kind() == ErrorKind::Application => {
                eprintln!("feldspar: application `{}`: {e}", self.app.subdomain);
                json_response(500, &serde_json::json!({ "error": reason_of(&e) }))
            }
            out => self.respond(req, out, &request),
        }
    }

    /// `POST /page/:name/action/:rndid` (The builder, 3.1): v1's
    /// `routes/page.ts`, which an action button on a page posts to through
    /// `saltcorn.js`'s `page_post_action`.
    ///
    /// The rules are a view route's: the page's `min_role` first, with the
    /// refusal rendering gives; the CSRF check has run in front of every
    /// application; and the action runs in the worker under the viewer's
    /// authority. v1 answers a page that does not exist with its "Action not
    /// found", and so does this.
    async fn page_action(
        &self,
        req: &AppRequest,
        cat: &Catalog,
        name: &str,
        rndid: &str,
    ) -> Result<AppResponse> {
        let set = view_sets().get(cat, self.app.id).await?;
        let Some(page) = set.page(name) else {
            return json_response(404, &serde_json::json!({ "error": "Action not found" }));
        };
        if let Some(refused) = self.refused(
            req,
            &t!(req.locale, "the page {name}", name = name),
            page.min_role,
        ) {
            return Ok(refused);
        }
        let request = self.view_request(req, None);
        let snapshot = self.snapshot(cat, &set, &req.base_url).await?;
        let schema = schema_snapshot(cat)?;
        let hosts = self.viewer_hosts(cat, req);
        let ctx = ViewContext {
            snapshot: &snapshot,
            request: &request,
            hosts: hosts.surfaces(),
            schema: Some(&schema),
        };
        match self.runtime.page_action(page, rndid, ctx).await {
            Err(e) if e.kind() == ErrorKind::Application => {
                eprintln!("feldspar: application `{}`: {e}", self.app.subdomain);
                json_response(500, &serde_json::json!({ "error": reason_of(&e) }))
            }
            out => self.respond(req, out, &request),
        }
    }

    /// `POST /delete/:table/:id`: v1's Delete action (§12.1), which a List's
    /// action column and an Edit's Delete button post to.
    ///
    /// The row is deleted **as the viewer** — `delete_row_as`, the function the
    /// agent tools and the GraphQL provider delete through — so the table's
    /// write role and its ownership formula decide, not the view (§11). A table
    /// outside the application's subset is not one it can delete from.
    ///
    /// An ajax post (what `ajax_post_btn` sends) answers `{ success, error? }`,
    /// as v1's does; a form post is redirected to the `redirect` it names, if
    /// that is a path on this application, else to `/`.
    async fn delete(
        &self,
        req: &AppRequest,
        cat: &Catalog,
        table_name: &str,
        id: &str,
    ) -> Result<AppResponse> {
        let outcome = self.delete_row(req, cat, table_name, id).await?;
        if is_xhr(req) {
            let mut answer = serde_json::json!({ "success": outcome.is_ok() });
            if let Err(error) = &outcome {
                answer["error"] = Json::String(error.clone());
            }
            return json_response(200, &answer);
        }
        if let Err(error) = &outcome {
            eprintln!(
                "feldspar: application `{}`: deleting {id} from `{table_name}`: {error}",
                self.app.subdomain
            );
        }
        Ok(AppResponse::redirect(safe_redirect(
            req.query.get("redirect").map(String::as_str),
        )))
    }

    /// The deletion itself: `Ok(Err(sentence))` for what the viewer is told —
    /// no such table here, not permitted, no such row — and `Err` for what is
    /// the server's.
    async fn delete_row(
        &self,
        req: &AppRequest,
        cat: &Catalog,
        table_name: &str,
        id: &str,
    ) -> Result<std::result::Result<(), String>> {
        let no_table = || {
            Ok(Err(format!(
                "application `{}` has no table named `{table_name}`",
                self.app.name
            )))
        };
        if !self.app.tables.iter().any(|t| t.0 == table_name) {
            return no_table();
        }
        let Some(table) = cat.get(table_name)? else {
            return no_table();
        };
        let deleted = sc_api::delete_row_as(
            cat,
            &table,
            id,
            role_of(req),
            req.user.as_ref(),
            self.evaluator.as_ref(),
            &[],
            &sc_api::rows::Executor::Pooled,
        )
        .await;
        match deleted {
            Ok(_) => Ok(Ok(())),
            Err(e) if e.kind() == ErrorKind::Application => Ok(Err(reason_of(&e))),
            Err(e) => Err(e),
        }
    }

    /// `/auth/login`, `/auth/logout` and `/auth/signup` (7.3): v1's three auth
    /// routes, answered by the application itself.
    ///
    /// A successful sign-in is a [`SessionAction::Start`] on the response, which
    /// the router applies with the code an API provider's login goes through —
    /// the same cookie, the same replacement of whatever session was there, the
    /// same login event. The POSTs have passed the CSRF check already, so a
    /// form on another site cannot sign a browser in as somebody else.
    async fn auth(&self, req: &AppRequest, cat: &Catalog, action: &str) -> Result<AppResponse> {
        let dest = field_of(req, "dest");
        match (action, &req.method) {
            ("login", Method::Get) => Ok(self.auth_form(req, AuthForm::Login, 200, "", None)),
            ("login", Method::Post) => self.login(req, cat, &dest).await,
            // v1's navbar links to it, so a GET signs out as a POST does.
            ("logout", Method::Get | Method::Post) => {
                let mut out = AppResponse::redirect("/");
                out.session = SessionAction::End;
                Ok(out)
            }
            ("signup", _) if !self.signup_allowed() => Ok(self.message(
                req,
                404,
                &t!(req.locale, "Not found"),
                &t!(req.locale, "This application does not offer sign-up."),
            )),
            ("signup", Method::Get) => Ok(self.auth_form(req, AuthForm::Signup, 200, "", None)),
            ("signup", Method::Post) => self.signup(req, cat, &dest).await,
            ("login" | "logout" | "signup", _) => Ok(AppResponse::method_not_allowed()),
            _ => Ok(self.message(
                req,
                404,
                &t!(req.locale, "Not found"),
                &t!(req.locale, "There is nothing at this address."),
            )),
        }
    }

    /// `POST /auth/login`: the credentials through `sc_auth::authenticate` — the
    /// check the admin login and an application's REST login make, for any role
    /// — and back to where the viewer was going.
    ///
    /// A wrong password, an unknown email and a disabled account are one answer,
    /// as they are everywhere else: telling them apart would tell a stranger
    /// which addresses have accounts.
    ///
    /// Throttled exactly as the admin and REST logins are, under the same keys
    /// ([`sc_api::throttle`]): the users table is the instance's, so a guess
    /// through an application's form counts against the same account and the
    /// same address as one through the admin's (security review 2026-10 #7).
    async fn login(&self, req: &AppRequest, cat: &Catalog, dest: &str) -> Result<AppResponse> {
        let email = field_of(req, "email");
        let password = field_of(req, "password");
        if let Err(limited) = sc_api::throttle::login_attempt(req.client_ip, &email) {
            let mut out = self.auth_form(
                req,
                AuthForm::Login,
                429,
                email.trim(),
                Some(&t!(
                    req.locale,
                    "Too many sign-in attempts. Try again in a few minutes."
                )),
            );
            out.headers.push((
                "retry-after".to_owned(),
                limited.retry_after_secs().to_string(),
            ));
            return Ok(out);
        }
        match sc_auth::authenticate(cat, &email, &password).await? {
            Some(user) => Ok(signed_in(user, dest)),
            None => {
                sc_api::throttle::login_failed(&email);
                Ok(self.auth_form(
                    req,
                    AuthForm::Login,
                    401,
                    email.trim(),
                    Some(&t!(req.locale, "Incorrect email or password.")),
                ))
            }
        }
    }

    /// `POST /auth/signup`: an account under the password rule every other way of
    /// making one has (not blank), with the role the settings give a new
    /// account, and signed in.
    async fn signup(&self, req: &AppRequest, cat: &Catalog, dest: &str) -> Result<AppResponse> {
        let email = field_of(req, "email").trim().to_owned();
        let password = field_of(req, "password");
        let again =
            |problem: &str| Ok(self.auth_form(req, AuthForm::Signup, 400, &email, Some(problem)));
        if email.is_empty() {
            return again(&t!(req.locale, "An email address is required."));
        }
        if password.is_empty() {
            return again(&t!(req.locale, "A password is required."));
        }
        if has_field(req, "passwordRepeat") && field_of(req, "passwordRepeat") != password {
            return again(&t!(req.locale, "The two passwords are not the same."));
        }
        if sc_auth::load_user_by_email(cat, &email).await?.is_some() {
            return again(&t!(
                req.locale,
                "There is already an account with this email address."
            ));
        }
        match sc_auth::create_user(cat, &email, &password, self.new_user_role()).await {
            Ok(user) => Ok(signed_in(user, dest)),
            // "role 80 does not exist" is the admin's to fix, and the one
            // sentence that says how.
            Err(e) if e.kind() == ErrorKind::Application => again(&reason_of(&e)),
            Err(e) => Err(e),
        }
    }

    fn signup_allowed(&self) -> bool {
        self.app
            .framework
            .config
            .get(CFG_ALLOW_SIGNUP)
            .and_then(Json::as_bool)
            .unwrap_or(false)
    }

    /// The role a new account gets. The settings are checked on save to be a
    /// role from 2 to 100 ([`check_saltcorn_ui_config`]).
    fn new_user_role(&self) -> u8 {
        self.app
            .framework
            .config
            .get(CFG_NEW_USER_ROLE)
            .and_then(Json::as_u64)
            .and_then(|r| u8::try_from(r).ok())
            .filter(|r| (ROLE_ADMIN + 1..=ROLE_PUBLIC).contains(r))
            .unwrap_or(DEFAULT_NEW_USER_ROLE)
    }

    /// The sign-in or sign-up form, with what was wrong with the last attempt.
    ///
    /// Saltcorn UI's own, in the document's Bootstrap — v1's is a `Form` its
    /// auth routes build, and those routes are v1's server rather than the
    /// view patterns this bundle vendors. `dest` rides along as a hidden field,
    /// so the POST knows where the viewer was going.
    fn auth_form(
        &self,
        req: &AppRequest,
        kind: AuthForm,
        status: u16,
        email: &str,
        problem: Option<&str>,
    ) -> AppResponse {
        let dest = field_of(req, "dest");
        let back = if dest.is_empty() {
            String::new()
        } else {
            format!("?dest={}", percent_encode(&dest))
        };
        let (heading, action, button, password_autocomplete) = match kind {
            AuthForm::Login => (
                t!(req.locale, "Sign in"),
                "/auth/login",
                t!(req.locale, "Sign in"),
                "current-password",
            ),
            AuthForm::Signup => (
                t!(req.locale, "Create an account"),
                "/auth/signup",
                t!(req.locale, "Sign up"),
                "new-password",
            ),
        };
        let alert = problem
            .map(|p| {
                format!(
                    "<div class=\"alert alert-danger\" role=\"alert\">{}</div>",
                    escape(p)
                )
            })
            .unwrap_or_default();
        let repeat = match kind {
            AuthForm::Login => String::new(),
            AuthForm::Signup => format!(
                "<div class=\"mb-3\"><label class=\"form-label\" for=\"passwordRepeat\">{}\
                 </label><input class=\"form-control\" type=\"password\" id=\"passwordRepeat\" \
                 name=\"passwordRepeat\" autocomplete=\"new-password\" required></div>",
                escape(&t!(req.locale, "Password again"))
            ),
        };
        let other = match kind {
            // The link is *inside* the sentence, so the sentence is the message
            // and the anchor is two of its arguments: a translator who has to
            // move "Sign up" to the front of the clause can, which is the whole
            // reason a message is not three concatenated fragments.
            AuthForm::Login if self.signup_allowed() => format!(
                "<p class=\"mt-3\">{}</p>",
                t!(
                    req.locale,
                    "No account yet? {link_start}Sign up{link_end}",
                    link_start = format!("<a href=\"/auth/signup{}\">", escape(&back)),
                    link_end = "</a>"
                )
            ),
            AuthForm::Login => String::new(),
            AuthForm::Signup => format!(
                "<p class=\"mt-3\">{}</p>",
                t!(
                    req.locale,
                    "Already have an account? {link_start}Sign in{link_end}",
                    link_start = format!("<a href=\"/auth/login{}\">", escape(&back)),
                    link_end = "</a>"
                )
            ),
        };
        let body = format!(
            "<main class=\"container py-5\"><div class=\"row justify-content-center\">\
             <div class=\"col-sm-10 col-md-6 col-lg-4\">\
             <h1 class=\"h3 mb-3\">{heading}</h1>\
             <p class=\"text-muted\">{site}</p>{alert}\
             <form action=\"{action}\" method=\"post\">\
             <input type=\"hidden\" name=\"_csrf\" value=\"{csrf}\">\
             <input type=\"hidden\" name=\"dest\" value=\"{dest}\">\
             <div class=\"mb-3\"><label class=\"form-label\" for=\"email\">{email_label}</label>\
             <input class=\"form-control\" type=\"email\" id=\"email\" name=\"email\" \
             value=\"{email}\" autocomplete=\"username\" required autofocus></div>\
             <div class=\"mb-3\"><label class=\"form-label\" \
             for=\"password\">{password_label}</label>\
             <input class=\"form-control\" type=\"password\" id=\"password\" name=\"password\" \
             autocomplete=\"{password_autocomplete}\" required></div>\
             {repeat}\
             <button class=\"btn btn-primary w-100\" type=\"submit\">{button}</button>\
             </form>{other}</div></div></main>",
            site = escape(&self.site_name()),
            csrf = escape(&req.csrf_token),
            dest = escape(&dest),
            email = escape(email),
            email_label = escape(&t!(req.locale, "Email")),
            password_label = escape(&t!(req.locale, "Password")),
        );
        AppResponse::html(status, self.document(req, &heading, &body, &[]))
    }

    /// `/page/:name`.
    async fn page(&self, req: &AppRequest, cat: &Catalog, name: &str) -> Result<AppResponse> {
        let set = view_sets().get(cat, self.app.id).await?;
        let Some(page) = set.page(name) else {
            return Ok(self.message(
                req,
                404,
                &t!(req.locale, "Not found"),
                &t!(
                    req.locale,
                    "This application has no page named {name}.",
                    name = name
                ),
            ));
        };
        self.render_page(req, cat, &set, page).await
    }

    async fn render_page(
        &self,
        req: &AppRequest,
        cat: &Catalog,
        set: &ViewSet,
        page: &Page,
    ) -> Result<AppResponse> {
        if let Some(refused) = self.refused(
            req,
            &t!(req.locale, "the page {name}", name = &page.name),
            page.min_role,
        ) {
            return Ok(refused);
        }
        let title = if page.title.trim().is_empty() {
            page.name.clone()
        } else {
            page.title.clone()
        };
        // v1's page route passes both attributes to `sendWrap` (3.2).
        let flag = |key: &str| {
            page.attributes
                .get(key)
                .and_then(Json::as_bool)
                .unwrap_or(false)
        };
        let request = self.view_request(
            req,
            (!is_xhr(req)).then(|| Wrap {
                title,
                current_url: req.path.clone(),
                no_menu: flag("no_menu"),
                fluid: flag("request_fluid_layout"),
            }),
        );
        let snapshot = self.snapshot(cat, set, &req.base_url).await?;
        let schema = schema_snapshot(cat)?;
        let hosts = self.viewer_hosts(cat, req);
        let ctx = ViewContext {
            snapshot: &snapshot,
            request: &request,
            hosts: hosts.surfaces(),
            schema: Some(&schema),
        };
        let out = self.runtime.render_page(page, ctx).await;
        self.respond(req, out, &request)
    }

    /// `/static_assets/:tag/*`: a file of the bundle's `public/`, and nothing
    /// outside it.
    fn static_asset(&self, rest: &[&str]) -> Result<AppResponse> {
        let parts: Vec<String> = rest.iter().map(|p| percent_decode(p)).collect();
        if !confined(&parts) {
            return Ok(AppResponse::not_found());
        }
        let file = parts
            .iter()
            .fold(self.public_dir.clone(), |dir, p| dir.join(p));
        match std::fs::read(&file) {
            Ok(bytes) => Ok(AppResponse::ok(
                sc_app::asset_content_type(&parts.join("/")),
                Bytes::from(bytes),
            )
            .header("Cache-Control", "public, max-age=31536000, immutable")),
            Err(_) => Ok(AppResponse::not_found()),
        }
    }

    /// `/files/serve/*`: [`serve_store_file`] for this application.
    async fn file(&self, req: &AppRequest, cat: &Catalog, rest: &[&str]) -> Result<AppResponse> {
        serve_store_file(&self.app, cat, req.user.as_ref(), rest).await
    }

    /// v1's `req`, for this request.
    fn view_request(&self, req: &AppRequest, wrap: Option<Wrap>) -> ViewRequest {
        let locale = req.locale.as_str().to_owned();
        // Already in the cache: `handle` reads it once, before dispatching, so
        // every call site below gets the catalogue without being async.
        let messages = self
            .catalogues
            .read()
            .ok()
            .and_then(|held| held.get(&locale).cloned())
            .map(|held| held.as_ref().clone())
            .unwrap_or_default();
        ViewRequest {
            method: req.method.as_str().to_owned(),
            path: req.path.clone(),
            query: req.query.clone(),
            body: body_json(&req.body),
            headers: req.headers.clone(),
            user: req.user.as_ref().map(view_user),
            base_url: req.base_url.clone(),
            csrf_token: req.csrf_token.clone(),
            wrap,
            locale: Some(locale),
            messages,
        }
    }

    /// How a view is wrapped for `req`: in the layout under its title, unless
    /// the request is an ajax one, which gets the HTML alone.
    fn wrap_for(&self, req: &AppRequest, view: &View) -> Option<Wrap> {
        let title = view
            .attributes
            .get("page_title")
            .and_then(Json::as_str)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(&view.name)
            .to_owned();
        (!is_xhr(req)).then(|| Wrap {
            title,
            current_url: req.path.clone(),
            ..Wrap::default()
        })
    }

    /// The surfaces a call for `req` reaches, on the viewer's terms.
    fn viewer_hosts<'a>(&'a self, cat: &'a Catalog, req: &AppRequest) -> ViewerHosts<'a> {
        ViewerHosts::new(
            cat,
            req.user.as_ref(),
            self.evaluator.clone(),
            self.triggers.as_deref(),
            &self.app.triggers,
        )
    }

    fn no_view(&self, req: &AppRequest, name: &str) -> AppResponse {
        self.message(
            req,
            404,
            &t!(req.locale, "Not found"),
            &t!(
                req.locale,
                "This application has no view named {name}.",
                name = name
            ),
        )
    }

    /// The snapshot of `set`, built once per generation.
    ///
    /// Keyed on the generation alone, as the worker's copy is: the base URL in
    /// it is the one the generation was first rendered at, and a request's own
    /// base URL reaches `req.get_base_url()` directly.
    async fn snapshot(
        &self,
        cat: &Catalog,
        set: &ViewSet,
        base_url: &str,
    ) -> Result<Arc<ViewSnapshot>> {
        if let Some(held) = self.snapshot.read().map_err(|_| poisoned())?.as_ref()
            && held.generation() == set.generation
        {
            return Ok(held.clone());
        }
        let built = Arc::new(
            application_snapshot(cat, &self.app, set, base_url, self.triggers.as_deref()).await?,
        );
        *self.snapshot.write().map_err(|_| poisoned())? = Some(built.clone());
        Ok(built)
    }

    /// What a render produced, as a response.
    fn respond(
        &self,
        req: &AppRequest,
        out: Result<ViewOutput>,
        request: &ViewRequest,
    ) -> Result<AppResponse> {
        let out = match out {
            Ok(out) => out,
            // A pattern that threw is the application's to fix: a document
            // saying which view and why, not a stack. A worker that could not be
            // reached is the server's, and goes up.
            Err(e) if e.kind() == ErrorKind::Application => {
                eprintln!("feldspar: application `{}`: {e}", self.app.subdomain);
                return Ok(self.message(req, 500, "This could not be shown", &reason_of(&e)));
            }
            Err(e) => return Err(e),
        };
        let mut response = if let Some(location) = out.redirect {
            let status = out.status.filter(|s| (300..400).contains(s)).unwrap_or(302);
            AppResponse::with_status(status, "text/plain; charset=utf-8", Bytes::new())
                .header("Location", location)
        } else if let Some(json) = out.json {
            AppResponse::with_status(
                out.status.unwrap_or(200),
                "application/json",
                serde_json::to_vec(&json).map_err(|e| Error::serde(e.to_string()))?,
            )
        } else {
            let html = match out.sent.unwrap_or(out.body) {
                Json::String(html) => html,
                Json::Null => String::new(),
                other => other.to_string(),
            };
            let status = out.status.unwrap_or(200);
            match &request.wrap {
                Some(wrap) => AppResponse::html(
                    status,
                    self.document(req, &wrap.title, &html, &out.patterns),
                ),
                None => AppResponse::html(status, html),
            }
        };
        for (name, value) in out.headers {
            response = response.header(name, value);
        }
        Ok(response)
    }

    /// `Some(refusal)` when the viewer's role is below `min_role` (§11's first
    /// check, 7.1) — and then the view is not run at all.
    ///
    /// Nobody, navigating, is sent to sign in with the way back; nobody asking
    /// by ajax or posting gets the 401 (a redirect to a form is no answer to
    /// either). Somebody signed in is told their role may not, naming what.
    fn refused(&self, req: &AppRequest, what: &str, min_role: u8) -> Option<AppResponse> {
        if role_of(req) <= min_role {
            return None;
        }
        Some(match req.user {
            None if matches!(req.method, Method::Get) && !is_xhr(req) => {
                AppResponse::redirect(login_location(req))
            }
            None => self.message(
                req,
                401,
                &t!(req.locale, "Please sign in"),
                &t!(
                    req.locale,
                    "You need to sign in to see {what}.",
                    what = what
                ),
            ),
            Some(_) => self.message(
                req,
                403,
                &t!(req.locale, "Not permitted"),
                &t!(req.locale, "Your role may not see {what}.", what = what),
            ),
        })
    }

    /// The answer to a caller the application does not let in at all: sent to
    /// sign in (as somebody who may use it) when navigating, otherwise a 401.
    fn not_admitted(&self, req: &AppRequest) -> AppResponse {
        if matches!(req.method, Method::Get) && !is_xhr(req) {
            return AppResponse::redirect(login_location(req));
        }
        self.message(
            req,
            401,
            &t!(req.locale, "Please sign in"),
            &t!(
                req.locale,
                "You need to sign in with an account that may use this application."
            ),
        )
    }

    /// A short document: a heading and a sentence.
    fn message(&self, req: &AppRequest, status: u16, heading: &str, sentence: &str) -> AppResponse {
        AppResponse::html(
            status,
            self.document(
                req,
                heading,
                &format!(
                    "<main class=\"container py-4\"><h1>{}</h1><p>{}</p></main>",
                    escape(heading),
                    escape(sentence)
                ),
                &[],
            ),
        )
    }

    fn site_name(&self) -> String {
        self.app
            .framework
            .config
            .get(CFG_SITE_NAME)
            .and_then(Json::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.app.name)
            .to_owned()
    }

    /// The document around a body (§9): Saltcorn UI's own head — v1's
    /// `wrapper.js`, ported — with the assets a v1 view needs to *work*.
    ///
    /// v1's `_sc_globalCsrf` is the viewer's CSRF token, which every ajax post
    /// `saltcorn.js` makes sends back as `CSRF-Token` (6.4).
    ///
    /// `patterns` are the view patterns the body rendered: the installed
    /// plugins' headers for them follow v1's own scripts, as v1's `headers` do
    /// (11.3).
    fn document(&self, req: &AppRequest, title: &str, body: &str, patterns: &[String]) -> String {
        let asset = |file: &str| format!("/static_assets/{ASSET_VERSION_TAG}/{file}");
        format!(
            "<!doctype html>\n\
             <html lang=\"en\" data-bs-theme=\"light\">\n\
             <head>\n\
             <meta charset=\"utf-8\">\n\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1, shrink-to-fit=no\">\n\
             <link rel=\"stylesheet\" href=\"{bootstrap_css}\">\n\
             <link rel=\"stylesheet\" href=\"{fontawesome}\">\n\
             <link rel=\"stylesheet\" href=\"{saltcorn_css}\">\n\
             <script src=\"{jquery}\"></script>\n\
             <script src=\"{bootstrap_js}\"></script>\n\
             <script src=\"{common}\"></script>\n\
             <script src=\"{saltcorn}\"></script>\n\
             {plugins}\
             {v1_globals}\
             <title>{title}</title>\n\
             </head>\n\
             <body id=\"page-top\">\n\
             <div id=\"wrapper\">{body}</div>\n\
             </body>\n\
             </html>\n",
            bootstrap_css = asset("bootstrap.min.css"),
            fontawesome = asset("fontawesome-free/css/all.min.css"),
            saltcorn_css = asset("saltcorn.css"),
            jquery = asset("jquery-3.6.0.min.js"),
            bootstrap_js = asset("bootstrap.bundle.min.js"),
            common = asset("saltcorn-common.js"),
            saltcorn = asset("saltcorn.js"),
            plugins = plugin_header_tags(&installed_plugin_assets(), patterns),
            title = escape(title),
            v1_globals = v1_page_globals(&req.csrf_token),
        )
    }
}

/// The page globals v1's `saltcorn.js` and `saltcorn-common.js` read, written
/// as v1's `wrapper.js` writes them. `_sc_` is v1's own browser namespace, not
/// this server's metadata prefix, and each line says so.
fn v1_page_globals(csrf_token: &str) -> String {
    // A token is hex; anything else in it is not put into a script.
    let csrf: String = csrf_token
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    let v1_globals = [
        format!("var _sc_globalCsrf = \"{csrf}\";"), // v1's CSRF token
        format!("var _sc_version_tag = \"{ASSET_VERSION_TAG}\";"), // v1's asset tag
        "var _sc_pageloadtag = \"\";".to_owned(),    // v1
        "var _sc_loglevel = 1;".to_owned(),          // v1
        "var _sc_lightmode = \"light\";".to_owned(), // v1
    ];
    format!("<script>{}</script>\n", v1_globals.join(" "))
}

/// The surfaces one render reaches, on the viewer's terms — built the way
/// `sc_core_actions::code_body::Hosts` builds a code body's from its event, with
/// the request's user as the caller. No `fetch`: the view runtime is granted no
/// network.
struct ViewerHosts<'a> {
    table: TableHost<'a>,
    files: FileStoreHost<'a>,
    triggers: Option<AppTriggers<'a>>,
    module_fns: Option<Arc<dyn ModuleFnHost>>,
}

impl<'a> ViewerHosts<'a> {
    fn new(
        cat: &'a Catalog,
        user: Option<&User>,
        evaluator: Option<Arc<dyn JsEvaluator>>,
        triggers: Option<&'a TriggerDispatcher>,
        declared: &'a [TriggerRef],
    ) -> ViewerHosts<'a> {
        let caller = sc_api::caller_context(user);
        ViewerHosts {
            // The viewer's authority is the ceiling (7.2): a read a pattern
            // leaves unmarked is the viewer's, not the admin's it would be in a
            // code body.
            table: TableHost::new(cat)
                .caused_by(caller.role, caller.user.clone())
                .with_evaluator(evaluator)
                .viewer_only(),
            files: FileStoreHost::new(cat).caused_by(caller.role, caller.user.as_ref()),
            triggers: triggers.map(|d| AppTriggers {
                inner: TriggerRunHost::new(d, cat).caused_by(caller.role, caller.user.clone()),
                declared,
            }),
            module_fns: cat.module_functions(),
        }
    }

    fn surfaces(&'a self) -> CodeHosts<'a> {
        CodeHosts {
            host: Some(&self.table),
            fetch: None,
            files: Some(&self.files),
            triggers: self.triggers.as_ref().map(|t| t as &dyn TriggerHost),
            module_fns: self.module_fns.as_deref(),
            // A view's code renders a page; it publishes nothing.
            live: None,
        }
    }
}

/// The trigger surface of a view call, bounded by the triggers the application
/// declares (§12.2).
///
/// `Trigger.findOne` in the worker already finds only those, so this is the
/// same bound where it cannot be talked around: a pattern — or a plugin's —
/// that asks for a trigger by name directly gets the sentence, not the run.
struct AppTriggers<'a> {
    inner: TriggerRunHost<'a>,
    declared: &'a [TriggerRef],
}

#[async_trait]
impl TriggerHost for AppTriggers<'_> {
    async fn run(&self, request: Json) -> Result<Json> {
        let name = request
            .get("trigger")
            .and_then(Json::as_str)
            .unwrap_or_default();
        if !self.declared.iter().any(|t| t.0 == name) {
            let declared: Vec<&str> = self.declared.iter().map(|t| t.0.as_str()).collect();
            return Err(Error::config(format!(
                "the trigger `{name}` is not one this application declares, so its views cannot \
                 run it; {}",
                if declared.is_empty() {
                    "it declares none".to_owned()
                } else {
                    format!("it declares {}", declared.join(", "))
                }
            )));
        }
        self.inner.run(request).await
    }

    fn trigger_names(&self) -> Vec<String> {
        let exist = self.inner.trigger_names();
        self.declared
            .iter()
            .filter(|t| exist.is_empty() || exist.contains(&t.0))
            .map(|t| t.0.clone())
            .collect()
    }
}

/// v1's `req.body`: a JSON body as it came, and a form's fields as an object —
/// a repeated field as the list of its values, which is what Express's
/// URL-encoded parser makes of `a=1&a=2`.
fn body_json(body: &RequestBody) -> Json {
    match body {
        RequestBody::Empty => Json::Null,
        RequestBody::Json(value) => value.clone(),
        RequestBody::Form(pairs) => {
            let mut out = Map::new();
            for (key, value) in pairs {
                let value = Json::String(value.clone());
                match out.get_mut(key) {
                    None => {
                        out.insert(key.clone(), value);
                    }
                    Some(Json::Array(values)) => values.push(value),
                    Some(first) => {
                        let first = first.take();
                        out.insert(key.clone(), Json::Array(vec![first, value]));
                    }
                }
            }
            Json::Object(out)
        }
    }
}

/// Where a form post that names `redirect` goes: the path, if it is one on this
/// application, else `/`. An absolute URL, a protocol-relative `//host` and a
/// backslash trick are all refused — v1's `safe_redirect` rule, because the
/// value is whatever was in the query.
///
/// What is kept is encoded as Express's `res.redirect` encodes a `Location`
/// (`encodeurl`): a space or a non-ASCII character becomes its `%XX`, and an
/// escape already there is left alone — the query decoded `%20` into the space
/// this puts back.
fn safe_redirect(redirect: Option<&str>) -> String {
    match redirect {
        Some(path)
            if path.starts_with('/')
                && !path.starts_with("//")
                && !path.contains('\\')
                && !path.chars().any(char::is_control) =>
        {
            let mut out = String::with_capacity(path.len());
            for byte in path.bytes() {
                if byte <= b' ' || byte >= 0x7f || b"\"<>^`{|}".contains(&byte) {
                    out.push_str(&format!("%{byte:02X}"));
                } else {
                    out.push(char::from(byte));
                }
            }
            out
        }
        _ => "/".to_owned(),
    }
}

/// A JSON response under `status`.
fn json_response(status: u16, value: &Json) -> Result<AppResponse> {
    Ok(AppResponse::with_status(
        status,
        "application/json",
        serde_json::to_vec(value).map_err(|e| Error::serde(e.to_string()))?,
    ))
}

/// An application error's own sentence, without the kind printed in front of
/// it.
fn reason_of(e: &Error) -> String {
    match e.repr() {
        Repr::Config(m) | Repr::Invalid(m) | Repr::NotFound(m) | Repr::Auth(m) => m.clone(),
        _ => e.to_string(),
    }
}

/// v1's `req.user`.
/// The view snapshot of `app`'s `set`, as every call through the view runtime is
/// given it (§4): its views, pages and settings, the server's roles, and what
/// each trigger the application declares runs — which v1's `run_action_column`
/// reads to decide how to run it (§12.2). Read at the generation, like the rest:
/// a change to an application's triggers is a save of the application, and a
/// save is a new generation.
///
/// One function for the framework and the configuration screen alike, because
/// the worker holds one snapshot per generation and it must be the same one
/// whichever of the two sent it first.
pub(crate) async fn application_snapshot(
    cat: &Catalog,
    app: &Application,
    set: &ViewSet,
    base_url: &str,
    triggers: Option<&TriggerDispatcher>,
) -> Result<ViewSnapshot> {
    let roles = sc_auth::list_roles(cat).await?;
    let actions: std::collections::HashMap<String, String> = triggers
        .and_then(|d| d.triggers().ok())
        .map(|triggers| {
            triggers
                .all()
                .iter()
                .map(|t| (t.name.clone(), t.action().unwrap_or("Workflow").to_owned()))
                .collect()
        })
        .unwrap_or_default();
    // A declared trigger this server does not have (a v1 trigger the restore
    // refused) is left out, so v1's `Trigger.findOne` finds nothing and
    // `run_action_column` says the action was not found, naming it. Left in, it
    // was found with no action kind and failed "Cannot read properties of
    // undefined (reading 'run')". Without a trigger set nothing is known to be
    // missing, and the declaration is sent as it is.
    let app = match triggers {
        Some(_) => {
            let mut present = app.clone();
            present.triggers.retain(|t| actions.contains_key(&t.0));
            std::borrow::Cow::Owned(present)
        }
        None => std::borrow::Cow::Borrowed(app),
    };
    ViewSnapshot::build_with_trigger_actions(&app, set, &roles, base_url, &|name| {
        actions.get(name).cloned()
    })
}

pub(crate) fn view_user(user: &User) -> ViewUser {
    ViewUser {
        id: user.id.to_string(),
        email: match user.extra.get("email") {
            Some(sc_query::Value::Text(email)) => email.clone(),
            _ => String::new(),
        },
        role_id: user.role,
    }
}

/// Which of the two auth forms.
#[derive(Clone, Copy)]
enum AuthForm {
    Login,
    Signup,
}

/// Signed in as `user`, and on to `dest` if it is a path on this application,
/// else to `/`.
fn signed_in(user: User, dest: &str) -> AppResponse {
    let mut out = AppResponse::redirect(safe_redirect(Some(dest).filter(|d| !d.is_empty())));
    out.session = SessionAction::Start(user);
    out
}

/// A field of the request: the form's (or the JSON body's) on a POST, the
/// query's on a GET. Empty when there is none.
fn field_of(req: &AppRequest, name: &str) -> String {
    match body_json(&req.body).get(name) {
        Some(Json::String(value)) => value.clone(),
        _ => req.query.get(name).cloned().unwrap_or_default(),
    }
}

/// Whether the request's body has the field at all.
fn has_field(req: &AppRequest, name: &str) -> bool {
    body_json(&req.body).get(name).is_some()
}

/// Where an anonymous viewer is sent to sign in (7.1): `/auth/login`, with the
/// path and query they asked for as `dest`, so the sign-in lands them back on
/// it.
fn login_location(req: &AppRequest) -> String {
    let mut dest = req.path.clone();
    if !req.query.is_empty() {
        dest.push('?');
        dest.push_str(
            &req.query
                .iter()
                .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
                .collect::<Vec<_>>()
                .join("&"),
        );
    }
    format!("/auth/login?dest={}", percent_encode(&dest))
}

/// `/files/serve/*`: a file from one of `app`'s stores, under the store's and
/// the path's access rules and the owner rule (TODO F2, §2.1), for `user`.
/// `/files/serve/<store>/<path>` names the store; a path whose first segment is
/// not one of the application's stores is in its first store, which is where
/// v1's one file area lands.
///
/// A file the viewer may not read is not found, as it is to the REST provider:
/// "forbidden" would say it exists. A readable one is answered as a
/// [`ServeFile`](sc_files::ServeFile), which the router serves through its one
/// file response builder — streamed, with Range and the file's own name.
pub async fn serve_store_file(
    app: &Application,
    cat: &Catalog,
    user: Option<&User>,
    rest: &[&str],
) -> Result<AppResponse> {
    let parts: Vec<String> = rest.iter().map(|p| percent_decode(p)).collect();
    // No dotfile and no dot-directory, whatever the rules on the files say: a
    // store that is a checkout holds `.git` and `.env` beside its content, and
    // nothing Saltcorn stores (uploads are named with a UUID prefix) starts
    // with a dot.
    if !confined(&parts) || parts.iter().any(|p| p.starts_with('.')) {
        return Ok(AppResponse::not_found());
    }
    let stores = &app.file_stores;
    let (store_name, path) = match stores.iter().find(|s| parts.len() > 1 && s.0 == parts[0]) {
        Some(store) => (store.0.clone(), parts[1..].join("/")),
        None => match stores.first() {
            Some(store) => (store.0.clone(), parts.join("/")),
            None => return Ok(AppResponse::not_found()),
        },
    };
    let Ok(store) = cat.require_file_store(&store_name) else {
        return Ok(AppResponse::not_found());
    };
    let floor = sc_catalog::load_file_store_by_name(cat, &store_name)
        .await?
        .and_then(|def| def.min_role);
    let caller = sc_files::FileCaller::new(
        user.map_or(ROLE_PUBLIC, |u| u.role),
        user.map(|u| u.id.to_string()),
    );
    if sc_files::check_access(store.as_ref(), floor, &path, &caller)
        .await
        .is_err()
    {
        return Ok(AppResponse::not_found());
    }
    Ok(AppResponse::serve_file(sc_files::ServeFile::new(
        store, path,
    )))
}

fn role_of(req: &AppRequest) -> u8 {
    req.user.as_ref().map_or(ROLE_PUBLIC, |u| u.role)
}

fn is_xhr(req: &AppRequest) -> bool {
    req.header("x-requested-with")
        .is_some_and(|v| v.eq_ignore_ascii_case("XMLHttpRequest"))
}

/// A view's state: the query, and the slug's parts under the fields v1's
/// `slug.steps` name, in order.
fn state_of(req: &AppRequest, view: &View, slug: &[&str]) -> Json {
    let mut state: Map<String, Json> = req
        .query
        .iter()
        .map(|(k, v)| (k.clone(), Json::String(v.clone())))
        .collect();
    let steps = view
        .slug
        .as_ref()
        .and_then(|s| s.get("steps"))
        .and_then(Json::as_array);
    for (step, part) in steps.into_iter().flatten().zip(slug) {
        if let Some(field) = step.get("field").and_then(Json::as_str) {
            state.insert(field.to_owned(), Json::String(percent_decode(part)));
        }
    }
    Json::Object(state)
}

/// `/plugins/public/:plugin/*`: a file of an installed plugin's `public/`
/// (11.2). Public, as v1's is: the headers that point here are in the head of a
/// page anybody may be shown. Long-cached only when the URL names the version
/// installed, so an upgraded plugin is not served stale under its old tag.
fn plugin_asset(plugin: &str, rest: &[&str]) -> AppResponse {
    let parts: Vec<String> = rest.iter().map(|p| percent_decode(p)).collect();
    let Some((file, current)) = plugin_public_file(&installed_plugin_assets(), plugin, &parts)
    else {
        return AppResponse::not_found();
    };
    match std::fs::read(&file) {
        Ok(bytes) => AppResponse::ok(
            sc_app::asset_content_type(&parts.join("/")),
            Bytes::from(bytes),
        )
        .header(
            "Cache-Control",
            if current {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
        ),
        Err(_) => AppResponse::not_found(),
    }
}

/// Whether decoded path parts stay inside the directory they are joined onto.
pub(crate) fn confined(parts: &[String]) -> bool {
    parts
        .iter()
        .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains(['/', '\\', '\0']))
}

/// Decode a path segment's `%XX` escapes, as UTF-8. A malformed escape stays as
/// it is; `+` is a plus (it is a space only in a query).
pub(crate) fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encode a name as a path segment, as v1's `encodeURIComponent` does.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(char::from(byte)),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Escape text for HTML content and attribute values.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn poisoned() -> Error {
    Error::msg("the Saltcorn UI snapshot lock is poisoned")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use sc_app::FrameworkRef;
    use serde_json::json;

    #[test]
    fn the_csp_widens_script_src_and_nothing_else() {
        let csp = saltcorn_ui_csp().header_value();
        assert!(csp.contains("default-src 'self'"), "{csp}");
        assert!(csp.contains("script-src 'self' 'unsafe-inline'"), "{csp}");
        assert!(!csp.contains("unsafe-eval"), "{csp}");
        assert!(!csp.contains("blob:"), "{csp}");
        assert_eq!(csp.matches("unsafe-inline").count(), 1, "{csp}");
    }

    #[test]
    fn the_settings_validate_as_every_frameworks_do() {
        let spec = saltcorn_ui_config_spec();
        let names: Vec<&str> = spec.iter().map(|f| f.name()).collect();
        assert_eq!(
            &names[..3],
            [CFG_SITE_NAME, MENU_CONFIG_KEY, CFG_ROOT_PAGES]
        );
        assert!(spec.iter().all(|f| !f.base.label.is_empty()));
        for derived in DERIVED_CONFIG_KEYS {
            assert!(!names.contains(&derived), "{derived} is not a setting");
        }

        let fw = FrameworkRef::new(SALTCORN_UI_FRAMEWORK)
            .with(CFG_SITE_NAME, "Books")
            .with(
                MENU_CONFIG_KEY,
                json!([{ "type": "View", "label": "Books", "viewname": "List Books" }]),
            )
            .with(CFG_ROOT_PAGES, json!({ "1": "Dashboard" }));
        sc_types::validate_attrs(&spec, &fw.config).unwrap();
        check_saltcorn_ui_config(&fw.config).unwrap();

        let bad_menu = fw
            .clone()
            .with(MENU_CONFIG_KEY, json!([{ "label": "Books" }]));
        let msg = check_saltcorn_ui_config(&bad_menu.config)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("menu_items[0]"), "{msg}");
        let bad_nesting = fw.clone().with(
            MENU_CONFIG_KEY,
            json!([{ "type": "Header", "subitems": [{}] }]),
        );
        let msg = check_saltcorn_ui_config(&bad_nesting.config)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("menu_items[0].subitems[0]"), "{msg}");
        let bad_roots = fw
            .clone()
            .with(CFG_ROOT_PAGES, json!({ "admin": "Dashboard" }));
        let msg = check_saltcorn_ui_config(&bad_roots.config)
            .unwrap_err()
            .to_string();
        assert!(msg.contains(CFG_ROOT_PAGES), "{msg}");

        // Sign-up (7.3) makes any role but the administrator's.
        let signup = fw
            .clone()
            .with(CFG_ALLOW_SIGNUP, true)
            .with(CFG_NEW_USER_ROLE, 40);
        sc_types::validate_attrs(&spec, &signup.config).unwrap();
        check_saltcorn_ui_config(&signup.config).unwrap();
        for bad in [json!(1), json!(0), json!(101), json!("80")] {
            let config = fw.clone().with(CFG_NEW_USER_ROLE, bad.clone()).config;
            let msg = check_saltcorn_ui_config(&config).unwrap_err().to_string();
            assert!(msg.contains("cannot be an administrator"), "{bad}: {msg}");
        }
    }

    /// 7.1: an anonymous viewer is sent to sign in with the way back — the path
    /// as it arrived and the query re-encoded — as one `dest` parameter.
    #[test]
    fn an_anonymous_viewer_is_sent_to_sign_in_with_the_way_back() {
        let mut req = AppRequest::get("/view/List%20Books");
        assert_eq!(
            login_location(&req),
            "/auth/login?dest=%2Fview%2FList%2520Books"
        );
        req.query.insert("author".into(), "1".into());
        req.query.insert("q".into(), "a&b".into());
        let location = login_location(&req);
        assert_eq!(
            location,
            "/auth/login?dest=%2Fview%2FList%2520Books%3Fauthor%3D1%26q%3Da%2526b"
        );
        // What the form posts back is the decoded `dest`, which the sign-in
        // redirects to as it stands: still a path on this application.
        let dest = percent_decode(location.split_once("dest=").unwrap().1);
        assert_eq!(dest, "/view/List%20Books?author=1&q=a%26b");
        assert_eq!(safe_redirect(Some(&dest)), dest);
    }

    #[test]
    fn a_field_is_the_forms_on_a_post_and_the_querys_on_a_get() {
        let mut get = AppRequest::get("/auth/login");
        get.query.insert("dest".into(), "/page/Home".into());
        assert_eq!(field_of(&get, "dest"), "/page/Home");
        assert_eq!(field_of(&get, "email"), "");
        assert!(!has_field(&get, "dest"));

        let mut post = AppRequest::new(Method::Post, "/auth/signup");
        post.body = RequestBody::Form(vec![
            ("email".into(), "ada@example.com".into()),
            ("passwordRepeat".into(), String::new()),
        ]);
        assert_eq!(field_of(&post, "email"), "ada@example.com");
        assert!(has_field(&post, "passwordRepeat"));
        assert!(!has_field(&post, "password"));
    }

    #[test]
    fn the_factory_is_in_the_registry_once_installed() {
        install_saltcorn_ui().unwrap();
        install_saltcorn_ui().unwrap();
        let listed: Vec<String> = sc_app::registered_frameworks();
        assert_eq!(
            listed
                .iter()
                .filter(|n| *n == SALTCORN_UI_FRAMEWORK)
                .count(),
            1,
            "{listed:?}"
        );
        assert_eq!(
            sc_app::framework_default_csp(SALTCORN_UI_FRAMEWORK),
            saltcorn_ui_csp()
        );
        assert_eq!(
            sc_app::framework_config_spec(SALTCORN_UI_FRAMEWORK).unwrap(),
            saltcorn_ui_config_spec()
        );
        assert!(sc_app::framework_serves_ui(SALTCORN_UI_FRAMEWORK));
        // There is no source tree to code in, so there is no builder agent.
        let app = Application::new("Books", "books", FrameworkRef::new(SALTCORN_UI_FRAMEWORK));
        assert!(sc_app::framework_builder_agent(&app.framework, &app).is_none());
        // And nothing to build from a store.
        assert!(sc_app::app_source_from_config(&app.framework).is_err());
        // The structural check runs the framework's own.
        let bad = FrameworkRef::new(SALTCORN_UI_FRAMEWORK).with(MENU_CONFIG_KEY, json!({}));
        assert!(sc_app::validate_framework_config_structure(&bad).is_err());
    }

    #[test]
    fn a_slug_fills_the_fields_its_steps_name() {
        let app = sc_app::AppId::new();
        let mut view = View::new(app, "Show Book", "Show", "books");
        view.slug = Some(json!({ "label": "", "steps": [{ "field": "title", "unique": true }] }));
        let mut req = AppRequest::get("/view/Show%20Book/Dune%20Messiah");
        req.query.insert("id".to_owned(), "3".to_owned());
        assert_eq!(
            state_of(&req, &view, &["Dune%20Messiah", "extra"]),
            json!({ "id": "3", "title": "Dune Messiah" })
        );
    }

    #[test]
    fn a_forms_fields_are_req_body_with_a_repeated_field_as_a_list() {
        let body = RequestBody::Form(vec![
            ("title".to_owned(), "Dune".to_owned()),
            ("tag".to_owned(), "a".to_owned()),
            ("tag".to_owned(), "b".to_owned()),
            ("tag".to_owned(), "c".to_owned()),
            ("_csrf".to_owned(), "t0k".to_owned()),
        ]);
        assert_eq!(
            body_json(&body),
            json!({ "title": "Dune", "tag": ["a", "b", "c"], "_csrf": "t0k" })
        );
        assert_eq!(body_json(&RequestBody::Empty), Json::Null);
        assert_eq!(
            body_json(&RequestBody::Json(json!({ "rndid": "ce2dfa" }))),
            json!({ "rndid": "ce2dfa" })
        );
    }

    #[test]
    fn a_redirect_after_a_post_stays_on_the_application() {
        assert_eq!(
            safe_redirect(Some("/view/List Books?author=1")),
            "/view/List%20Books?author=1"
        );
        assert_eq!(safe_redirect(Some("/view/Caf%C3%A9")), "/view/Caf%C3%A9");
        assert_eq!(safe_redirect(Some("/view/Café")), "/view/Caf%C3%A9");
        for bad in [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "view/x",
            "",
        ] {
            assert_eq!(safe_redirect(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(safe_redirect(None), "/");
    }

    #[test]
    fn paths_are_decoded_and_confined() {
        assert_eq!(percent_decode("List%20Books"), "List Books");
        assert_eq!(percent_decode("caf%C3%A9+%zz"), "café+%zz");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_encode("List Books/é"), "List%20Books%2F%C3%A9");
        assert!(confined(&["css".into(), "all.min.css".into()]));
        for bad in ["..", ".", "", "a\\b", "a/b"] {
            assert!(!confined(&["x".into(), bad.to_owned()]), "{bad:?}");
        }
    }

    #[test]
    fn the_v1_page_globals_are_the_line_v1s_wrapper_writes() {
        // Byte for byte what the document carried before the globals moved into
        // their own function; the golden files are fragments and would not see it.
        let v1_line = [
            "<script>var _sc_globalCsrf = \"abc123\"; ", // v1
            &format!("var _sc_version_tag = \"{ASSET_VERSION_TAG}\"; "), // v1
            "var _sc_pageloadtag = \"\"; ",              // v1
            "var _sc_loglevel = 1; ",                    // v1
            "var _sc_lightmode = \"light\";</script>\n", // v1
        ]
        .concat();
        assert_eq!(v1_page_globals("abc123"), v1_line);
        // A token is hex, and nothing else in it reaches the script.
        assert!(v1_page_globals("ab\"</script>").contains("_sc_globalCsrf = \"abscript\";")); // v1
    }
}
