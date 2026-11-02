//! [`ViewRuntime`]: the seam a view is rendered through (TODO "Saltcorn UI" §3,
//! Phase 3).
//!
//! A view pattern is v1's own JavaScript, and it runs on the module worker this
//! process already has (`sc-module`), so it is declared here and implemented one
//! layer up — the arrangement `sc-catalog` has with `TableProviderHost` and
//! `sc-app` with `FrameworkHost`. Nothing at this layer knows a worker exists.
//!
//! What crosses the seam is **the name of a view and data**. The view itself —
//! its pattern, its configuration, the views it embeds — is read out of the
//! [`ViewSnapshot`] the call carries, because an embedded view recurses inside
//! the worker and has to find its neighbours the way the outer one was found.
//!
//! Every call carries the viewer's surfaces ([`CodeHosts`]) and the schema
//! snapshot, so every read a pattern makes is a read under the viewer's
//! authority (§11). The seam does not decide that authority; its caller does.

use sc_types::Role;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_expr::{CodeHosts, SchemaSnapshot};
use sc_types::{Attrs, FormField};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::snapshot::ViewSnapshot;
use crate::view::{Page, View};

/// One view pattern as the runtime describes it — the registry manifest that
/// crosses **once**, when the runtime is first asked (§3.3).
///
/// Data only, because the admin UI asks these on the path that renders a form.
/// A configuration step's **fields** are not here: a step's form does not exist
/// without a table and the context gathered before it (§6), so it is a call —
/// [`ViewRuntime::config_step`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternManifest {
    /// v1's name for it — `List`, `Show` — and what a view's `viewpattern` holds.
    pub name: String,
    /// What the pattern picker shows. v1 patterns have no label of their own, so
    /// this is the name unless the pattern declares one.
    #[serde(default)]
    pub label: String,
    /// v1's one-line description.
    #[serde(default)]
    pub description: String,
    /// Whether a view of it must name a table (v1's `tableless`, inverted).
    #[serde(default)]
    pub table_required: bool,
    /// v1's `view_quantity`: `One`, `Many`, `ZeroOrOne`, or none declared.
    #[serde(default)]
    pub view_quantity: Option<String>,
    /// The routes it answers at `POST /view/:name/:route`.
    #[serde(default)]
    pub routes: Vec<String>,
    /// The names of its `configuration_workflow` steps, in order.
    #[serde(default)]
    pub steps: Vec<String>,
}

/// v1's `req.user`: who is looking, in the shape a pattern reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewUser {
    /// The user's id. This server's ids are UUIDs; a pattern compares them and
    /// never does arithmetic on them.
    pub id: String,
    /// The user's email.
    pub email: String,
    /// The user's role on the `1..=100` scale (v1's `role_id`).
    pub role_id: Role,
}

/// The request a view is rendered for: v1's `req`, as data.
///
/// The fields v1's patterns read and nothing else. Built by the framework from
/// the application request (§8), turned back into a `req`/`res` pair on the
/// worker, and read back out of it after the call ([`ViewOutput`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewRequest {
    /// `GET`, `POST`.
    pub method: String,
    /// The path the request arrived at, under the application.
    pub path: String,
    /// v1's `req.query`.
    pub query: BTreeMap<String, String>,
    /// v1's `req.body`: the form's fields, or the JSON posted. `null` for none.
    pub body: Json,
    /// The few headers v1 reads — `referer`, `x-requested-with` — lower-cased.
    pub headers: BTreeMap<String, String>,
    /// v1's `req.user`; `None` for an anonymous viewer.
    pub user: Option<ViewUser>,
    /// v1's `req.get_base_url()`: the application's own origin.
    pub base_url: String,
    /// What `req.csrfToken()` answers.
    pub csrf_token: String,
    /// Wrap what the call renders in the application's layout — the navbar,
    /// the menu, the alerts (§9) — or `None` for the HTML alone, which is what
    /// an ajax reload of a view asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap: Option<Wrap>,
    /// The locale this request is served in (§16.1, D8), which is what v1's
    /// `req.getLocale()` answers. `None` is English.
    ///
    /// Carried on the request rather than reached for, like every other locale
    /// in this system: a trigger emailing a customer translates against *that
    /// customer's* language, and an ambient locale is the mechanism that gets
    /// that wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// The application's catalogue for that locale, flat — what v1's `req.__`
    /// and `translateLayout` look a phrase up in.
    ///
    /// Sent with the request because a worker call is one round trip and the
    /// catalogue is small: the alternative is the worker asking back for every
    /// phrase of a page. **Strings only**: v1's `__` has no plural forms, only
    /// positional `%s`, so a plural entry in the catalogue is left out and its
    /// English renders.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub messages: BTreeMap<String, String>,
}

/// How a rendered view or page is wrapped in the layout (§9): v1's
/// `res.sendWrap(title, …)`, decided before the call rather than inside it, so
/// the layout is drawn in the same worker call as what it wraps.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wrap {
    /// The document's title.
    pub title: String,
    /// The path the navbar marks as current.
    pub current_url: String,
    /// Leave the navbar out: a page's `no_menu` attribute, which v1's page
    /// route passes to `sendWrap` (The builder, 3.2).
    #[serde(default)]
    pub no_menu: bool,
    /// Draw the layout's container fluid: a page's `request_fluid_layout`
    /// attribute, v1's `requestFluidLayout` (The builder, 3.2).
    #[serde(default)]
    pub fluid: bool,
}

/// Everything one call through the seam may reach.
pub struct ViewContext<'a> {
    /// The application's views and pages, as the worker sees them (§4).
    pub snapshot: &'a ViewSnapshot,
    /// The request.
    pub request: &'a ViewRequest,
    /// The viewer's surfaces: every read and write the pattern makes goes
    /// through these, under the viewer's authority (§11).
    pub hosts: CodeHosts<'a>,
    /// The tables, as v1's synchronous `Table.findOne` answers them.
    pub schema: Option<&'a SchemaSnapshot>,
}

impl<'a> ViewContext<'a> {
    /// A context with no surfaces and no schema — which can render nothing that
    /// reads a table, and says so by name. What a test of the seam itself uses.
    pub fn bare(snapshot: &'a ViewSnapshot, request: &'a ViewRequest) -> ViewContext<'a> {
        ViewContext {
            snapshot,
            request,
            hosts: CodeHosts::default(),
            schema: None,
        }
    }
}

/// One flash message a pattern set with `req.flash(type, message)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flash {
    /// v1's type: `success`, `warning`, `danger`.
    pub kind: String,
    /// The message.
    pub message: String,
}

/// What a call through the seam produced: what the pattern returned, and what it
/// did to `res` on the way.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ViewOutput {
    /// What the pattern returned — the HTML of a run, whatever a POST or a
    /// route answered. `null` for nothing.
    pub body: Json,
    /// `res.status(code)`, if it was called.
    pub status: Option<u16>,
    /// `res.redirect(url)`, if it was called.
    pub redirect: Option<String>,
    /// `res.json(value)`, if it was called.
    pub json: Option<Json>,
    /// `res.send(value)` / `res.sendWrap(title, body)`, if either was called.
    pub sent: Option<Json>,
    /// Every `res.set(name, value)`, in order — v1's route sets `Page-Title`.
    pub headers: Vec<(String, String)>,
    /// Every `req.flash`, in order.
    pub flashes: Vec<Flash>,
    /// The patterns the call ran — the view's own and every view it embeds, in
    /// the order they ran — which is what decides the plugin headers the
    /// document gets (11.3).
    pub patterns: Vec<String>,
}

/// One step of a pattern's `configuration_workflow`, over the context gathered
/// so far.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigStep {
    /// The step's name.
    pub name: String,
    /// How many steps the workflow has, so a wizard knows where it is.
    pub count: usize,
    /// Whether the step is a drag-and-drop **builder** (a layout) rather than a
    /// form.
    pub builder: bool,
    /// For a builder step, the options object v1 passes to
    /// `builder.renderBuilder`: the step's `builder(context)` and what
    /// `Workflow.runStep` adds, computed in the worker by v1's code (TODO "The
    /// builder" §5). `None` for a form step and a skipped one. This server never
    /// looks inside it; it is handed to the builder whole.
    pub builder_options: Option<Json>,
    /// Whether v1 skips the step for this context: its `onlyWhen` answered
    /// false (Edit's *Fixed and blocked fields* when every field is on the
    /// form). A skipped step has no form.
    pub skip: bool,
    /// Where the step's values land: under this key of the configuration (v1's
    /// `contextField` — List's *Default state* is `default_state`), or at its
    /// top level when `None`.
    pub context_field: Option<String>,
    /// The sentence the step's form opens with (v1's `blurb`), if it has one.
    pub blurb: Option<String>,
    /// The step's form as this server's form vocabulary — what the admin UI
    /// renders, and what a saved configuration is checked against.
    pub fields: Vec<FormField>,
    /// The values the form opens with, keyed by field name: what the context
    /// already says, else the form's own.
    pub values: Attrs,
    /// What the translation into [`fields`](ConfigStep::fields) could not
    /// express faithfully, each naming the field.
    pub issues: Vec<String>,
    /// The step's v1 form as data — v1's own `configFields` shape — or `null`
    /// for a builder step and a skipped one.
    pub form: Json,
}

/// What refers to one view by name (TODO "Saltcorn UI" 10.4): v1's
/// `View.inbound_connected_objects`, and the pages whose layout shows or links
/// to it. Every name here stops finding the view if it is renamed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewReferences {
    /// The views that embed it.
    #[serde(default)]
    pub embedded_in: Vec<String>,
    /// The views that link to it.
    #[serde(default)]
    pub linked_from: Vec<String>,
    /// The pages that show it or link to it.
    #[serde(default)]
    pub pages: Vec<String>,
}

impl ViewReferences {
    /// Whether nothing refers to the view.
    pub fn is_empty(&self) -> bool {
        self.embedded_in.is_empty() && self.linked_from.is_empty() && self.pages.is_empty()
    }
}

/// The seam a view is rendered through.
///
/// Object-safe, so the server holds one `Arc<dyn ViewRuntime>` and a test can
/// hold a fake.
#[async_trait]
pub trait ViewRuntime: Send + Sync {
    /// Every pattern the runtime has. Asked of the runtime once and kept; a
    /// runtime that has not started yet starts on this call.
    async fn patterns(&self) -> Result<Vec<PatternManifest>>;

    /// v1's `View.run`: the view's HTML for `state`.
    async fn render(&self, view: &View, state: &Json, ctx: ViewContext<'_>) -> Result<ViewOutput>;

    /// v1's `Page.run` and `renderLayout`: the page's HTML, with every view it
    /// embeds rendered in place.
    async fn render_page(&self, page: &Page, ctx: ViewContext<'_>) -> Result<ViewOutput>;

    /// v1's `POST /page/:name/action/:rndid`: run the `action` segment of the
    /// page's layout whose `rndid` is `rndid` — found inside the library items
    /// the layout places too — and answer v1's JSON: `{ success: "ok", … }`,
    /// `{ error }` with 400 when the action failed, or 404 "Action not found".
    async fn page_action(
        &self,
        page: &Page,
        rndid: &str,
        ctx: ViewContext<'_>,
    ) -> Result<ViewOutput>;

    /// v1's `View.runPost`: a form posted to the view.
    async fn post(&self, view: &View, body: &Json, ctx: ViewContext<'_>) -> Result<ViewOutput>;

    /// v1's `View.runRoute`: one of the pattern's named routes.
    async fn route(
        &self,
        view: &View,
        route: &str,
        body: &Json,
        ctx: ViewContext<'_>,
    ) -> Result<ViewOutput>;

    /// One step of `pattern`'s configuration workflow over `table`, for the view
    /// named `view`, with the `context` the earlier steps gathered — a call per
    /// step, because a step's form does not exist without that context (§6).
    /// `view` is v1's `viewname`: a step lists the views it may name, and leaves
    /// the one being configured out.
    async fn config_step(
        &self,
        pattern: &str,
        table: Option<&str>,
        view: Option<&str>,
        step: usize,
        context: &Json,
        ctx: ViewContext<'_>,
    ) -> Result<ConfigStep>;

    /// The configuration a new view of `pattern` over `table` starts with — v1's
    /// `initial_config`, which for a List is its table's columns — or an empty
    /// one for a pattern that declares none (TODO "Saltcorn UI" 10.2).
    async fn initial_config(
        &self,
        pattern: &str,
        table: Option<&str>,
        view: Option<&str>,
        ctx: ViewContext<'_>,
    ) -> Result<Attrs>;

    /// What in the snapshot's application refers to the view named `view`, by
    /// the patterns' own `connectedObjects` (TODO "Saltcorn UI" 10.4).
    async fn references(&self, view: &str, ctx: ViewContext<'_>) -> Result<ViewReferences>;

    /// v1's `getStringsForI18n`: the strings a view's own configuration puts in
    /// front of a person — a column's header, a link's text, an action's label
    /// (§16.1, task 4.5).
    ///
    /// These are **type B** strings, the admin's: they were written when the
    /// view was configured, so they cannot ship in any catalogue of ours, and
    /// they are not in a repository either because a Saltcorn UI application's
    /// definition is rows. This is the Translations screen's equivalent of the
    /// tree-sitter extraction a code application gets, and it is a method on
    /// the runtime because only the pattern knows which of its configuration's
    /// values are sentences and which are column names.
    ///
    /// A pattern that declares no `getStringsForI18n` contributes nothing,
    /// exactly as in v1.
    async fn strings_for_i18n(&self, view: &View, ctx: ViewContext<'_>) -> Result<Vec<String>>;

    /// The options v1's builder is opened with for `page`: v1's
    /// `pageBuilderData`, ported into the worker and computed over the snapshot
    /// (TODO "The builder" 5.5). Like a builder step's options, handed on whole
    /// and never looked inside.
    async fn page_builder_options(&self, page: &Page, ctx: ViewContext<'_>) -> Result<Json>;

    /// v1's `POST /field/preview/:table/:field/:fieldview`
    /// (`server/routes/fields.ts`; TODO "The builder" §10): the fieldview
    /// `fieldview` of `field` — a field of `table`, or `key.field` through one
    /// of its keys — rendered over the first row the caller can read, for the
    /// builder's canvas. `body` is v1's `{ configuration, row_id }`. The HTML
    /// v1's route sends, empty where it sends nothing.
    async fn builder_field_preview(
        &self,
        table: &str,
        field: &str,
        fieldview: &str,
        body: &Json,
        ctx: ViewContext<'_>,
    ) -> Result<String>;

    /// v1's `POST /field/fieldviewcfgform/:table?accept=json`: a fieldview's
    /// `configFields` as v1's form JSON. `body` is v1's (`field_name`,
    /// `fieldview`, `type`, `join_field`, `agg_outcome_type`, …).
    async fn builder_fieldview_config(
        &self,
        table: &str,
        body: &Json,
        ctx: ViewContext<'_>,
    ) -> Result<Json>;

    /// v1's `POST /view/:name/preview`: the view `view` rendered with `state`
    /// for the canvas, each required state field it lacks taken from the first
    /// row the caller can read.
    async fn builder_view_preview(
        &self,
        view: &str,
        state: &Json,
        ctx: ViewContext<'_>,
    ) -> Result<String>;

    /// v1's `POST /page/:name/preview`: the page `page` rendered for the canvas,
    /// with no layout around it.
    async fn builder_page_preview(&self, page: &str, ctx: ViewContext<'_>) -> Result<String>;

    /// v1's `GET /api/:table/distinct/:field`, as the builder's *Tabs* element
    /// asks it: `{ success: [...] }`, over the application's tables only.
    async fn builder_distinct_values(
        &self,
        table: &str,
        field: &str,
        ctx: ViewContext<'_>,
    ) -> Result<Json>;
}

/// The installed runtime.
///
/// A global for the reason `sc-app`'s framework registry is one: there is one
/// view runtime per server, and threading it through every mount would put it
/// in the signature of most of the application layer for the benefit of a
/// lookup.
static INSTALLED: RwLock<Option<Arc<dyn ViewRuntime>>> = RwLock::new(None);

/// Install the view runtime, replacing whatever was installed.
pub fn install_view_runtime(runtime: Arc<dyn ViewRuntime>) -> Result<()> {
    let mut guard = INSTALLED
        .write()
        .map_err(|_| Error::msg("the view runtime registry lock is poisoned"))?;
    *guard = Some(runtime);
    Ok(())
}

/// The installed view runtime, or the sentence saying there is none.
///
/// A server built without the Saltcorn UI bundle installs none, and an
/// application that needs one is refused on mount (`require_view_runtime`); this
/// is the same fact reached from the other side.
pub fn view_runtime() -> Result<Arc<dyn ViewRuntime>> {
    let guard = match INSTALLED.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clone().ok_or_else(|| {
        Error::config(
            "no view runtime is running on this server, so no Saltcorn UI view can be \
             rendered; it is started with the modules, from the Saltcorn UI bundle",
        )
    })
}
